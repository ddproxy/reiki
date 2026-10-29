use std::path::PathBuf;

use toml::{Table, Value};

use crate::syslog::facility_code;

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
const DEFAULT_SYSLOG_FACILITY: u8 = 16;
const DEFAULT_SYSLOG_APP_NAME: &str = "reiki";
/// RFC 5424 APP-NAME is at most 48 printable ASCII characters.
const MAX_SYSLOG_APP_NAME: usize = 48;
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

/// How each log record is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Reiki's own JSON lines.
    Json,
    /// OCSF events: HTTP Activity for requests and responses, Detection Finding for findings.
    Ocsf,
}

/// Where the syslog export sends its records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyslogTarget {
    /// `host:port`, one message per datagram.
    Udp(String),
    /// `host:port`, messages framed by octet count.
    Tcp(String),
    /// A Unix datagram socket, usually `/dev/log`.
    Unix(PathBuf),
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
    pub syslog: Option<SyslogTarget>,
    /// The RFC 5424 facility code, `local0` (16) by default.
    pub syslog_facility: u8,
    pub syslog_app_name: String,
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
    pub format: LogFormat,
}

impl Config {
    pub fn parse(table: &Table) -> Result<Self, String> {
        const KNOWN: &[&str] = &[
            "file",
            "file_max_bytes",
            "file_keep",
            "socket",
            "syslog",
            "syslog_facility",
            "syslog_app_name",
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
            "format",
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
        let syslog = syslog_target(table)?;
        if file.is_none() && socket.is_none() && syslog.is_none() {
            return Err(
                "set `file`, `socket`, `syslog` or several; nothing would be logged".to_owned(),
            );
        }
        if syslog.is_none()
            && (table.contains_key("syslog_facility") || table.contains_key("syslog_app_name"))
        {
            return Err("`syslog_facility` and `syslog_app_name` need `syslog`".to_owned());
        }
        let syslog_facility = match string(table, "syslog_facility")? {
            None => DEFAULT_SYSLOG_FACILITY,
            Some(name) => facility_code(&name).ok_or_else(|| {
                format!("`syslog_facility` must be a syslog facility name such as \"local0\", got \"{name}\"")
            })?,
        };
        let syslog_app_name = match string(table, "syslog_app_name")? {
            None => DEFAULT_SYSLOG_APP_NAME.to_owned(),
            Some(name) => {
                let valid = (1..=MAX_SYSLOG_APP_NAME).contains(&name.len())
                    && name.bytes().all(|b| b.is_ascii_graphic())
                    && name != "-";
                if !valid {
                    return Err(format!(
                        "`syslog_app_name` must be 1 to {MAX_SYSLOG_APP_NAME} printable ASCII characters without spaces, got \"{name}\""
                    ));
                }
                name
            }
        };

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
        let format = match string(table, "format")?.as_deref() {
            None | Some("json") => LogFormat::Json,
            Some("ocsf") => LogFormat::Ocsf,
            Some(other) => {
                return Err(format!(
                    "`format` must be \"json\" or \"ocsf\", got \"{other}\""
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
            syslog,
            syslog_facility,
            syslog_app_name,
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
            format,
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

/// `udp://host:port`, `tcp://host:port` or `unix:///absolute/path`.
fn syslog_target(table: &Table) -> Result<Option<SyslogTarget>, String> {
    let Some(text) = string(table, "syslog")? else {
        return Ok(None);
    };
    let target = if let Some(rest) = text.strip_prefix("udp://") {
        SyslogTarget::Udp(host_port(rest)?)
    } else if let Some(rest) = text.strip_prefix("tcp://") {
        SyslogTarget::Tcp(host_port(rest)?)
    } else if let Some(rest) = text.strip_prefix("unix://") {
        let path = PathBuf::from(rest);
        if !path.is_absolute() {
            return Err("`syslog` unix path must be absolute".to_owned());
        }
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(format!(
                "`syslog` unix path is longer than {MAX_SOCKET_PATH} bytes"
            ));
        }
        SyslogTarget::Unix(path)
    } else {
        return Err(format!(
            "`syslog` must start with udp://, tcp:// or unix://, got \"{text}\""
        ));
    };
    Ok(Some(target))
}

fn host_port(rest: &str) -> Result<String, String> {
    let (host, port) = rest
        .rsplit_once(':')
        .ok_or_else(|| format!("`syslog` must be host:port, got \"{rest}\""))?;
    if host.is_empty() || host.contains(|c: char| c.is_whitespace() || c == '/') {
        return Err(format!("`syslog` has an invalid host in \"{rest}\""));
    }
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        return Err("`syslog` IPv6 addresses must be in brackets, such as [::1]:514".to_owned());
    }
    match port.parse::<u16>() {
        Ok(port) if port != 0 => Ok(format!("{host}:{port}")),
        _ => Err(format!("`syslog` port must be 1 to 65535, got \"{port}\"")),
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
    fn syslog_targets_parse_for_each_transport() {
        let udp = parse("syslog = \"udp://collector.internal:514\"").unwrap();
        assert_eq!(
            udp.syslog,
            Some(SyslogTarget::Udp("collector.internal:514".to_owned()))
        );
        let tcp = parse("syslog = \"tcp://[::1]:6514\"").unwrap();
        assert_eq!(tcp.syslog, Some(SyslogTarget::Tcp("[::1]:6514".to_owned())));
        let unix = parse("syslog = \"unix:///dev/log\"").unwrap();
        assert_eq!(
            unix.syslog,
            Some(SyslogTarget::Unix(PathBuf::from("/dev/log")))
        );
        assert_eq!(
            (unix.syslog_facility, unix.syslog_app_name.as_str()),
            (16, "reiki")
        );
    }

    #[test]
    fn a_syslog_only_config_is_a_valid_sink() {
        let config = parse("syslog = \"udp://127.0.0.1:514\"\nbody = \"none\"").unwrap();
        assert!(config.file.is_none() && config.socket.is_none());
    }

    #[test]
    fn bad_syslog_targets_are_refused() {
        for text in [
            "syslog = \"http://collector:514\"",
            "syslog = \"udp://collector\"",
            "syslog = \"udp://collector:0\"",
            "syslog = \"tcp://collector:70000\"",
            "syslog = \"udp://:514\"",
            "syslog = \"udp://::1:514\"",
            "syslog = \"unix://dev/log\"",
            "syslog = 514",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
        let long = format!("unix:///{}/log", "d".repeat(100));
        assert!(parse(&format!("syslog = \"{long}\"")).is_err());
    }

    #[test]
    fn syslog_facility_and_app_name_are_validated() {
        let config = parse(
            "syslog = \"tcp://collector:514\"\nsyslog_facility = \"local5\"\nsyslog_app_name = \"gateway\"",
        )
        .unwrap();
        assert_eq!(
            (config.syslog_facility, config.syslog_app_name.as_str()),
            (21, "gateway")
        );
        assert!(
            parse("syslog = \"tcp://c:514\"\nsyslog_facility = \"local9\"")
                .unwrap_err()
                .contains("syslog_facility")
        );
        assert!(parse("syslog = \"tcp://c:514\"\nsyslog_app_name = \"has space\"").is_err());
        assert!(parse("syslog = \"tcp://c:514\"\nsyslog_app_name = \"-\"").is_err());
        let too_long = "a".repeat(49);
        assert!(parse(&format!(
            "syslog = \"tcp://c:514\"\nsyslog_app_name = \"{too_long}\""
        ))
        .is_err());
    }

    #[test]
    fn syslog_options_need_a_syslog_target() {
        assert!(parse("file = \"/x\"\nsyslog_facility = \"local0\"")
            .unwrap_err()
            .contains("need `syslog`"));
        assert!(parse("file = \"/x\"\nsyslog_app_name = \"reiki\"").is_err());
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
