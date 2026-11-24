# Traffic shaping

Shaping limits what Reiki sends. It keeps a stream within budgets that a provider or a free tier sets, and keeps one tenant from crowding another. There are two layers:

- The gateway admits a client request once, using `[streams.NAME.limits]`, `[streams.NAME].allowed_*` and `weight`. It runs before a key is chosen, so failover and fallback attempts do not spend another stream request.
- `[upstream.NAME.limits]` gives each key of that upstream its own budget: requests per minute, tokens per minute, concurrency and daily totals. It counts every upstream attempt and can wait or move to the next key.

The two layers answer different `429`s. See [response classification](classification.md#response-rules) for the status and `error.type` mapping.

## Gateway admission

```toml
[gateway]
on_store_error = "closed"   # or "open"
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

Every limit is optional; an omitted one is not enforced. `rpm` and `tpm` use fixed 60-second windows. Daily and monthly windows start at local midnight in `period_zone` (`UTC` by default, or a fixed offset). A request is charged one request and an estimated `body bytes / 4` tokens when admitted; when the response body ends, the provider's measured token count replaces the estimate in every window.

`allowed_upstreams` and `allowed_models` match names and declared models with `*` and `?`. Omitting a list means unrestricted; an explicit empty list denies everything. With `allowed_models` set, a request whose model cannot be determined is refused. Unknown upstream names are refused before any key is picked.

`weight` is the stream's relative share of a key's contested rate and concurrency budget. Fairness only engages when two or more streams with recent traffic on the same key could use the same route and model; it compares admitted requests divided by weight. An idle stream does not hold a share, and a key used by only one active stream is not throttled by fairness.

Refusals carry a fixed JSON `error.type` and message; no model name, token, request body or upstream key appears in either. Allowlist failures are `403`; limit failures are `429` with `Retry-After` as the whole seconds left in the window, at least 1. No key is cooled, no fallback is tried and no `x-reiki-origin` header is added by the gateway.

| `error.type` | Reason |
| --- | --- |
| `gateway_forbidden`, `gateway_upstream_forbidden`, `gateway_model_unknown`, `gateway_model_forbidden` | Unknown stream, upstream or model outside the allowlists, or no determinable model |
| `gateway_rate_limited`, `gateway_token_limited` | `rpm` or `tpm` reached |
| `gateway_daily_requests`, `gateway_daily_tokens`, `gateway_monthly_requests`, `gateway_monthly_tokens` | A calendar quota reached |
| `gateway_concurrent_limited` | Too many requests from the stream are in flight |
| `gateway_fairness_limited` | Another stream's turn on the shared key |
| `gateway_store_unavailable` | Counter state is unavailable in `closed` mode; fixed `503` |

`on_store_error = "closed"` (the default) is fail-closed. `"open"` serves requests without a reliable counter and emits `GatewayStoreError` events and `reiki_gateway_store_errors_total{stream}`; the mode can be overridden per stream in `[streams.NAME.limits]`.

A failed state write closes the store immediately. A store closed by an unreadable state file at startup is not probed, so a timer alone never rewrites the file with empty counters. The state writer then probes the current counters without waiting for new traffic: after 5 seconds, doubling through 10 and 20 to a 30-second cap while writes keep failing. A successful probe reopens admission. The transition logs one `WARN` on close and one `INFO` on recovery, not one line per refused request. `reiki_gateway_store_ok` is `0` while closed and `1` while open.

Each stream's token can call `GET /v1/usage` on the client listener to see only its own request and token totals. That route has its own budget, `[gateway] usage_rpm`, separate from request admission. See [monitoring](monitoring.md#get-v1usage-client-listener).

## Key limits

```toml
[upstream.zen.limits]
preset = "zen-free"        # or "openrouter-free": named examples, not provider detection
rpm = 20
tpm = 100000
concurrent = 2
daily_requests = 50
daily_tokens = 250000
min_rpm = 1
max_wait_secs = 20
```

Every field is optional. An explicit `0` means unlimited, and it overrides the preset's value for that field. A table with no preset and no non-zero `rpm`, `tpm`, `concurrent`, `daily_requests` or `daily_tokens` is refused at startup. A `min_rpm` alone is accepted.

| Setting | Default | Meaning |
| --- | --- | --- |
| `[upstream.NAME.limits]` | absent: no shaping | Turns shaping on for every key of the upstream. |
| `preset` | none | Starting values from an optional named example: `zen-free` or `openrouter-free`. An unknown name is refused; no provider name selects a preset. |
| `rpm` | `0`, unlimited | Requests per minute, per key. |
| `tpm` | `0`, unlimited | Tokens per minute, per key. Measured tokens replace the estimate; see below. |
| `concurrent` | `0`, unlimited | Requests in flight at once, per key. |
| `daily_requests` | `0`, unlimited | Requests per UTC day, per key. |
| `daily_tokens` | `0`, unlimited | Tokens per UTC day, per key. |
| `min_rpm` | `1`, or the preset's value | Floor for the rate Reiki learns from responses. |
| `max_wait_secs` | `20` | Longest a request may sleep for a slot on one key. Must be 1 to 120. |
| `[keys.NAME] quota_resets` | unset | Reset schedule for a quota failure. Also closes the key's budget after a quota 429. See [cooldown values](classification.md#cooldowns). |

Optional named examples seed the table. Explicit fields override them.

| Example | `rpm` | `tpm` | `concurrent` | `daily_requests` | `min_rpm` |
| --- | --- | --- | --- | --- | --- |
| `zen-free` | 10 | unlimited | 2 | unlimited | 1 |
| `openrouter-free` | 20 | unlimited | unlimited | 50 | 1 |

These values are illustrative starting points, not published or guaranteed limits. Reiki does not infer a preset from a provider name, request or response; the operator chooses one by name and can override every field.

Each key on the upstream gets its own bucket with these settings. Keys of other upstreams are not shaped.

The shaper has no monthly limits and no window offset. Every daily window starts at UTC midnight.

## How a request gets a slot

For each key, in chain order:

1. The bucket checks, in this order: a closed window, the daily request and token counts, a pause from a 429, the concurrency limit, the rpm limit, then the tpm limit.
2. If the bucket grants a slot, the request is sent. The request is counted at once.
3. If the bucket reports a wait, Reiki sleeps when the wait fits. A request sleeps at most twice per key. The total sleep for the request must stay within that key's `max_wait_secs`.
4. A longer wait, or a closed window, skips the key for this request. The key's health does not change.

Waits follow from each limit:

- rpm: until the oldest request in the last 60 seconds expires.
- tpm: until enough earlier token counts expire to fit this request.
- concurrency: retried after 1 second.
- pause: until the Retry-After from an earlier 429 has run out.

A concurrency slot is held until the response body ends or the client disconnects. For a compressed response that is not an event stream, Reiki reads the body first, so the slot frees after that read. A failed attempt frees its slot when Reiki moves on.

## Measured usage

Before sending, Reiki estimates a request's tokens as the body bytes divided by four. The body is the one sent upstream, after rewriting.

When a success response is read, Reiki reads its provider `usage`. It reads the body only when the body is not compressed:

- JSON bodies are read up to 1 MiB.
- Event streams are read one line at a time. Each line is read up to 64 KiB.

The count is input plus output tokens. If neither is reported, `total_tokens` is used. Later values override earlier ones field by field.

When the body ends, or the client disconnects after some usage was seen, the measured count replaces the estimate. It replaces the estimate in the tpm log and in the daily token count.

The estimate stays when there is no usage, when the body is compressed or over the cap, and for error responses.

## Learning from responses

These apply only to the key that sent the attempt. They use the bucket for that key.

| Response | Effect on the bucket |
| --- | --- |
| 429, rate limit, request with no `model` | Pauses for `Retry-After` (capped at 24 hours) when given. Caps rpm at 80 % of the last minute's grants, never below `min_rpm`. |
| 429, rate limit, request with a `model` | No change. The failure cools only that route and model. |
| 429, quota (any request) | Closes the bucket until the quota cooldown ends. |
| 402, request with no `model` | Closes the bucket until the quota cooldown ends. |
| 402, request with a `model` | No change. The failure cools only that route and model. |
| Any response with `remaining: 0` and a reset time in its rate headers | Closes the bucket until that reset, capped at 24 hours. |
| Success, at least a minute after the last change | Raises a learned cap by 5 % of the configured `rpm`, at least 1. When the cap reaches the ceiling, it is cleared. Without a configured `rpm`, the next success after a quiet minute clears the learned cap. |
| Transient, `access_disabled`, `unauthorized` | No change. |

The quota cooldown is the same one that cools the key. See [cooldown values](classification.md#cooldowns).

The limit is scoped to the route and model on purpose: a provider can rate-limit each model and route independently, so one model hitting its limit must not stop the key's traffic to the others.

Rate headers do not lower the learned cap. They only close the bucket when `remaining` is 0.

## Persisting across restarts

The state file sits next to the config file, as `<config>.state`, written atomically with mode `0600`. It has a `shaper` table with an entry for each key that has something to save:

- `learned_rpm`
- `pause_secs` and `closed_secs`, the time left when written, rounded up
- `daily`, with the window start, requests and tokens

It also has a `gateway` table with one entry per stream: the token hash already present in the config (never the token itself), lifetime request and token totals, and the current minute, day and month counters. Writes are coalesced through a background writer and flushed at shutdown.

Not saved: the shaper's minute log and in-flight count, and the gateway's concurrency slots, fairness history and usage-route counters.

Reiki restores shaper state at startup, and only when the key file and the upstream are unchanged. Gateway counters restore for streams whose name and token hash are unchanged. Restored shaper values are limited:

- A pause is capped at 24 hours. A closed time is capped at 31 days.
- Daily counts restore only into the same UTC day, and are capped at the configured limits.
- The learned rpm stays between `min_rpm` and the configured `rpm`.

## Reload

- If a key's window is unchanged, its bucket is kept. That covers counters, learned cap, pause and closed time. A new `max_wait_secs` applies at once.
- If the window changed, the bucket is new and starts empty. The saved state for that key is replaced at the next save.
- If the key is no longer shaped, its bucket is dropped.
- A changed key secret resets key health, but not the bucket, if the window is the same. A changed key's saved shaper state is not restored at the next start.
- Gateway counters survive a reload while a stream and its `token_sha256` stay the same. A rotated token starts with fresh counters; a removed stream's state is dropped. In-flight requests keep their concurrency slots until they finish.

## When every key is shaped out

If every key is skipped for shaping, and no refusal was recorded, the client gets:

| Part | Value |
| --- | --- |
| Status | `429` |
| JSON code | `reiki_shaped` |
| `Retry-After` | The shortest wait among the skipped keys, rounded up, between 1 and 3600 seconds |
| `x-reiki-origin` | `local`. The 429 came from Reiki, not the provider. |

No key is cooled or disabled. The fallback is not tried unless a refusal was recorded during the request.

A chain with some keys cooling and the rest shaped out gets this 429, not the `503`.

Shaper state does not appear in `GET /v1/status`, `reiki keys` or `reiki top`. The logs record `key is shaped out, trying the next key` and `every key is shaped out`.

## Not part of key shaping

`[accounts.LABEL.limits]` is parsed by the config loader. The shaper does not apply it, so it is not in the tables above.
