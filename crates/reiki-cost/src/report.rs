//! Read-only views of the cost counters: admin JSON, CSV export, status and metrics.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::{
    config::Config,
    money::format_micros,
    state::{Granularity, State, parse_period, period_key, unix_seconds},
};

pub const MAX_LABEL_BYTES: usize = 64;
pub const MAX_METRIC_SERIES: usize = 64;
pub const ESTIMATE_NOTE: &str = "Estimates from the operator price table, not provider invoices.";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Json,
    Csv,
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub period: Option<String>,
    pub stream: Option<String>,
    pub upstream: Option<String>,
    pub format: Format,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub period: String,
    pub stream: String,
    pub upstream: String,
    pub model: String,
    pub currency: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub micros: u128,
    pub unpriced: u64,
}

impl Row {
    fn json(&self) -> Value {
        json!({
            "period": self.period,
            "stream": self.stream,
            "upstream": self.upstream,
            "model": self.model,
            "currency": self.currency,
            "requests": self.requests,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "estimated_micros": self.micros.to_string(),
            "estimated_display": format_micros(self.micros),
            "unpriced_requests": self.unpriced,
        })
    }
}

/// Aggregates buckets at the granularity implied by the requested period.
pub fn rows(state: &State, filter: &Filter, now: std::time::SystemTime) -> Vec<Row> {
    let (granularity, start, end) = filter
        .period
        .as_deref()
        .and_then(parse_period)
        .unwrap_or_else(|| {
            let seconds = unix_seconds(now);
            (
                Granularity::Month,
                crate::state::period_start(seconds, Granularity::Month),
                seconds + 1,
            )
        });
    let mut grouped: BTreeMap<(String, String, String), Row> = BTreeMap::new();
    for bucket in state.buckets() {
        if bucket.granularity != granularity || bucket.start < start || bucket.start >= end {
            continue;
        }
        if filter
            .stream
            .as_deref()
            .is_some_and(|stream| stream != bucket.stream)
            || filter
                .upstream
                .as_deref()
                .is_some_and(|upstream| upstream != bucket.upstream)
        {
            continue;
        }
        let key = (
            bucket.stream.clone(),
            bucket.upstream.clone(),
            bucket.model.clone(),
        );
        let row = grouped.entry(key).or_insert_with(|| Row {
            period: period_key(bucket.start, granularity),
            stream: bucket.stream.clone(),
            upstream: bucket.upstream.clone(),
            model: bucket.model.clone(),
            currency: bucket.currency.clone(),
            requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            micros: 0,
            unpriced: 0,
        });
        row.requests = row.requests.saturating_add(bucket.requests);
        row.input_tokens = row.input_tokens.saturating_add(bucket.input_tokens);
        row.output_tokens = row.output_tokens.saturating_add(bucket.output_tokens);
        row.micros = row.micros.saturating_add(bucket.micros);
        row.unpriced = row.unpriced.saturating_add(bucket.unpriced);
    }
    grouped.into_values().collect()
}

pub fn render_json(
    config: &Config,
    state: &State,
    filter: &Filter,
    now: std::time::SystemTime,
) -> String {
    let rows = rows(state, filter, now);
    let micros = rows
        .iter()
        .fold(0u128, |total, row| total.saturating_add(row.micros));
    let requests = rows
        .iter()
        .fold(0u64, |total, row| total.saturating_add(row.requests));
    let unpriced = rows
        .iter()
        .fold(0u64, |total, row| total.saturating_add(row.unpriced));
    let period = filter
        .period
        .clone()
        .unwrap_or_else(|| period_key(unix_seconds(now), Granularity::Month));
    json!({
        "currency": config.currency(),
        "period": period,
        "estimated": true,
        "note": ESTIMATE_NOTE,
        "state_healthy": state.healthy,
        "totals": {
            "requests": requests,
            "estimated_micros": micros.to_string(),
            "estimated_display": format_micros(micros),
            "unpriced_requests": unpriced,
        },
        "rows": rows.iter().map(Row::json).collect::<Vec<_>>(),
    })
    .to_string()
}

pub fn render_csv(
    config: &Config,
    state: &State,
    filter: &Filter,
    now: std::time::SystemTime,
) -> String {
    let mut out = String::from(
        "period,stream,upstream,model,currency,requests,input_tokens,output_tokens,estimated_micros,unpriced_requests,estimated_display,note\n",
    );
    for row in rows(state, filter, now) {
        let cells = [
            row.period,
            row.stream,
            row.upstream,
            row.model,
            row.currency,
            row.requests.to_string(),
            row.input_tokens.to_string(),
            row.output_tokens.to_string(),
            row.micros.to_string(),
            row.unpriced.to_string(),
            format_micros(row.micros),
            ESTIMATE_NOTE.to_owned(),
        ];
        out.push_str(
            &cells
                .iter()
                .map(|cell| csv_cell(cell))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    let _ = config;
    out
}

fn csv_cell(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

pub fn status_fragment(config: &Config, state: &State, now: std::time::SystemTime) -> Value {
    let seconds = unix_seconds(now);
    let period = period_key(seconds, Granularity::Month);
    let start = crate::state::period_start(seconds, Granularity::Month);
    let micros = state
        .buckets()
        .iter()
        .filter(|bucket| bucket.granularity == Granularity::Month && bucket.start == start)
        .fold(0u128, |total, bucket| total.saturating_add(bucket.micros));
    let unpriced = state
        .buckets()
        .iter()
        .filter(|bucket| bucket.granularity == Granularity::Month && bucket.start == start)
        .fold(0u64, |total, bucket| total.saturating_add(bucket.unpriced));
    json!({
        "cost": {
            "currency": config.currency(),
            "period": period,
            "estimated": true,
            "estimated_micros": micros.to_string(),
            "estimated_display": format_micros(micros),
            "unpriced_requests": unpriced,
            "note": ESTIMATE_NOTE,
        }
    })
}

/// Maps an arbitrary name to a bounded, safe Prometheus label value.
pub fn label(name: &str) -> String {
    name.chars()
        .take(MAX_LABEL_BYTES)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn escape(value: &str) -> String {
    label(value).replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn metrics_text(config: &Config, state: &State) -> String {
    #[derive(Default)]
    struct Series {
        requests: u64,
        micros: u128,
        unpriced: u64,
        models: BTreeMap<String, u128>,
    }
    let mut series: BTreeMap<(String, String), Series> = BTreeMap::new();
    for bucket in state.buckets() {
        if bucket.granularity != Granularity::Day {
            continue;
        }
        let entry = series
            .entry((bucket.stream.clone(), bucket.upstream.clone()))
            .or_default();
        entry.requests = entry.requests.saturating_add(bucket.requests);
        entry.micros = entry.micros.saturating_add(bucket.micros);
        entry.unpriced = entry.unpriced.saturating_add(bucket.unpriced);
        if config.model_labels.contains(&bucket.model) {
            let model = entry.models.entry(bucket.model.clone()).or_insert(0);
            *model = model.saturating_add(bucket.micros);
        }
    }
    let mut out = String::from(
        "# HELP reiki_cost_estimated_spend_micros Estimated spend in integer micro-units.\n\
         # TYPE reiki_cost_estimated_spend_micros gauge\n",
    );
    for (shown, ((stream, upstream), values)) in series.into_iter().enumerate() {
        if shown >= MAX_METRIC_SERIES {
            break;
        }
        let labels = format!(
            "stream=\"{}\",upstream=\"{}\",currency=\"{}\"",
            escape(&stream),
            escape(&upstream),
            escape(config.currency())
        );
        out.push_str(&format!(
            "reiki_cost_estimated_spend_micros{{{labels}}} {}\n",
            values.micros
        ));
        out.push_str(&format!(
            "reiki_cost_requests_total{{{labels}}} {}\n",
            values.requests
        ));
        out.push_str(&format!(
            "reiki_cost_unpriced_requests_total{{{labels}}} {}\n",
            values.unpriced
        ));
        for (model, micros) in values.models {
            out.push_str(&format!(
                "reiki_cost_model_spend_micros{{{labels},model=\"{}\"}} {micros}\n",
                escape(&model)
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_bounded_and_safe() {
        let long = "x".repeat(200);
        let rendered = label(&format!("{long}\n\"quoted\""));
        assert_eq!(rendered.len(), MAX_LABEL_BYTES);
        assert!(
            rendered
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        );
    }

    #[test]
    fn csv_quotes_commas_and_quotes() {
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("a\"b"), "\"a\"\"b\"");
    }
}
