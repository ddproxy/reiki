//! The plugin shell the detection plugins share.
//!
//! A detection plugin supplies its identifier kinds and a scanner. This module supplies the rest:
//! parsing `mode`, `kinds` and `upstreams`, skipping other upstreams, failing closed on a body it
//! cannot read, reporting findings, and blocking or letting the request through. Findings name
//! kinds and counts, never matched text.

use std::collections::BTreeMap;

use http::StatusCode;
use reiki_plugin::{Plugin, PluginError, RequestCtx, HOST_KEYS};
use toml::Value;

use crate::Hit;

/// An identifier kind a detection plugin looks for.
pub trait Kind: Copy + Ord + Send + Sync + 'static {
    /// The name used in config and in findings.
    fn label(self) -> &'static str;
    /// The kind with this label, if any.
    fn parse(label: &str) -> Option<Self>;
    /// The kinds that are on unless the config narrows the list.
    fn defaults() -> &'static [Self];
}

/// What distinguishes one detection plugin from another.
pub struct Spec<K: Kind> {
    /// The `[plugins.<name>]` key.
    pub name: &'static str,
    /// What a finding is, for the log: `possible protected data`.
    pub finding: &'static str,
    /// Finds identifiers of the wanted kinds in `text`.
    pub scan: fn(&str, &[K]) -> Vec<Hit<K>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Block,
    Report,
}

struct Guard<K: Kind> {
    spec: Spec<K>,
    mode: Mode,
    kinds: Vec<K>,
    /// Upstreams to check; empty means all of them.
    upstreams: Vec<String>,
}

/// Builds a detection plugin from its config table.
pub fn build<K: Kind>(spec: Spec<K>, table: &toml::Table) -> Result<Box<dyn Plugin>, String> {
    let name = spec.name;
    for key in table.keys() {
        if !["mode", "kinds", "upstreams"].contains(&key.as_str())
            && !HOST_KEYS.contains(&key.as_str())
        {
            return Err(format!("{name}: unknown setting `{key}`"));
        }
    }
    let mode = match table.get("mode") {
        Some(Value::String(m)) if m == "block" => Mode::Block,
        Some(Value::String(m)) if m == "report" => Mode::Report,
        Some(other) => {
            return Err(format!(
                "{name}: `mode` must be \"block\" or \"report\", got {other}"
            ))
        }
        None => return Err(format!("{name}: set `mode` to \"block\" or \"report\"")),
    };
    let kinds = match strings(name, table, "kinds")? {
        None => K::defaults().to_vec(),
        Some(labels) => {
            let kinds = labels
                .iter()
                .map(|l| K::parse(l).ok_or_else(|| format!("{name}: unknown kind `{l}`")))
                .collect::<Result<Vec<_>, _>>()?;
            if kinds.is_empty() {
                return Err(format!("{name}: `kinds` must not be empty"));
            }
            kinds
        }
    };
    let upstreams = strings(name, table, "upstreams")?.unwrap_or_default();
    Ok(Box::new(Guard {
        spec,
        mode,
        kinds,
        upstreams,
    }))
}

fn strings(name: &str, table: &toml::Table, key: &str) -> Result<Option<Vec<String>>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => Ok(s.clone()),
                other => Err(format!(
                    "{name}: `{key}` must hold strings, got {}",
                    other.type_str()
                )),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(other) => Err(format!(
            "{name}: `{key}` must be an array of strings, got {}",
            other.type_str()
        )),
    }
}

fn counts<K: Kind>(hits: &[Hit<K>]) -> BTreeMap<K, usize> {
    let mut counts = BTreeMap::new();
    for hit in hits {
        *counts.entry(hit.kind).or_default() += 1;
    }
    counts
}

fn summary<K: Kind>(hits: &[Hit<K>]) -> String {
    counts(hits)
        .iter()
        .map(|(kind, n)| format!("{} x{n}", kind.label()))
        .collect::<Vec<_>>()
        .join(", ")
}

impl<K: Kind> Plugin for Guard<K> {
    fn name(&self) -> &str {
        self.spec.name
    }

    fn uses_bodies(&self) -> bool {
        false
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        let name = self.spec.name;
        if ctx.body.is_empty()
            || (!self.upstreams.is_empty() && !self.upstreams.iter().any(|u| u == ctx.upstream))
        {
            return Ok(());
        }
        // An encoded body cannot be read, so block mode fails closed rather than let it through.
        if crate::body_is_encoded(ctx.headers) {
            return match self.mode {
                Mode::Block => Err(PluginError::Reject(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    format!("{name}: the request body is encoded, so it cannot be checked"),
                )),
                Mode::Report => {
                    tracing::warn!(
                        request = ctx.id,
                        stream = ctx.stream,
                        upstream = ctx.upstream,
                        "{name}: encoded request body not checked"
                    );
                    Ok(())
                }
            };
        }
        // A lossy read could split a digit run around a replacement character, so an invalid
        // UTF-8 body is treated like an encoded one rather than scanned.
        let Ok(text) = std::str::from_utf8(ctx.body) else {
            return match self.mode {
                Mode::Block => Err(PluginError::Reject(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    format!("{name}: the request body is not valid UTF-8, so it cannot be checked"),
                )),
                Mode::Report => {
                    tracing::warn!(
                        request = ctx.id,
                        stream = ctx.stream,
                        upstream = ctx.upstream,
                        "{name}: non-UTF-8 request body not checked"
                    );
                    Ok(())
                }
            };
        };
        let hits = (self.spec.scan)(text, &self.kinds);
        if hits.is_empty() {
            return Ok(());
        }
        let found = summary(&hits);
        for (kind, n) in counts(&hits) {
            ctx.report(kind.label(), n, self.mode == Mode::Block);
        }
        tracing::warn!(
            request = ctx.id,
            stream = ctx.stream,
            upstream = ctx.upstream,
            found = %found,
            "{name}: {} in an outbound request",
            self.spec.finding
        );
        match self.mode {
            Mode::Block => Err(PluginError::Reject(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("{name}: request blocked, it contains {found}"),
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

    const SSN: &str = "123-45-6789";

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum TestKind {
        Ssn,
    }

    impl Kind for TestKind {
        fn label(self) -> &'static str {
            "ssn"
        }

        fn parse(label: &str) -> Option<Self> {
            (label == "ssn").then_some(Self::Ssn)
        }

        fn defaults() -> &'static [Self] {
            &[Self::Ssn]
        }
    }

    fn scan_ssn(text: &str, _: &[TestKind]) -> Vec<Hit<TestKind>> {
        let mut hits = Vec::new();
        let mut from = 0;
        while let Some(at) = text[from..].find(SSN) {
            let start = from + at;
            hits.push(Hit {
                kind: TestKind::Ssn,
                start,
                end: start + SSN.len(),
            });
            from = start + SSN.len();
        }
        hits
    }

    fn plugin(mode: &str) -> Box<dyn Plugin> {
        build(
            Spec {
                name: "test",
                finding: "possible test data",
                scan: scan_ssn,
            },
            &format!("mode = \"{mode}\"").parse().unwrap(),
        )
        .unwrap()
    }

    fn send(p: &dyn Plugin, headers: &mut HeaderMap, body: &[u8]) -> Result<(), PluginError> {
        let mut payload = Bytes::copy_from_slice(body);
        let mut state = ();
        p.on_request(&mut RequestCtx::new(
            "ct",
            "zen",
            &Method::POST,
            "/x",
            headers,
            &mut payload,
            &mut state,
        ))
    }

    fn reported(
        p: &dyn Plugin,
        headers: &mut HeaderMap,
        body: &[u8],
    ) -> Vec<reiki_plugin::Reported> {
        let mut payload = Bytes::copy_from_slice(body);
        let mut state = ();
        let mut ctx = RequestCtx::new(
            "ct",
            "zen",
            &Method::POST,
            "/x",
            headers,
            &mut payload,
            &mut state,
        );
        let _ = p.on_request(&mut ctx);
        ctx.reported
    }

    fn encoding(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn identity_content_encoding_is_scanned_as_absent() {
        for value in ["identity", "Identity", " identity ", "identity, identity"] {
            let body = format!("ssn {SSN}");
            let err = send(
                plugin("block").as_ref(),
                &mut encoding(value),
                body.as_bytes(),
            )
            .unwrap_err();
            let PluginError::Reject(status, message) = err else {
                panic!("{value}: {err}")
            };
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{value}");
            assert!(message.contains("ssn x1"), "{value}: {message}");

            let seen = reported(
                plugin("report").as_ref(),
                &mut encoding(value),
                body.as_bytes(),
            );
            assert_eq!(seen.len(), 1, "{value}: {seen:?}");
            assert_eq!(
                (seen[0].kind, seen[0].count, seen[0].blocked),
                ("ssn", 1, false),
                "{value}"
            );
        }
    }

    #[test]
    fn absent_content_encoding_is_scanned_normally() {
        let body = format!("ssn {SSN}");
        let mut headers = HeaderMap::new();
        assert!(matches!(
            send(plugin("block").as_ref(), &mut headers, body.as_bytes()),
            Err(PluginError::Reject(StatusCode::UNPROCESSABLE_ENTITY, _))
        ));
        assert_eq!(
            reported(plugin("report").as_ref(), &mut headers, body.as_bytes()).len(),
            1
        );
        assert!(send(plugin("block").as_ref(), &mut headers, b"hello").is_ok());
    }

    #[test]
    fn real_and_unknown_content_encodings_still_fail_closed() {
        for value in ["gzip", "identity, gzip", "br", "GZIP", "identity,", ""] {
            let body = format!("ssn {SSN}");
            assert!(
                matches!(
                    send(
                        plugin("block").as_ref(),
                        &mut encoding(value),
                        body.as_bytes()
                    ),
                    Err(PluginError::Reject(StatusCode::UNSUPPORTED_MEDIA_TYPE, _))
                ),
                "{value}"
            );
            assert!(
                reported(
                    plugin("report").as_ref(),
                    &mut encoding(value),
                    body.as_bytes()
                )
                .is_empty(),
                "{value}"
            );
        }
    }

    #[test]
    fn a_non_utf8_body_fails_closed() {
        let body = b"ssn 123-45-\xff6789";
        let mut headers = HeaderMap::new();
        let err = send(plugin("block").as_ref(), &mut headers, body).unwrap_err();
        let PluginError::Reject(status, message) = err else {
            panic!("{err}")
        };
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(message.contains("not valid UTF-8"), "{message}");
        assert!(reported(plugin("report").as_ref(), &mut headers, body).is_empty());
    }
}
