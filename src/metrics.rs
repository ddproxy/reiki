use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use crate::classify::FailureClass;

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

/// Why a key left rotation, as a stable metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    RateLimited,
    QuotaExhausted,
    AccessDisabled,
    Unauthorized,
    Transient,
}

impl FailureKind {
    fn label(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::QuotaExhausted => "quota_exhausted",
            Self::AccessDisabled => "access_disabled",
            Self::Unauthorized => "unauthorized",
            Self::Transient => "transient",
        }
    }
}

impl From<FailureClass> for FailureKind {
    fn from(class: FailureClass) -> Self {
        match class {
            FailureClass::RateLimited(_) => Self::RateLimited,
            FailureClass::QuotaExhausted => Self::QuotaExhausted,
            FailureClass::AccessDisabled => Self::AccessDisabled,
            FailureClass::Unauthorized => Self::Unauthorized,
            FailureClass::Transient => Self::Transient,
        }
    }
}

/// Current key totals, derived from key health at scrape time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyGauge {
    pub available: u64,
    pub cooling: u64,
    pub out_of_rotation: u64,
}

#[derive(Debug, Default)]
struct Inner {
    requests: BTreeMap<(String, String, &'static str), u64>,
    failovers: BTreeMap<(String, String, &'static str), u64>,
    cooldowns: BTreeMap<(String, &'static str), u64>,
    rate_limit_rejections: BTreeMap<String, u64>,
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
    pub fn render(&self, keys: KeyGauge) -> String {
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
    fn failure_classes_map_to_stable_labels() {
        assert_eq!(
            FailureKind::from(FailureClass::RateLimited(None)).label(),
            "rate_limited"
        );
        assert_eq!(
            FailureKind::from(FailureClass::QuotaExhausted).label(),
            "quota_exhausted"
        );
        assert_eq!(
            FailureKind::from(FailureClass::AccessDisabled).label(),
            "access_disabled"
        );
        assert_eq!(
            FailureKind::from(FailureClass::Unauthorized).label(),
            "unauthorized"
        );
        assert_eq!(
            FailureKind::from(FailureClass::Transient).label(),
            "transient"
        );
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

        let text = metrics.render(KeyGauge {
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
        let text = Metrics::new().render(KeyGauge::default());
        for family in [
            "reiki_requests_total",
            "reiki_failovers_total",
            "reiki_cooldowns_total",
            "reiki_rate_limit_rejections_total",
            "reiki_keys",
        ] {
            assert!(text.contains(&format!("# TYPE {family} ")), "{family}");
        }
        assert!(text.contains("reiki_keys{state=\"available\"} 0"));
    }

    #[test]
    fn labels_are_escaped_in_the_exposition() {
        let metrics = Metrics::new();
        metrics.record_request("a\"b", "c\\d", StatusClass::ServerError);
        let text = metrics.render(KeyGauge::default());
        assert!(text.contains(
            "reiki_requests_total{stream=\"a\\\"b\",upstream=\"c\\\\d\",status=\"server_error\"} 1"
        ));
    }
}
