use std::path::PathBuf;

use reiki_plugin::HOST_KEYS;
use toml::Value;

const SETTINGS: &[&str] = &[
    "mode",
    "block_severity",
    "allow_hosts",
    "check_declared",
    "check_hosts",
    "check_tripwires",
    "journal",
    "journal_max_bytes",
    "upstreams",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Medium,
    High,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Report,
    Block,
}

#[derive(Debug)]
pub struct Config {
    pub mode: Mode,
    pub block_severity: Severity,
    pub allow_hosts: Vec<String>,
    pub check_declared: bool,
    pub check_hosts: bool,
    pub check_tripwires: bool,
    pub journal: Option<PathBuf>,
    pub journal_max_bytes: u64,
    pub upstreams: Vec<String>,
    pub dry_run: bool,
}

impl Config {
    pub fn runs_on(&self, upstream: &str) -> bool {
        self.upstreams.is_empty() || self.upstreams.iter().any(|name| name == upstream)
    }
}

pub fn parse(table: &toml::Table) -> Result<Config, String> {
    for key in table.keys() {
        if !SETTINGS.contains(&key.as_str()) && !HOST_KEYS.contains(&key.as_str()) {
            return Err(format!("unknown setting `{key}`"));
        }
    }
    let mode = match table.get("mode") {
        None => Mode::Report,
        Some(Value::String(s)) if s == "report" => Mode::Report,
        Some(Value::String(s)) if s == "block" => Mode::Block,
        Some(other) => {
            return Err(format!(
                "`mode` must be \"report\" or \"block\", got {other}"
            ));
        }
    };
    let block_severity = match table.get("block_severity") {
        None => Severity::High,
        Some(Value::String(s)) if s == "high" => Severity::High,
        Some(Value::String(s)) if s == "medium" => Severity::Medium,
        Some(other) => {
            return Err(format!(
                "`block_severity` must be \"high\" or \"medium\", got {other}"
            ));
        }
    };
    Ok(Config {
        mode,
        block_severity,
        allow_hosts: table
            .get("allow_hosts")
            .map(|v| strings("allow_hosts", v))
            .transpose()?
            .unwrap_or_default(),
        check_declared: boolean("check_declared", table)?,
        check_hosts: boolean("check_hosts", table)?,
        check_tripwires: boolean("check_tripwires", table)?,
        journal: match table.get("journal") {
            None => None,
            Some(Value::String(s)) if !s.is_empty() => Some(PathBuf::from(s)),
            Some(other) => return Err(format!("`journal` must be a non-empty path, got {other}")),
        },
        journal_max_bytes: match table.get("journal_max_bytes") {
            None => 1024 * 1024,
            Some(Value::Integer(n)) if *n > 0 => u64::try_from(*n).map_err(|_| {
                "`journal_max_bytes` must fit in a 64-bit unsigned integer".to_owned()
            })?,
            Some(other) => {
                return Err(format!(
                    "`journal_max_bytes` must be a positive integer, got {other}"
                ));
            }
        },
        upstreams: table
            .get("upstreams")
            .map(|v| strings("upstreams", v))
            .transpose()?
            .unwrap_or_default(),
        dry_run: match table.get("dry_run") {
            None => false,
            Some(Value::Boolean(v)) => *v,
            Some(other) => return Err(format!("`dry_run` must be a boolean, got {other}")),
        },
    })
}

fn boolean(key: &str, table: &toml::Table) -> Result<bool, String> {
    match table.get(key) {
        None => Ok(true),
        Some(Value::Boolean(v)) => Ok(*v),
        Some(other) => Err(format!("`{key}` must be a boolean, got {other}")),
    }
}

fn strings(key: &str, value: &Value) -> Result<Vec<String>, String> {
    let Value::Array(items) = value else {
        return Err(format!("`{key}` must be an array of strings, got {value}"));
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(s) if !s.is_empty() => Ok(s.clone()),
            Value::String(_) => Err(format!("`{key}` must not contain an empty string")),
            other => Err(format!("`{key}` must contain strings, got {other}")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_report_and_all_checks() {
        let config = parse(&toml::Table::new()).unwrap();
        assert_eq!(config.mode, Mode::Report);
        assert_eq!(config.block_severity, Severity::High);
        assert!(config.check_declared && config.check_hosts && config.check_tripwires);
        assert!(config.allow_hosts.is_empty());
        assert!(config.journal.is_none());
        assert!(config.runs_on("zen"));
    }

    #[test]
    fn bad_settings_name_the_key() {
        for (source, key) in [
            ("mode = \"deny\"", "mode"),
            ("block_severity = \"low\"", "block_severity"),
            ("allow_hosts = 1", "allow_hosts"),
            ("allow_hosts = [\"\"]", "allow_hosts"),
            ("check_hosts = \"yes\"", "check_hosts"),
            ("upstreams = [1]", "upstreams"),
            ("journal = 4", "journal"),
            ("journal_max_bytes = 0", "journal_max_bytes"),
            ("dry_run = 1", "dry_run"),
            ("nope = true", "nope"),
        ] {
            let table: toml::Table = source.parse().unwrap();
            let error = parse(&table).unwrap_err();
            assert!(error.contains(key), "{source}: {error}");
        }
    }

    #[test]
    fn upstreams_limit_where_the_plugin_runs() {
        let table: toml::Table = "upstreams = [\"zen\"]".parse().unwrap();
        let config = parse(&table).unwrap();
        assert!(config.runs_on("zen"));
        assert!(!config.runs_on("other"));
    }
}
