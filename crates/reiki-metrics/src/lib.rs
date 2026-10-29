//! Prometheus metrics for the relay, served on the admin listener at `/metrics`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

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
    metrics: Arc<Metrics>,
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
        Box::new(RequestState::new(Arc::clone(&self.metrics)))
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
        state.feed(ctx.chunk);
        if ctx.is_last {
            state.finish();
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

/// The most JSON response body kept to read `usage` from.
const MAX_JSON_BODY: usize = 1024 * 1024;
/// The longest event-stream line kept while waiting for its newline.
const MAX_SSE_LINE: usize = 64 * 1024;

/// What one request needs to count its tokens. Dropping it counts them too: with a known
/// content length the host never makes the final `is_last` call.
#[derive(Debug)]
struct RequestState {
    metrics: Arc<Metrics>,
    stream: Option<String>,
    upstream: Option<String>,
    key: Option<String>,
    status: Option<u16>,
    is_sse: bool,
    /// A JSON body, or the unfinished line of an event stream.
    buffer: Vec<u8>,
    exceeded: bool,
    input: Option<u64>,
    output: Option<u64>,
    done: bool,
}

impl RequestState {
    fn new(metrics: Arc<Metrics>) -> Self {
        Self {
            metrics,
            stream: None,
            upstream: None,
            key: None,
            status: None,
            is_sse: false,
            buffer: Vec::new(),
            exceeded: false,
            input: None,
            output: None,
            done: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        if self.is_sse {
            self.buffer.extend_from_slice(chunk);
            while let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                self.read_event_line(&line);
            }
            if self.buffer.len() > MAX_SSE_LINE {
                self.buffer.clear();
            }
        } else if !self.exceeded {
            if self.buffer.len() + chunk.len() > MAX_JSON_BODY {
                self.exceeded = true;
                self.buffer.clear();
            } else {
                self.buffer.extend_from_slice(chunk);
            }
        }
    }

    /// An event stream reports usage in whichever events carry it: one final chunk for OpenAI
    /// style, `message_start` and `message_delta` for Anthropic style. Later values win.
    fn read_event_line(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let Some(data) = text.trim().strip_prefix("data:") else {
            return;
        };
        if !data.contains("\"usage\"") {
            return;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(data.trim()) {
            self.take(usage_in(&value));
        }
    }

    fn take(&mut self, (input, output): (Option<u64>, Option<u64>)) {
        self.input = input.or(self.input);
        self.output = output.or(self.output);
    }

    fn finish(&mut self) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        if self.is_sse {
            let rest = std::mem::take(&mut self.buffer);
            self.read_event_line(&rest);
        } else if !self.exceeded {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&self.buffer) {
                self.take(usage_in(&value));
            }
        }
        if self.input.is_none() && self.output.is_none() {
            return;
        }
        let stream = self.stream.as_deref().unwrap_or("");
        let upstream = self.upstream.as_deref().unwrap_or("");
        self.metrics
            .record_tokens(stream, upstream, "input", self.input.unwrap_or(0));
        self.metrics
            .record_tokens(stream, upstream, "output", self.output.unwrap_or(0));
        self.metrics.record_usage_request(stream, upstream);
    }
}

impl Drop for RequestState {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Input and output tokens from a response or an event, in OpenAI
/// (`prompt_tokens`/`completion_tokens`) or Anthropic (`input_tokens`/`output_tokens`) naming.
/// Anthropic's `message_start` nests them under `message.usage`.
fn usage_in(value: &serde_json::Value) -> (Option<u64>, Option<u64>) {
    let usage = value
        .get("usage")
        .or_else(|| value.pointer("/message/usage"));
    let Some(usage) = usage else {
        return (None, None);
    };
    let count = |names: [&str; 2]| names.iter().find_map(|n| usage.get(*n)?.as_u64());
    (
        count(["prompt_tokens", "input_tokens"]),
        count(["completion_tokens", "output_tokens"]),
    )
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
        let mut streams = serde_json::Map::new();
        let mut cell = |stream: &str, upstream: &str, field: &str, count: u64| {
            let upstreams = streams
                .entry(stream.to_owned())
                .or_insert_with(|| serde_json::json!({}));
            let totals = upstreams
                .as_object_mut()
                .expect("an object")
                .entry(upstream.to_owned())
                .or_insert_with(|| serde_json::json!({"input": 0, "output": 0, "requests": 0}));
            totals[field] = serde_json::json!(count);
        };
        for ((stream, upstream), requests) in &inner.usage_requests {
            cell(stream, upstream, "requests", *requests);
        }
        for ((stream, upstream, direction), count) in &inner.tokens {
            cell(stream, upstream, direction, *count);
        }
        serde_json::Value::Object(streams).to_string()
    }
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

    fn usage_of(metrics: &Arc<Metrics>) -> serde_json::Value {
        serde_json::from_str(&metrics.render_usage()).unwrap()
    }

    fn state(metrics: &Arc<Metrics>, sse: bool) -> RequestState {
        let mut state = RequestState::new(Arc::clone(metrics));
        state.stream = Some("s".to_owned());
        state.upstream = Some("zen".to_owned());
        state.is_sse = sse;
        state
    }

    #[test]
    fn a_json_body_counts_openai_and_anthropic_usage() {
        let metrics = Arc::new(Metrics::default());
        let mut openai = state(&metrics, false);
        openai.feed(br#"{"usage":{"prompt_tokens":10,"completion_tokens":20}}"#);
        openai.finish();
        let mut anthropic = state(&metrics, false);
        anthropic.feed(br#"{"usage":{"input_tokens":3,"output_tokens":4}}"#);
        anthropic.finish();
        let usage = usage_of(&metrics);
        assert_eq!(usage["s"]["zen"]["input"], 13);
        assert_eq!(usage["s"]["zen"]["output"], 24);
        assert_eq!(usage["s"]["zen"]["requests"], 2);
    }

    #[test]
    fn an_event_stream_usage_split_across_chunks_is_counted() {
        let metrics = Arc::new(Metrics::default());
        let mut s = state(&metrics, true);
        let stream = "data: {\"choices\":[]}\n\ndata: {\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":9}}\n\ndata: [DONE]\n\n";
        for piece in stream.as_bytes().chunks(5) {
            s.feed(piece);
        }
        s.finish();
        let usage = usage_of(&metrics);
        assert_eq!(
            (
                usage["s"]["zen"]["input"].as_u64(),
                usage["s"]["zen"]["output"].as_u64()
            ),
            (Some(7), Some(9))
        );
    }

    #[test]
    fn anthropic_streams_combine_message_start_and_message_delta() {
        let metrics = Arc::new(Metrics::default());
        let mut s = state(&metrics, true);
        s.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n");
        s.feed(&vec![b'x'; 200_000]);
        s.feed(b"\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":77}}\n\n");
        s.finish();
        let usage = usage_of(&metrics);
        assert_eq!(
            (
                usage["s"]["zen"]["input"].as_u64(),
                usage["s"]["zen"]["output"].as_u64()
            ),
            (Some(25), Some(77))
        );
    }

    #[test]
    fn a_response_without_usage_or_over_the_bound_counts_nothing() {
        let metrics = Arc::new(Metrics::default());
        let mut none = state(&metrics, false);
        none.feed(br#"{"choices":[]}"#);
        none.finish();
        let mut big = state(&metrics, false);
        big.feed(&vec![b' '; MAX_JSON_BODY]);
        big.feed(br#"{"usage":{"prompt_tokens":1,"completion_tokens":1}}"#);
        assert!(big.buffer.is_empty(), "an oversized body is not kept");
        big.finish();
        assert_eq!(usage_of(&metrics), serde_json::json!({}));
    }

    #[test]
    fn dropping_the_state_counts_a_response_that_never_saw_its_last_chunk() {
        let metrics = Arc::new(Metrics::default());
        {
            let mut s = state(&metrics, false);
            s.feed(br#"{"usage":{"prompt_tokens":2,"completion_tokens":3}}"#);
        }
        assert_eq!(usage_of(&metrics)["s"]["zen"]["output"], 3);
    }

    #[test]
    fn the_token_counter_is_exposed_in_prometheus_text() {
        let metrics = Arc::new(Metrics::default());
        let mut s = state(&metrics, false);
        s.feed(br#"{"usage":{"prompt_tokens":2,"completion_tokens":3}}"#);
        s.finish();
        let text = metrics.render(KeyCounts::default());
        assert!(
            text.contains(
                "reiki_tokens_total{stream=\"s\",upstream=\"zen\",direction=\"input\"} 2"
            ),
            "{text}"
        );
    }
}
