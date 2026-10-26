use std::path::PathBuf;

use toml::{Table, Value};

/// Credential headers, refused in `headers` so they can never be logged.
pub const DENIED_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "x-goog-api-key",
    "cookie",
    "set-cookie",
    "x-auth-token",
];

const DEFAULT_FILE_MAX_BYTES: u64 = 50 * 1024 * 1024;
const DEFAULT_FILE_KEEP: u32 = 5;
const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES_LIMIT: usize = 16 * 1024 * 1024;
const DEFAULT_QUEUE: usize = 1024;
/// Below the 104-byte limit of the smallest platform (macOS), leaving room for the terminator.
const MAX_SOCKET_PATH: usize = 100;
const HOST_KEYS: &[&str] = &["enabled", "on_error", "dry_run"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyMode {
    Full,
    Summary,
    None,
}

/// What the plugin records about each response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseMode {
    /// Nothing.
    None,
    /// Status, duration, size, the serving key and every attempt.
    Status,
    /// Everything in `Status`, plus the response body.
    Body,
}

/// How an event-stream response body is logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseMode {
    /// An array of the parsed `data:` events.
    Events,
    /// The text, finish reason and usage of the streamed completion.
    Assembled,
    /// The stream's raw text.
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub file: Option<PathBuf>,
    pub file_max_bytes: u64,
    pub file_keep: u32,
    pub socket: Option<PathBuf>,
    pub body: BodyMode,
    pub max_body_bytes: usize,
    pub headers: Vec<String>,
    pub response: ResponseMode,
    pub redact: bool,
    pub queue: usize,
    pub dry_run: bool,
    /// Top-level keys or JSON pointers dropped from logged JSON bodies.
    pub exclude: Vec<String>,
    /// Paths replaced by a short description (tool names, a count) in logged JSON bodies.
    pub summarize: Vec<String>,
    /// Longest string kept in a logged JSON body; 0 keeps them whole.
    pub max_string_bytes: usize,
    pub sse: SseMode,
}

impl Config {
    pub fn parse(table: &Table) -> Result<Self, String> {
        const KNOWN: &[&str] = &[
            "file",
            "file_max_bytes",
            "file_keep",
            "socket",
            "body",
            "max_body_bytes",
            "headers",
            "response",
            "redact",
            "queue",
            "exclude",
            "summarize",
            "max_string_bytes",
            "sse",
        ];
        for key in table.keys() {
            if !KNOWN.contains(&key.as_str()) && !HOST_KEYS.contains(&key.as_str()) {
                return Err(format!("unknown key `{key}`"));
            }
        }

        let file = path(table, "file")?;
        let socket = path(table, "socket")?;
        if socket
            .as_ref()
            .is_some_and(|p| p.as_os_str().len() > MAX_SOCKET_PATH)
        {
            return Err(format!(
                "`socket` is longer than {MAX_SOCKET_PATH} bytes, which a Unix socket path cannot be"
            ));
        }
        if file.is_none() && socket.is_none() {
            return Err("set `file`, `socket` or both; nothing would be logged".to_owned());
        }

        let body = match string(table, "body")?.as_deref() {
            None | Some("full") => BodyMode::Full,
            Some("summary") => BodyMode::Summary,
            Some("none") => BodyMode::None,
            Some(other) => {
                return Err(format!(
                    "`body` must be \"full\", \"summary\" or \"none\", got \"{other}\""
                ))
            }
        };
        let response = match string(table, "response")?.as_deref() {
            None | Some("status") => ResponseMode::Status,
            Some("body") => ResponseMode::Body,
            Some("none") => ResponseMode::None,
            Some(other) => {
                return Err(format!(
                    "`response` must be \"status\", \"body\" or \"none\", got \"{other}\""
                ))
            }
        };

        let file_max_bytes = integer(table, "file_max_bytes", DEFAULT_FILE_MAX_BYTES, 0, u64::MAX)?;
        let file_keep = integer(table, "file_keep", u64::from(DEFAULT_FILE_KEEP), 0, 100)? as u32;
        let max_body_bytes = integer(
            table,
            "max_body_bytes",
            DEFAULT_MAX_BODY_BYTES as u64,
            1,
            MAX_BODY_BYTES_LIMIT as u64,
        )? as usize;
        let queue = integer(table, "queue", DEFAULT_QUEUE as u64, 16, 65_536)? as usize;

        let headers = headers(table)?;
        let exclude = paths(table, "exclude")?;
        let summarize = paths(table, "summarize")?;
        let max_string_bytes =
            integer(table, "max_string_bytes", 0, 0, MAX_BODY_BYTES_LIMIT as u64)? as usize;
        let sse = match string(table, "sse")?.as_deref() {
            None | Some("events") => SseMode::Events,
            Some("assembled") => SseMode::Assembled,
            Some("raw") => SseMode::Raw,
            Some(other) => {
                return Err(format!(
                    "`sse` must be \"events\", \"assembled\" or \"raw\", got \"{other}\""
                ))
            }
        };
        let dry_run = match table.get("dry_run") {
            None => false,
            Some(Value::Boolean(value)) => *value,
            Some(other) => {
                return Err(format!(
                    "`dry_run` must be a boolean, got {}",
                    other.type_str()
                ))
            }
        };
        let redact = match table.get("redact") {
            None => true,
            Some(Value::Boolean(value)) => *value,
            Some(other) => {
                return Err(format!(
                    "`redact` must be a boolean, got {}",
                    other.type_str()
                ))
            }
        };

        Ok(Self {
            file,
            file_max_bytes,
            file_keep,
            socket,
            body,
            max_body_bytes,
            headers,
            response,
            redact,
            queue,
            dry_run,
            exclude,
            summarize,
            max_string_bytes,
            sse,
        })
    }
}

fn string(table: &Table, key: &str) -> Result<Option<String>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(other) => Err(format!(
            "`{key}` must be a string, got {}",
            other.type_str()
        )),
    }
}

fn path(table: &Table, key: &str) -> Result<Option<PathBuf>, String> {
    let Some(text) = string(table, key)? else {
        return Ok(None);
    };
    let path = PathBuf::from(text);
    if !path.is_absolute() {
        return Err(format!("`{key}` must be an absolute path"));
    }
    Ok(Some(path))
}

fn integer(table: &Table, key: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    match table.get(key) {
        None => Ok(default),
        Some(Value::Integer(value)) => u64::try_from(*value)
            .ok()
            .filter(|v| (min..=max).contains(v))
            .ok_or_else(|| format!("`{key}` must be between {min} and {max}, got {value}")),
        Some(other) => Err(format!(
            "`{key}` must be an integer, got {}",
            other.type_str()
        )),
    }
}

/// A list of top-level keys or JSON pointers.
fn paths(table: &Table, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = table.get(key) else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err(format!(
            "`{key}` must be an array of strings, got {}",
            value.type_str()
        ));
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(path) if !path.is_empty() && path != "/" => Ok(path.clone()),
            _ => Err(format!(
                "`{key}` must hold non-empty strings: a key like \"tools\" or a pointer like \"/messages/0\""
            )),
        })
        .collect()
}

fn headers(table: &Table) -> Result<Vec<String>, String> {
    let Some(value) = table.get("headers") else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err(format!(
            "`headers` must be an array of strings, got {}",
            value.type_str()
        ));
    };
    let mut out = Vec::new();
    for item in items {
        let Value::String(name) = item else {
            return Err("`headers` must contain only strings".to_owned());
        };
        let name = name.to_ascii_lowercase();
        if DENIED_HEADERS.contains(&name.as_str()) {
            return Err(format!(
                "header `{name}` carries credentials and cannot be logged"
            ));
        }
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(format!("`{name}` is not a valid header name"));
        }
        out.push(name);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, String> {
        Config::parse(&text.parse::<Table>().unwrap())
    }

    #[test]
    fn defaults_apply_when_only_a_file_is_set() {
        let config = parse("file = \"/var/log/reiki/req.jsonl\"").unwrap();
        assert_eq!(config.body, BodyMode::Full);
        assert_eq!(config.response, ResponseMode::Status);
        assert!(config.redact);
        assert_eq!(config.file_keep, 5);
        assert_eq!(config.max_body_bytes, 65_536);
        assert!(config.headers.is_empty() && config.socket.is_none());
    }

    #[test]
    fn shaping_options_parse_and_default_to_off() {
        let config = parse("file = \"/var/log/reiki/req.jsonl\"").unwrap();
        assert!(config.exclude.is_empty() && config.summarize.is_empty());
        assert_eq!((config.max_string_bytes, config.sse), (0, SseMode::Events));
        let config = parse(
            "file = \"/var/log/reiki/req.jsonl\"\nexclude = [\"tools\", \"/messages/0\"]\nsummarize = [\"tools\"]\nmax_string_bytes = 2000\nsse = \"assembled\"",
        )
        .unwrap();
        assert_eq!(config.exclude, ["tools", "/messages/0"]);
        assert_eq!(
            (config.max_string_bytes, config.sse),
            (2000, SseMode::Assembled)
        );
        assert!(parse("file = \"/a/b\"\nexclude = [\"\"]").is_err());
        assert!(parse("file = \"/a/b\"\nsse = \"xml\"").is_err());
    }

    #[test]
    fn a_sink_is_required() {
        assert!(parse("body = \"none\"").unwrap_err().contains("nothing"));
    }

    #[test]
    fn host_keys_are_accepted_and_unknown_keys_are_not() {
        assert!(parse("socket = \"/run/r.sock\"\nenabled = true\non_error = \"reject\"").is_ok());
        assert!(parse("socket = \"/run/r.sock\"\nfiel = 1")
            .unwrap_err()
            .contains("fiel"));
    }

    #[test]
    fn a_socket_path_too_long_for_unix_is_refused_up_front() {
        let long = format!("/run/{}/r.sock", "d".repeat(100));
        assert!(parse(&format!("socket = \"{long}\""))
            .unwrap_err()
            .contains("longer"));
        let ok = format!("/run/{}/r.sock", "d".repeat(40));
        assert!(parse(&format!("socket = \"{ok}\"")).is_ok());
    }

    #[test]
    fn relative_paths_are_refused() {
        assert!(parse("file = \"req.jsonl\"")
            .unwrap_err()
            .contains("absolute"));
        assert!(parse("socket = \"run/r.sock\"")
            .unwrap_err()
            .contains("absolute"));
    }

    #[test]
    fn credential_headers_cannot_be_logged() {
        for name in [
            "Authorization",
            "x-api-key",
            "COOKIE",
            "proxy-authorization",
        ] {
            let text = format!("file = \"/x\"\nheaders = [\"{name}\"]");
            assert!(parse(&text).unwrap_err().contains("credentials"), "{name}");
        }
        let ok =
            parse("file = \"/x\"\nheaders = [\"User-Agent\", \"x-opencode-session\"]").unwrap();
        assert_eq!(ok.headers, ["user-agent", "x-opencode-session"]);
    }

    #[test]
    fn modes_and_ranges_are_validated() {
        assert!(parse("file = \"/x\"\nbody = \"all\"").is_err());
        assert!(parse("file = \"/x\"\nresponse = \"all\"").is_err());
        assert_eq!(
            parse("file = \"/x\"\nresponse = \"body\"")
                .unwrap()
                .response,
            ResponseMode::Body
        );
        assert_eq!(
            parse("file = \"/x\"\nresponse = \"none\"")
                .unwrap()
                .response,
            ResponseMode::None
        );
        assert!(parse("file = \"/x\"\nmax_body_bytes = 0").is_err());
        assert!(parse("file = \"/x\"\nmax_body_bytes = 99999999").is_err());
        assert!(parse("file = \"/x\"\nqueue = 4").is_err());
        assert!(parse("file = \"/x\"\nfile_keep = 500").is_err());
        assert!(parse("file = \"/x\"\nredact = \"yes\"").is_err());
        assert!(parse("file = \"/x\"\nfile_max_bytes = -1").is_err());
    }
}
