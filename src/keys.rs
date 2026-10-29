//! `reiki keys`: key health from a running instance or from the config's key files.

use std::fmt::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// One key's health as `reiki keys` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyHealth {
    pub name: String,
    pub upstream: String,
    pub state: String,
    pub cooldown_seconds: Option<u64>,
    pub last_status_code: Option<u16>,
    pub streams: Vec<String>,
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
    let mut widths: Vec<usize> = headers.iter().map(|header| header.len()).collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.len());
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

/// Checks every configured key file without reading a key into the output.
pub fn check_files(config: &Config) -> Vec<KeyCheck> {
    config
        .keys
        .iter()
        .map(|(name, key)| {
            let file = PathBuf::from(&key.file);
            let mut issues = Vec::new();
            match std::fs::metadata(&file) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    issues.push(CheckIssue::Missing);
                }
                Err(error) => issues.push(CheckIssue::Unreadable(error.to_string())),
                Ok(metadata) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mode = metadata.permissions().mode() & 0o777;
                        if mode & 0o044 != 0 {
                            issues.push(CheckIssue::GroupOrWorldReadable(mode));
                        }
                    }
                    match std::fs::read_to_string(&file) {
                        Err(error) => issues.push(CheckIssue::Unreadable(error.to_string())),
                        Ok(text) if text.trim().is_empty() => issues.push(CheckIssue::Empty),
                        Ok(_) => {}
                    }
                }
            }
            KeyCheck {
                name: name.clone(),
                file,
                issues,
            }
        })
        .collect()
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
