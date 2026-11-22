//! Local cost summary fetched from the admin listener, kept separate from peer health.
//!
//! Cost is an estimate from the operator's own price table. It is never published to peers
//! and never used as a provider invoice.

use serde::Deserialize;

/// The `totals` object of `GET /v1/cost`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CostTotals {
    pub requests: u64,
    pub estimated_micros: String,
    pub estimated_display: String,
    pub unpriced_requests: u64,
}

/// A bounded parse of the admin cost answer. Only the fields `top` renders are kept.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CostAnswer {
    currency: String,
    period: String,
    estimated: bool,
    note: String,
    state_healthy: bool,
    totals: CostTotals,
}

/// One local instance's estimated spend for one UTC period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCost {
    pub currency: String,
    pub period: String,
    pub totals: CostTotals,
    pub state_healthy: bool,
}

impl LocalCost {
    /// Parses an admin `/v1/cost` JSON body. Length is capped by the caller's HTTP client;
    /// unknown fields and non-string micro amounts are rejected rather than guessed.
    pub fn from_json(body: &str) -> Result<Self, String> {
        let answer: CostAnswer =
            serde_json::from_str(body).map_err(|e| format!("bad cost report: {e}"))?;
        if answer.currency.is_empty()
            || answer.currency.len() > 16
            || answer.period.len() > 16
            || answer.note.len() > 256
            || !answer.estimated
            || answer.totals.estimated_micros.len() > 64
            || answer.totals.estimated_display.len() > 64
            || answer.totals.estimated_micros.parse::<u128>().is_err()
        {
            return Err("bad cost report".to_owned());
        }
        Ok(Self {
            currency: answer.currency,
            period: answer.period,
            totals: answer.totals,
            state_healthy: answer.state_healthy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bounded_cost_answer() {
        let body = r#"{"currency":"USD","period":"2026-11","estimated":true,"note":"Estimates from the operator price table, not provider invoices.","state_healthy":true,"totals":{"requests":3,"estimated_micros":"2500000","estimated_display":"2.500000","unpriced_requests":1}}"#;
        let cost = LocalCost::from_json(body).unwrap();
        assert_eq!(cost.currency, "USD");
        assert_eq!(cost.period, "2026-11");
        assert_eq!(cost.totals.requests, 3);
        assert_eq!(cost.totals.estimated_micros, "2500000");
        assert!(cost.state_healthy);
    }

    #[test]
    fn rejects_unknown_or_unestimated_answers() {
        let body = r#"{"currency":"USD","period":"2026-11","estimated":false,"note":"x","state_healthy":true,"totals":{"requests":0,"estimated_micros":"0","estimated_display":"0.000000","unpriced_requests":0}}"#;
        assert!(LocalCost::from_json(body).is_err());
        let borrowed = body.replace("\"note\":\"x\"", "\"note\":\"x\",\"extra\":1");
        assert!(LocalCost::from_json(&borrowed).is_err());
    }
}
