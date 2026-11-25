//! Compiled-in, operator-priced usage accounting. Never guesses a price, never sees a
//! credential, and never makes a network call.

mod config;
mod money;
mod report;
mod state;

use std::{sync::Arc, sync::Mutex, time::SystemTime};

use reiki_plugin::{AdminCtx, AdminReply, AdmissionRefusal, Event, Plugin, PluginHost};
use serde_json::{Value, json};

pub const NAME: &str = "cost";
pub const ADMIN_PATH: &str = "/cost";
pub const ADMIN_METRICS_PATH: &str = "/cost/metrics";

/// The current UTC month in `YYYY-MM` form, for the CLI's default export period.
pub fn current_period() -> String {
    let seconds = state::unix_seconds(SystemTime::now());
    reiki_plugin::format_utc(seconds, 'T', None, "Z")[..7].to_owned()
}

/// Whether `value` is a supported UTC period filter.
pub fn validate_period(value: &str) -> bool {
    state::parse_period(value).is_some()
}

const BUDGET_MONTHLY_KIND: &str = "budget_monthly";
const BUDGET_MONTHLY_MESSAGE: &str = "monthly cost budget exceeded";
const BUDGET_DAILY_KIND: &str = "budget_daily";
const BUDGET_DAILY_MESSAGE: &str = "daily cost budget exceeded";
const BUDGET_STATE_KIND: &str = "budget_state_unavailable";
const BUDGET_STATE_MESSAGE: &str = "cost budget state is unavailable";

pub struct CostPlugin {
    config: config::Config,
    state: Mutex<state::State>,
    dry_run: bool,
    clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
}

impl CostPlugin {
    /// Test seam for period rollover: the host uses [`SystemTime::now`].
    pub fn with_clock<F: Fn() -> SystemTime + Send + Sync + 'static>(mut self, clock: F) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    fn now(&self) -> SystemTime {
        (self.clock)()
    }

    fn save(&self, now: SystemTime) {
        if self.dry_run {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.save(now);
        }
    }

    fn filter_from_query(&self, query: &str) -> Result<report::Filter, AdminReply> {
        let mut filter = report::Filter::default();
        for pair in query.split('&').filter(|part| !part.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if value.len() > 128 || key.len() > 32 {
                return Err(bad_request("invalid cost filter"));
            }
            match key {
                "period" => {
                    filter.period = Some(value.to_owned());
                    if state::parse_period(value).is_none() {
                        return Err(bad_request("invalid period"));
                    }
                }
                "stream" => filter.stream = Some(value.to_owned()),
                "upstream" => filter.upstream = Some(value.to_owned()),
                "format" => {
                    filter.format = match value {
                        "json" => report::Format::Json,
                        "csv" => report::Format::Csv,
                        _ => return Err(bad_request("invalid export format")),
                    };
                }
                _ => return Err(bad_request("unknown cost filter")),
            }
        }
        Ok(filter)
    }
}

fn bad_request(message: &str) -> AdminReply {
    AdminReply {
        content_type: "application/json",
        body: serde_json::json!({
            "ok": false,
            "error": {"code": "bad_request", "message": message},
        })
        .to_string(),
    }
}

impl Plugin for CostPlugin {
    fn name(&self) -> &str {
        NAME
    }

    fn configure(&mut self, host: &PluginHost) {
        if self.dry_run {
            return;
        }
        if let Some(path) = &host.state_path {
            let mut name = path.clone().into_os_string();
            name.push(".cost.state");
            if let Ok(mut state) = self.state.lock() {
                state.set_path(std::path::PathBuf::from(name), self.now());
            }
        }
    }

    fn live_reload(&self) -> bool {
        false
    }

    fn needs_response_hooks(&self) -> bool {
        false
    }

    fn uses_bodies(&self) -> bool {
        false
    }

    fn on_event(&self, event: &Event<'_>) {
        let Event::Usage {
            stream,
            upstream,
            model,
            input_tokens,
            output_tokens,
            cached_input_tokens,
            ..
        } = event
        else {
            return;
        };
        let now = self.now();
        let rule = self.config.price(upstream, model);
        let (micros, unpriced) = match rule {
            Some(rule) => {
                let cached = (*cached_input_tokens).min(*input_tokens);
                let regular = input_tokens.saturating_sub(cached);
                let mut micros = money::line_cost_micros(rule.input, regular);
                if let Some(cached_price) = rule.cached {
                    micros = micros.saturating_add(money::line_cost_micros(cached_price, cached));
                } else {
                    micros = micros.saturating_add(money::line_cost_micros(rule.input, cached));
                }
                micros =
                    micros.saturating_add(money::line_cost_micros(rule.output, *output_tokens));
                (micros, false)
            }
            None => (0, true),
        };
        let safe_stream = bounded_name(stream);
        let safe_upstream = bounded_name(upstream);
        let safe_model = bounded_name(model);
        if let Ok(mut state) = self.state.lock() {
            state.record(
                now,
                self.config.currency(),
                &safe_stream,
                &safe_upstream,
                &safe_model,
                micros,
                *input_tokens,
                *output_tokens,
                unpriced,
            );
        }
        self.save(now);
    }

    fn on_shutdown(&self) {
        self.save(self.now());
    }

    fn admin_paths(&self) -> Vec<&'static str> {
        vec![ADMIN_PATH, ADMIN_METRICS_PATH]
    }

    fn on_admin(&self, ctx: &AdminCtx<'_>) -> Option<AdminReply> {
        if ctx.path == ADMIN_METRICS_PATH {
            let text = self
                .state
                .lock()
                .map(|state| report::metrics_text(&self.config, &state))
                .unwrap_or_default();
            return Some(AdminReply {
                content_type: "text/plain; version=0.0.4; charset=utf-8",
                body: text,
            });
        }
        if ctx.path != ADMIN_PATH {
            return None;
        }
        let filter = match self.filter_from_query(ctx.query) {
            Ok(filter) => filter,
            Err(reply) => return Some(reply),
        };
        let state = self.state.lock().ok()?;
        let body = match filter.format {
            report::Format::Json => report::render_json(&self.config, &state, &filter, self.now()),
            report::Format::Csv => report::render_csv(&self.config, &state, &filter, self.now()),
        };
        let content_type = match filter.format {
            report::Format::Json => "application/json",
            report::Format::Csv => "text/csv; charset=utf-8",
        };
        Some(AdminReply { content_type, body })
    }

    fn status_fragment(&self) -> Option<Value> {
        let state = self.state.lock().ok()?;
        Some(report::status_fragment(&self.config, &state, self.now()))
    }

    fn metrics_text(&self) -> String {
        self.state
            .lock()
            .map(|state| report::metrics_text(&self.config, &state))
            .unwrap_or_default()
    }

    fn usage_cost_fragment(&self, stream: &str) -> Option<Value> {
        let state = self.state.lock().ok()?;
        let now = self.now();
        let seconds = state::unix_seconds(now);
        let start = state::period_start(seconds, state::Granularity::Month);
        let micros = state
            .buckets()
            .iter()
            .filter(|bucket| {
                bucket.granularity == state::Granularity::Month
                    && bucket.start == start
                    && bucket.stream == stream
            })
            .fold(0u128, |total, bucket| total.saturating_add(bucket.micros));
        let unpriced = state
            .buckets()
            .iter()
            .filter(|bucket| {
                bucket.granularity == state::Granularity::Month
                    && bucket.start == start
                    && bucket.stream == stream
            })
            .fold(0u64, |total, bucket| total.saturating_add(bucket.unpriced));
        Some(json!({
            "estimated": true,
            "currency": self.config.currency(),
            "period": state::period_key(seconds, state::Granularity::Month),
            "estimated_micros": micros.to_string(),
            "estimated_display": money::format_micros(micros),
            "unpriced_requests": unpriced,
            "note": report::ESTIMATE_NOTE,
        }))
    }

    fn admission_refusal(&self, stream: &str, now: SystemTime) -> Option<AdmissionRefusal> {
        let budget = self.config.budgets.get(stream)?;
        if budget.on_exceed != config::OnExceed::Block {
            return None;
        }
        let state = self.state.lock().ok()?;
        if !state.healthy {
            return Some(AdmissionRefusal {
                kind: BUDGET_STATE_KIND,
                message: BUDGET_STATE_MESSAGE,
            });
        }
        if budget.monthly.is_some_and(|limit| {
            state.current_spend(stream, state::Granularity::Month, now) >= limit
        }) {
            return Some(AdmissionRefusal {
                kind: BUDGET_MONTHLY_KIND,
                message: BUDGET_MONTHLY_MESSAGE,
            });
        }
        if budget
            .daily
            .is_some_and(|limit| state.current_spend(stream, state::Granularity::Day, now) >= limit)
        {
            return Some(AdmissionRefusal {
                kind: BUDGET_DAILY_KIND,
                message: BUDGET_DAILY_MESSAGE,
            });
        }
        None
    }
}

fn bounded_name(name: &str) -> String {
    let mut out = String::new();
    for character in name.chars().take(128) {
        if character.is_ascii_alphanumeric()
            || matches!(character, '_' | '-' | '.' | '/' | '*' | '?')
        {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_owned()
    } else {
        out
    }
}

/// Builds the plugin from its raw config table.
pub fn build(table: &toml::Table) -> Result<Box<dyn Plugin>, String> {
    let config = config::Config::parse(table)?;
    let dry_run = table
        .get("dry_run")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    Ok(Box::new(CostPlugin {
        config,
        state: Mutex::new(state::State::load(None, SystemTime::now())),
        dry_run,
        clock: Arc::new(SystemTime::now),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Granularity;

    fn now(stamp: &str) -> SystemTime {
        state::now_for_test(stamp)
    }

    fn plugin(text: &str, at: SystemTime) -> CostPlugin {
        let table: toml::Table = toml::from_str(text).unwrap();
        CostPlugin {
            config: config::Config::parse(&table).unwrap(),
            state: Mutex::new(state::State::load(None, at)),
            dry_run: false,
            clock: Arc::new(move || at),
        }
    }

    fn usage<'a>(
        stream: &'a str,
        upstream: &'a str,
        model: &'a str,
        input: u64,
        output: u64,
    ) -> Event<'a> {
        Event::Usage {
            stream,
            upstream,
            model,
            key: "operator-key-name",
            tokens: input + output,
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: 0,
            status_class: "2xx",
        }
    }

    #[test]
    fn prices_are_fixed_point_and_unpriced_models_are_counted() {
        let at = now("2026-11-02T03:00");
        let cost = plugin(
            r#"
[prices.main]
currency = "USD"
[prices.main.models]
"exact" = { input_per_million = "2.50", output_per_million = "10" }
"*" = { input_per_million = "1", output_per_million = "2" }
"#,
            at,
        );
        cost.on_event(&usage("one", "main", "exact", 1_000_000, 1_000_000));
        cost.on_event(&usage("one", "main", "unknown", 1_000, 1_000));
        {
            let state = cost.state.lock().unwrap();
            let day = state
                .buckets()
                .iter()
                .filter(|bucket| bucket.granularity == Granularity::Day);
            let total: u128 = day.clone().map(|bucket| bucket.micros).sum();
            // 2.50 + 10.00 for the exact model, 0.001 + 0.002 for the glob.
            assert_eq!(total, 12_503_000);
            assert_eq!(day.filter(|b| b.unpriced > 0).count(), 0); // unknown matched the glob
        }
        let state = cost.state.lock().unwrap();
        assert_eq!(state.current_spend("one", Granularity::Day, at), 12_503_000);
    }

    #[test]
    fn unmatched_model_is_reported_as_unpriced() {
        let at = now("2026-11-02T03:00");
        let cost = plugin(
            r#"
[prices.main]
currency = "USD"
[prices.main.models]
"exact" = { input_per_million = "2.50", output_per_million = "10" }
"#,
            at,
        );
        cost.on_event(&usage("one", "main", "other", 500, 500));
        let filter = report::Filter::default();
        let body = report::render_json(&cost.config, &cost.state.lock().unwrap(), &filter, at);
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["totals"]["unpriced_requests"], 1);
        assert_eq!(value["totals"]["estimated_micros"], "0");
    }

    #[test]
    fn budget_block_uses_static_refusal_and_ignores_other_streams() {
        let at = now("2026-11-02T03:00");
        let cost = plugin(
            r#"
[prices.main]
currency = "USD"
[prices.main.models]
"m" = { input_per_million = "1", output_per_million = "1" }
[budgets.one]
budget_daily = "0.000001"
on_exceed = "block"
"#,
            at,
        );
        assert!(cost.admission_refusal("one", at).is_none());
        cost.on_event(&usage("one", "main", "m", 1, 0));
        let refusal = cost.admission_refusal("one", at).unwrap();
        assert_eq!(refusal.kind, "budget_daily");
        assert_eq!(refusal.message, "daily cost budget exceeded");
        assert!(cost.admission_refusal("two", at).is_none());
    }

    #[test]
    fn period_rollover_uses_the_injected_clock() {
        let path = std::env::temp_dir().join(format!(
            "reiki-cost-rollover-{}-{}.state",
            std::process::id(),
            "test"
        ));
        let _ = std::fs::remove_file(format!("{}.cost.state", path.display()));
        let config = r#"
[prices.main]
currency = "USD"
[prices.main.models]
"m" = { input_per_million = "1000000", output_per_million = "1000000" }
"#;
        let mut october =
            plugin(config, now("2026-10-31T23:00")).with_clock(|| now("2026-10-31T23:00"));
        let host = reiki_plugin::PluginHost::with_state_path(path.clone());
        october.configure(&host);
        october.on_event(&usage("one", "main", "m", 1, 0));
        october.save(now("2026-10-31T23:00"));

        let november = plugin(config, now("2026-11-01T00:00"));
        let november = november.with_clock(|| now("2026-11-01T00:00"));
        let mut november = november;
        november.configure(&host);
        let state = november.state.lock().unwrap();
        let oct = now("2026-10-31T23:00");
        let nov = now("2026-11-01T00:00");
        assert_eq!(
            state.current_spend("one", Granularity::Month, oct),
            1_000_000
        );
        assert_eq!(state.current_spend("one", Granularity::Month, nov), 0);
        drop(state);
        november.on_event(&usage("one", "main", "m", 1, 0));
        let state = november.state.lock().unwrap();
        assert_eq!(
            state.current_spend("one", Granularity::Month, oct),
            1_000_000
        );
        assert_eq!(
            state.current_spend("one", Granularity::Month, nov),
            1_000_000
        );
        drop(state);
        let _ = std::fs::remove_file(format!("{}.cost.state", path.display()));
    }

    #[test]
    fn budget_report_never_refuses() {
        let at = now("2026-11-02T03:00");
        let cost = plugin(
            r#"
[prices.main]
currency = "USD"
[prices.main.models]
"m" = { input_per_million = "1", output_per_million = "1" }
[budgets.one]
budget_daily = "0.000001"
"#,
            at,
        );
        cost.on_event(&usage("one", "main", "m", 1, 0));
        assert!(cost.admission_refusal("one", at).is_none());
    }

    #[test]
    fn a_block_budget_fails_closed_when_state_is_unreadable() {
        let at = now("2026-11-02T03:00");
        let mut cost = plugin(
            r#"
[prices.main]
currency = "USD"
[prices.main.models]
"m" = { input_per_million = "1", output_per_million = "1" }
[budgets.one]
budget_daily = "1"
on_exceed = "block"
"#,
            at,
        );
        // A directory at the state path cannot be read as a state file, but exists,
        // so the load cannot be mistaken for a fresh, healthy empty state.
        let directory = std::env::temp_dir().join(format!(
            "reiki-cost-unreadable-{}-{}.state",
            std::process::id(),
            "test"
        ));
        let blocked = directory.with_extension("state.cost.state");
        let _ = std::fs::remove_dir_all(&blocked);
        std::fs::create_dir_all(&blocked).unwrap();
        let host = reiki_plugin::PluginHost::with_state_path(directory);
        cost.configure(&host);
        let refusal = cost.admission_refusal("one", at).unwrap();
        assert_eq!(refusal.kind, "budget_state_unavailable");
        assert_eq!(refusal.message, "cost budget state is unavailable");
        std::fs::remove_dir_all(&blocked).unwrap();
    }

    #[test]
    fn only_the_authenticated_streams_cost_is_returned() {
        let at = now("2026-11-02T03:00");
        let cost = plugin(
            r#"
[prices.main]
currency = "USD"
[prices.main.models]
"m" = { input_per_million = "1", output_per_million = "1" }
"#,
            at,
        );
        cost.on_event(&usage("one", "main", "m", 1_000_000, 0));
        let one = cost.usage_cost_fragment("one").unwrap();
        let two = cost.usage_cost_fragment("two").unwrap();
        assert_eq!(one["estimated_micros"], "1000000");
        assert_eq!(two["estimated_micros"], "0");
    }

    #[test]
    fn exports_are_json_or_csv() {
        let at = now("2026-11-02T03:00");
        let cost = plugin(
            r#"
[prices.main]
currency = "USD"
[prices.main.models]
"m" = { input_per_million = "1", output_per_million = "1" }
"#,
            at,
        );
        cost.on_event(&usage("one", "main", "m", 1_000_000, 0));
        let table: toml::Table = toml::from_str("").unwrap();
        let _ = table;
        let state = cost.state.lock().unwrap();
        let json = report::render_json(&cost.config, &state, &report::Filter::default(), at);
        assert!(json.contains("\"estimated\":true"));
        let csv = report::render_csv(&cost.config, &state, &report::Filter::default(), at);
        assert!(csv.starts_with("period,stream,upstream,model,currency,"));
        assert!(csv.contains("2026-11,one,main,m,USD"));
    }
}
