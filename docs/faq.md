# FAQ

Short answers. The linked pages are the source of truth.

## Why do I get a 429 when every key looks fine?

`reiki keys` shows key health, not client limits. Four different gates answer `429`:

| Where | What it means | What it does to keys |
| --- | --- | --- |
| Gateway admission | A `[streams.NAME.limits]` budget or fair-share turn was reached; `error.type` starts with `gateway_`, with `Retry-After` at least 1 | None |
| Local shaping | An upstream `limits` budget held the request; `reiki_shaped` with `x-reiki-origin: local` | None |
| Login backoff | Too many wrong stream or admin tokens from one address | None |
| Upstream | A `rate_limited` or `quota_exhausted` ring row | Cools a scope or the key |

Run `reiki failures --limit 20` and `reiki keys`. A `rate_limited` ring row with a `60s` cooldown is an upstream answer; a `429` with no ring row is Reiki's own gate. With the default `[auth]` settings, ten wrong tokens from one address in a minute make the next attempt get `429`; `grep 'failed login'` confirms it. See [troubleshooting](troubleshooting.md#worked-example-quota-rate-limit-or-request-size) and [classification](classification.md#client-errors-from-reiki).

## Does my key leave the machine?

The upstream key leaves. Reiki reads it from a file that only Reiki's user can read, then sets the credential header on each upstream request. The key goes to the configured `base_url` over HTTPS (plain HTTP is allowed only on loopback), or to a configured forwarder socket or command. It is not written to the config, the client listener's answers, a request-log record or the state file. An upstream that echoes it back has the echo scrubbed; see the [redaction reference](redaction.md). Reiki's own `debug` log carries the first 160 characters of an upstream error body, which can include part of a rejected key, so keep it off in shared logs. The stream token is separate: the client sends it to Reiki and Reiki stores only its SHA-256 hash.

Two deliberate exceptions: a `passthrough` upstream forwards the client's own credentials unchanged, and an opaque tunnel copies the client's bytes without reading them. See [security](security.md) and [providers](providers.md#passing-the-clients-own-credentials-through).

## Why does OpenCode still call a provider directly?

Reiki only sees traffic pointed at it. The SDK must use Reiki as its base URL: set `provider.opencode.options.baseURL` to `http://127.0.0.1:4097/<upstream>` and `apiKey` to the stream token. An SDK left on the provider's own URL does not traverse Reiki at all.

For tunnel traffic, only hosts listed under `[tunnel.intercept]` are read. An allowed host that is not listed stays opaque and uses the client's own credentials. A `passthrough` upstream is another deliberate bypass of key injection. Zen's free models need OpenCode's own headers; Reiki forwards client headers except the credential, so that path keeps working. See [providers](providers.md#opencode-zen) and [tunnel](tunnel.md#trust-boundary).

## What happens when every key cools?

With no fallback, the client gets `503` `reiki_exhausted`. `Retry-After` is the shortest wait until a non-disabled key recovers, rounded up to at least one second; when every usable key is disabled there is no `Retry-After`. If a key refused the request for a reason that does not cool it, such as `access_disabled`, the client sees that upstream's last status and body instead. If every key was shaped out, the answer is `429` `reiki_shaped`. A configured fallback upstream is tried once before any `503`. See [failover](failover.md#client-responses-when-keys-run-out).

Cooldowns that outlive a restart are saved in `<config>.state`; route-and-model cooldowns live in memory. A key file whose digest changed starts healthy. See [troubleshooting](troubleshooting.md#key-health).

## How do I roll a key?

Write the new secret to a mode `0600` file, add its `[keys.NAME]` table, and put it in the chain in the same config edit. Then run `reiki check`, `reiki keys --check`, `reiki reload --dry-run`, and `reiki reload`, and confirm with `reiki keys`. Requests already in flight keep the old key in memory until they finish. Revoke the old key at the provider only after they drain; Reiki does not revoke it. A changed file digest makes the key start healthy. See [keys](keys.md#rotate-a-key-without-downtime) and [worked examples](examples.md#key-rotation).

For a key that is merely disabled, `reiki keys --enable NAME` clears its cooldown, disabled flag, failure count and scoped cooldowns without a file change. Reload re-enables a key only when its file changed or the key is new.

## What does a failover cost?

| Cost | Where it shows |
| --- | --- |
| One extra upstream attempt per skipped key | `attempts` in the request log; `reiki_failovers_total` with the metrics plugin |
| The request spends one gateway admission; each attempt spends an outbound budget | Stream and upstream limits |
| A cooldown shared by every stream that lists the key | `reiki keys`, `/v1/status`, `reiki top` |
| A 3-second wait and one repeated request when `confirm_quota = true` | First quota `429` only; it still enters the failure ring |

There is no replay after a `2xx` or `3xx` status line: a body that breaks mid-stream stays broken. Transport failures replay only when a side effect cannot be duplicated. An explicit upstream error response fails over for every method. See [failover](failover.md#transport-failures) and [classification](classification.md#reading-order).

## Why is one key disabled while its neighbours stay available?

A `401`, a `400` whose parsed JSON has a real `"reason": "API_KEY_INVALID"` property, or a `403` naming an invalid, expired or revoked credential disables the key. Other `403` answers are `access_disabled`: the key is skipped for that request only. One request disables at most one key; a second `401` in the same walk is tried but left alone. With `hold_unauthorized = true`, the disable waits until a later key serves the request; if none does, no key is disabled. A `429` cools but does not disable. See [classification](classification.md#response-rules) and [providers](providers.md#unauthorized-keys).

## Why did a 429 or 402 cool one model instead of the whole key?

A `rate_limited` failure and a 402 treated as quota exhaustion are scoped to `(route, model)` when the request names a model. A request with no model cools the whole key. A quota `429` is always key-wide because it concerns the account. When enough distinct models are cooling, `scope_escalate_after` (default 3) escalates the next combination failure to a whole-key cooldown. See [failover](failover.md#scoped-cooldowns).

## Does a reload apply every config change?

No. SIGHUP, `POST /v1/reload` and `reiki reload` re-read the config, key files and TLS material, build the whole new state, and swap it only if every part is valid. Keys, streams, upstreams, plugin settings and tunnel policy apply live. Listen addresses, `[log]` file settings and adding or removing `[tunnel]` need a restart and appear in `restart_needed`. A failed reload keeps the running state. See [troubleshooting](troubleshooting.md#reload) and [upgrades](upgrades.md#reload-vs-restart).

## Why did my POST stay on the first key after a timeout?

A POST or PATCH whose bytes may have reached the upstream is not replayed on another key: a retry could duplicate a side effect. Reiki returns `504` on timeout or `502` on connection failure. Idempotent methods (`GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`, `TRACE`) and a failure before any bytes were sent do move to the next key. An explicit upstream error status is not a transport failure and fails over for every method. See [failover](failover.md#transport-failures).
