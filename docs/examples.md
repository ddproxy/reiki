# Worked examples

These examples use local paths and illustrative names, ids and timestamps. Commands and flags follow the CLI definitions. Exact test output appears where a test asserts it; elsewhere the output shape comes from source and tests, and sample values are illustrative. Placeholder ids use a process-lifetime salt, so they change on restart.

## Find PHI before blocking

Scan stdin first:

```console
$ printf 'first line\nthe ssn is 123-45-6789 ok\n' | reiki scan
-:2:12 phi ssn
```

Exit status is `1`. Stderr contains `1 finding(s): phi ssn x1`. The matched value is not printed. A clean input exits `0` and prints no findings.

One JSON object per finding:

```console
$ printf 'ssn 123-45-6789\n' | reiki scan --json
{"file":"-","line":1,"col":5,"plugin":"phi","kind":"ssn"}
```

Then switch the plugin to block:

```toml
[plugins.phi]
mode = "block"
```

A request body with an SSN and a card is rejected before a key is used:

```http
HTTP/1.1 422 Unprocessable Entity
content-type: application/json

{"ok":false,"error":{"code":"blocked","message":"phi: request blocked, it contains ssn x1, card x1"}}
```

> **Limit:** The rejection names kinds and counts, never values. Only the request body is checked. In `block` mode an encoded or invalid UTF-8 body gets `415`; in `report` mode it is logged as unchecked. See [PHI behavior](phi.md#behavior).

## Placeholder round trip

Mask a configured term and credential-shaped strings before the request leaves Reiki:

```toml
[plugins.placeholders]
terms = ["Falcon"]      # builtin = true and rehydrate = true are defaults
```

| Stage | Body |
| --- | --- |
| Client sends | `{"q":"Is Falcon late? use <credential>"}` |
| Upstream sees | `{"q":"Is [[REIKI_TERM_<id>]] late? use [[REIKI_SECRET_<id>]]"}` |
| Client receives | the original text, byte for byte |
| Request log holds | the masked request and the masked response |

With a placeholder split across SSE deltas, the assembled client text is `The status of Falcon is green.`, and the stream ends with `data: [DONE]`.

> **Limit:** Ids are illustrative: each is 16 lowercase hex characters derived from the value, a process-lifetime salt and a counter. A private key becomes `[[REIKI_PRIVATE_KEY_REMOVED]]` and is not restored. Headers, paths and query strings are not masked; images, PDFs and thinking payloads are not scanned. See [what placeholders do](placeholders.md#what-it-does).

## Read a quota failure row

Ask the admin listener for recent failures on one key:

```console
$ reiki --config /etc/reiki/reiki.toml failures --limit 20 --key zen-b --json
{"failures":[{"time_at":"2025-11-18T11:24:11Z","stream":"client-a","key":"zen-b","status":429,"class":"quota_exhausted","retry_after":"18000","cooled":true,"cooldown_seconds":17999,"note":"quota exceeded for this key"}],"failures_total":1,"failures_truncated":false}
```

`time_at` is an RFC3339 UTC instant. `retry_after` is the received header, not a remaining duration; `cooldown_seconds` is the wait left at the snapshot. `note` is a printable body snippet of at most 200 characters with credentials scrubbed. The response is an envelope: `failures` is the bounded row array, with `failures_total` and `failures_truncated` beside it.

A quota `429` with `Retry-After: 90` cools the key for 60 to 90 seconds. The next request is refused locally with `503` and a `retry-after` header; the cooled key is not tried again. The ring holds `failure_ring_size` records in memory (200 by default) and is empty after a restart. See [GET /v1/failures](monitoring.md#get-v1failures).

> **Limit:** The row above is illustrative. Failure records are a live sample, not the request log; a record can be dropped when another thread holds the buffer.

## Diagnose tunnel denials

With an allowlist of `api.example.com` and an intercepted host that requires a stream token:

```toml
[tunnel]
listen = "127.0.0.1:4098"
allow = ["api.example.com"]

[tunnel.intercept]
ca_cert = "/etc/reiki/ca/ca.pem"
ca_key = "/etc/reiki/ca/ca.key"
require_auth = true

[[tunnel.intercept.hosts]]
host = "api.example.com"
upstream = "example"
stream = "client-a"
```

| Cause | Client sees |
| --- | --- |
| Destination not allowed | `HTTP/1.1 403 Forbidden`, body `destination not allowed` |
| Path outside the intercepted API | `403`, `{"ok":false,"error":{"code":"forbidden","message":"the path is outside the intercepted API"}}` |
| `Host` header differs from the tunnel destination | `421`, `{"ok":false,"error":{"code":"misdirected","message":"the Host header does not match the tunnel destination"}}` |
| Intercepted host without a valid stream token | `HTTP/1.1 407 Proxy Authentication Required`, `proxy-authenticate: Basic realm="reiki"`, body `a stream token is required` |
| Too many bad `Proxy-Authorization` attempts from one address | `HTTP/1.1 429 Too Many Requests`, `retry-after: <seconds>`, body `too many failed logins` |

The 407 response also carries `content-type: text/plain`, `content-length: 26` and `connection: close`. A valid bearer token, or a basic password equal to the stream token, gets `200 Connection Established` and clears that address's failed-login count.

> **Limit:** A plain CONNECT to an allowed host needs no token: the allowlist is the gate. Opaque tunnels have no request log, placeholders, key injection or failover. Only intercepted hosts get those controls. See [interception](tunnel.md#interception).

## Preview a reload

Change a plugin table, then preview before applying it:

```console
$ reiki --config /etc/reiki/reiki.toml reload --dry-run --token-file /etc/reiki/admin.token
{"dry_run":true,"generation":0,"keys":1,"streams":1,"changed":["plugins phi,requestlog"],"restart_needed":[]}
```

The test asserts `"dry_run":true`, `"generation":0` and `plugins phi,requestlog`; the key and stream counts here are illustrative. A preview applies nothing: `/v1/status` still reports `generation:0` and `last_reload:null`.

`changed` carries plugin, peer TLS and tunnel or intercept diff labels. Key, stream and upstream edits are applied but are not named there. `restart_needed` lists settings that the file changed but the running instance cannot apply. Exit status is `0` for success, `1` for a refused or failed reload and `2` when the admin listener cannot be reached. See [reload](troubleshooting.md#reload).

## Key rotation

Write the new secret, then add the new key and remove the old key from every chain in one config edit:

```bash
umask 077
printf '%s' "$NEW_PROVIDER_KEY" > /etc/reiki/keys/zen-b
chmod 0600 /etc/reiki/keys/zen-b
```

```toml
[keys.zen-b]
upstream = "zen"
file = "/etc/reiki/keys/zen-b"

[streams.client-a]
token_sha256 = "<unchanged 64 hex characters>"
chain = ["zen-b"]
strategy = "sticky"
```

```bash
reiki --config /etc/reiki/reiki.toml check
reiki --config /etc/reiki/reiki.toml keys --check
reiki --config /etc/reiki/reiki.toml reload --dry-run --token-file /etc/reiki/admin.token
reiki --config /etc/reiki/reiki.toml reload --token-file /etc/reiki/admin.token
reiki --config /etc/reiki/reiki.toml keys --token-file /etc/reiki/admin.token
```

A successful reload swaps the whole config. New requests use only the new chain; requests already in flight keep the old key in memory until they finish. A key whose file digest changed starts healthy, and the state file holds the digest, not the secret. Wait for in-flight requests to drain, then revoke the old key at the provider and remove its file. Reiki does not revoke provider keys. See [rotate a key without downtime](keys.md#rotate-a-key-without-downtime).
