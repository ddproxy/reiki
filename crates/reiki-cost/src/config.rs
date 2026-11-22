//! Strict parsing of `[plugins.cost]`. Prices stay in plugin config; the host never sees them.

use std::collections::{BTreeMap, BTreeSet};

use reiki_plugin::HOST_KEYS;

use crate::money::{NANOS_PER_UNIT, price_per_million, round_div};

pub const MAX_NAME_BYTES: usize = 128;
pub const MAX_PATTERN_BYTES: usize = 128;
pub const MAX_PRICE_TABLES: usize = 64;
pub const MAX_MODELS_PER_TABLE: usize = 256;
pub const MAX_BUDGET_STREAMS: usize = 512;
pub const MAX_MODEL_LABELS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnExceed {
    Report,
    Block,
}

#[derive(Debug, Clone)]
pub struct Budget {
    pub monthly: Option<u128>,
    pub daily: Option<u128>,
    pub on_exceed: OnExceed,
}

#[derive(Debug, Clone)]
pub struct PriceRule {
    pub pattern: String,
    pub input: u128,
    pub output: u128,
    pub cached: Option<u128>,
    pub glob: bool,
}

#[derive(Debug, Clone)]
pub struct PriceTable {
    pub currency: String,
    pub exact: BTreeMap<String, PriceRule>,
    /// Longest patterns first, so the first match is the most specific one.
    pub globs: Vec<PriceRule>,
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub prices: BTreeMap<String, PriceTable>,
    pub budgets: BTreeMap<String, Budget>,
    pub model_labels: BTreeSet<String>,
}

impl Config {
    /// Parses the raw table the host passes to `build`, rejecting every unknown key.
    pub fn parse(table: &toml::Table) -> Result<Self, String> {
        let mut config = Config::default();
        let mut allowed: Vec<&str> = HOST_KEYS.to_vec();
        allowed.extend(["prices", "budgets", "model_label_allowlist"]);
        reject_unknown(table, &allowed, "plugins.cost")?;

        let prices = table
            .get("prices")
            .ok_or("plugins.cost.prices is required")?
            .as_table()
            .ok_or("plugins.cost.prices must be a table")?;
        if prices.is_empty() || prices.len() > MAX_PRICE_TABLES {
            return Err(format!(
                "plugins.cost.prices must have between 1 and {MAX_PRICE_TABLES} tables"
            ));
        }
        let mut currency = String::new();
        for (upstream, value) in prices {
            check_name(upstream, "upstream in prices")?;
            let table = value
                .as_table()
                .ok_or_else(|| format!("prices.{upstream} must be a table"))?;
            reject_unknown(
                table,
                &["currency", "models"],
                &format!("prices.{upstream}"),
            )?;
            let label = table
                .get("currency")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| format!("prices.{upstream}.currency must be a string"))?;
            if !(3..=16).contains(&label.len())
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
                || !label
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_uppercase())
            {
                return Err(format!(
                    "prices.{upstream}.currency must be a 3 to 16 character uppercase label"
                ));
            }
            if currency.is_empty() {
                currency = label.to_owned();
            } else if currency != label {
                return Err("all price tables must use the same currency label".into());
            }
            let models = table
                .get("models")
                .and_then(toml::Value::as_table)
                .ok_or_else(|| format!("prices.{upstream}.models must be a table"))?;
            if models.len() > MAX_MODELS_PER_TABLE {
                return Err(format!(
                    "prices.{upstream}.models has more than {MAX_MODELS_PER_TABLE} entries"
                ));
            }
            let mut price_table = PriceTable {
                currency: label.to_owned(),
                exact: BTreeMap::new(),
                globs: Vec::new(),
            };
            for (pattern, rule) in models {
                check_name(pattern, "model pattern")?;
                if pattern.len() > MAX_PATTERN_BYTES {
                    return Err("model pattern is too long".into());
                }
                let rule = rule
                    .as_table()
                    .ok_or("a model price must be an inline table")?;
                reject_unknown(
                    rule,
                    &[
                        "input_per_million",
                        "output_per_million",
                        "cached_input_per_million",
                    ],
                    "a model price",
                )?;
                let field = |name: &str| -> Result<u128, String> {
                    let text = rule
                        .get(name)
                        .and_then(toml::Value::as_str)
                        .ok_or_else(|| format!("{name} must be a decimal string"))?;
                    price_per_million(text, name)
                };
                let parsed = PriceRule {
                    pattern: pattern.clone(),
                    input: field("input_per_million")?,
                    output: field("output_per_million")?,
                    cached: rule
                        .get("cached_input_per_million")
                        .map(|_| field("cached_input_per_million"))
                        .transpose()?,
                    glob: pattern.contains(['*', '?']),
                };
                if parsed.glob {
                    price_table.globs.push(parsed);
                } else if price_table.exact.insert(pattern.clone(), parsed).is_some() {
                    return Err("a model pattern is defined twice".into());
                }
            }
            price_table.globs.sort_by(|a, b| {
                b.pattern
                    .len()
                    .cmp(&a.pattern.len())
                    .then_with(|| a.pattern.cmp(&b.pattern))
            });
            config.prices.insert(upstream.clone(), price_table);
        }

        if let Some(value) = table.get("budgets") {
            let budgets = value
                .as_table()
                .ok_or("plugins.cost.budgets must be a table")?;
            if budgets.len() > MAX_BUDGET_STREAMS {
                return Err(format!(
                    "plugins.cost.budgets has more than {MAX_BUDGET_STREAMS} streams"
                ));
            }
            for (stream, value) in budgets {
                check_name(stream, "stream in budgets")?;
                let table = value
                    .as_table()
                    .ok_or_else(|| format!("budgets.{stream} must be a table"))?;
                reject_unknown(
                    table,
                    &["budget_monthly", "budget_daily", "on_exceed"],
                    &format!("budgets.{stream}"),
                )?;
                let amount = |name: &str| -> Result<Option<u128>, String> {
                    table
                        .get(name)
                        .map(|value| {
                            let text = value
                                .as_str()
                                .ok_or_else(|| format!("{name} must be a decimal string"))?;
                            Ok(round_div(
                                price_per_million(text, name)?,
                                NANOS_PER_UNIT / 1_000_000,
                            ))
                        })
                        .transpose()
                };
                let monthly = amount("budget_monthly")?;
                let daily = amount("budget_daily")?;
                if monthly.is_none() && daily.is_none() {
                    return Err(format!(
                        "budgets.{stream} needs budget_monthly or budget_daily"
                    ));
                }
                let on_exceed = match table
                    .get("on_exceed")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("report")
                {
                    "report" => OnExceed::Report,
                    "block" => OnExceed::Block,
                    _ => return Err("on_exceed must be report or block".into()),
                };
                config.budgets.insert(
                    stream.clone(),
                    Budget {
                        monthly,
                        daily,
                        on_exceed,
                    },
                );
            }
        }

        if let Some(value) = table.get("model_label_allowlist") {
            let list = value
                .as_array()
                .ok_or("model_label_allowlist must be an array")?;
            if list.len() > MAX_MODEL_LABELS {
                return Err(format!(
                    "model_label_allowlist has more than {MAX_MODEL_LABELS} entries"
                ));
            }
            for entry in list {
                let model = entry
                    .as_str()
                    .ok_or("model_label_allowlist entries must be strings")?;
                check_name(model, "model label")?;
                config.model_labels.insert(model.to_owned());
            }
        }
        Ok(config)
    }

    pub fn currency(&self) -> &str {
        // parse() enforces one shared label across all tables.
        self.prices
            .values()
            .next()
            .map_or("UNKNOWN", |table| table.currency.as_str())
    }

    pub fn price<'a>(&'a self, upstream: &str, model: &str) -> Option<&'a PriceRule> {
        let table = self.prices.get(upstream)?;
        if let Some(rule) = table.exact.get(model) {
            return Some(rule);
        }
        table
            .globs
            .iter()
            .find(|rule| glob_matches(&rule.pattern, model))
    }
}

fn reject_unknown(table: &toml::Table, allowed: &[&str], where_: &str) -> Result<(), String> {
    if let Some(key) = table.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("{where_} has unknown key `{key}`"));
    }
    Ok(())
}

fn check_name(name: &str, what: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'*' | b'?')
        })
    {
        return Err(format!("invalid {what}"));
    }
    Ok(())
}

/// Bounded wildcard match: `*` spans any sequence, `?` matches one byte.
pub fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    if pattern.len() > MAX_PATTERN_BYTES || value.len() > MAX_NAME_BYTES {
        return false;
    }
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for &byte in pattern {
        let mut current = vec![false; value.len() + 1];
        match byte {
            b'*' => {
                current[0] = previous[0];
                for index in 1..=value.len() {
                    current[index] = previous[index] || current[index - 1];
                }
            }
            b'?' => {
                current[1..].copy_from_slice(&previous[..value.len()]);
            }
            literal => {
                for index in 1..=value.len() {
                    current[index] = previous[index - 1] && value[index - 1] == literal;
                }
            }
        }
        previous = current;
    }
    previous[value.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> toml::Table {
        toml::from_str(text).unwrap()
    }

    const BASE: &str = r##"
[prices.main]
currency = "USD"
[prices.main.models]
"exact-model" = { input_per_million = "2.50", output_per_million = "10" }
"batch-*" = { input_per_million = "1.25", output_per_million = "5", cached_input_per_million = "0.10" }
"batch-large-*" = { input_per_million = "0.50", output_per_million = "2" }
"##;

    #[test]
    fn exact_wins_then_longest_glob() {
        let config = Config::parse(&table(BASE)).unwrap();
        assert_eq!(
            config.price("main", "exact-model").unwrap().input,
            2_500_000_000
        );
        assert_eq!(
            config.price("main", "batch-small").unwrap().input,
            1_250_000_000
        );
        assert_eq!(
            config.price("main", "batch-large-x").unwrap().input,
            500_000_000
        );
        assert!(config.price("main", "other").is_none());
        assert!(config.price("missing", "exact-model").is_none());
    }

    #[test]
    fn unknown_keys_are_errors() {
        let bad = BASE.replace("currency =", "currency_bad =");
        let error = Config::parse(&table(&bad)).unwrap_err();
        assert!(error.contains("unknown key"), "{error}");
        let bad = BASE.replace("[prices.main.models]", "[prices.main.models]\n\"x\" = { input_per_million = \"1\", output_per_million = \"1\", extra = \"no\" }\n");
        let error = Config::parse(&table(&bad)).unwrap_err();
        assert!(error.contains("unknown key"), "{error}");
    }

    #[test]
    fn budgets_parse_and_default_to_report() {
        let text = format!(
            "{BASE}\n[budgets.team-a]\nbudget_monthly = \"100.50\"\nbudget_daily = \"5\"\n"
        );
        let config = Config::parse(&table(&text)).unwrap();
        let budget = &config.budgets["team-a"];
        assert_eq!(budget.monthly, Some(100_500_000));
        assert_eq!(budget.daily, Some(5_000_000));
        assert_eq!(budget.on_exceed, OnExceed::Report);
    }

    #[test]
    fn mixed_currencies_are_rejected() {
        let text = format!(
            "{BASE}\n[prices.other]\ncurrency = \"EUR\"\n[prices.other.models]\n\"m\" = {{ input_per_million = \"1\", output_per_million = \"1\" }}\n"
        );
        assert!(
            Config::parse(&table(&text))
                .unwrap_err()
                .contains("same currency")
        );
    }

    #[test]
    fn globs_are_bounded() {
        assert!(glob_matches("a*", "abc"));
        assert!(glob_matches("a?c", "abc"));
        assert!(!glob_matches("a?c", "ac"));
        assert!(!glob_matches(&"*".repeat(200), "x"));
    }
}
