# Traffic shaping

Shaping limits what Reiki sends. It keeps a stream within budgets and keeps one tenant from crowding another.

- The gateway admits a client request once, using `[streams.NAME.limits]`, `[streams.NAME].allowed_*` and `weight`. It runs before a key is chosen, so failover and fallback attempts do not spend another stream request.
- `[upstream.NAME.limits]` gives each key of that upstream its own budget: requests and tokens per minute, concurrency and daily totals. It counts every upstream attempt and can wait or move to the next key.

The two layers answer different `429`s. See [failover](failover.md) for status and `error.code` mapping and [configuration](configuration.md) for every field.

## Gateway admission

```toml
[gateway]
on_store_error = "closed"
usage_rpm = 60

[streams.client-a]
token_sha256 = "<sha256 of the stream token>"
chain = ["zen-a", "zen-b"]
allowed_upstreams = ["zen*", "openrouter"]
allowed_models = ["gpt-*", "claude-*"]
weight = 2

[streams.client-a.limits]
rpm = 60
tpm = 100000
concurrent = 4
daily_requests = 1000
daily_tokens = 500000
monthly_requests = 20000
monthly_tokens = 10000000
period_zone = "+02:00"
```

Every limit is optional; an omitted one is not enforced. `rpm` and `tpm` use fixed 60-second windows. Daily and monthly windows start at local midnight in `period_zone`, `UTC` by default or a fixed offset. A request is charged one request and an estimated `body bytes / 4` tokens when admitted; when the response body ends, measured provider usage replaces the estimate in every window.

`allowed_upstreams` and `allowed_models` match names and declared models with `*` and `?`. Omitting a list means unrestricted; an explicit empty list denies everything. With `allowed_models` set, a request whose model cannot be determined is refused. Unknown upstream names are refused before any key is picked.

`weight` is the stream's relative share of a key's contested rate and concurrency budget. Fairness engages only when two or more streams with recent traffic on the same key could use the same route and model; it compares admitted requests divided by weight. An idle stream holds no share, and a key used by one active stream is not throttled by fairness.

Refusals carry a fixed JSON `error.code` and message; no model name, token, request body or upstream key appears in either. Allowlist failures are `403`; limit failures are `429` with `Retry-After` as the whole seconds left in the window, at least 1. No key is cooled, no fallback is tried and the gateway adds no `x-reiki-origin` header. The full code list is in [client errors from Reiki](failover.md#client-errors-from-reiki).

`on_store_error = "closed"` is fail-closed. `"open"` serves without reliable counters and emits `GatewayStoreError` events and `reiki_gateway_store_errors_total{stream}`; `[streams.NAME.limits]` overrides the mode for one stream. A failed state write closes the store immediately. A store closed by an unreadable state file at startup is not probed, so a timer alone never rewrites it with empty counters. Otherwise the writer probes current counters after 5 seconds, doubling through 10 and 20 to a 30-second cap while writes fail; a successful probe reopens admission. The transition logs one `WARN` on close and one `INFO` on recovery. `reiki_gateway_store_ok` is `0` while closed and `1` while open.

Each stream's token can call `GET /v1/usage` on the client listener to see its own request and token totals. That route has its own budget, `[gateway] usage_rpm`, separate from request admission. See [monitoring](monitoring.md).

## Key limits

```toml
[upstream.zen.limits]
preset = "zen-free"
rpm = 20
tpm = 100000
concurrent = 2
daily_requests = 50
daily_tokens = 250000
min_rpm = 1
max_wait_secs = 20
```

An `[upstream.NAME.limits]` table turns shaping on for every key of that upstream. Unset fields do not limit anything; an explicit `0` means unlimited and overrides the preset. A table with no preset and no non-zero limit is refused; `min_rpm` alone is accepted. `preset` seeds the table from a named example, `zen-free` or `openrouter-free`; no provider name or response selects one. See [configuration](configuration.md) for the preset values and ranges.

Each key gets its own bucket. There are no monthly key limits and no window offset: daily windows start at UTC midnight.

## How a request gets a slot

For each key, in chain order:

1. The bucket checks a closed window, daily request and token counts, a pause from a 429, the concurrency limit, the rpm limit, then the tpm limit.
2. A granted slot sends the request and counts it at once.
3. If the bucket reports a wait, Reiki sleeps when the wait fits. A request sleeps at most twice per key, and total sleep stays within that key's `max_wait_secs`.
4. A longer wait or a closed window skips the key for this request. The key's health does not change.

Waits come from the oldest request in the last 60 seconds (rpm), earlier token counts expiring (tpm), a one-second retry (concurrency) or the remaining `Retry-After` (pause). A concurrency slot is held until the response body ends or the client disconnects; for a compressed non-SSE response Reiki reads the body first. A failed attempt frees its slot when Reiki moves on.

## Measured usage

Before sending, Reiki estimates tokens as the rewritten body bytes divided by four. On a success response it reads provider `usage` when the body is uncompressed: JSON bodies up to 1 MiB and event-stream lines up to 64 KiB. The count is input plus output tokens, or `total_tokens` when those are absent; later values override earlier ones field by field.

When the body ends, or the client disconnects after usage was seen, the measured count replaces the estimate in the tpm log and daily token count. The estimate stays when there is no usage, the body is compressed or over the cap, or the response is an error.

## Learning from responses

These effects apply only to the bucket of the key that sent the attempt.

| Response | Effect on the bucket |
| --- | --- |
| 429 rate limit, request with no `model` | Pause for `Retry-After` (capped at 24 hours) when given; cap rpm at 80% of the last minute's grants, never below `min_rpm`. |
| 429 rate limit, request with a `model` | No change: the failure cools only that route and model. |
| 429 quota, any request | Close the bucket until the quota cooldown ends. |
| 402, request with no `model` | Close the bucket until the quota cooldown ends. |
| 402, request with a `model` | No change: the failure cools only that route and model. |
| Any response with `remaining: 0` and a reset time | Close until that reset, capped at 24 hours. |
| Success at least a minute after the last change | Raise a learned cap by 5% of configured `rpm`, at least 1; clear it when it reaches the ceiling. Without `rpm`, the next quiet success clears the cap. |
| Transient, `access_disabled`, `unauthorized` | No change. |

Rate headers do not lower the learned cap; they only close the bucket when `remaining` is 0. The quota cooldown is the one that cools the key; see [failover](failover.md).

## Persisting across restarts

The state file sits beside the config as `<config>.state`, written atomically with mode `0600`. Its `shaper` table has an entry per saved key with `learned_rpm`, `pause_secs`, `closed_secs` and `daily` counts. Its `gateway` table has one entry per stream: the token hash already in the config, lifetime totals and current minute, day and month counters. Writes are coalesced by a background writer and flushed at shutdown.

Not saved: shaper minute log and in-flight count; gateway concurrency slots, fairness history and usage-route counters. Shaper state restores only when the key file and upstream are unchanged; gateway counters restore when the stream name and token hash are unchanged. Restored pauses are capped at 24 hours, closed times at 31 days, daily counts to the same UTC day and configured limits, and learned rpm between `min_rpm` and `rpm`. See [keys](keys.md) for state-file permissions.

## Reload

- An unchanged window keeps the key's bucket: counters, learned cap, pause and closed time. A new `max_wait_secs` applies at once.
- A changed window creates an empty bucket; the saved state is replaced at the next save.
- A key no longer shaped loses its bucket.
- A changed secret resets key health but not the bucket when the window is unchanged; the saved shaper state is not restored at the next start.
- Gateway counters survive while the stream and its `token_sha256` stay the same. A rotated token starts fresh; a removed stream's state is dropped. In-flight requests keep their slots until they finish.

## When every key is shaped out

If every key is skipped for shaping and no refusal was recorded:

| Part | Value |
| --- | --- |
| Status | `429` |
| JSON code | `reiki_shaped` |
| `Retry-After` | Shortest wait among skipped keys, rounded up, 1 to 3600 seconds |
| `x-reiki-origin` | `local`: the 429 came from Reiki, not the provider |

No key is cooled or disabled. The fallback is tried only if a refusal was recorded during the request. A chain with some keys cooling and the rest shaped out gets this `429`, not `503`. Shaper state does not appear in `/v1/status`, `reiki keys` or `reiki top`. The logs record `key is shaped out, trying the next key` and `every key is shaped out`.
