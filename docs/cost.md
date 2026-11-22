# Cost accounting

The optional `cost` plugin turns measured token usage into **estimates** from an operator-supplied price table. It never contacts a provider, never guesses a price and is not a provider invoice. Every answer it produces says the figures are estimates.

This plugin is off by default. Build Reiki with `--features plugin-cost` and enable `[plugins.cost]` to use it. The plugin only sees the final usage event Reiki already parses from a response body; if token accounting is off, there is nothing to price. Enabling the cost table turns response usage parsing on for this instance even when `[peers] expose_usage = false`; the figures stay local unless an admin caller asks for them.

## Configure

```toml
[plugins.cost]

# One price table per upstream. Names are the configured upstream names.
[plugins.cost.prices.zen]
currency = "USD"

[plugins.cost.prices.zen.models]
# Exact names win over patterns. Patterns use `*` and `?`; the longest match wins.
"gpt-5-large" = { input_per_million = "2.50", output_per_million = "10.00", cached_input_per_million = "1.25" }
"gpt-5-*"     = { input_per_million = "1.00", output_per_million = "4.00" }

[plugins.cost.prices.other]
currency = "USD"
[plugins.cost.prices.other.models]
"*" = { input_per_million = "0.25", output_per_million = "1.00" }

# Optional per-stream budgets. `on_exceed` defaults to "report".
[plugins.cost.budgets.client-a]
budget_monthly = "200.00"
budget_daily = "10.00"
on_exceed = "block"

[plugins.cost.budgets.client-b]
budget_monthly = "50.00"
# on_exceed = "report" is the default: spend is shown, requests keep running.

# Only these exact model names may appear as a Prometheus label.
model_label_allowlist = ["gpt-5-large"]
```

All price tables must use the same currency label. It is a 3 to 16 character uppercase label such as `USD` or `EUR`; the plugin adds currency arithmetic, not conversion. Prices and budgets are decimal **strings**, never TOML floats. A value such as `2.50` means two currency units and fifty hundredths per one million tokens.

`cached_input_per_million` is optional. When a response reports cached input tokens, those tokens use this price; the rest of the input uses `input_per_million`. When it is absent, cached tokens use the input price. Cached counts are capped at the reported input count.

A model is matched against its upstream's table: exact names first, then globs ordered longest first. An unmatched model is recorded as `unpriced` with zero estimated cost. Reiki never invents a price to fill a gap. An upstream with no price table is unpriced too.

Unknown keys are errors at every level, including inside a model rule and a budget. See [configuration](configuration.md#pluginsname) for the host keys every plugin accepts.

## What is recorded

Each final usage event adds to hour, day and month buckets in UTC. A bucket is keyed by stream, upstream, model, currency and period start. State lives in a sibling file of the Reiki config (`<config>.state.cost.state`) with mode 0600, written through a unique temporary file, synced and renamed. Hourly buckets are kept for 7 days, daily for 62 days and monthly for 25 months, with an overall cap of 4096 buckets.

Prices are parsed to nine decimal places (nanounits). The cost of a line is `round_half_up(price_nanounits × tokens / 1_000_000_000)` integer micro-units, where one micro-unit is one millionth of the currency unit. Input, cached input and output are rounded separately. This is deterministic: the same events and table always give the same estimate, and no floating point is involved.

If the state file cannot be read and a stream has a `block` budget, that stream is refused until the state is readable again. `report` budgets keep serving. The admin report marks `state_healthy` as false when this happens.

## Budgets

`budget_monthly` and `budget_daily` are amounts in the configured currency. At least one is required. `on_exceed` is `report` (default) or `block`:

- `report`: the route still serves; spend and unpriced counts appear in the reports.
- `block`: a new request on that stream is refused once the current UTC period's recorded spend reaches the amount. The refusal is a 429 with `kind` naming `budget_monthly` or `budget_daily` and a fixed public message. It goes through the same gateway admission path as rate and token limits, so no request counter is reserved and no separate refusal shape is introduced. Streams without a block budget are unaffected.

Budgets use recorded estimates, not provider invoices. A price change applies to new events; past buckets keep the amounts they were recorded with.

## Read the figures

### Admin `GET /v1/cost`

Requires the ordinary admin token. Returns every configured stream by default, for the current UTC month.

| Query | Meaning |
| --- | --- |
| `period` | `YYYY-MM` (default), `YYYY-MM-DD` or `YYYY-MM-DDTHH` in UTC. |
| `stream` | Only this configured stream. |
| `upstream` | Only this upstream. |
| `format` | `json` (default) or `csv`. |

```bash
curl -H "Authorization: Bearer $REIKI_ADMIN_TOKEN" \
  'http://127.0.0.1:4098/v1/cost?period=2026-11&stream=client-a&upstream=zen'
```

```json
{
  "currency": "USD",
  "period": "2026-11",
  "estimated": true,
  "note": "Estimates from the operator price table, not provider invoices.",
  "state_healthy": true,
  "totals": {
    "requests": 2,
    "estimated_micros": "12500000",
    "estimated_display": "12.500000",
    "unpriced_requests": 0
  },
  "rows": [
    {
      "period": "2026-11",
      "stream": "client-a",
      "upstream": "zen",
      "model": "gpt-5-large",
      "currency": "USD",
      "requests": 2,
      "input_tokens": 1000000,
      "output_tokens": 1000000,
      "estimated_micros": "12500000",
      "estimated_display": "12.500000",
      "unpriced_requests": 0
    }
  ]
}
```

The same route with `format=csv` returns a header and one row per stream/upstream/model, with `estimated_micros` as an integer string and `estimated_display` as a fixed-point decimal. `GET /v1/cost/metrics` serves the same Prometheus text that is appended to `/v1/metrics`.

### Self-service `GET /v1/usage`

The client listener's own-token route gains a `cost` object on the caller's stream only. It contains the current UTC month's estimate, unpriced request count, currency and the estimate note. No other stream's figures are returned. See [monitoring](monitoring.md#get-v1usage-client-listener).

### `reiki top`

When the cost plugin is enabled and the local admin listener is reachable, the fleet view and the detail pane show one bounded `cost (period currency, estimate)` line from the local instance only. Peer health documents never carry prices, spend or model names. See [top](top.md).

### Export

```bash
reiki cost export --period 2026-11 --format csv
reiki cost export --period 2026-11 --format json --token-file /etc/reiki/admin.token
```

The command calls the admin listener with the same token rules as `reiki failures`. Without `--url` it reads `[admin]` from the config. It prints the route's body unchanged, so the CSV and JSON formats have the same columns and fields as the API.

## Limits and privacy

- No prices, credentials, stream tokens, key values or request text appear in logs, errors, metrics labels, request-log records or peer health documents.
- The plugin makes no network calls. It only reads usage events and its state file.
- Prometheus series use bounded, sanitised `stream` and `upstream` labels (64 bytes) and at most 64 series in total. A `model` label is emitted only for exact names in `model_label_allowlist` (at most 32). Unlisted and unmatched models are reported only as a count. See [monitoring](monitoring.md#metrics).
- At most 64 price tables, 256 models per table, 128-byte names and patterns, and 512 budget streams. Invalid values are startup/config errors; values are never echoed in error messages.
- Amounts are estimates. Rounding, price changes, missing cached-token reports and unmatched models all mean the figure can differ from an invoice.
