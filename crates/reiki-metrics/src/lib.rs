//! Prometheus metrics for the relay, served on the admin listener at `/metrics`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use reiki_plugin::{
    AdminCtx, AdminReply, ChunkCtx, Event, FailureKind, KeyCounts, Plugin, RequestCtx,
    ResponseHeadCtx,
};

/// The `[plugins.metrics]` table this plugin reads.
pub const NAME: &str = "metrics";

const HOST_KEYS: &[&str] = &["enabled", "on_error", "dry_run"];

/// Builds the plugin from its config table, which takes no settings of its own.
pub fn build(table: &toml::Table) -> Result<Box<dyn Plugin>, String> {
    if let Some(key) = table.keys().find(|k| !HOST_KEYS.contains(&k.as_str())) {
        return Err(format!("{NAME}: unknown setting `{key}`"));
    }
    Ok(Box::new(MetricsPlugin::default()))
}

/// Counts events and renders them for the admin `/metrics` path.
#[derive(Debug, Default)]
pub struct MetricsPlugin {
    metrics: Metrics,
}

impl Plugin for MetricsPlugin {
    fn name(&self) -> &str {
        NAME
    }

    fn uses_bodies(&self) -> bool {
        false
    }

    fn on_event(&self, event: &Event<'_>) {
        match *event {
            Event::RequestDone {
                stream,
                upstream,
                status,
            } => self
                .metrics
                .record_request(stream, upstream, StatusClass::from_status(status)),
            Event::Failover { stream, key, kind } => {
                self.metrics.record_failover(stream, key, kind);
            }
            Event::Cooldown { key, kind } => self.metrics.record_cooldown(key, kind),
            Event::RateLimited { stream } => self.metrics.record_rate_limit_rejection(stream),
            Event::Finding {
                stream,
                plugin,
                kind,
                count,
                blocked,
                ..
            } => self
                .metrics
                .record_finding(stream, plugin, kind, count, blocked),
            _ => {}
        }
    }

    fn new_state(&self) -> Box<dyn std::any::Any + Send> {
        Box::new(RequestState::default())
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), reiki_plugin::PluginError> {
        if let Some(state) = ctx.state.downcast_mut::<RequestState>() {
            state.stream = Some(ctx.stream.to_owned());
            state.upstream = Some(ctx.upstream.to_owned());
        }
        Ok(())
    }

    fn on_response_head(&self, ctx: &mut ResponseHeadCtx<'_>) {
        let Some(state) = ctx.state.downcast_mut::<RequestState>() else {
            return;
        };
        state.key = Some(ctx.key.to_owned());
        state.status = Some(ctx.status.as_u16());
        let content_type = ctx
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        state.is_sse = content_type.contains("text/event-stream");
    }

    fn on_response_chunk(&self, ctx: &mut ChunkCtx<'_>) {
        let Some(state) = ctx.state.downcast_mut::<RequestState>() else {
            return;
        };
        if state.exceeded {
            if ctx.is_last {
                // a response that exceeds the bound counts nothing and is not an error
            }
            return;
        }
        if state.is_sse {
            if state.buffer.len() < 16 * 1024 {
                let remaining = 16 * 1024 - state.buffer.len();
                if ctx.chunk.len() <= remaining {
                    state.buffer.extend_from_slice(ctx.chunk);
                } else {
                    state.buffer.extend_from_slice(&ctx.chunk[..remaining]);
                }
            } else {
                state.buffer.extend_from_slice(ctx.chunk);
                if state.buffer.len() > 16 * 1024 {
                    let start = state.buffer.len() - 16 * 1024;
                    state.buffer.drain(0..start);
                }
            }
        } else if state.buffer.len() < 1024 * 1024 {
            let remaining = 1024 * 1024 - state.buffer.len();
            if ctx.chunk.len() <= remaining {
                state.buffer.extend_from_slice(ctx.chunk);
            } else {
                state.buffer.extend_from_slice(&ctx.chunk[..remaining]);
                state.exceeded = true;
            }
        } else {
            state.exceeded = true;
        }
        if ctx.is_last {
            if state.exceeded && !state.is_sse {
                // exceeded bound - counts nothing
                return;
            }
            if state.is_sse && state.buffer.len() > 16 * 1024 {
                return;
            }
            if let Some((input, output)) = extract_usage(&state.buffer, state.is_sse) {
                let stream = state.stream.as_deref().unwrap_or("");
                let upstream = state.upstream.as_deref().unwrap_or("");
                self.metrics.record_tokens(stream, upstream, "input", input);
                self.metrics
                    .record_tokens(stream, upstream, "output", output);
                self.metrics.record_usage_request(stream, upstream);
            }
        }
    }

    fn admin_paths(&self) -> Vec<&'static str> {
        vec!["/metrics", "/usage"]
    }

    fn on_admin(&self, ctx: &AdminCtx<'_>) -> Option<AdminReply> {
        if ctx.path == "/metrics" {
            Some(AdminReply {
                content_type: "text/plain; version=0.0.4; charset=utf-8",
                body: self.metrics.render(ctx.keys),
            })
        } else if ctx.path == "/usage" {
            Some(AdminReply {
                content_type: "application/json",
                body: self.metrics.render_usage(),
            })
        } else {
            None
        }
    }
}

/// Response class recorded for the `reiki_requests_total` counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Success,
    ClientError,
    ServerError,
}

impl StatusClass {
    pub fn from_status(status: u16) -> Self {
        match status {
            100..=399 => Self::Success,
            400..=499 => Self::ClientError,
            _ => Self::ServerError,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ClientError => "client_error",
            Self::ServerError => "server_error",
        }
    }
}

#[derive(Debug, Default)]
struct RequestState {
    stream: Option<String>,
    upstream: Option<String>,
    key: Option<String>,
    status: Option<u16>,
    buffer: Vec<u8>,
    is_sse: bool,
    exceeded: bool,
}

#[derive(Debug, Default)]
struct Inner {
    requests: BTreeMap<(String, String, &'static str), u64>,
    failovers: BTreeMap<(String, String, &'static str), u64>,
    cooldowns: BTreeMap<(String, &'static str), u64>,
    rate_limit_rejections: BTreeMap<String, u64>,
    findings: BTreeMap<(String, String, String, &'static str), u64>,
    tokens: BTreeMap<(String, String, &'static str), u64>,
    usage_requests: BTreeMap<(String, String), u64>,
}

/// In-memory counters for the admin `/metrics` endpoint. Bounded by the number
/// of configured streams and keys, never by request volume.
#[derive(Debug, Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    fn with<F: FnOnce(&mut Inner)>(&self, update: F) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update(&mut inner);
    }

    pub fn record_request(&self, stream: &str, upstream: &str, class: StatusClass) {
        self.with(|inner| {
            *inner
                .requests
                .entry((stream.to_owned(), upstream.to_owned(), class.label()))
                .or_insert(0) += 1;
        });
    }

    pub fn record_failover(&self, stream: &str, key: &str, class: FailureKind) {
        self.with(|inner| {
            *inner
                .failovers
                .entry((stream.to_owned(), key.to_owned(), class.label()))
                .or_insert(0) += 1;
        });
    }

    pub fn record_cooldown(&self, key: &str, class: FailureKind) {
        self.with(|inner| {
            *inner
                .cooldowns
                .entry((key.to_owned(), class.label()))
                .or_insert(0) += 1;
        });
    }

    /// Counts identifiers a detection plugin found. Labels are bounded by the configured streams
    /// and the plugins' fixed kinds, never by request volume.
    pub fn record_finding(
        &self,
        stream: &str,
        plugin: &str,
        kind: &str,
        count: u64,
        blocked: bool,
    ) {
        let action = if blocked { "blocked" } else { "reported" };
        self.with(|inner| {
            *inner
                .findings
                .entry((
                    stream.to_owned(),
                    plugin.to_owned(),
                    kind.to_owned(),
                    action,
                ))
                .or_insert(0) += count;
        });
    }

    pub fn record_rate_limit_rejection(&self, stream: &str) {
        self.with(|inner| {
            *inner
                .rate_limit_rejections
                .entry(stream.to_owned())
                .or_insert(0) += 1;
        });
    }

    pub fn record_tokens(&self, stream: &str, upstream: &str, direction: &'static str, count: u64) {
        self.with(|inner| {
            *inner
                .tokens
                .entry((stream.to_owned(), upstream.to_owned(), direction))
                .or_insert(0) += count;
        });
    }

    pub fn record_usage_request(&self, stream: &str, upstream: &str) {
        self.with(|inner| {
            *inner
                .usage_requests
                .entry((stream.to_owned(), upstream.to_owned()))
                .or_insert(0) += 1;
        });
    }

    /// Render the Prometheus text exposition. `keys` is the current gauge,
    /// sampled by the caller so it reflects the same instant as the scrape.
    pub fn render(&self, keys: KeyCounts) -> String {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = String::new();

        out.push_str("# HELP reiki_requests_total Proxied requests by stream, upstream and response class.\n");
        out.push_str("# TYPE reiki_requests_total counter\n");
        for ((stream, upstream, class), count) in &inner.requests {
            let _ = writeln!(
                out,
                "reiki_requests_total{{stream=\"{}\",upstream=\"{}\",status=\"{}\"}} {}",
                escape_label(stream),
                escape_label(upstream),
                class,
                count
            );
        }

        out.push_str("# HELP reiki_failovers_total Key failures handled by trying the next key.\n");
        out.push_str("# TYPE reiki_failovers_total counter\n");
        for ((stream, key, class), count) in &inner.failovers {
            let _ = writeln!(
                out,
                "reiki_failovers_total{{stream=\"{}\",key=\"{}\",class=\"{}\"}} {}",
                escape_label(stream),
                escape_label(key),
                class,
                count
            );
        }

        out.push_str(
            "# HELP reiki_cooldowns_total Keys put on cooldown or taken out of rotation.\n",
        );
        out.push_str("# TYPE reiki_cooldowns_total counter\n");
        for ((key, class), count) in &inner.cooldowns {
            let _ = writeln!(
                out,
                "reiki_cooldowns_total{{key=\"{}\",class=\"{}\"}} {}",
                escape_label(key),
                class,
                count
            );
        }

        out.push_str(
            "# HELP reiki_rate_limit_rejections_total Requests rejected by a stream rate limit.\n",
        );
        out.push_str("# TYPE reiki_rate_limit_rejections_total counter\n");
        for (stream, count) in &inner.rate_limit_rejections {
            let _ = writeln!(
                out,
                "reiki_rate_limit_rejections_total{{stream=\"{}\"}} {}",
                escape_label(stream),
                count
            );
        }

        out.push_str(
            "# HELP reiki_findings_total Identifiers found in request bodies by a detection plugin.\n",
        );
        out.push_str("# TYPE reiki_findings_total counter\n");
        for ((stream, plugin, kind, action), count) in &inner.findings {
            let _ = writeln!(
                out,
                "reiki_findings_total{{stream=\"{}\",plugin=\"{}\",kind=\"{}\",action=\"{}\"}} {}",
                escape_label(stream),
                escape_label(plugin),
                escape_label(kind),
                action,
                count
            );
        }

        out.push_str("# HELP reiki_tokens_total Tokens by stream, upstream and direction.\n");
        out.push_str("# TYPE reiki_tokens_total counter\n");
        for ((stream, upstream, direction), count) in &inner.tokens {
            let _ = writeln!(
                out,
                "reiki_tokens_total{{stream=\"{}\",upstream=\"{}\",direction=\"{}\"}} {}",
                escape_label(stream),
                escape_label(upstream),
                direction,
                count
            );
        }

        out.push_str("# HELP reiki_keys Keys by rotation state.\n");
        out.push_str("# TYPE reiki_keys gauge\n");
        let _ = writeln!(out, "reiki_keys{{state=\"available\"}} {}", keys.available);
        let _ = writeln!(out, "reiki_keys{{state=\"cooling\"}} {}", keys.cooling);
        let _ = writeln!(
            out,
            "reiki_keys{{state=\"out_of_rotation\"}} {}",
            keys.out_of_rotation
        );

        out
    }

    pub fn render_usage(&self) -> String {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut streams: BTreeMap<String, BTreeMap<String, (u64, u64, u64)>> = BTreeMap::new();
        for ((stream, upstream), requests) in &inner.usage_requests {
            let entry = streams
                .entry(stream.clone())
                .or_default()
                .entry(upstream.clone())
                .or_insert((0, 0, 0));
            entry.2 = *requests;
        }
        for ((stream, upstream, direction), count) in &inner.tokens {
            let entry = streams
                .entry(stream.clone())
                .or_default()
                .entry(upstream.clone())
                .or_insert((0, 0, 0));
            if *direction == "input" {
                entry.0 += *count;
            } else if *direction == "output" {
                entry.1 += *count;
            }
        }
        let mut stream_entries: Vec<String> = Vec::new();
        for (stream, upstreams) in &streams {
            let mut upstream_entries: Vec<String> = Vec::new();
            for (upstream, (input, output, requests)) in upstreams {
                upstream_entries.push(format!(
                    "\"{}\":{{\"input\":{},\"output\":{},\"requests\":{}}}",
                    json_escape(upstream),
                    input,
                    output,
                    requests
                ));
            }
            stream_entries.push(format!(
                "\"{}\":{{{}}}",
                json_escape(stream),
                upstream_entries.join(",")
            ));
        }
        format!("{{{}}}", stream_entries.join(","))
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn extract_usage(buffer: &[u8], is_sse: bool) -> Option<(u64, u64)> {
    if is_sse {
        // Look for the last data: event
        let text = String::from_utf8_lossy(buffer);
        let mut last_data: Option<String> = None;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.trim_start();
                if rest == "[DONE]" {
                    continue;
                }
                last_data = Some(rest.to_string());
            }
        }
        if let Some(data) = last_data {
            return extract_usage_from_json_str(&data);
        }
        None
    } else {
        let text = String::from_utf8_lossy(buffer);
        extract_usage_from_json_str(&text)
    }
}

fn extract_usage_from_json_str(s: &str) -> Option<(u64, u64)> {
    // Try to parse as JSON
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    // Try OpenAI style
    if let Some(usage) = v.get("usage") {
        if let Some(prompt) = usage.get("prompt_tokens").and_then(|x| x.as_u64()) {
            if let Some(completion) = usage.get("completion_tokens").and_then(|x| x.as_u64()) {
                return Some((prompt, completion));
            }
        }
        if let Some(input) = usage.get("input_tokens").and_then(|x| x.as_u64()) {
            if let Some(output) = usage.get("output_tokens").and_then(|x| x.as_u64()) {
                return Some((input, output));
            }
        }
    }
    // Also check Anthropic style at top level sometimes? The requirement says "OpenAI-style usage.prompt_tokens / usage.completion_tokens and Anthropic-style usage.input_tokens / usage.output_tokens" - both under usage object typically
    None
}

/// Escape a value for a Prometheus text label: backslash, double quote and newline.
fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classes_map_ranges() {
        assert_eq!(StatusClass::from_status(200), StatusClass::Success);
        assert_eq!(StatusClass::from_status(302), StatusClass::Success);
        assert_eq!(StatusClass::from_status(404), StatusClass::ClientError);
        assert_eq!(StatusClass::from_status(503), StatusClass::ServerError);
    }

    #[test]
    fn label_escaping_covers_backslash_quote_and_newline() {
        assert_eq!(escape_label("a\"b"), "a\\\"b");
        assert_eq!(escape_label("a\\b"), "a\\\\b");
        assert_eq!(escape_label("a\nb"), "a\\nb");
        assert_eq!(escape_label("plain"), "plain");
    }

    #[test]
    fn render_emits_headers_and_samples() {
        let metrics = Metrics::new();
        metrics.record_request("client", "zen", StatusClass::Success);
        metrics.record_request("client", "zen", StatusClass::Success);
        metrics.record_failover("client", "zen-a", FailureKind::RateLimited);
        metrics.record_cooldown("zen-a", FailureKind::QuotaExhausted);
        metrics.record_rate_limit_rejection("client");

        let text = metrics.render(KeyCounts {
            available: 1,
            cooling: 2,
            out_of_rotation: 0,
        });

        assert!(text.contains("# TYPE reiki_requests_total counter"));
        assert!(text.contains(
            "reiki_requests_total{stream=\"client\",upstream=\"zen\",status=\"success\"} 2"
        ));
        assert!(text.contains(
            "reiki_failovers_total{stream=\"client\",key=\"zen-a\",class=\"rate_limited\"} 1"
        ));
        assert!(text.contains("reiki_cooldowns_total{key=\"zen-a\",class=\"quota_exhausted\"} 1"));
        assert!(text.contains("reiki_rate_limit_rejections_total{stream=\"client\"} 1"));
        assert!(text.contains("reiki_keys{state=\"available\"} 1"));
        assert!(text.contains("reiki_keys{state=\"cooling\"} 2"));
        assert!(text.contains("reiki_keys{state=\"out_of_rotation\"} 0"));
    }

    #[test]
    fn render_always_emits_type_lines_and_the_full_gauge() {
        let text = Metrics::new().render(KeyCounts::default());
        for family in [
            "reiki_requests_total",
            "reiki_failovers_total",
            "reiki_cooldowns_total",
            "reiki_rate_limit_rejections_total",
            "reiki_findings_total",
            "reiki_tokens_total",
            "reiki_keys",
        ] {
            assert!(text.contains(&format!("# TYPE {family} ")), "{family}");
        }
        assert!(text.contains("reiki_keys{state=\"available\"} 0"));
    }

    #[test]
    fn findings_are_counted_by_stream_plugin_kind_and_action() {
        let plugin = build(&toml::Table::new()).unwrap();
        let finding = |count, blocked| Event::Finding {
            id: "r1",
            stream: "s",
            plugin: "phi",
            kind: "ssn",
            count,
            blocked,
        };
        plugin.on_event(&finding(2, false));
        plugin.on_event(&finding(1, false));
        plugin.on_event(&finding(3, true));
        let metrics = Metrics::new();
        metrics.record_finding("s", "phi", "ssn", 2, false);
        metrics.record_finding("s", "phi", "ssn", 3, true);
        let text = metrics.render(KeyCounts::default());
        assert!(text.contains(
            "reiki_findings_total{stream=\"s\",plugin=\"phi\",kind=\"ssn\",action=\"reported\"} 2"
        ));
        assert!(text.contains(
            "reiki_findings_total{stream=\"s\",plugin=\"phi\",kind=\"ssn\",action=\"blocked\"} 3"
        ));
    }

    #[test]
    fn labels_are_escaped_in_the_exposition() {
        let metrics = Metrics::new();
        metrics.record_request("a\"b", "c\\d", StatusClass::ServerError);
        let text = metrics.render(KeyCounts::default());
        assert!(text.contains(
            "reiki_requests_total{stream=\"a\\\"b\",upstream=\"c\\\\d\",status=\"server_error\"} 1"
        ));
    }

    #[test]
    fn events_drive_the_counters_the_admin_path_renders() {
        let plugin = build(&toml::Table::new()).unwrap();
        plugin.on_event(&Event::RequestDone {
            stream: "s",
            upstream: "u",
            status: 503,
        });
        plugin.on_event(&Event::Cooldown {
            key: "k",
            kind: FailureKind::RateLimited,
        });
        let keys = KeyCounts {
            available: 4,
            ..KeyCounts::default()
        };
        let reply = plugin.on_admin(&AdminCtx::new("/metrics", keys)).unwrap();
        assert!(reply.body.contains(
            "reiki_requests_total{stream=\"s\",upstream=\"u\",status=\"server_error\"} 1"
        ));
        assert!(reply
            .body
            .contains("reiki_cooldowns_total{key=\"k\",class=\"rate_limited\"} 1"));
        assert!(reply.body.contains("reiki_keys{state=\"available\"} 4"));
        assert_eq!(plugin.admin_paths(), ["/metrics", "/usage"]);
        assert!(!plugin.uses_bodies());
    }

    #[test]
    fn a_setting_the_plugin_does_not_have_is_refused() {
        let table: toml::Table = "path = \"/m\"".parse().unwrap();
        assert!(build(&table).is_err());
        let host: toml::Table = "enabled = true\non_error = \"reject\"".parse().unwrap();
        assert!(build(&host).is_ok());
    }
}
