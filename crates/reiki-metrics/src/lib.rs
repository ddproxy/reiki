//! Prometheus metrics for the relay, served on the admin listener at `/metrics`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use reiki_plugin::{AdminCtx, AdminReply, Event, FailureKind, KeyCounts, Plugin};

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
            } => self
                .metrics
                .record_finding(stream, plugin, kind, count, blocked),
            _ => {}
        }
    }

    fn admin_paths(&self) -> Vec<&'static str> {
        vec!["/metrics"]
    }

    fn on_admin(&self, ctx: &AdminCtx<'_>) -> Option<AdminReply> {
        Some(AdminReply {
            content_type: "text/plain; version=0.0.4; charset=utf-8",
            body: self.metrics.render(ctx.keys),
        })
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
struct Inner {
    requests: BTreeMap<(String, String, &'static str), u64>,
    failovers: BTreeMap<(String, String, &'static str), u64>,
    cooldowns: BTreeMap<(String, &'static str), u64>,
    rate_limit_rejections: BTreeMap<String, u64>,
    findings: BTreeMap<(String, String, String, &'static str), u64>,
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
        assert_eq!(plugin.admin_paths(), ["/metrics"]);
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
