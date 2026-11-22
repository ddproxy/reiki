//! Rotating JSONL journal of flagged calls, written with owner-only permissions.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use reiki_plugin::format_utc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Entry<'a> {
    pub stream: &'a str,
    pub upstream: &'a str,
    pub tool: &'a str,
    pub rules: &'a [&'static str],
    pub severity: &'static str,
    pub action: &'static str,
    pub arguments: &'a str,
}

pub struct Journal {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max_bytes: u64,
    warned: bool,
}

fn open(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o777 != 0o600 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(file)
}

impl Journal {
    pub fn new(path: &Path, max_bytes: u64, dry_run: bool) -> Result<Self, String> {
        let mut journal = Self {
            path: path.to_owned(),
            file: None,
            size: 0,
            max_bytes,
            warned: false,
        };
        if dry_run {
            return Ok(journal);
        }
        let file = open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        journal.size = file.metadata().map(|m| m.len()).unwrap_or(0);
        journal.file = Some(file);
        Ok(journal)
    }

    pub fn write(&mut self, entry: &Entry<'_>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let stamp = format_utc(
            i64::try_from(now.as_secs()).unwrap_or(i64::MAX),
            'T',
            Some(now.subsec_millis()),
            "Z",
        );
        let line = serde_json::json!({
            "timestamp": stamp,
            "stream": entry.stream,
            "upstream": entry.upstream,
            "tool": entry.tool,
            "rules": entry.rules,
            "severity": entry.severity,
            "action": entry.action,
            "arguments": redact(entry.arguments, 2048),
        })
        .to_string();
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if self.size > 0 && self.size + line.len() as u64 + 1 > self.max_bytes {
            let rotated = rotated_path(&self.path);
            let _ = std::fs::remove_file(&rotated);
            let _ = std::fs::rename(&self.path, &rotated);
            self.size = 0;
            match open(&self.path) {
                Ok(new) => *file = new,
                Err(e) => {
                    self.warn_once(&format!("cannot reopen {}: {e}", self.path.display()));
                    return;
                }
            }
        }
        match file
            .write_all(line.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
        {
            Ok(()) => {
                self.size += line.len() as u64 + 1;
            }
            Err(e) => self.warn_once(&format!("cannot write {}: {e}", self.path.display())),
        }
    }

    fn warn_once(&mut self, reason: &str) {
        if !self.warned {
            tracing::warn!("toolpolicy journal: {reason}");
            self.warned = true;
        }
    }
}

/// Rotates to `<name>.1.jsonl` whether or not the journal had an extension.
fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("path", &self.path)
            .field("size", &self.size)
            .finish()
    }
}

/// Replaces credential-shaped spans and sensitive field values, then cuts at `limit` bytes.
pub fn redact(text: &str, limit: usize) -> String {
    let bytes = text.as_bytes();
    let mut spans: Vec<(usize, usize)> = reiki_redact::scan(text)
        .into_iter()
        .map(|span| (span.start, span.end))
        .collect();
    spans.extend(field_spans(bytes));
    spans.sort_unstable();
    let mut out = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    for (start, end) in spans {
        if start < cursor || end > bytes.len() {
            continue;
        }
        out.extend_from_slice(&bytes[cursor..start]);
        out.extend_from_slice(b"[redacted]");
        cursor = end;
    }
    out.extend_from_slice(&bytes[cursor..]);
    let mut value = String::from_utf8_lossy(&out).into_owned();
    if value.len() > limit {
        let mut cut = limit;
        while cut > 0 && !value.is_char_boundary(cut) {
            cut -= 1;
        }
        value.truncate(cut);
    }
    value
}

fn sensitive(name: &[u8]) -> bool {
    let mut normalized = String::new();
    for &b in name {
        if b.is_ascii_alphanumeric() {
            normalized.push((b as char).to_ascii_lowercase());
        }
    }
    ["key", "token", "secret", "password", "credential", "apikey"]
        .iter()
        .any(|k| normalized.contains(k))
}

/// Values of `{"api_key": "..."}` and `TOKEN=...` shaped fields.
fn field_spans(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let Some(end) = bytes[i + 1..].iter().position(|b| *b == b'"') else {
                break;
            };
            let name_end = i + 1 + end;
            let after = name_end + 1;
            let mut cursor = after;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor < bytes.len() && bytes[cursor] == b':' {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                let value_start = cursor;
                if value_start < bytes.len() && bytes[value_start] == b'"' {
                    if let Some(len) = bytes[value_start + 1..].iter().position(|b| *b == b'"') {
                        if sensitive(&bytes[i + 1..name_end]) {
                            spans.push((value_start, value_start + len + 2));
                        }
                        i = value_start + len + 2;
                        continue;
                    }
                } else {
                    while cursor < bytes.len() && !matches!(bytes[cursor], b',' | b'}' | b' ') {
                        cursor += 1;
                    }
                    if cursor > value_start && sensitive(&bytes[i + 1..name_end]) {
                        spans.push((value_start, cursor));
                    }
                }
            }
            i = name_end + 1;
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
            {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'=' && sensitive(&bytes[start..i]) {
                let value_start = i + 1;
                let mut end = value_start;
                while end < bytes.len() && !bytes[end].is_ascii_whitespace() && bytes[end] != b'&' {
                    end += 1;
                }
                if end > value_start {
                    spans.push((value_start, end));
                }
            }
            continue;
        }
        i += 1;
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_masks_key_shapes_and_sensitive_fields() {
        let text = format!(
            "token={} {{\"api_key\":\"{}\"}}",
            "sk-".to_owned() + &"a".repeat(30),
            "abcdef0123456789abcdef"
        );
        let out = redact(&text, 2048);
        assert!(
            out.contains("[redacted]"),
            "the redaction marker must be present"
        );
        assert!(
            !out.contains("sk-aaa"),
            "a credential leaked into the journal"
        );
        assert!(
            !out.contains("abcdef0123456789abcdef"),
            "a credential leaked into the journal"
        );
    }

    #[test]
    fn redaction_cuts_at_a_character_boundary() {
        assert_eq!(redact("héllo wörld", 4), "hél");
        assert_eq!(redact("plain", 100), "plain");
    }

    #[test]
    fn a_journal_rotation_keeps_owner_only_mode() {
        let dir =
            std::env::temp_dir().join(format!("reiki-toolpolicy-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("findings.jsonl");
        let mut journal = Journal::new(&path, 80, false).unwrap();
        for i in 0..8 {
            journal.write(&Entry {
                stream: "client",
                upstream: "zen",
                tool: "get_data",
                rules: &["unapproved_host"],
                severity: "high",
                action: "blocked",
                arguments: &format!("{{\"n\":{i},\"api_key\":\"abcdef0123456789abcdef\"}}"),
            });
        }
        drop(journal);
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(lines > 0 && lines < 8, "rotated: {lines}");
        assert!(dir.join("findings.jsonl.1").exists());
        let text = std::fs::read_to_string(&path).unwrap();
        let record: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(record["stream"], "client");
        assert_eq!(record["tool"], "get_data");
        assert_eq!(record["rules"][0], "unapproved_host");
        assert!(record["arguments"].as_str().unwrap().contains("[redacted]"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "journal mode was {mode:o}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dry_run_does_not_open_the_journal() {
        let dir = std::env::temp_dir().join(format!("reiki-toolpolicy-dry-{}", std::process::id()));
        let path = dir.join("findings.jsonl");
        let mut journal = Journal::new(&path, 100, true).unwrap();
        journal.write(&Entry {
            stream: "s",
            upstream: "u",
            tool: "t",
            rules: &[],
            severity: "high",
            action: "reported",
            arguments: "{}",
        });
        assert!(!path.exists());
    }
}
