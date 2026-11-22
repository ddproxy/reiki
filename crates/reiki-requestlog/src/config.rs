use std::path::PathBuf;

use reiki_plugin::HOST_KEYS;
use toml::{Table, Value};

use crate::syslog::facility_code;

/// Credential-like substrings in header names, refused in `headers` so they can never be logged.
pub const DENIED_HEADERS: &[&str] = &["auth", "token", "key", "secret", "cookie", "credential"];

const DEFAULT_FILE_MAX_BYTES: u64 = 50 * 1024 * 1024;
const DEFAULT_FILE_KEEP: u32 = 5;
const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES_LIMIT: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_CAPTURE_BYTES: usize = 1024 * 1024;
const MAX_CAPTURE_BYTES_LIMIT: usize = 64 * 1024 * 1024;
const DEFAULT_QUEUE: usize = 1024;
const DEFAULT_QUEUE_MAX_BYTES: u64 = 64 * 1024 * 1024;
const MIN_QUEUE_MAX_BYTES: u64 = 1024 * 1024;
const MAX_QUEUE_MAX_BYTES: u64 = 4096 * 1024 * 1024;
/// Below the 104-byte limit of the smallest platform (macOS), leaving room for the terminator.
const MAX_SOCKET_PATH: usize = 100;
const DEFAULT_SYSLOG_FACILITY: u8 = 16;
const DEFAULT_SYSLOG_APP_NAME: &str = "reiki";
/// RFC 5424 APP-NAME is at most 48 printable ASCII characters.
const MAX_SYSLOG_APP_NAME: usize = 48;

const DEFAULT_EXPORT_MAX_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_EXPORT_MAX_FILES: u32 = 3;
const DEFAULT_EXPORT_QUEUE: usize = 256;
const DEFAULT_EXPORT_SYSLOG_APP_NAME: &str = "reiki-export";

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

/// Wire format for summary-only export records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Jsonl,
    Syslog,
}

impl ExportFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
            Self::Syslog => "syslog",
        }
    }
}

/// Where summary-only records go, independent of the main request log sinks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportConfig {
    pub format: ExportFormat,
    pub file: Option<PathBuf>,
    pub max_bytes: u64,
    pub max_files: u32,
    pub syslog: Option<SyslogTarget>,
    pub syslog_facility: u8,
    pub syslog_app_name: String,
    pub queue: usize,
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
    /// Whether request and response bodies may be written to the sinks at all.
    pub capture_bodies: bool,
    pub body: BodyMode,
    pub max_body_bytes: usize,
    /// Most body bytes copied per request onto the writer queue.
    pub max_capture_bytes: usize,
    pub headers: Vec<String>,
    pub response: ResponseMode,
    pub redact: bool,
    pub queue: usize,
    /// Most bytes of queued records held for the writer thread.
    pub queue_max_bytes: u64,
    pub dry_run: bool,
    /// Top-level keys or JSON pointers dropped from logged JSON bodies.
    pub exclude: Vec<String>,
    /// Paths replaced by a short description (tool names, a count) in logged JSON bodies.
    pub summarize: Vec<String>,
    /// Longest string kept in a logged JSON body; 0 keeps them whole.
    pub max_string_bytes: usize,
    pub sse: SseMode,
    pub format: LogFormat,
    /// Summary-only export for a log collector; off when absent.
    pub export: Option<ExportConfig>,
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
            "capture_bodies",
            "body",
            "max_body_bytes",
            "max_capture_bytes",
            "headers",
            "response",
            "redact",
            "queue",
            "queue_max_bytes",
            "exclude",
            "summarize",
            "max_string_bytes",
            "sse",
            "format",
            "export",
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
        let export = export_config(table)?;
        if file.is_none() && socket.is_none() && syslog.is_none() && export.is_none() {
            return Err(
                "set `file`, `socket`, `syslog`, `export` or several; nothing would be logged"
                    .to_owned(),
            );
        }
        if syslog.is_none()
            && (table.contains_key("syslog_facility") || table.contains_key("syslog_app_name"))
        {
            return Err("`syslog_facility` and `syslog_app_name` need `syslog`".to_owned());
        }
        let syslog_facility = syslog_facility(table, "syslog_facility", DEFAULT_SYSLOG_FACILITY)?;
        let syslog_app_name = syslog_app_name(table, "syslog_app_name", DEFAULT_SYSLOG_APP_NAME)?;

        let capture_bodies = match table.get("capture_bodies") {
            None => false,
            Some(Value::Boolean(value)) => *value,
            Some(other) => {
                return Err(format!(
                    "`capture_bodies` must be a boolean, got {}",
                    other.type_str()
                ));
            }
        };
        let body = match string(table, "body")?.as_deref() {
            None | Some("summary") => BodyMode::Summary,
            Some("full") => BodyMode::Full,
            Some("none") => BodyMode::None,
            Some(other) => {
                return Err(format!(
                    "`body` must be \"full\", \"summary\" or \"none\", got \"{other}\""
                ));
            }
        };
        let response = match string(table, "response")?.as_deref() {
            None | Some("status") => ResponseMode::Status,
            Some("body") => ResponseMode::Body,
            Some("none") => ResponseMode::None,
            Some(other) => {
                return Err(format!(
                    "`response` must be \"status\", \"body\" or \"none\", got \"{other}\""
                ));
            }
        };

        if !capture_bodies && body == BodyMode::Full {
            return Err("`body = \"full\"` needs `capture_bodies = true`".to_owned());
        }
        if !capture_bodies && response == ResponseMode::Body {
            return Err("`response = \"body\"` needs `capture_bodies = true`".to_owned());
        }

        let file_max_bytes = integer(table, "file_max_bytes", DEFAULT_FILE_MAX_BYTES, 0, u64::MAX)?;
        let file_keep = integer(table, "file_keep", u64::from(DEFAULT_FILE_KEEP), 0, 100)? as u32;
        let max_body_bytes = integer(
            table,
            "max_body_bytes",
            DEFAULT_MAX_BODY_BYTES as u64,
            1,
            MAX_BODY_BYTES_LIMIT as u64,
        )? as usize;
        let max_capture_bytes = integer(
            table,
            "max_capture_bytes",
            DEFAULT_MAX_CAPTURE_BYTES as u64,
            1,
            MAX_CAPTURE_BYTES_LIMIT as u64,
        )? as usize;
        let queue = integer(table, "queue", DEFAULT_QUEUE as u64, 16, 65_536)? as usize;
        let queue_max_bytes = integer(
            table,
            "queue_max_bytes",
            DEFAULT_QUEUE_MAX_BYTES,
            MIN_QUEUE_MAX_BYTES,
            MAX_QUEUE_MAX_BYTES,
        )?;

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
                ));
            }
        };
        let format = match string(table, "format")?.as_deref() {
            None | Some("json") => LogFormat::Json,
            Some("ocsf") => LogFormat::Ocsf,
            Some(other) => {
                return Err(format!(
                    "`format` must be \"json\" or \"ocsf\", got \"{other}\""
                ));
            }
        };
        let dry_run = match table.get("dry_run") {
            None => false,
            Some(Value::Boolean(value)) => *value,
            Some(other) => {
                return Err(format!(
                    "`dry_run` must be a boolean, got {}",
                    other.type_str()
                ));
            }
        };
        let redact = match table.get("redact") {
            None => true,
            Some(Value::Boolean(value)) => *value,
            Some(other) => {
                return Err(format!(
                    "`redact` must be a boolean, got {}",
                    other.type_str()
                ));
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
            capture_bodies,
            body,
            max_body_bytes,
            max_capture_bytes,
            headers,
            response,
            redact,
            queue,
            queue_max_bytes,
            dry_run,
            exclude,
            summarize,
            max_string_bytes,
            sse,
            format,
            export,
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

fn syslog_facility(table: &Table, key: &str, default: u8) -> Result<u8, String> {
    match string(table, key)? {
        None => Ok(default),
        Some(name) => facility_code(&name).ok_or_else(|| {
            format!("`{key}` must be a syslog facility name such as \"local0\", got \"{name}\"")
        }),
    }
}

fn syslog_app_name(table: &Table, key: &str, default: &str) -> Result<String, String> {
    match string(table, key)? {
        None => Ok(default.to_owned()),
        Some(name) => {
            let valid = (1..=MAX_SYSLOG_APP_NAME).contains(&name.len())
                && name.bytes().all(|b| b.is_ascii_graphic())
                && name != "-";
            if !valid {
                return Err(format!(
                    "`{key}` must be 1 to {MAX_SYSLOG_APP_NAME} printable ASCII characters without spaces, got \"{name}\""
                ));
            }
            Ok(name)
        }
    }
}

/// The `[plugins.requestlog.export]` table: summary-only records for a log collector.
fn export_config(table: &Table) -> Result<Option<ExportConfig>, String> {
    const KNOWN: &[&str] = &[
        "format",
        "file",
        "max_bytes",
        "max_files",
        "syslog",
        "syslog_facility",
        "syslog_app_name",
        "queue",
    ];
    let Some(Value::Table(export)) = table.get("export") else {
        return match table.get("export") {
            None => Ok(None),
            Some(other) => Err(format!(
                "`export` must be a table, got {}",
                other.type_str()
            )),
        };
    };
    for key in export.keys() {
        if !KNOWN.contains(&key.as_str()) {
            return Err(format!("unknown key `export.{key}`"));
        }
    }
    let format = match string(export, "format")?.as_deref() {
        None | Some("jsonl") => ExportFormat::Jsonl,
        Some("syslog") => ExportFormat::Syslog,
        Some(other) => {
            return Err(format!(
                "`export.format` must be \"jsonl\" or \"syslog\", got \"{other}\""
            ));
        }
    };
    let file = path(export, "file")?;
    let syslog = syslog_target(export)?;
    if let Some(SyslogTarget::Tcp(_)) = &syslog {
        return Err("`export.syslog` must be udp:// or unix://; tcp is not supported".to_owned());
    }
    match format {
        ExportFormat::Jsonl if file.is_none() => {
            return Err("`export.file` is required for `export.format = \"jsonl\"`".to_owned());
        }
        ExportFormat::Jsonl if syslog.is_some() => {
            return Err("`export.syslog` needs `export.format = \"syslog\"`".to_owned());
        }
        ExportFormat::Syslog if syslog.is_none() => {
            return Err("`export.syslog` is required for `export.format = \"syslog\"`".to_owned());
        }
        ExportFormat::Syslog if file.is_some() => {
            return Err("`export.file` needs `export.format = \"jsonl\"`".to_owned());
        }
        _ => {}
    }
    let max_bytes = integer(export, "max_bytes", DEFAULT_EXPORT_MAX_BYTES, 1, u64::MAX)?;
    let max_files = integer(
        export,
        "max_files",
        u64::from(DEFAULT_EXPORT_MAX_FILES),
        0,
        100,
    )? as u32;
    let queue = integer(export, "queue", DEFAULT_EXPORT_QUEUE as u64, 1, 65_536)? as usize;
    Ok(Some(ExportConfig {
        format,
        file,
        max_bytes,
        max_files,
        syslog,
        syslog_facility: syslog_facility(export, "syslog_facility", DEFAULT_SYSLOG_FACILITY)?,
        syslog_app_name: syslog_app_name(
            export,
            "syslog_app_name",
            DEFAULT_EXPORT_SYSLOG_APP_NAME,
        )?,
        queue,
    }))
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

/// True when an already-lowercased header name might carry a credential.
fn is_denied_header(name: &str) -> bool {
    DENIED_HEADERS.iter().any(|pattern| name.contains(pattern))
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
        if is_denied_header(&name) {
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
        assert!(!config.capture_bodies);
        assert_eq!(config.body, BodyMode::Summary);
        assert_eq!(config.response, ResponseMode::Status);
        assert!(config.redact);
        assert_eq!(config.file_keep, 5);
        assert_eq!(config.max_body_bytes, 65_536);
        assert_eq!(config.max_capture_bytes, 1_048_576);
        assert_eq!(config.queue_max_bytes, 67_108_864);
        assert!(config.headers.is_empty() && config.socket.is_none());
    }

    #[test]
    fn full_bodies_require_an_explicit_opt_in() {
        assert!(
            parse("file = \"/x\"\nbody = \"full\"")
                .unwrap_err()
                .contains("capture_bodies")
        );
        assert!(
            parse("file = \"/x\"\nresponse = \"body\"")
                .unwrap_err()
                .contains("capture_bodies")
        );
        let config =
            parse("file = \"/x\"\ncapture_bodies = true\nbody = \"full\"\nresponse = \"body\"")
                .unwrap();
        assert!(config.capture_bodies);
        assert_eq!(config.body, BodyMode::Full);
        assert_eq!(config.response, ResponseMode::Body);
        let summary = parse("file = \"/x\"\ncapture_bodies = false").unwrap();
        assert_eq!(summary.body, BodyMode::Summary);
        assert!(parse("file = \"/x\"\ncapture_bodies = \"yes\"").is_err());
        assert!(parse("file = \"/x\"\ncapture_bodies = true\nbody = \"none\"").is_ok());
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
        assert!(parse("socket = \"/run/r.sock\"\nstreams = [\"a\"]").is_ok());
        assert!(
            parse("socket = \"/run/r.sock\"\nfiel = 1")
                .unwrap_err()
                .contains("fiel")
        );
    }

    #[test]
    fn a_socket_path_too_long_for_unix_is_refused_up_front() {
        let long = format!("/run/{}/r.sock", "d".repeat(100));
        assert!(
            parse(&format!("socket = \"{long}\""))
                .unwrap_err()
                .contains("longer")
        );
        let ok = format!("/run/{}/r.sock", "d".repeat(40));
        assert!(parse(&format!("socket = \"{ok}\"")).is_ok());
    }

    #[test]
    fn relative_paths_are_refused() {
        assert!(
            parse("file = \"req.jsonl\"")
                .unwrap_err()
                .contains("absolute")
        );
        assert!(
            parse("socket = \"run/r.sock\"")
                .unwrap_err()
                .contains("absolute")
        );
    }

    #[test]
    fn credential_headers_cannot_be_logged() {
        for name in [
            "Authorization",
            "proxy-authorization",
            "x-api-key",
            "api-key",
            "X-Auth-Foo",
            "x-credential",
            "my-token",
            "COOKIE",
            "Cookie",
        ] {
            let text = format!("file = \"/x\"\nheaders = [\"{name}\"]");
            assert!(parse(&text).unwrap_err().contains("credentials"), "{name}");
        }
        let ok =
            parse("file = \"/x\"\nheaders = [\"User-Agent\", \"x-opencode-session\"]").unwrap();
        assert_eq!(ok.headers, ["user-agent", "x-opencode-session"]);
    }

    #[test]
    fn credential_headers_are_refused_even_without_redaction() {
        let text = "file = \"/x\"\nredact = false\nheaders = [\"api-key\"]";
        assert!(parse(text).unwrap_err().contains("credentials"));
        let ok = parse("file = \"/x\"\nredact = false\nheaders = [\"user-agent\"]").unwrap();
        assert_eq!(ok.headers, ["user-agent"]);
    }

    #[test]
    fn syslog_targets_parse_for_each_transport() {
        let udp = parse("syslog = \"udp://collector.example.com:514\"").unwrap();
        assert_eq!(
            udp.syslog,
            Some(SyslogTarget::Udp("collector.example.com:514".to_owned()))
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
        assert!(
            parse(&format!(
                "syslog = \"tcp://c:514\"\nsyslog_app_name = \"{too_long}\""
            ))
            .is_err()
        );
    }

    #[test]
    fn syslog_options_need_a_syslog_target() {
        assert!(
            parse("file = \"/x\"\nsyslog_facility = \"local0\"")
                .unwrap_err()
                .contains("need `syslog`")
        );
        assert!(parse("file = \"/x\"\nsyslog_app_name = \"reiki\"").is_err());
    }

    #[test]
    fn modes_and_ranges_are_validated() {
        assert!(parse("file = \"/x\"\nbody = \"all\"").is_err());
        assert!(parse("file = \"/x\"\nresponse = \"all\"").is_err());
        assert_eq!(
            parse("file = \"/x\"\ncapture_bodies = true\nresponse = \"body\"")
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
        assert!(parse("file = \"/x\"\nmax_capture_bytes = 0").is_err());
        assert!(parse("file = \"/x\"\nmax_capture_bytes = 99999999999").is_err());
        assert_eq!(
            parse("file = \"/x\"\nmax_capture_bytes = 4096")
                .unwrap()
                .max_capture_bytes,
            4096
        );
        assert!(parse("file = \"/x\"\nqueue = 4").is_err());
        assert!(parse("file = \"/x\"\nqueue_max_bytes = 1048575").is_err());
        assert!(parse("file = \"/x\"\nqueue_max_bytes = 4294967297").is_err());
        assert_eq!(
            parse("file = \"/x\"\nqueue_max_bytes = 1048576")
                .unwrap()
                .queue_max_bytes,
            1_048_576
        );
        assert_eq!(
            parse("file = \"/x\"\nqueue_max_bytes = 4294967296")
                .unwrap()
                .queue_max_bytes,
            4_294_967_296
        );
        assert!(parse("file = \"/x\"\nfile_keep = 500").is_err());
        assert!(parse("file = \"/x\"\nredact = \"yes\"").is_err());
        assert!(parse("file = \"/x\"\nfile_max_bytes = -1").is_err());
    }

    #[test]
    fn export_defaults_apply_and_an_export_only_config_is_valid() {
        let text = "[export]\nformat = \"jsonl\"\nfile = \"/var/log/reiki/export.jsonl\"";
        let config = parse(text).unwrap();
        let export = config.export.as_ref().unwrap();
        assert_eq!(export.format, ExportFormat::Jsonl);
        assert_eq!(
            export.file.as_deref(),
            Some(std::path::Path::new("/var/log/reiki/export.jsonl"))
        );
        assert_eq!(
            (export.max_bytes, export.max_files, export.queue),
            (10_485_760, 3, 256)
        );
        assert_eq!(
            (export.syslog_facility, export.syslog_app_name.as_str()),
            (16, "reiki-export")
        );
        assert!(config.file.is_none() && config.socket.is_none() && config.syslog.is_none());
    }

    #[test]
    fn export_syslog_accepts_udp_and_unix_but_not_tcp() {
        let udp = parse(
            "[export]\nformat = \"syslog\"\nsyslog = \"udp://collector:514\"\nsyslog_facility = \"local4\"\nsyslog_app_name = \"collector\"",
        )
        .unwrap()
        .export
        .unwrap();
        assert_eq!(
            udp.syslog,
            Some(SyslogTarget::Udp("collector:514".to_owned()))
        );
        assert_eq!(
            (udp.syslog_facility, udp.syslog_app_name.as_str()),
            (20, "collector")
        );
        let unix = parse("[export]\nformat = \"syslog\"\nsyslog = \"unix:///dev/log\"")
            .unwrap()
            .export
            .unwrap();
        assert_eq!(
            unix.syslog,
            Some(SyslogTarget::Unix(PathBuf::from("/dev/log")))
        );
        assert!(
            parse("[export]\nformat = \"syslog\"\nsyslog = \"tcp://collector:514\"")
                .unwrap_err()
                .contains("tcp is not supported")
        );
    }

    #[test]
    fn export_rejects_unknown_keys_and_wrong_sink_shapes() {
        assert!(
            parse("[export]\nfile = \"/x\"\nformt = \"jsonl\"")
                .unwrap_err()
                .contains("export.formt")
        );
        assert!(parse("[export]\nfile = \"/x\"\nextra = 1").is_err());
        assert!(parse("[export]\nformat = \"jsonl\"").is_err());
        assert!(
            parse("[export]\nformat = \"jsonl\"\nfile = \"/x\"\nsyslog = \"udp://c:514\"").is_err()
        );
        assert!(parse("[export]\nformat = \"syslog\"").is_err());
        assert!(
            parse("[export]\nformat = \"syslog\"\nsyslog = \"udp://c:514\"\nfile = \"/x\"")
                .is_err()
        );
        assert!(parse("[export]\nformat = \"yaml\"\nfile = \"/x\"").is_err());
        assert!(parse("export = []").is_err());
    }

    #[test]
    fn export_ranges_are_validated() {
        let base = "[export]\nfile = \"/x\"";
        assert!(parse(&format!("{base}\nmax_bytes = 0")).is_err());
        assert!(parse(&format!("{base}\nmax_files = 101")).is_err());
        assert!(parse(&format!("{base}\nqueue = 0")).is_err());
        assert!(parse(&format!("{base}\nqueue = 65537")).is_err());
        let config = parse(&format!(
            "{base}\nmax_bytes = 100\nmax_files = 0\nqueue = 16"
        ))
        .unwrap()
        .export
        .unwrap();
        assert_eq!(
            (config.max_bytes, config.max_files, config.queue),
            (100, 0, 16)
        );
    }
}
