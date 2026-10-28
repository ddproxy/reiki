//! The plugin shell the detection plugins share.
//!
//! A detection plugin supplies its identifier kinds and a scanner. This module supplies the rest:
//! parsing `mode`, `kinds` and `upstreams`, skipping other upstreams, failing closed on a body it
//! cannot read, reporting findings, and blocking or letting the request through. Findings name
//! kinds and counts, never matched text.

use std::collections::BTreeMap;

use http::header::CONTENT_ENCODING;
use http::StatusCode;
use reiki_plugin::{Plugin, PluginError, RequestCtx};
use toml::Value;

use crate::Hit;

const HOST_KEYS: &[&str] = &["enabled", "on_error", "dry_run"];

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
        if ctx.headers.contains_key(CONTENT_ENCODING) {
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
        let hits = (self.spec.scan)(&String::from_utf8_lossy(ctx.body), &self.kinds);
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
