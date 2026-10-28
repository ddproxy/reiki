use std::path::PathBuf;

use toml::{Table, Value};

/// Keys the host reads itself; the plugin accepts them but ignores them.
const HOST_KEYS: &[&str] = &["enabled", "on_error", "dry_run"];

const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES_LIMIT: usize = 256 * 1024 * 1024;
const MIN_TERM_CHARS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Literal terms to mask, matched without regard to case.
    pub terms: Vec<String>,
    pub terms_file: Option<PathBuf>,
    /// Also mask credential-shaped strings.
    pub builtin: bool,
    /// Put the original values back in responses.
    pub rehydrate: bool,
    /// Also restore credentials inside tool-call arguments; off keeps them masked there.
    pub restore_tool_args: bool,
    /// The most response bytes held to restore a body that is not an event stream.
    pub max_response_bytes: usize,
    pub dry_run: bool,
}

impl Config {
    pub fn parse(table: &Table) -> Result<Self, String> {
        const KNOWN: &[&str] = &[
            "terms",
            "terms_file",
            "builtin",
            "rehydrate",
            "restore_tool_args",
            "max_response_bytes",
        ];
        for key in table.keys() {
            if !KNOWN.contains(&key.as_str()) && !HOST_KEYS.contains(&key.as_str()) {
                return Err(format!("unknown key `{key}`"));
            }
        }

        let terms = match table.get("terms") {
            None => Vec::new(),
            Some(Value::Array(items)) => {
                let mut terms = Vec::new();
                for item in items {
                    let Value::String(text) = item else {
                        return Err(format!(
                            "`terms` must hold strings, got {}",
                            item.type_str()
                        ));
                    };
                    terms.push(text.clone());
                }
                terms
            }
            Some(other) => {
                return Err(format!(
                    "`terms` must be an array of strings, got {}",
                    other.type_str()
                ))
            }
        };
        let terms = validate_terms(terms)?;

        let terms_file = match table.get("terms_file") {
            None => None,
            Some(Value::String(path)) => {
                let path = PathBuf::from(path);
                if !path.is_absolute() {
                    return Err("`terms_file` must be an absolute path".to_owned());
                }
                Some(path)
            }
            Some(other) => {
                return Err(format!(
                    "`terms_file` must be a string, got {}",
                    other.type_str()
                ))
            }
        };

        let builtin = boolean(table, "builtin", true)?;
        let rehydrate = boolean(table, "rehydrate", true)?;
        let restore_tool_args = boolean(table, "restore_tool_args", false)?;
        let dry_run = boolean(table, "dry_run", false)?;
        let max_response_bytes = match table.get("max_response_bytes") {
            None => DEFAULT_MAX_RESPONSE_BYTES,
            Some(Value::Integer(value)) => usize::try_from(*value)
                .ok()
                .filter(|v| (1..=MAX_RESPONSE_BYTES_LIMIT).contains(v))
                .ok_or_else(|| {
                    format!(
                        "`max_response_bytes` must be between 1 and {MAX_RESPONSE_BYTES_LIMIT}, got {value}"
                    )
                })?,
            Some(other) => {
                return Err(format!(
                    "`max_response_bytes` must be an integer, got {}",
                    other.type_str()
                ))
            }
        };

        if !builtin && terms.is_empty() && terms_file.is_none() {
            return Err(
                "nothing to mask: set `terms`, `terms_file` or leave `builtin` on".to_owned(),
            );
        }
        Ok(Self {
            terms,
            terms_file,
            builtin,
            rehydrate,
            restore_tool_args,
            max_response_bytes,
            dry_run,
        })
    }
}

/// Terms match raw JSON text, so characters JSON escapes could never match.
pub fn validate_terms(terms: Vec<String>) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for term in terms {
        let term = term.trim().to_owned();
        if term.is_empty() {
            continue;
        }
        if term.chars().count() < MIN_TERM_CHARS {
            return Err(format!(
                "term \"{term}\" is shorter than {MIN_TERM_CHARS} characters and would match too much"
            ));
        }
        if term
            .chars()
            .any(|c| c == '"' || c == '\\' || c.is_control())
        {
            return Err(format!(
                "term \"{term}\" contains a quote, a backslash or a control character, which JSON escapes"
            ));
        }
        if !out.contains(&term) {
            out.push(term);
        }
    }
    Ok(out)
}

fn boolean(table: &Table, key: &str, default: bool) -> Result<bool, String> {
    match table.get(key) {
        None => Ok(default),
        Some(Value::Boolean(value)) => Ok(*value),
        Some(other) => Err(format!(
            "`{key}` must be a boolean, got {}",
            other.type_str()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, String> {
        Config::parse(&text.parse::<Table>().unwrap())
    }

    #[test]
    fn defaults_mask_credentials_and_restore_them() {
        let config = parse("").unwrap();
        assert!(config.builtin && config.rehydrate && config.terms.is_empty());
        assert_eq!(config.max_response_bytes, 8 * 1024 * 1024);
    }

    #[test]
    fn terms_are_trimmed_deduplicated_and_validated() {
        let config = parse("terms = [\" Acme Corp \", \"Acme Corp\", \"\"]").unwrap();
        assert_eq!(config.terms, vec!["Acme Corp"]);
        assert!(parse("terms = [\"ab\"]").is_err());
        assert!(parse("terms = [\"say \\\"hi\\\"\"]").is_err());
        assert!(parse("terms = [3]").is_err());
        assert!(parse("terms = \"Acme\"").is_err());
    }

    #[test]
    fn something_must_be_left_to_mask() {
        assert!(parse("builtin = false").is_err());
        assert!(parse("builtin = false\nterms = [\"Falcon\"]").is_ok());
        assert!(parse("builtin = false\nterms_file = \"/etc/reiki/terms.txt\"").is_ok());
    }

    #[test]
    fn a_terms_file_must_be_absolute_and_unknown_keys_fail() {
        assert!(parse("terms_file = \"terms.txt\"").is_err());
        assert!(parse("surprise = 1").is_err());
        assert!(parse("enabled = true\non_error = \"reject\"").is_ok());
        assert!(parse("max_response_bytes = 0").is_err());
        assert!(parse("rehydrate = \"yes\"").is_err());
    }
}
