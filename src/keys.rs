//! `reiki keys`: key health from a running instance or from the config's key files.

use std::fmt::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// One cooling route-and-model combination on a key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyScope {
    pub route: String,
    pub model: String,
    pub retry_after_seconds: u64,
    pub cooldown_class: String,
}

/// One key's health as `reiki keys` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyHealth {
    pub name: String,
    pub upstream: String,
    pub state: String,
    pub cooldown_seconds: Option<u64>,
    pub last_status_code: Option<u16>,
    pub streams: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_class: Option<String>,
    /// Cooling combinations, soonest first; omitted when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<KeyScope>,
}

#[derive(Deserialize)]
struct Status {
    keys: Vec<StatusKey>,
    streams: Vec<StatusStream>,
}

#[derive(Deserialize)]
struct StatusKey {
    name: String,
    upstream: String,
    state: String,
    #[serde(default)]
    retry_after_seconds: Option<u64>,
    #[serde(default)]
    last_status_code: Option<u16>,
    #[serde(default)]
    cooldown_class: Option<String>,
    #[serde(default)]
    scopes: Vec<KeyScope>,
}

#[derive(Deserialize)]
struct StatusStream {
    name: String,
    chain: Vec<String>,
}

/// Turns a `/status` answer into the rows `reiki keys` shows.
pub fn parse_status(body: &str) -> Result<Vec<KeyHealth>, String> {
    let status: Status =
        serde_json::from_str(body).map_err(|e| format!("cannot read /status: {e}"))?;
    let mut keys = Vec::with_capacity(status.keys.len());
    for key in status.keys {
        let (state, cooldown_seconds) = match key.state.as_str() {
            "available" => ("ok".to_owned(), None),
            "cooling" => ("cooling".to_owned(), key.retry_after_seconds),
            "out_of_rotation" => ("disabled".to_owned(), None),
            other => return Err(format!("unknown key state `{other}` in /status")),
        };
        let streams = status
            .streams
            .iter()
            .filter(|stream| stream.chain.iter().any(|name| name == &key.name))
            .map(|stream| stream.name.clone())
            .collect();
        keys.push(KeyHealth {
            name: key.name,
            upstream: key.upstream,
            state,
            cooldown_seconds,
            last_status_code: key.last_status_code,
            streams,
            cooldown_class: key.cooldown_class,
            scopes: key.scopes,
        });
    }
    Ok(keys)
}

/// The table `reiki keys` prints.
pub fn render_table(keys: &[KeyHealth]) -> String {
    if keys.is_empty() {
        return "no keys\n".to_owned();
    }
    let headers = ["KEY", "UPSTREAM", "STATE", "COOLDOWN", "LAST", "STREAMS"];
    let rows: Vec<Vec<String>> = keys
        .iter()
        .map(|key| {
            vec![
                key.name.clone(),
                key.upstream.clone(),
                key.state.clone(),
                key.cooldown_seconds
                    .map_or_else(|| "-".to_owned(), |secs| format!("{secs}s")),
                key.last_status_code
                    .map_or_else(|| "-".to_owned(), |code| code.to_string()),
                if key.streams.is_empty() {
                    "-".to_owned()
                } else {
                    key.streams.join(",")
                },
            ]
        })
        .collect();
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let header_row: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
    write_row(&mut out, &header_row, &widths);
    for row in &rows {
        write_row(&mut out, row, &widths);
    }
    out
}

fn write_row(out: &mut String, cells: &[String], widths: &[usize]) {
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        if index + 1 == cells.len() {
            out.push_str(cell);
        } else {
            let _ = write!(out, "{cell:<width$}", width = widths[index]);
        }
    }
    out.push('\n');
}

/// The JSON document `reiki keys --json` prints.
pub fn render_json(keys: &[KeyHealth]) -> Result<String, String> {
    serde_json::to_string(&KeysOutput { keys })
        .map(|text| format!("{text}\n"))
        .map_err(|e| format!("cannot write JSON: {e}"))
}

#[derive(Serialize)]
struct KeysOutput<'a> {
    keys: &'a [KeyHealth],
}

/// One key file's offline check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyCheck {
    pub name: String,
    pub file: PathBuf,
    pub issues: Vec<CheckIssue>,
}

/// A problem or warning found by `reiki keys --check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckIssue {
    Missing,
    Unreadable(String),
    Empty,
    GroupOrWorldReadable(u32),
}

/// The permission problem with a key file whose mode is `mode`, if there is one.
/// The same verdict drives the `reiki keys --check` warning and the load-time refusal.
pub fn permission_verdict(mode: u32) -> Option<CheckIssue> {
    (mode & 0o044 != 0).then_some(CheckIssue::GroupOrWorldReadable(mode))
}

/// Checks every configured key file without reading a key into the output.
pub fn check_files(config: &Config) -> Vec<KeyCheck> {
    config
        .keys
        .iter()
        .map(|(name, key)| {
            let file = PathBuf::from(&key.file);
            let mut issues = Vec::new();
            // Metadata and contents come from one handle, so the file cannot change between
            // the permission check and the read.
            match std::fs::File::open(&file) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    issues.push(CheckIssue::Missing);
                }
                Err(error) => issues.push(CheckIssue::Unreadable(error.to_string())),
                Ok(mut handle) => match handle.metadata() {
                    Err(error) => issues.push(CheckIssue::Unreadable(error.to_string())),
                    Ok(metadata) if !metadata.is_file() => {
                        issues.push(CheckIssue::Unreadable("not a regular file".to_owned()));
                    }
                    Ok(metadata) => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let mode = metadata.permissions().mode() & 0o777;
                            if let Some(issue) = permission_verdict(mode) {
                                issues.push(issue);
                            }
                        }
                        let mut text = String::new();
                        match std::io::Read::read_to_string(&mut handle, &mut text) {
                            Err(error) => issues.push(CheckIssue::Unreadable(error.to_string())),
                            Ok(_) if text.trim().is_empty() => issues.push(CheckIssue::Empty),
                            Ok(_) => {}
                        }
                    }
                },
            }
            KeyCheck {
                name: name.clone(),
                file,
                issues,
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn write_key(path: &std::path::Path, text: &str) {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

/// True when any key file has a problem or warning.
pub fn check_has_problems(checks: &[KeyCheck]) -> bool {
    checks.iter().any(|check| !check.issues.is_empty())
}

/// The lines `reiki keys --check` prints.
pub fn render_check(checks: &[KeyCheck]) -> String {
    if checks.is_empty() {
        return "no keys\n".to_owned();
    }
    let mut out = String::new();
    for check in checks {
        if check.issues.is_empty() {
            let _ = writeln!(out, "{}: ok", check.name);
            continue;
        }
        for issue in &check.issues {
            let (level, message) = match issue {
                CheckIssue::Missing => {
                    ("error", format!("{} does not exist", check.file.display()))
                }
                CheckIssue::Unreadable(error) => (
                    "error",
                    format!("cannot read {}: {error}", check.file.display()),
                ),
                CheckIssue::Empty => ("error", format!("{} is empty", check.file.display())),
                CheckIssue::GroupOrWorldReadable(mode) => (
                    "warning",
                    format!(
                        "{} is group or world readable (mode {mode:04o})",
                        check.file.display()
                    ),
                ),
            };
            let _ = writeln!(out, "{}: {level}: {message}", check.name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health(name: &str, streams: &[&str]) -> KeyHealth {
        KeyHealth {
            name: name.to_owned(),
            upstream: "zen".to_owned(),
            state: "ok".to_owned(),
            cooldown_seconds: None,
            last_status_code: None,
            streams: streams.iter().map(|s| (*s).to_owned()).collect(),
            cooldown_class: None,
            scopes: Vec::new(),
        }
    }

    #[test]
    fn parse_status_keeps_key_order_and_lists_streams_in_status_order() {
        let body = r#"{"keys":[
            {"name":"b","upstream":"zen","state":"cooling","retry_after_seconds":7,"cooldown_class":"quota_exhausted","last_status_code":429},
            {"name":"a","upstream":"zen","state":"out_of_rotation"},
            {"name":"c","upstream":"zen","state":"available","scopes":[{"route":"/responses","model":"gpt-big","retry_after_seconds":9,"cooldown_class":"quota_exhausted"}]}],
          "streams":[{"name":"s2","chain":["a","b"]},{"name":"s1","chain":["b"]}]}"#;
        let keys = parse_status(body).unwrap();
        let names: Vec<_> = keys.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, ["b", "a", "c"]);
        assert_eq!(keys[0].state, "cooling");
        assert_eq!(keys[0].cooldown_seconds, Some(7));
        assert_eq!(keys[0].streams, ["s2", "s1"]);
        assert_eq!(keys[0].cooldown_class.as_deref(), Some("quota_exhausted"));
        assert_eq!(keys[2].cooldown_class, None);
        assert_eq!(keys[2].scopes.len(), 1);
        assert_eq!(keys[2].scopes[0].model, "gpt-big");
        assert!(keys[0].scopes.is_empty());
        assert_eq!(keys[1].state, "disabled");
        assert!(keys[2].streams.is_empty());
        assert!(parse_status(
            r#"{"keys":[{"name":"a","upstream":"u","state":"weird"}],"streams":[]}"#
        )
        .is_err());
    }

    #[test]
    fn table_columns_align_by_characters_not_bytes() {
        let table = render_table(&[health("clé-日本", &["s"]), health("abcdefg", &["s"])]);
        let columns: Vec<usize> = table
            .lines()
            .skip(1)
            .map(|line| line.chars().take_while(|c| *c != 'z').count())
            .collect();
        assert_eq!(columns, [9, 9]);
        assert!(table.contains("clé-日本   zen"), "{table}");
        assert_eq!(render_table(&[]), "no keys\n");
    }

    fn config_for(files: &[(&str, &std::path::Path)]) -> Config {
        let mut text = String::from("[upstream.zen]\nbase_url = \"http://127.0.0.1:1\"\n");
        for (name, file) in files {
            text.push_str(&format!(
                "[keys.{name}]\nupstream = \"zen\"\nfile = \"{}\"\n",
                file.display()
            ));
        }
        Config::parse(&text).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_permission_verdict_flags_group_and_world_read_but_not_owner_only() {
        assert_eq!(permission_verdict(0o600), None);
        assert_eq!(permission_verdict(0o700), None);
        assert_eq!(
            permission_verdict(0o640),
            Some(CheckIssue::GroupOrWorldReadable(0o640))
        );
        assert_eq!(
            permission_verdict(0o604),
            Some(CheckIssue::GroupOrWorldReadable(0o604))
        );
        assert_eq!(
            permission_verdict(0o644),
            Some(CheckIssue::GroupOrWorldReadable(0o644))
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_files_flags_modes_directories_empty_and_missing_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("reiki-keys-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, text: &str, mode: u32| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let good = write("good", "secret\n", 0o600);
        let loose = write("loose", "secret\n", 0o644);
        let empty = write("empty", " \n", 0o600);
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let missing = dir.join("missing");
        let weird = write("clé-日本", "secret", 0o600);
        let config = config_for(&[
            ("good", &good),
            ("loose", &loose),
            ("empty", &empty),
            ("dir", &sub),
            ("missing", &missing),
            ("weird", &weird),
        ]);
        let checks = check_files(&config);
        let by = |name: &str| checks.iter().find(|c| c.name == name).unwrap();
        assert!(by("good").issues.is_empty());
        assert_eq!(
            by("loose").issues,
            [CheckIssue::GroupOrWorldReadable(0o644)]
        );
        assert_eq!(by("empty").issues, [CheckIssue::Empty]);
        assert_eq!(by("missing").issues, [CheckIssue::Missing]);
        assert!(matches!(
            by("dir").issues.as_slice(),
            [CheckIssue::Unreadable(_)]
        ));
        assert!(by("weird").issues.is_empty());
        assert!(check_has_problems(&checks));
        let text = render_check(&checks);
        assert!(text.contains("good: ok"), "{text}");
        assert!(!text.contains("secret"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
