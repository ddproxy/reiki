//! Reiki plugin that stops financial identifiers from leaving in an outbound request.
//!
//! In `block` mode a request that holds a recognised identifier is rejected before any key is
//! used. In `report` mode it goes through and the finding is logged. Either way the log and the
//! rejection name the kinds and counts found, never the values.

mod columnar;
mod detect;

use std::collections::BTreeMap;

use http::header::CONTENT_ENCODING;
use http::StatusCode;
use reiki_plugin::{Plugin, PluginError, RequestCtx};
use toml::Value;

pub use detect::{scan, Hit, Kind};

/// The `[plugins.finance]` table this plugin reads.
pub const NAME: &str = "finance";

const HOST_KEYS: &[&str] = &["enabled", "on_error", "dry_run"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Block,
    Report,
}

struct Finance {
    mode: Mode,
    kinds: Vec<Kind>,
    /// Upstreams to check; empty means all of them.
    upstreams: Vec<String>,
}

/// Builds the plugin from its config table.
pub fn build(table: &toml::Table) -> Result<Box<dyn Plugin>, String> {
    for key in table.keys() {
        if !["mode", "kinds", "upstreams"].contains(&key.as_str())
            && !HOST_KEYS.contains(&key.as_str())
        {
            return Err(format!("{NAME}: unknown setting `{key}`"));
        }
    }
    let mode = match table.get("mode") {
        Some(Value::String(m)) if m == "block" => Mode::Block,
        Some(Value::String(m)) if m == "report" => Mode::Report,
        Some(other) => {
            return Err(format!(
                "{NAME}: `mode` must be \"block\" or \"report\", got {other}"
            ))
        }
        None => return Err(format!("{NAME}: set `mode` to \"block\" or \"report\"")),
    };
    let kinds = match strings(table, "kinds")? {
        None => Kind::DEFAULT.to_vec(),
        Some(labels) => {
            let kinds = labels
                .iter()
                .map(|l| Kind::parse(l).ok_or_else(|| format!("{NAME}: unknown kind `{l}`")))
                .collect::<Result<Vec<_>, _>>()?;
            if kinds.is_empty() {
                return Err(format!("{NAME}: `kinds` must not be empty"));
            }
            kinds
        }
    };
    let upstreams = strings(table, "upstreams")?.unwrap_or_default();
    Ok(Box::new(Finance {
        mode,
        kinds,
        upstreams,
    }))
}

fn strings(table: &toml::Table, key: &str) -> Result<Option<Vec<String>>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => Ok(s.clone()),
                other => Err(format!(
                    "{NAME}: `{key}` must hold strings, got {}",
                    other.type_str()
                )),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(other) => Err(format!(
            "{NAME}: `{key}` must be an array of strings, got {}",
            other.type_str()
        )),
    }
}

fn summary(hits: &[Hit]) -> String {
    let mut counts: BTreeMap<Kind, usize> = BTreeMap::new();
    for hit in hits {
        *counts.entry(hit.kind).or_default() += 1;
    }
    counts
        .iter()
        .map(|(kind, n)| format!("{} x{n}", kind.label()))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Plugin for Finance {
    fn name(&self) -> &str {
        NAME
    }

    fn uses_bodies(&self) -> bool {
        false
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        if ctx.body.is_empty()
            || (!self.upstreams.is_empty() && !self.upstreams.iter().any(|u| u == ctx.upstream))
        {
            return Ok(());
        }
        // An encoded body cannot be read, so block mode fails closed rather than let it through.
        if ctx.headers.contains_key(CONTENT_ENCODING) {
            return match self.mode {
                Mode::Block => Err(PluginError::Reject(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    format!("{NAME}: the request body is encoded, so it cannot be checked"),
                )),
                Mode::Report => {
                    tracing::warn!(
                        request = ctx.id,
                        stream = ctx.stream,
                        upstream = ctx.upstream,
                        "finance: encoded request body not checked"
                    );
                    Ok(())
                }
            };
        }
        let hits = scan(&String::from_utf8_lossy(ctx.body), &self.kinds);
        if hits.is_empty() {
            return Ok(());
        }
        let found = summary(&hits);
        tracing::warn!(request = ctx.id, stream = ctx.stream, upstream = ctx.upstream, found = %found, "finance: possible financial data in an outbound request");
        match self.mode {
            Mode::Block => Err(PluginError::Reject(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("{NAME}: request blocked, it contains {found}"),
            )),
            Mode::Report => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::{HeaderMap, HeaderValue, Method};

    fn plugin(text: &str) -> Result<Box<dyn Plugin>, String> {
        build(&text.parse().unwrap())
    }

    fn send(
        p: &dyn Plugin,
        upstream: &str,
        headers: &mut HeaderMap,
        body: &str,
    ) -> Result<(), PluginError> {
        let mut payload = Bytes::from(body.to_owned());
        let mut state = ();
        p.on_request(&mut RequestCtx::new(
            "ct",
            upstream,
            &Method::POST,
            "/x",
            headers,
            &mut payload,
            &mut state,
        ))
    }

    #[test]
    fn block_mode_rejects_and_names_kinds_without_values() {
        let p = plugin("mode = \"block\"").unwrap();
        let body = "{\"q\":\"iban GB82WEST12345698765432 isin US0378331005\"}";
        let err = send(p.as_ref(), "zen", &mut HeaderMap::new(), body).unwrap_err();
        let PluginError::Reject(status, message) = err else {
            panic!("{err}")
        };
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            message,
            "finance: request blocked, it contains iban x1, isin x1"
        );
        assert!(!message.contains("GB82") && !message.contains("US03"));
    }

    #[test]
    fn report_mode_lets_the_request_through() {
        let p = plugin("mode = \"report\"").unwrap();
        assert!(send(
            p.as_ref(),
            "zen",
            &mut HeaderMap::new(),
            "iban GB82WEST12345698765432"
        )
        .is_ok());
    }

    #[test]
    fn clean_bodies_and_other_upstreams_pass() {
        let p = plugin("mode = \"block\"\nupstreams = [\"zen\"]").unwrap();
        assert!(send(
            p.as_ref(),
            "zen",
            &mut HeaderMap::new(),
            "{\"q\":\"hello\"}"
        )
        .is_ok());
        assert!(send(
            p.as_ref(),
            "forgejo",
            &mut HeaderMap::new(),
            "iban GB82WEST12345698765432"
        )
        .is_ok());
        assert!(send(
            p.as_ref(),
            "zen",
            &mut HeaderMap::new(),
            "iban GB82WEST12345698765432"
        )
        .is_err());
    }

    #[test]
    fn an_encoded_body_fails_closed_in_block_mode_only() {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        let block = plugin("mode = \"block\"").unwrap();
        assert!(matches!(
            send(block.as_ref(), "zen", &mut headers, "x"),
            Err(PluginError::Reject(StatusCode::UNSUPPORTED_MEDIA_TYPE, _))
        ));
        let report = plugin("mode = \"report\"").unwrap();
        assert!(send(report.as_ref(), "zen", &mut headers, "x").is_ok());
    }

    #[test]
    fn kinds_narrow_and_extend_the_default_set() {
        let iban_only = plugin("mode = \"block\"\nkinds = [\"iban\"]").unwrap();
        assert!(send(
            iban_only.as_ref(),
            "zen",
            &mut HeaderMap::new(),
            "cusip 037833100"
        )
        .is_ok());
        let with_account = plugin("mode = \"block\"\nkinds = [\"account\"]").unwrap();
        assert!(send(
            with_account.as_ref(),
            "zen",
            &mut HeaderMap::new(),
            "account number: 000123456789"
        )
        .is_err());
    }

    #[test]
    fn the_config_is_strict() {
        assert!(plugin("").is_err(), "mode is required");
        assert!(plugin("mode = \"maybe\"").is_err());
        assert!(plugin("mode = \"block\"\nkinds = [\"bogus\"]").is_err());
        assert!(plugin("mode = \"block\"\nkinds = []").is_err());
        assert!(plugin("mode = \"block\"\nbogus = 1").is_err());
        assert!(plugin("mode = \"block\"\nupstreams = [1]").is_err());
    }
}
