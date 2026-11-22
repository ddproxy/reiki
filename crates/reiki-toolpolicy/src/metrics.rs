//! Plugin-local Prometheus counters, served by the admin listener.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use reiki_plugin::{AdminCtx, AdminReply};

use crate::config::Severity;

pub const ADMIN_PATH: &str = "/toolpolicy/metrics";

#[derive(Debug, Default)]
pub struct Metrics {
    inner: Arc<Mutex<Inner>>,
}

impl Clone for Metrics {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    calls: BTreeMap<(String, String, String), u64>,
    findings: BTreeMap<(String, String), u64>,
    unparsed: BTreeMap<(String, String), u64>,
}

impl Metrics {
    pub fn record_call(&self, stream: &str, upstream: &str, action: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *inner
            .calls
            .entry((stream.to_owned(), upstream.to_owned(), action.to_owned()))
            .or_default() += 1;
    }

    pub fn record_finding(&self, rule: &'static str, severity: Severity) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *inner
            .findings
            .entry((rule.to_owned(), severity.label().to_owned()))
            .or_default() += 1;
    }

    pub fn record_unparsed(&self, stream: &str, upstream: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *inner
            .unparsed
            .entry((stream.to_owned(), upstream.to_owned()))
            .or_default() += 1;
    }

    pub fn reply(&self, ctx: &AdminCtx<'_>) -> Option<AdminReply> {
        if ctx.path != ADMIN_PATH {
            return None;
        }
        Some(AdminReply {
            content_type: "text/plain; version=0.0.4; charset=utf-8",
            body: self.render(),
        })
    }

    fn render(&self) -> String {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = String::new();
        let _ = writeln!(
            out,
            "# HELP reiki_toolpolicy_calls_total Tool calls seen in responses."
        );
        let _ = writeln!(out, "# TYPE reiki_toolpolicy_calls_total counter");
        for ((stream, upstream, action), count) in &inner.calls {
            let _ = writeln!(
                out,
                "reiki_toolpolicy_calls_total{{stream={},upstream={},action={}}} {count}",
                label(stream),
                label(upstream),
                label(action)
            );
        }
        let _ = writeln!(
            out,
            "# HELP reiki_toolpolicy_findings_total Tool calls that matched a policy rule."
        );
        let _ = writeln!(out, "# TYPE reiki_toolpolicy_findings_total counter");
        for ((rule, severity), count) in &inner.findings {
            let _ = writeln!(
                out,
                "reiki_toolpolicy_findings_total{{rule={},severity={}}} {count}",
                label(rule),
                label(severity)
            );
        }
        let _ = writeln!(
            out,
            "# HELP reiki_toolpolicy_unparsed_total Responses that could not be parsed."
        );
        let _ = writeln!(out, "# TYPE reiki_toolpolicy_unparsed_total counter");
        for ((stream, upstream), count) in &inner.unparsed {
            let _ = writeln!(
                out,
                "reiki_toolpolicy_unparsed_total{{stream={},upstream={}}} {count}",
                label(stream),
                label(upstream)
            );
        }
        out
    }
}

fn label(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reiki_plugin::KeyCounts;

    #[test]
    fn renders_counters_with_escaped_labels() {
        let metrics = Metrics::default();
        metrics.record_call("cli\"one", "zen", "blocked");
        metrics.record_finding("unapproved_host", Severity::High);
        metrics.record_unparsed("cli", "zen");
        let ctx = AdminCtx::new(ADMIN_PATH, KeyCounts::default());
        let body = metrics.reply(&ctx).unwrap().body;
        assert!(body.contains("action=\"blocked\"} 1"));
        assert!(body.contains("cli\\\"one"));
        assert!(body.contains("rule=\"unapproved_host\",severity=\"high\"} 1"));
        assert!(
            body.contains("reiki_toolpolicy_unparsed_total{stream=\"cli\",upstream=\"zen\"} 1")
        );
        assert!(
            metrics
                .reply(&AdminCtx::new("/other", KeyCounts::default()))
                .is_none()
        );
    }
}
