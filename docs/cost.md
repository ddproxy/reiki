# Cost accounting

The optional `cost` plugin turns measured token usage into **estimates** from an operator price table. It never contacts a provider, never guesses a price and is not an invoice; every answer says the figures are estimates. Build with `--features plugin-cost` and enable `[plugins.cost]`. The plugin reads the final usage event Reiki already parses; enabling the table turns usage parsing on locally even when `[peers] expose_usage = false`.

## Configure

```toml
[plugins.cost]

[plugins.cost.prices.example]
currency = "USD"

[plugins.cost.prices.example.models]
# Exact names win over `*` and `?` patterns; the longest match wins.
"example-large" = { input_per_million = "2.50", output_per_million = "10.00", cached_input_per_million = "1.25" }
"example-*"     = { input_per_million = "1.00", output_per_million = "4.00" }

[plugins.cost.budgets.client-a]
budget_monthly = "200.00"
budget_daily = "10.00"
on_exceed = "block"

# Only these exact model names may appear as a Prometheus label.
model_label_allowlist = ["example-large"]
```

One price table per configured upstream. All tables use the same 3 to 16 character uppercase currency label, such as `USD`; prices and budgets are decimal **strings**, never TOML floats. `cached_input_per_million` is optional; without it cached tokens use the input price, and cached counts are capped at the reported input count. An unmatched model or an upstream with no table is `unpriced` at zero cost. Unknown keys are errors at every level. See [configuration](configuration.md#pluginsname) for the host keys every plugin accepts.

## What is recorded

Each final usage event adds to UTC hour, day and month buckets keyed by stream, upstream, model, currency and period start. State lives in `<config>.state.cost.state` with mode `0600`, written through a unique temporary file, synced and renamed. Hourly buckets are kept for 7 days, daily for 62 days and monthly for 25 months, capped at 4096 buckets.

Prices are parsed to nine decimal places. A line costs `round_half_up(price_nanounits × tokens / 1_000_000_000)` integer micro-units, with one micro-unit one millionth of the currency unit. Input, cached input and output are rounded separately, so the same events and table always give the same estimate. If the state file cannot be read, streams with a `block` budget are refused; `report` budgets keep serving, and the admin report marks `state_healthy` false.

## Budgets

`budget_monthly` and `budget_daily` are amounts in the configured currency; at least one is required. `on_exceed` is `report` (default) or `block`:

- `report` keeps serving and shows spend and unpriced counts.
- `block` refuses a new request on that stream once the current UTC period's recorded spend reaches the amount. The refusal is a 429 through the gateway admission path, with `kind` naming `budget_monthly` or `budget_daily`.

Budgets use recorded estimates, not provider invoices. A price change applies to new events; past buckets keep their recorded amounts.

## Read the figures

### Admin `GET /v1/cost`

Requires an admin token; the read-only token is enough. Returns every configured stream for the current UTC month by default.

| Query | Meaning |
| --- | --- |
| `period` | `YYYY-MM` (default), `YYYY-MM-DD` or `YYYY-MM-DDTHH` in UTC. |
| `stream` | Only this configured stream. |
| `upstream` | Only this upstream. |
| `format` | `json` (default) or `csv`. |

```bash
curl -H "Authorization: Bearer $REIKI_ADMIN_TOKEN" \
  'http://127.0.0.1:4098/v1/cost?period=2026-11&stream=client-a&upstream=example'
```

```json
{
  "currency": "USD",
  "period": "2026-11",
  "estimated": true,
  "note": "Estimates from the operator price table, not provider invoices.",
  "state_healthy": true,
  "totals": { "requests": 2, "estimated_micros": "12500000", "estimated_display": "12.500000", "unpriced_requests": 0 },
  "rows": [
    {
      "period": "2026-11", "stream": "client-a", "upstream": "example", "model": "example-large",
      "currency": "USD", "requests": 2, "input_tokens": 1000000, "output_tokens": 1000000,
      "estimated_micros": "12500000", "estimated_display": "12.500000", "unpriced_requests": 0
    }
  ]
}
```

`format=csv` returns a header and one row per stream/upstream/model. `GET /v1/cost/metrics` serves the same bounded Prometheus text appended to `/v1/metrics`.

### Self-service `GET /v1/usage`

The client listener's own-token route gains a `cost` object on the caller's stream only: the current UTC month's estimate, unpriced request count, currency and estimate note. No other stream's figures are returned. See [monitoring](monitoring.md#get-v1usage-client-listener).

### Reiki top

With the plugin enabled and the local admin listener reachable, the fleet view and detail pane show one bounded `cost (period currency, estimate)` line from the local instance only. Peer health documents never carry prices, spend or model names. See [top](top.md).

## Export

```bash
reiki cost export --period 2026-11 --format csv
reiki cost export --period 2026-11 --format json --token-file /etc/reiki/admin.token
```

The command calls the admin listener with the same token rules as `reiki failures`. Without `--url` it reads `[admin]` from the config. It prints the route's body unchanged, so CSV columns and JSON fields match the API.

## Limits and privacy

- No prices, credentials, stream tokens, key values or request text appear in logs, errors, metrics labels, request-log records or peer health documents.
- The plugin makes no network calls.
- Prometheus series use bounded, sanitised `stream` and `upstream` labels (64 bytes) and at most 64 series; a `model` label is emitted only for exact names in `model_label_allowlist` (at most 32). Others are counted only.
- At most 64 price tables, 256 models per table, 128-byte names and patterns, and 512 budget streams. Invalid values are startup/config errors and are never echoed.
- Rounding, price changes, missing cached-token reports and unmatched models can make an estimate differ from an invoice.
