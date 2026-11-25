# Configuration

Reiki reads one TOML file. `--config` sets the path; the default is `/etc/reiki/reiki.toml`. `reiki check` loads the file and reports the first problem it finds. See [cli](cli.md).

Conventions:

- **Required** means the key has no default and must be set when its table exists.
- Durations are whole seconds unless the name says otherwise. Sizes are bytes.
- Unknown keys are errors. Reiki tables reject them while parsing; each plugin rejects unknown keys in its own table, apart from the host keys `enabled`, `on_error`, `dry_run` and `streams`.

## Tables

| Table | Purpose | Required |
| --- | --- | --- |
| `listen` | Client listener address | no |
| `[admin]` | Admin listener and its tokens | no |
| `[stream_admin]` | Opt-in dynamic stream management on the admin listener | no |
| `[auth]` | Failed-token backoff on client, admin, peer and intercepted-tunnel listeners | no |
| `[server]` | Connection caps on client and admin listeners | no |
| `[service]` | Shutdown drain time | no |
| `[gateway]` | Defaults for stream admission and the self-service usage route | no |
| `[reload]` | Reload on file change | no |
| `[log]` | Reiki's service log | no |
| `[upstream.NAME]` | A provider endpoint and how requests reach it | no |
| `[keys.NAME]` | An upstream key file | no |
| `[streams.NAME]` | A client token and the key chain it may use | no |
| `[tunnel]` | CONNECT forward proxy | no |
| `[tunnel.intercept]` | TLS interception for tunnelled hosts | no |
| `[peers]` | Health sharing between instances | no |
| `[peers.NAME]` | A peer whose health `reiki peers` reads | no |
| `[plugins.NAME]` | A compiled-in plugin, or a runtime Wasm component with `plugin-wasm` | no |
| `[plugins.cost]` | Opt-in estimated spend from operator prices; needs `plugin-cost` | no |

There is no top-level `[limits]` table. Per-stream limits live in `[streams.NAME.limits]`; outbound key budgets live in `[upstream.NAME.limits]`. See [shaping](shaping.md).

## Listeners

Client, admin and tunnel addresses must differ. A publishing peer listener must also differ from those three.

| Listener | Setting | Default | Notes |
| --- | --- | --- | --- |
| Client | `listen` | `127.0.0.1:4097` | Serves the proxy API. |
| Admin | `[admin].listen` | none | Enabled only with `[admin]`. Loopback only and no TLS option; terminate TLS in front for remote access. Also serves `/v1/streams*` with `[stream_admin]`. |
| Tunnel | `[tunnel].listen` | none | Enabled only with `[tunnel]`. |
| Peers | `[peers].listen` | none | Used when `[peers].publish` is on. Loopback unless TLS is configured. |

## Top level

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | string | `"127.0.0.1:4097"` | Client listener address. `--listen` and `REIKI_LISTEN` override it. |
| `strict_key_permissions` | bool | `true` | Refuse a key file with group or other read or write bits; execute bits do not matter. `false` warns instead. |
| `failure_ring_size` | integer | `200` | Recent failures in memory across all streams; `0` disables the ring, maximum `10000`. A reload with a smaller cap drops the oldest records. See [monitoring](monitoring.md). |

## `[admin]`

Serves `GET /v1/status`, `GET /v1/failures`, `POST /v1/reload`, `POST /v1/keys/enable`, plugin routes such as `GET /v1/metrics`, and `GET /v1/openapi.json`. Without the table there is no admin listener. See [monitoring](monitoring.md).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | string | required | Literal loopback IP and port, such as `127.0.0.1:4098` or `[::1]:4098`. Hostnames and non-loopback IPs are errors; must differ from `listen`. No TLS option: bind on loopback and terminate TLS in front. |
| `token_sha256` | string | required | SHA-256 hex of the admin token, 64 characters; distinct from every stream token. Make it with `reiki hash-token` from at least 16 characters. |
| `read_token_sha256` | string | unset | SHA-256 hex of an optional read-only admin token. It opens read routes only; mutations answer `403`. Must differ from `token_sha256`, every stream token, the stream-management token and every peer token. Callers use `REIKI_ADMIN_READ_TOKEN` for `reiki top` or a bearer token otherwise. See [monitoring](monitoring.md). |

## `[stream_admin]`

Opt-in dynamic stream management on the admin listener. Without the table there are no management routes, no dynamic streams and no access to state or audit files. It needs `[admin]`, and `[admin].listen` must appear in `allowed_listen`. Management uses a dedicated token, not `[admin].token_sha256`. The table is startup-only: a reload reports it in `restart_needed`. See [keys](keys.md), [dynamic streams](dynamic-streams.md) and the [API examples](README.md).

Required keys:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `audit_log` | string | required | Append-only JSONL audit file: a relative name ending in `.jsonl` in the state directory (the directory holding `<config>.state`). A sibling `audit.head` stores the last sequence number and hash. Must differ from `state_file`. Management fails closed when a record cannot be written. |
| `max_streams` | integer | required | Hard cap on dynamic streams, 1 to 10000. |
| `ceilings` | table | required | Upper bounds for every dynamic stream; each present field must be at least 1. |
| `allowed_listen` | array of strings | required | Literal loopback `IP:port` values where these routes may be served. Must contain `[admin].listen` exactly. |
| `token_sha256` | string | required | SHA-256 hex of the dedicated management token, 64 characters; distinct from all other credential hashes. |
| `actor_id_sha256` | string | required | SHA-256 hex of the operator identity written to audit records. A hash, not a token; distinct from `token_sha256` and confirmation hashes. |

`[stream_admin.ceilings]` accepts these optional integer fields; an omitted field is not a ceiling.

| Field | Meaning |
| --- | --- |
| `rpm`, `tpm` | Requests and estimated tokens per 60-second window. |
| `concurrent` | Requests in flight at once. |
| `daily_requests`, `daily_tokens` | Requests and tokens per calendar day. |
| `monthly_requests`, `monthly_tokens` | Requests and tokens per calendar month. |

Optional keys:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `token_env` | string | `"REIKI_STREAM_ADMIN_TOKEN"` | Environment variable the CLI reads the raw token from; never a CLI argument. |
| `confirm_env` | string | `"REIKI_STREAM_ADMIN_CONFIRM_TOKEN"` | Environment variable with the second token when `require_two_party` is set. Must differ from `token_env`. |
| `allowed_keys` | table of tables | `{}` | Key labels from `[keys.*]` the API may assign. Each `[stream_admin.allowed_keys.NAME]` sets `assignable = true`; unknown keys or `false` are errors. Empty means creation is refused. The API never creates keys. |
| `state_file` | string | `"stream-admin.json"` | Relative name ending in `.json` in the state directory; written atomically with mode `0600`. |
| `max_tokens_per_stream` | integer | `3` | Maximum tokens per dynamic stream, including the first; 1 to 100. |
| `default_ttl` | integer | `3600` | Token lifetime in seconds when a request omits `ttl`; at least 1 and no greater than `max_ttl`. |
| `max_ttl` | integer | `86400` | Hard cap on token lifetime, up to 2592000 (30 days). |
| `require_two_party` | array of strings | `[]` | Extra SHA-256 token hashes whose bearer must send `X-Confirm-Token` on every mutation. Unique and distinct from the other hashes. |
| `max_body_bytes` | integer | `16384` | Largest management request body, 1024 to 1048576. |
| `lockout_threshold` | integer | `5` | Failed attempts from one source before lockout; at least 1. |
| `lockout_secs` | integer | `900` | Lockout length in seconds, 1 to 86400. |
| `rpm` | integer | `60` | Management requests per minute from one source; at least 1. |

```toml
[admin]
listen = "127.0.0.1:4098"
token_sha256 = "<64 hex admin token hash>"

[stream_admin]
token_sha256 = "<64 hex dedicated management token hash>"
actor_id_sha256 = "<64 hex operator id hash>"
audit_log = "stream-admin-audit.jsonl"
max_streams = 20
allowed_listen = ["127.0.0.1:4098"]

# zen-a must already exist as a [keys.zen-a] label
[stream_admin.allowed_keys.zen-a]
assignable = true

[stream_admin.ceilings]
rpm = 120
tpm = 100000
concurrent = 8
daily_requests = 10000
daily_tokens = 1000000
monthly_requests = 100000
monthly_tokens = 10000000
```

Both file names are relative names, not paths: separators, `..` or a wrong suffix are refused. State, audit and sidecar files live beside `<config>.state`. Copy the sidecar with the JSONL file when archiving it for verification. A reload or restart re-reads the state file; an invalid file drops dynamic streams while static streams continue. See [monitoring](monitoring.md).

## `[auth]`

Counts failed token checks per peer address on the client, admin, peer and intercepted-tunnel listeners. Intercepted `CONNECT` requests share the client listener's count. A locked-out address gets `429` with `Retry-After`. A reload applies changes without a restart.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `backoff` | bool | `true` | Refuse an address for `window_secs` after `max_failures` failures. |
| `max_failures` | integer | `10` | Failures allowed in `window_secs`, 1 to 10000. |
| `window_secs` | integer | `60` | How long a failure counts and how long a refused address waits, 1 to 86400. |

## `[server]`

Connection caps read at startup; a change on reload needs a restart. Loopback addresses are exempt from per-address caps.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `max_connections` | integer | `1024` | Client listener connections, 1 to 100000. |
| `max_connections_per_addr` | integer | `256` | Client connections from one address, up to `max_connections`. |
| `admin_max_connections` | integer | `64` | Admin listener connections, 1 to 10000. |
| `admin_max_connections_per_addr` | integer | `16` | Admin connections from one address, up to `admin_max_connections`. |

## `[service]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `shutdown_timeout_secs` | integer | `25` | Time for in-flight requests after SIGTERM or SIGINT, 1 to 300. Keep the unit's `TimeoutStopSec` above it. See [deploy](deploy.md). |

## `[gateway]`

Shared defaults for stream admission and the self-service usage route. See [shaping](shaping.md).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `on_store_error` | `"closed"` or `"open"` | `"closed"` | What to do when per-stream counter state cannot be read or written. `closed` refuses with a fixed `503`; `open` serves and counts `GatewayStoreError` events and `reiki_gateway_store_errors_total`. `[streams.NAME.limits]` overrides it for one stream. |
| `usage_rpm` | integer | `60` | Requests per minute to the client listener's `GET /v1/usage` route, per stream; 1 to 3600. |

## `[reload]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `watch` | bool | `false` | Reload when the config, a key file, the intercept CA, a peer CA or TLS file, or a plugin's input file changes. A change applies after files stay unchanged for one check. |
| `interval_secs` | integer | `5` | Seconds between checks, 1 to 3600. |

Reload also runs on SIGHUP and `reiki reload`. See [troubleshooting](troubleshooting.md).

## `[log]`

Reiki's service log. The request log is a plugin; see [request log](requestlog.md).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `level` | string | `"info"` | A level such as `info`, or a filter such as `info,reiki::tunnel=debug`. `RUST_LOG` keeps overriding it after reload. |
| `file` | string | unset | Also write lines to this absolute path. Enables rotation. |
| `format` | `"text"` or `"json"` | `"text"` | File line layout. |
| `stderr` | bool | `true` | Keep writing to stderr. Must stay `true` when `file` is unset. |
| `file_max_bytes` | integer | `52428800` (50 MiB) | Rotate when the next line would pass this size; `0` never rotates. |
| `file_keep` | integer | `5` | Rotated files kept, `<file>.1` (newest) to `<file>.N`; at most 100. |

## `[upstream.NAME]`

One provider endpoint. Keys and streams refer to it by `NAME`; see [providers](providers.md). Subsections cover the `fallback`, `forward` and `limits` tables.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `base_url` | string | required | Provider base URL. Must be `https://`, or `http://` on a loopback host. |
| `auth_header` | string | `"authorization"` | Header carrying the key; must be a valid header name. |
| `auth_format` | string | `"Bearer {key}"` | Header value template; `{key}` appears exactly once. |
| `model_aliases` | table of strings | `{}` | Replaces a matching top-level `model` value. Empty keys or values are refused. |
| `drop_params` | array of strings | `[]` | Top-level request fields removed before the request goes upstream. |
| `http2` | bool | `false` | Negotiate HTTP/2 when offered. Reverse-proxy routes only. |
| `forward_accept_encoding` | bool | `false` | Send the client's `Accept-Encoding` upstream instead of `identity`. Reverse-proxy routes only. |
| `max_decoded_bytes` | integer | `67108864` (64 MiB) | Largest decoded response body; a larger buffered body becomes a 502. At least 1. |
| `response_idle_timeout_secs` | integer | `300` | Longest gap between body chunks before the body is dropped. At least 1. |
| `response_timeout_secs` | integer | unset | Longest total body time after the head. Unset means 300 for a non-SSE body and no limit for an event stream; at least 1 when set. |
| `title_case_headers` | bool | `false` | Send Title-Case header names on HTTP/1.1. Not allowed with `forward`. |
| `send_keep_alive` | bool | `false` | Send `Connection: keep-alive` explicitly. Not allowed with `forward`. |
| `pass_errors` | bool | `false` | Relay every upstream error status unchanged: no key is cooled, disabled or retried. |
| `confirm_quota` | bool | `false` | Re-send a quota 429 once on the same key after 3 seconds before cooling it. |
| `request_error_phrases` | array of strings | `[]` | Extra 400/402/413 body phrases that pass through without changing the key. See [failover](failover.md). |
| `quota_phrases` | array of strings | `[]` | Extra body phrases that cool the whole key as quota exhaustion. |
| `rate_limit_phrases` | array of strings | `[]` | Extra body phrases that cool the route and model as a rate limit. |
| `scope_escalate_after` | integer | `3` | Cool the whole key when this many models cool and another failure arrives; 2 to 16. |
| `hold_unauthorized` | bool | `false` | Delay disabling the first 401 key until a later key serves the same request. |
| `passthrough` | bool | `false` | Plain reverse proxy: forward the client's credentials without a Reiki token or key. Cannot have `fallback`. |
| `capacity_group` | string | unset | Operator label published on peer health; 1 to 64 printable ASCII characters, not blank. Never inferred. |
| `account` | string | `"default"` | Operator label grouping observed passthrough answers; 1 to 64 printable ASCII characters, not blank. |
| `fallback` | table | unset | Where requests go when no key can serve. |
| `forward` | table | unset | Hand requests to a command or Unix socket. |
| `limits` | table | unset | Outbound shaping for this upstream's keys. |

Model and body rewrites run once, before a key is picked; failover replays the same bytes. Client-sent `X-Reiki-*` request headers are removed on every transport, including `passthrough` and `forward`. Reiki reserves that prefix for headers it adds to its own answers. See [failover](failover.md) for response rules and observed passthrough headers.

### `[upstream.NAME.fallback]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `upstream` | string | required | Another upstream that exists and is not this one. |
| `model_aliases` | table of strings | `{}` | Applied before the target's own aliases; `"*"` matches any model. |

Every stream whose chain uses this upstream must also hold a key for the target. See [failover](failover.md).

### `[upstream.NAME.forward]`

Hands each request to a child command or Unix socket speaking HTTP/1.1. Exactly one of `command` or `socket` must be set.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | string | unset | Program run once per request; token and outbound headers go to stdin, never argv. |
| `args` | array of strings | `[]` | Arguments for `command`. |
| `pass_env` | array of strings | `[]` | Environment variables passed to `command`. |
| `socket` | string | unset | Absolute Unix socket path, at most 103 bytes. |
| `expect_uid` | integer | unset | Required user id of the socket peer; only for `socket`. |
| `max_concurrent` | integer | `16` | Copies of `command` running at once; only for `command`. |
| `streaming` | bool | `false` | Relay the body as it arrives instead of buffering it. |
| `max_stream_bytes` | integer | `67108864` (64 MiB) | Largest streamed body relayed. At least 1. |
| `stream_timeout_secs` | integer | `1800` | Longest total streamed body time after the head. At least 1. |

See [providers](providers.md).

### `[upstream.NAME.limits]`

Each key of the upstream gets a budget. Unset fields do not limit anything; an explicit `0` means unlimited, even over a preset. A table must set `preset` or at least one limit. Presets are named examples, not provider detection. See [shaping](shaping.md).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `preset` | string | unset | `zen-free` or `openrouter-free`; see [limit presets](#limit-presets). |
| `rpm` | integer | from preset | Requests per minute. |
| `tpm` | integer | from preset | Estimated tokens per minute. |
| `concurrent` | integer | from preset | Requests in flight. |
| `daily_requests` | integer | from preset | Requests per UTC day. |
| `daily_tokens` | integer | unset | Tokens per UTC day. |
| `min_rpm` | integer | from preset, else `1` | Floor for the learned requests-per-minute cap. |
| `max_wait_secs` | integer | `20` | Longest wait for a slot before a 429, 1 to 120. |

## `[keys.NAME]`

One upstream credential in a file. The config holds only its path.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `upstream` | string | required | Upstream this key authenticates to; not a `passthrough` upstream. |
| `file` | string | required | Path to the key file. Read at startup and reload; contents are trimmed and must be 8 to 4096 bytes. |
| `quota_resets` | string | unset | When a quota cooldown ends: `daily HH:MM ZONE` or `monthly D HH:MM ZONE`, with `D` 1 to 28 and `ZONE` `UTC` or `+HH:MM` / `-HH:MM`. Without it a quota cooldown lasts 5 hours. |

A quota 429 with `Retry-After` cools the key for that long; a `quota_resets` schedule takes precedence over both. A schedule also publishes the absolute reset as `quota_reset_at` in peer health. See [failover](failover.md) and [keys](keys.md).

## `[streams.NAME]`

A stream is a client identity. Its token authenticates the client; its chain lists the keys it may use.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `token_sha256` | string | required | SHA-256 hex of the client token, 64 characters, unique across streams. Make it with `reiki hash-token`. |
| `chain` | array of strings | required | Non-empty key names from `[keys.*]`, tried in order. |
| `strategy` | `"sticky"` or `"round_robin"` | `"sticky"` | Keep the current key until it cannot serve, or start each request after the last key used. |
| `limits` | table | unset | Per-stream admission limits. |
| `allowed_upstreams` | array of strings | unset | Glob patterns for upstream names. Omitted means any; empty denies all. `*` and `?` are supported. |
| `allowed_models` | array of strings | unset | Glob patterns for declared model names. Omitted means any; empty denies all. A request with no determinable model is refused when set. |
| `weight` | integer | `1` | Relative share of a contested key's rate and concurrency budget; at least 1. |

The stream name `passthrough` is reserved for unauthenticated passthrough traffic. See [security](security.md).

### `[streams.NAME.limits]`

All fields are optional. An omitted limit is not enforced. Unset `period_zone` means `UTC`; a fixed offset such as `+02:00` is also accepted.

| Key | Type | Meaning |
| --- | --- | --- |
| `rpm` | integer | Requests per fixed 60-second window; at least 1. |
| `tpm` | integer | Tokens per fixed 60-second window, using the request-size estimate until measured usage arrives. |
| `concurrent` | integer | Requests in flight for this stream. |
| `daily_requests` | integer | Requests per calendar day in `period_zone`. |
| `daily_tokens` | integer | Tokens per calendar day in `period_zone`. |
| `monthly_requests` | integer | Requests per calendar month in `period_zone`. |
| `monthly_tokens` | integer | Tokens per calendar month in `period_zone`. |
| `period_zone` | string | `UTC` or a fixed `+HH:MM` / `-HH:MM` offset for daily and monthly windows. |
| `on_store_error` | `"closed"` or `"open"` | Overrides `[gateway] on_store_error` for this stream. |

A changed `token_sha256` resets that stream's counters on reload; an unchanged token keeps them. See [shaping](shaping.md).

## `[tunnel]`

A CONNECT-only forward proxy. It copies bytes without reading them unless `intercept` is set. See [tunnel](tunnel.md).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | string | required | Tunnel listener address; must differ from client and admin. |
| `allow` | array of strings | required | Destinations clients may open: `host`, `host:port`, `*.host` or `*`. Cannot be empty. |
| `connect_timeout_secs` | integer | `10` | Time allowed to open the upstream connection; at least 1. |
| `allow_loopback` | bool | `false` | Let the tunnel reach loopback addresses. For tests and local development. |
| `deny_private` | bool | `false` | Refuse private address ranges; local, link-local and metadata addresses are always refused. |
| `close_idle_on_stop` | bool | `true` | Close tunnels idle for at least one second when a stop begins. |
| `intercept` | table | unset | TLS interception. |

### `[tunnel.intercept]`

Reiki answers as the listed hosts with certificates signed by a name-constrained CA, then relays each request like any other.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `ca_cert` | string | required | PEM certificate of the signing CA. |
| `ca_key` | string | required | PEM private key of the signing CA. |
| `leaf_ttl_hours` | integer | `24` | Validity of each minted certificate, 1 to 720. |
| `require_auth` | bool | `true` | A CONNECT to a host served from a stream must carry that stream's token. |
| `ca_key_permissions` | `"error"` or `"warn"` | `"error"` | Response to a CA key readable by group or others. |
| `hosts` | array of tables | required | Non-empty list of intercepted hosts. |

Each entry is `[[tunnel.intercept.hosts]]`:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `host` | string | required | Lowercase DNS name covered by `tunnel.allow`, listed once. |
| `upstream` | string | required | Upstream receiving the requests; an `https` base URL must use the same host. |
| `stream` | string | unset | Stream whose chain serves the requests; required unless the upstream is `passthrough`. |
| `paths` | array of strings | `[]` | Path prefixes relayed as sent. Each starts with `/` and has no `?`. |

The CA must cover every intercepted host; `reiki check` verifies it.

## `[peers]`

Health sharing. Need the `peers` build feature, on by default. Without this table, or with `enabled = false`, nothing is published or read. Scalar keys go in `[peers]`; `[peers.NAME]` entries are peers to read; `[[peers.tokens]]` entries are read-only tokens other instances may present. See [provider health](provider-health.md).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Master switch. |
| `publish` | bool | `false` | Serve `GET /v1/health` on `listen` to token holders; needs `enabled`. |
| `listen` | string | unset | Peer listener address; required with `publish`, loopback unless TLS is set. |
| `peer_token_sha256` | string | unset | Shorthand for one token named `default`. |
| `tokens` | array of tables | `[]` | `[[peers.tokens]]` entries with `name` and `token_sha256`. Publishing needs at least one. |
| `allowed_urls` | array of strings | `[]` | Exact base URLs `[peers.NAME]` entries may use; empty keeps the usual rule: https, or http on loopback. |
| `instance` | string | `"reiki"` | Published instance name; letters, digits, `-`, `_`, `.`, at most 64 characters. |
| `poll_interval_secs` | integer | `30` | Reader poll period, 1 to 86400; data older than three periods is unknown. |
| `expose_key_labels` | bool | `false` | Publish per-key labels and states instead of only counts. |
| `expose_host_stats` | bool | `false` | Publish load, memory, disk use and active sessions. |
| `expose_usage` | bool | `true` | Publish per-upstream request and token counts without key or route names. |
| `session_header` | string | `"x-opencode-session"` | Header whose distinct values count as active sessions; lowercase, not a credential header. |
| `session_window_secs` | integer | `300` | How long a session stays active, 10 to 86400. |
| `stream_interval_secs` | integer | `5` | Period of `host` events on `/v1/health/stream`, 1 to 3600. |
| `max_subscribers` | integer | `16` | Open stream connections in total, 1 to 256. |
| `max_subscribers_per_peer` | integer | `4` | Open stream connections from one address. |
| `max_subscribers_per_token` | integer | `8` | Open stream connections for one peer token, 1 to 256. |
| `max_connections_per_addr` | integer | `4` | TCP connections from one address to the peer listener, 1 to 256. |
| `cooldown_bucket_secs` | integer | `300` | Published cooldowns round up to a multiple, or `0` for exact, 0 to 86400. |
| `tls_cert` | string | unset | PEM certificate chain for HTTPS on the peer listener; set with `tls_key`. |
| `tls_key` | string | unset | PEM private key for `tls_cert`. |
| `tls_key_permissions` | `"error"` or `"warn"` | `"error"` | Response to a `tls_key` readable by group or others. |

Peer token names use `[a-z0-9-]`, at most 32 characters. Every hash must be unique and differ from the admin, stream-management and stream tokens.

### `[[peers.tokens]]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | required | Lowercase letters, digits and `-`, at most 32 characters. |
| `token_sha256` | string | required | SHA-256 hex of the token, 64 characters. |

### `[peers.NAME]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `url` | string | required | Peer listener base URL: https, or http on loopback; no credentials, query or fragment. |
| `token_file` | string | required | File holding the token the peer issued to this instance. |
| `ca_file` | string | required for https | PEM CA certificates trusted for this peer. |

Peer names use letters, digits, `-`, `_` and `.`, at most 64 characters.

## `[plugins.NAME]`

Each table turns on a compiled-in plugin. With `plugin-wasm`, a table setting `wasm` and `sha256` loads a runtime component at the fixed D5 slot. Compiled-in order is set at build time. See [plugins](plugins.md) and [plugin authoring](plugin-authoring.md).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Set `false` to keep the table without running the plugin. |
| `on_error` | `"pass_through"` or `"reject"` | plugin-specific | On plugin failure: continue without it, or stop with 503. Masking plugins and detectors in `block` mode reject by default. |
| `dry_run` | bool | `false` where used | Validate settings without opening files, sockets or threads; `reiki check` adds `dry_run = true`. |
| `streams` | array of strings | all streams | Run only for these streams; each name is a configured stream or `passthrough`. |

| Plugin | Table | Page |
| --- | --- | --- |
| Placeholders, PHI and NPPI, finance, controlled information, tool policy | `[plugins.placeholders]`, `[plugins.phi]`, `[plugins.finance]`, `[plugins.controlled]`, `[plugins.toolpolicy]` | [plugins](plugins.md) |
| Request log | `[plugins.requestlog]` | [request log](requestlog.md) |
| Metrics | `[plugins.metrics]` | [monitoring](monitoring.md) |
| Cost | `[plugins.cost]` | [cost](cost.md) |

The request log records summaries by default (`body = "summary"`, `capture_bodies = false`). Body text needs `capture_bodies = true` together with `body = "full"` or `response = "body"`; the opt-in warns at startup and applies to every sink. A summary-only export can be the only sink at `[plugins.requestlog.export]`. See [request log](requestlog.md).

## Limit presets

`limits.preset` takes one of these example names. Explicit fields override the preset.

| Name | `rpm` | `tpm` | `concurrent` | `daily_requests` | `min_rpm` |
| --- | --- | --- | --- | --- | --- |
| `zen-free` | 10 | none | 2 | none | 1 |
| `openrouter-free` | 20 | none | none | 50 | 1 |

These are illustrative values shipped in the build, not authoritative service limits. No provider name, header or response selects a preset.

## Validation

`reiki check` runs the same load as the service: parse the file, read every key file, build each plugin in dry-run mode, check the intercept CA and check peer TLS when published. It prints one `ok:` line on success and exits `1` otherwise.

- **File.** Valid TOML; no unknown keys in a Reiki table. Syntax and unknown-key errors give a line and column and list accepted keys. Quoted text is hidden in these messages.
- **Logging.** `level` is a valid level or filter. `file` is absolute; `stderr = false` needs `file`; `file_keep` is at most 100.
- **Service, reload, auth.** Each numeric value is in the range above.
- **Server.** Each cap is in range; a per-address cap is at most its total.
- **Listeners.** Admin, tunnel and publishing-peer addresses are distinct; admin listens on a literal loopback address. A non-loopback peer listener needs TLS.
- **Upstream.** `base_url` is https, or http on loopback. `auth_header` and `auth_format` are valid. Alias and drop-param entries are non-empty. Timeouts and `max_decoded_bytes` are at least 1; `scope_escalate_after` is 2 to 16. Each phrase list holds at most 16 unique phrases of 3 to 120 non-control characters.
- **Upstream limits.** A preset or a limit is set; `max_wait_secs` is 1 to 120; the preset name is known.
- **Upstream forward.** Exactly one of `command` or `socket`; field combinations and socket directory permissions follow the table above; size and timeout values are at least 1. `http2`, `title_case_headers` and `send_keep_alive` are refused.
- **Upstream fallback.** Not on `passthrough`; the target exists, differs from the source and has valid aliases.
- **Keys.** The upstream exists and is not `passthrough`; `quota_resets` parses; each file is readable and holds 8 to 4096 bytes after trimming, with no group or other read or write bits unless `strict_key_permissions = false`.
- **Gateway.** `usage_rpm` is 1 to 3600; `on_store_error` is `closed` or `open`.
- **Streams.** Each chain entry names a known key; the chain is non-empty; `token_sha256` is 64 hex characters and unique; the name `passthrough` is reserved; `weight` is at least 1. Limits are positive, `period_zone` is `UTC` or a fixed offset, `on_store_error` is valid, and allowlists use only `*` and `?`. A stream using a fallback upstream holds a key for the target.
- **Admin.** `token_sha256` and `read_token_sha256` are 64 hex characters and distinct from each other and from all other credentials; `listen` is a literal loopback IP and port.
- **Stream admin.** Needs `[admin]`, `allowed_listen` containing that address, distinct 64-hex hashes, valid and distinct environment names, and relative file names with the required suffixes. Ranges: `max_streams` 1 to 10000, `max_tokens_per_stream` 1 to 100, `default_ttl` at least 1 and no greater than `max_ttl` (at most 2592000), `max_body_bytes` 1024 to 1048576, `lockout_secs` 1 to 86400, `lockout_threshold` and `rpm` at least 1. Each `allowed_keys` entry names a known key with `assignable = true`; each ceiling is positive.
- **Tunnel.** `allow` is non-empty and valid; `connect_timeout_secs` is at least 1. Intercepted hosts are unique, covered by `allow`, and match their upstream and stream.
- **Peers.** Each numeric setting is in range; `session_header` is lowercase and not a credential header; names, hashes and URLs are valid and unique; an `https` peer has `ca_file`; publishing needs `listen` and a token; `tls_cert` and `tls_key` are both set or both unset. Malformed URLs are reported without echoing them.
- **Plugins.** The plugin is compiled in, or is a Wasm component when `plugin-wasm` is enabled. Host fields and plugin settings are valid; each `streams` entry names a stream or `passthrough`. `requestlog` refuses body capture without `capture_bodies = true`; `cost` needs price tables, a shared currency, decimal prices and budgets, and `on_exceed`.
- **Build features.** `[peers]` needs `peers`. `[plugins.cost]` needs `plugin-cost`. A plugin table with `wasm` needs `plugin-wasm` and pins the component by `sha256`.

Startup also fails on an unreadable key file or a file shorter than 8 or longer than 4096 bytes after trimming. See [cli](cli.md) for the commands that read this file and [security](security.md) for its trust boundaries.
