# Traffic shaping

Shaping limits what Reiki sends. It keeps a stream within budgets that a provider or a free tier sets. There are two layers:

- `rate_limit_per_minute` on a stream caps the upstream attempts that stream can make.
- `[upstream.NAME.limits]` gives each key of that upstream its own budget: requests per minute, tokens per minute, concurrency and daily totals.

Both count upstream attempts. Neither is a per-client limit. A request that does not fit a budget waits briefly or moves to the next key.

## Stream limit

```toml
[streams.client-a]
token_sha256 = "<sha256 of the stream token>"
chain = ["zen-a", "zen-b"]
rate_limit_per_minute = 60
```

- Each upstream attempt counts. That includes every key tried, every failover retry and every fallback attempt.
- Windows are fixed at 60 seconds. The first one starts at startup or reload. A new window starts with the first attempt after the previous one ends.
- A rejected attempt is not counted, and it does not reach an upstream. No key is cooled.
- Over the limit, Reiki answers `429` with JSON code `rate_limited` and the message `stream request rate limit exceeded`. `Retry-After` is the whole seconds left in the window, at least 1. The fallback upstream is not tried.
- This response has no `x-reiki-origin` header. Only the shaped-out 429 described below has one.
- A reload resets the counter.
- A value of `0` is refused at startup.

| Setting | Default | Meaning |
| --- | --- | --- |
| `[streams.NAME] rate_limit_per_minute` | unset: no limit | Upstream attempts per 60-second window for the stream. `0` is refused. |

## Key limits

```toml
[upstream.zen.limits]
preset = "zen-free"        # or "openrouter-free"
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
| `preset` | none | Starting values: `zen-free` or `openrouter-free`. An unknown name is refused. |
| `rpm` | `0`, unlimited | Requests per minute, per key. |
| `tpm` | `0`, unlimited | Tokens per minute, per key. Measured tokens replace the estimate; see below. |
| `concurrent` | `0`, unlimited | Requests in flight at once, per key. |
| `daily_requests` | `0`, unlimited | Requests per UTC day, per key. |
| `daily_tokens` | `0`, unlimited | Tokens per UTC day, per key. |
| `min_rpm` | `1`, or the preset's value | Floor for the rate Reiki learns from responses. |
| `max_wait_secs` | `20` | Longest a request may sleep for a slot on one key. Must be 1 to 120. |
| `[keys.NAME] quota_resets` | unset | Reset schedule for a quota failure. Also closes the key's budget after a quota 429. See [failover](failover.md#cooldowns). |

Presets seed the table. Explicit fields override them.

| Preset | `rpm` | `tpm` | `concurrent` | `daily_requests` | `min_rpm` |
| --- | --- | --- | --- | --- | --- |
| `zen-free` | 10 | unlimited | 2 | unlimited | 1 |
| `openrouter-free` | 20 | unlimited | unlimited | 50 | 1 |

The `zen-free` values are a conservative start, since no limits are published for it. The `openrouter-free` values come from the provider's documented free tier.

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

The quota cooldown is the same one that cools the key. See [failover](failover.md#cooldowns).

Rate headers do not lower the learned cap. They only close the bucket when `remaining` is 0.

## Persisting across restarts

The state file sits next to the config file, as `<config>.state`. It has a `shaper` table with an entry for each key that has something to save:

- `learned_rpm`
- `pause_secs` and `closed_secs`, the time left when written, rounded up
- `daily`, with the window start, requests and tokens

Writes happen after shaper events. Counter-only changes are saved at most once a minute.

Not saved: the minute log and the in-flight count.

Reiki restores this state at startup, and only when the key file and the upstream are unchanged. Restored values are limited:

- A pause is capped at 24 hours. A closed time is capped at 31 days.
- Daily counts restore only into the same UTC day, and are capped at the configured limits.
- The learned rpm stays between `min_rpm` and the configured `rpm`.

## Reload

- If a key's window is unchanged, its bucket is kept. That covers counters, learned cap, pause and closed time. A new `max_wait_secs` applies at once.
- If the window changed, the bucket is new and starts empty. The saved state for that key is replaced at the next save.
- If the key is no longer shaped, its bucket is dropped.
- A changed key secret resets key health, but not the bucket, if the window is the same. A changed key's saved shaper state is not restored at the next start.
- The stream's `rate_limit_per_minute` counter resets on every reload.

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

Shaper state does not appear in `GET /status`, `reiki keys` or `reiki top`. The logs record `key is shaped out, trying the next key` and `every key is shaped out`.

## Not part of key shaping

`[accounts.LABEL.limits]` is parsed by the config loader. The shaper does not apply it, so it is not in the tables above.
