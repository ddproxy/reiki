# Response classification

This page is the single source for upstream response classes, key effects and cooldown values. [Failover](failover.md) explains how a stream walks its chain, and [providers](providers.md) shows the per-upstream switches that change classification.

## Reading order

1. `/__reiki/healthz` and every `2xx` or `3xx` response reach the client directly. Byte-level failures after the status line belong to the client stream; no key cools.
2. `pass_errors = true` on an upstream makes every status a pass-through: the response is relayed after secret scrubbing, and no key changes state.
3. A transport failure is a `transient` class against the selected key. Idempotent methods are replayed on the next key; a non-idempotent method ends with `504` on timeout or `502` on connection failure.
4. Error bodies are read up to 64 KiB, scrubbed of key and credential forms, and truncated with `x-reiki-truncated: 1` when longer. The body is lowercased with `to_lowercase()` before checks.
5. Request-size errors are checked first for `400`, `402` and `413` and short-circuit every status rule; then the rules below are evaluated in the order shown, first match wins. All body conditions use lowercase substrings, except the `400` invalid-key marker, which must be a `reason` string property in a body that parses as JSON. Text that only looks like a pair, including an echoed prompt or a non-JSON body, is ignored.
6. With `confirm_quota = true`, the first quota `429` from a key in a request waits 3 seconds and retries the same key. That first response is added to the failure ring but does not update key health or cool the key. Only a second quota `429` from that key updates key health and cools it. This switch applies to `429` only, never to `402`.
7. One request disables at most one key. With `hold_unauthorized = true`, a `401` is held as a refusal until a later key serves the request; a second `401` in the same walk stays a refusal.

## Response rules

| Upstream status | Body check | Class | Key effect | Next step for the request |
| --- | --- | --- | --- | --- |
| 200-399 | none | success | Transient count reset | Response is delivered |
| 401 | none | `unauthorized` | Disabled | Next key |
| 400 | `fewer max_tokens` or `can only afford` (JSON `error.message`/`message`, or whole body when not JSON) | `request_error` | None; the key is not cooled | Next key; the last upstream 400 is relayed unchanged if no key serves |
| 400 | Parsed JSON `reason` string property equal to `api_key_invalid` | `unauthorized` | Disabled | Next key |
| 400 | `credit balance is too low` | `quota_exhausted` | Cooling, whole key | Next key |
| 400 | anything else (after the request-size, credential, quota and configured rate-limit checks) | pass-through | None | Response is relayed |
| 402 | JSON `error.message` or `message` contains `fewer max_tokens` or `can only afford`; whole-body substring match only when the body is not JSON | `request_error` | None; the key is not cooled | Next key; the last upstream 402 is relayed unchanged if no key serves |
| 402 | anything else (after the request-size, quota and configured rate-limit checks) | `quota_exhausted` | Cooling for `(route, model)` | Next key |
| 429 | `usage exceeded`, `quota`, `free usage limit` or `insufficient` | `quota_exhausted` | Cooling, whole key | Next key |
| 429 | none of the above | `rate_limited` | Cooling for `(route, model)` | Next key |
| 403 | `error code: 10` (edge block page) | pass-through | None | Response is relayed |
| 403 | `access is disabled`, `free tier` or `not enabled` | `access_disabled` | None | Skip key for this request |
| 403 | names an invalid, expired or revoked key or token | `unauthorized` | Disabled | Next key |
| 403 | anything else | `access_disabled` | None | Skip key for this request |
| 5xx | `model access is disabled` or `access is disabled` | `access_disabled` | None | Skip key for this request |
| 5xx | `rate limit` | `rate_limited` | Cooling for `(route, model)` | Next key |
| 5xx | anything else | `transient` | Cooling after two in a row | Skip key for this request |
| 413 | `fewer max_tokens` or `can only afford` | `request_error` | None; the key is not cooled | Next key; the last upstream 413 is relayed unchanged if no key serves |
| other (404, 413 with no matching phrase, 422, ...) | none | pass-through | None | Response is relayed |
| No response (connect or timeout) | none | `transient` | Counted as a transient failure | See transport rules below |

`(route, model)` means that combination. For a request with no `model`, the same failure cools the whole key.

Each upstream can extend the built-in phrases; see [custom response phrases](#custom-response-phrases).

With `pass_errors = true` on the upstream, every status is a pass-through. No key changes state. See [providers](providers.md#relaying-errors-as-they-are).

With `hold_unauthorized = true`, a 401 does not disable the key until a later key serves the same request. See [providers](providers.md#unauthorized-keys).

A second key that refuses the same request with a 401 is not disabled. One request disables at most one key.

## Custom response phrases

Each upstream may add body phrases to the built-in lists:

```toml
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
request_error_phrases = ["fewer max_tokens", "your max_tokens is too high"]
quota_phrases = ["plan limit reached", "top up your balance", "free-models-per-day"]
rate_limit_phrases = ["slow down"]
```

- Each list holds at most 16 unique phrases of 3 to 120 non-control characters. A bad list is a config error naming the upstream and the phrase.
- Matching is case-insensitive, against a substring of the first 64 KiB of the body. Configured phrases extend, never replace, the built-ins above.
- `request_error_phrases` apply to 400, 402 and 413 only. The response is relayed and the key stays healthy. Built-ins: `fewer max_tokens`, `can only afford`.
- `quota_phrases` cool the whole key, like a quota 429. Built-ins: `credit balance is too low` on 400 and the four 429 phrases in the table above. A configured 402 quota phrase also cools the whole key; the built-in 402 request-size phrases still pass through. Vendor-specific phrases such as `free-models-per-day` belong here, not in the built-in rules.
- `rate_limit_phrases` cool the `(route, model)` scope. A configured match on `400`, `402` or `413` classifies the response as `rate_limited` when no request-size or quota phrase matched; built-in: `rate limit` on 5xx.
- When lists overlap in one response, the order is request error, then quota, then rate limit.

Changes to these lists take effect on reload.

## Cooldowns

These fixed durations are compiled in. `quota_resets` can override the quota window per key, and `scope_escalate_after` changes when scoped cooldowns become key-wide, but the 60 s, 5 h and 30 s values themselves are not configurable.

| Class | Cooldown | Scope |
| --- | --- | --- |
| `unauthorized` | Until enabled again. No timer. | Key |
| `rate_limited` | `Retry-After` when present (zero means no cooldown), else 60 seconds | `(route, model)` |
| `quota_exhausted` (402) | Same as the quota row below | `(route, model)` |
| `quota_exhausted` (429) | Same as the quota row below | Key |
| `access_disabled` | None. The key is skipped for the request only. | None |
| `request_error` | None. The key is skipped for the request only. | None |
| `transient` | 30 seconds, after two failures in a row with no success between | Key |

Quota cooldown takes the first of these that applies:

| Order | Source | Cooldown |
| --- | --- | --- |
| 1 | `quota_resets` on the key: `daily HH:MM zone` or `monthly D HH:MM zone` | Until the next reset |
| 2 | `Retry-After` on the response, non-zero | That long |
| 3 | Neither | 5 hours |

`Retry-After` is capped at 24 hours, whether it is seconds or an HTTP date. An HTTP date in the past gives zero. For a quota, zero counts as absent, so the next rule applies.

A quota 429 with `Retry-After` cools the key for that long, not for the 5-hour default. A configured `quota_resets` still wins over the header. With neither, the 5-hour window applies. The test `a_quota_429_with_retry_after_cools_the_key_for_that_long` in `tests/exhausted.rs` covers this.

When the wait comes from a `quota_resets` schedule or a non-zero `Retry-After`, the quota failure also carries the absolute reset instant. Peer health publishes it as `quota_reset_at` on the labelled key view; the fixed 5-hour window has no known reset and publishes nothing. A daily free-tier cap detected from the response body therefore gets a usable reset time when the key names its daily reset schedule.

A cooldown only moves later. A shorter failure does not shorten a longer cooldown. A later, longer failure extends it.

Invalid-credential phrases matched on `403`: `invalid api key`, `incorrect api key`, `api key is invalid`, `api key not valid`, `api key is not valid`, `api key has expired`, `api key has been revoked`, `api key is disabled`, `api_key_invalid`, `invalid_api_key`, `invalid x-api-key`, `invalid credential`, `credential has been revoked`, `credential has expired`, `invalid token`, `token has been revoked`, `token has expired`.

## Observed passthrough headers

A `passthrough` upstream holds no Reiki key, so its answer cannot cool or disable one. Reiki still classifies the status and a bounded error-body prefix, then records one observation under the upstream's `account` label. The class vocabulary is fixed in code and never taken from a response header or body: `ok`, `rate_limited`, `quota_exhausted`, `credential_rejected`, `access_disabled`, `request_error`, `error` and `pass_through`.

Response headers are read for observation only. The supported families are:

| Header | Meaning |
| --- | --- |
| `Retry-After` | Remaining wait, in seconds or an HTTP date; capped at 24 hours and counted down from the host clock. |
| `x-ratelimit-limit-requests`, `x-ratelimit-remaining-requests`, `x-ratelimit-reset-requests` | OpenAI-style request quota. Reset accepts seconds, `h`/`m`/`s`/`ms` components, a small epoch delta or an epoch value. |
| `x-ratelimit-limit-tokens`, `x-ratelimit-remaining-tokens`, `x-ratelimit-reset-tokens` | OpenAI-style token quota. |
| `x-ratelimit-limit`, `x-ratelimit-remaining`, `x-ratelimit-reset` | Generic request quota, used where the family above is absent. |
| `anthropic-ratelimit-requests-limit`, `anthropic-ratelimit-requests-remaining`, `anthropic-ratelimit-requests-reset` | Anthropic request quota; the reset is an RFC3339 instant, read relative to the response's `Date` when present. |
| `anthropic-ratelimit-tokens-limit`, `anthropic-ratelimit-tokens-remaining`, `anthropic-ratelimit-tokens-reset` | Anthropic token quota. |

Unparseable numbers and resets beyond 24 hours are ignored. The observed view appears in `GET /v1/status`, in the peer health document and as display-only columns in `reiki top`; it never holds a credential value. Client-supplied `X-Reiki-*` request headers are removed before the request is sent.

## Client errors from Reiki

| Status | `error.code` | Cause |
| --- | --- | --- |
| `400` | `bad_path` | Malformed request path |
| `401` | `unauthorized` | Missing or unknown stream token |
| `403` | `gateway_forbidden`, `gateway_upstream_forbidden`, `gateway_model_unknown`, `gateway_model_forbidden` | Stream, upstream or model outside its allowlist, or no determinable model |
| `404` | `not_found` | No upstream prefix, or unknown upstream |
| `408` | `body_timeout` | Request body timed out |
| `413` | `too_large` | Request body over 8 MiB |
| `429` | `gateway_rate_limited`, `gateway_token_limited`, `gateway_daily_*`, `gateway_monthly_*`, `gateway_concurrent_limited`, `gateway_fairness_limited` | A stream limit or fair-share turn; `Retry-After` at least 1 |
| `429` | `gateway_usage_limited` | The client listener's `/v1/usage` route exceeded `[gateway] usage_rpm` |
| `429` | `rate_limited` | Failed-login backoff after repeated wrong tokens from one address |
| `429` | `reiki_shaped` | Local key shaping held the request; `x-reiki-origin: local` |
| `502` | `bad_gateway` | Unreachable upstream, oversized buffered response, unreadable body, or failed forwarder link |
| `503` | `gateway_store_unavailable` | Counter state is unavailable and the stream is fail-closed |
| `503` | `reiki_exhausted` | Every key in the stream's chain is unavailable |
| `504` | `gateway_timeout` | Upstream headers or buffered body timed out |

After ten bad tokens from one address in a minute, the next request gets `429` with `Retry-After`. Gateway and local-shaping `429`s leave every key available. See [troubleshooting](troubleshooting.md) for first commands for each symptom.
