# Configuration

Reiki reads one TOML file. `--config` sets the path; the default is `/etc/reiki/reiki.toml`. `reiki check` loads the file and reports the first problem it finds.

Conventions in this page:

- **Required** means the key has no default and must be set when its table exists.
- Durations are whole seconds unless the name says otherwise. Sizes are bytes.
- Unknown keys are errors everywhere. Reiki tables reject them while parsing, and each plugin rejects unknown keys in its own table, apart from the host keys `enabled`, `on_error`, `dry_run` and `streams`.

## Tables

| Table | Purpose | Required |
| --- | --- | --- |
| `listen` | Client listener address | no |
| `[admin]` | Admin listener and its token | no |
| `[stream_admin]` | Opt-in dynamic stream management on the admin listener | no |
| `[auth]` | Failed-token backoff on the client, admin, peer and intercepted-tunnel listeners | no |
| `[server]` | Connection caps on the client and admin listeners | no |
| `[service]` | Shutdown drain time | no |
| `[gateway]` | Defaults for per-stream admission and the self-service usage route | no |
| `[reload]` | Reload on file change | no |
| `[log]` | Reiki's own log output | no |
| `[upstream.NAME]` | A provider endpoint and how requests reach it | no |
| `[keys.NAME]` | An upstream key file | no |
| `[streams.NAME]` | A client token and the key chain it may use | no |
| `[tunnel]` | CONNECT forward proxy | no |
| `[tunnel.intercept]` | TLS interception for tunnelled hosts | no |
| `[peers]` | Health sharing between instances | no |
| `[peers.NAME]` | A peer whose health `reiki peers` reads | no |
| `[plugins.NAME]` | A compiled-in plugin | no |
| `[plugins.cost]` | Opt-in estimated spend from operator prices; needs `plugin-cost` | no |
| `[hubs.NAME]`, `[hub]`, `[accounts.LABEL]` | Hub configuration; see [hub](hub.md) | no |

There is no top-level `[limits]` table. Outbound limits are set per upstream in `[upstream.NAME.limits]` and per account in `[accounts.LABEL.limits]`.

## Default listeners and ports

Client, admin and tunnel addresses must differ from each other. A publishing peer listener must also differ from the client, admin and tunnel addresses. The hub listener must differ from the client, admin and tunnel addresses and must be on loopback.

| Listener | Setting | Default | Notes |
| --- | --- | --- | --- |
| Client | `listen` | `127.0.0.1:4097` | Serves the proxy API. |
| Admin | `[admin].listen` | none | Enabled only with an `[admin]` table. Loopback only; terminate TLS in front for remote access. Also serves `/v1/streams*` when `[stream_admin]` is configured. |
| Tunnel | `[tunnel].listen` | none | Enabled only with a `[tunnel]` table. |
| Peers | `[peers].listen` | none | Used when `[peers]` publishes health. Loopback unless TLS is configured. |
| Hub | `[hub].listen` | none | Parsed and validated; not served in this release. Loopback only. |

## Top level

| Key | Type | Default | Meaning | Since |
| --- | --- | --- | --- | --- |
| `listen` | string | `"127.0.0.1:4097"` | Client listener address. `--listen` and `REIKI_LISTEN` override it. | |
| `strict_key_permissions` | bool | `true` | Refuse to load a key file with group or other read or write bits; execute bits do not matter. Set to `false` to accept it with a warning instead. | |
| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | string | `"127.0.0.1:4097"` | Client listener address. `--listen` and `REIKI_LISTEN` override it. |
| `strict_key_permissions` | bool | `true` | Refuse to load a key file that is readable or writable by group or others. Set to `false` to accept it with a warning instead. |

## `[admin]`

The admin listener serves `GET /v1/status`, `GET /v1/failures`, `POST /v1/reload`, `POST /v1/keys/enable`, plugin routes such as `GET /v1/metrics`, and `GET /v1/openapi.json`. See [monitoring](monitoring.md). The table is optional; without it there is no admin listener. When `[stream_admin]` is configured, the same listener also serves the `/v1/streams*` management routes with a dedicated token, not the ordinary admin token.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | string | required | Admin listener address: a literal loopback IP and port, such as `127.0.0.1:9090` or `[::1]:9090`. Hostnames such as `localhost:9090` and non-loopback IPs are config errors. Must differ from `listen`. There is no admin TLS option; terminate TLS in front and bind on loopback for remote access. |
| `token_sha256` | string | required | SHA-256 hex of the admin token, 64 characters. Must differ from every stream token. Make it with `reiki hash-token`, from a token of at least 16 characters. |

## `[stream_admin]`

Opt-in management of dynamic streams on the admin listener. **Absence of the table means the feature is off**: no management routes, no dynamic streams, and no state or audit file access. The table needs an `[admin]` table, and the configured `[admin].listen` must appear in `allowed_listen`. Management requests use a dedicated token, not `[admin].token_sha256`; see [keys](keys.md#dynamic-stream-credentials) and the [API route list](README.md#stream-admin-api).

Required keys:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `audit_log` | string | required | Append-only JSONL audit file, a relative name ending in `.jsonl` in the state directory (the directory holding `<config>.state`). Reiki also keeps a small `audit.head` sidecar beside it with the last sequence number and hash. Must differ from `state_file`. Management fails closed when a record cannot be written. |
| `max_streams` | integer | required | Hard cap on dynamic streams. Range 1 to 10000. |
| `allowed_keys` | table of tables | required | Key labels from `[keys.*]` the API may assign. Each `[stream_admin.allowed_keys.NAME]` entry must set `assignable = true`; an unknown key or `false` is a config error. The API never creates keys. |
| `ceilings` | table | required | Upper bounds for every dynamic stream; see below. Each present field must be at least 1. |
| `allowed_listen` | array of strings | required | Literal loopback `IP:port` values where these routes may be served. Must contain the configured `[admin].listen` exactly; hostnames such as `localhost:4098` are refused. |
| `token_sha256` | string | required | SHA-256 hex of the dedicated stream-management token, 64 characters. Must differ from `[admin].token_sha256`, every stream token and every two-party confirmation hash. Make it with `reiki hash-token`. |
| `actor_id_sha256` | string | required | SHA-256 hex, 64 characters, of the operator identity written to audit records. It is a hash, not a token, and must differ from `token_sha256` and the confirmation hashes. |

`[stream_admin.ceilings]` accepts these optional integer fields. An omitted field is not a ceiling.

| Field | Meaning |
| --- | --- |
| `rpm`, `tpm` | Requests and estimated tokens per 60-second window. |
| `concurrent` | Requests in flight at once. |
| `daily_requests`, `daily_tokens` | Requests and tokens per calendar day. |
| `monthly_requests`, `monthly_tokens` | Requests and tokens per calendar month. |

Optional keys:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `token_env` | string | `"REIKI_STREAM_ADMIN_TOKEN"` | Environment variable the CLI reads the raw token from. The token is never a CLI argument. |
| `confirm_env` | string | `"REIKI_STREAM_ADMIN_CONFIRM_TOKEN"` | Environment variable holding the second token when `require_two_party` is set. Must differ from `token_env`. |
| `state_file` | string | `"stream-admin.json"` | Relative name ending in `.json` in the state directory. Written atomically with mode `0600`. |
| `max_tokens_per_stream` | integer | `3` | Maximum tokens per dynamic stream, including the first one. Range 1 to 100. |
| `default_ttl` | integer | `3600` | Token lifetime in seconds when a request omits `ttl`. At least 1 and no greater than `max_ttl`. |
| `max_ttl` | integer | `86400` | Hard cap on token lifetime in seconds. At most 2592000 (30 days). |
| `default_limits` | table | unset | Limits for new streams unless the request provides its own. Uses the same fields as `[streams.NAME.limits]`; each present numeric limit must stay within the matching `ceilings`. |
| `require_two_party` | array of strings | `[]` | Extra SHA-256 token hashes whose bearer must send `X-Confirm-Token` on every mutation. Hashes must be unique and differ from the admin, stream-management and actor hashes. |
| `max_body_bytes` | integer | `16384` | Largest management request body. Range 1024 to 1048576. |
| `lockout_threshold` | integer | `5` | Failed attempts from one source before a lockout. At least 1. |
| `lockout_secs` | integer | `900` | Lockout length in seconds. Range 1 to 86400. |
| `rpm` | integer | `60` | Management requests per minute from one source. At least 1. |

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

[stream_admin.default_limits]
rpm = 60
```

Both file names are relative names, not paths: a separator, `..`, or a wrong suffix is refused. State and audit files live beside `<config>.state`, along with the audit sidecar (`stream-admin-audit.head` in this example). Copy the sidecar with the JSONL file when archiving it for verification. A reload or restart re-reads the state file; if it is invalid, dynamic streams are dropped while static streams continue. The table itself is startup-only: adding, removing or changing `[stream_admin]` needs a restart and is reported in `restart_needed` on a reload preview. See [upgrades](upgrades.md#dynamic-stream-state) and [monitoring](monitoring.md#stream-administration-routes).

## `[auth]`

Counts failed token checks per peer address on the client, admin, peer and tunnel listeners. Intercepted `CONNECT` requests share the client listener's count; a `429` with `Retry-After` answers a locked-out address. A reload changes these values without a restart.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `backoff` | bool | `true` | Refuse an address for `window_secs` after `max_failures` failures. |
| `max_failures` | integer | `10` | Failures allowed in `window_secs`. Range 1 to 10000. |
| `window_secs` | integer | `60` | How long a failure counts, and how long a refused address waits. Range 1 to 86400. |

## `[server]`

Connection caps, read at startup. A changed value on reload is reported as needing a restart. Loopback addresses are exempt from the per-address caps.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `max_connections` | integer | `1024` | Open connections on the client listener. Range 1 to 100000. |
| `max_connections_per_addr` | integer | `256` | Open connections on the client listener from one remote address. Range 1 to `max_connections`. |
| `admin_max_connections` | integer | `64` | Open connections on the admin listener. Range 1 to 10000. |
| `admin_max_connections_per_addr` | integer | `16` | Open connections on the admin listener from one address. Range 1 to `admin_max_connections`. |

## `[service]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `shutdown_timeout_secs` | integer | `25` | After SIGTERM or SIGINT, time allowed for requests in flight before the listeners close. Range 1 to 300. Keep the unit's `TimeoutStopSec` above it. |

See [deploy](deploy.md).

## `[gateway]`

Shared defaults for the multi-user gateway. See [traffic shaping](shaping.md#gateway-admission) for the admission order, refusals and persistence.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `on_store_error` | `"closed"` or `"open"` | `"closed"` | What to do when per-stream counter state cannot be read or written. `closed` refuses with a fixed `503`; `open` serves and counts `GatewayStoreError` events and `reiki_gateway_store_errors_total`. A `[streams.NAME.limits]` setting overrides it for one stream. |
| `usage_rpm` | integer | `60` | Requests per minute allowed against the client listener's `GET /v1/usage` route, per stream. Range 1 to 3600. |

## `[reload]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `watch` | bool | `false` | Reload when the config file, a key file, an interception CA file, a `[peers.NAME] ca_file`, a peer TLS certificate or key (when the peer listener publishes), or a file a plugin reads changes. A change applies once the files stay unchanged for one more check. |
| `interval_secs` | integer | `5` | Seconds between checks. Range 1 to 3600. |

Reload also runs on SIGHUP and on `reiki reload`. See [upgrades](upgrades.md#reload-vs-restart) and [troubleshooting](troubleshooting.md#reload).

## `[log]`

Reiki's own service log. The request log is a plugin; see [requestlog](requestlog.md).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `level` | string | `"info"` | A level such as `info`, or a filter such as `info,reiki::tunnel=debug`. `RUST_LOG`, when set at startup, keeps overriding `level` after reload. |
| `file` | string | unset | Also write lines to this absolute path. The service user must be able to write it. Enables rotation. |
| `format` | `"text"` or `"json"` | `"text"` | Line layout for the file. |
| `stderr` | bool | `true` | Keep writing to stderr. Must stay `true` when `file` is unset. |
| `file_max_bytes` | integer | `52428800` (50 MiB) | Rotate when the next line would pass this size. `0` never rotates. |
| `file_keep` | integer | `5` | Rotated files kept, `<file>.1` (newest) to `<file>.N`. At most 100. |

## `[upstream.NAME]`

One provider endpoint. Keys and streams refer to it by `NAME`. Fields marked as tables are covered in the subsections below.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `base_url` | string | required | Base URL of the provider. Must be `https://`, or `http://` on a loopback host. |
| `auth_header` | string | `"authorization"` | Header that carries the key. Must be a valid header name. |
| `auth_format` | string | `"Bearer {key}"` | Header value template. `{key}` must appear exactly once. |
| `model_aliases` | table of strings | `{}` | Replaces the request's top-level `model` value when it matches a key. Empty keys or values are refused. |
| `drop_params` | array of strings | `[]` | Top-level request fields removed before the request goes upstream. |
| `http2` | bool | `false` | Negotiate HTTP/2 when the provider offers it. Reverse-proxy routes only. |
| `forward_accept_encoding` | bool | `false` | Send the client's `Accept-Encoding` upstream instead of `identity`. Reverse-proxy routes only. |
| `max_decoded_bytes` | integer | `67108864` (64 MiB) | Largest response body after decoding. A larger buffered body becomes a 502. At least 1. |
| `response_idle_timeout_secs` | integer | `300` | Longest gap between body chunks before the body is dropped. At least 1. |
| `response_timeout_secs` | integer | unset | Longest total time for the body after the head arrives. Unset means 300 for a body that is not an event stream and no total limit for an event stream. An explicit value also applies to event streams. At least 1. |
| `title_case_headers` | bool | `false` | Send header names in Title-Case on HTTP/1.1. Not allowed with `forward`. |
| `send_keep_alive` | bool | `false` | Send `Connection: keep-alive` explicitly. Not allowed with `forward`. |
| `pass_errors` | bool | `false` | Relay every upstream error status unchanged. No key is cooled, disabled or retried. Use it for upstreams that are not LLM providers. |
| `confirm_quota` | bool | `false` | Re-send a quota 429 once on the same key after 3 seconds before cooling the key. |
| `request_error_phrases` | array of strings | `[]` | Extra body phrases that make a 400, 402 or 413 pass through without changing the key. See [classification](classification.md#custom-response-phrases). |
| `quota_phrases` | array of strings | `[]` | Extra body phrases that cool the whole key as quota exhaustion. See [classification](classification.md#custom-response-phrases). |
| `rate_limit_phrases` | array of strings | `[]` | Extra body phrases that cool the route and model as a rate limit. See [classification](classification.md#custom-response-phrases). |
| `scope_escalate_after` | integer | `3` | Cool the whole key when this many models cool on it and another failure arrives. Range 2 to 16. |
| `hold_unauthorized` | bool | `false` | Delay disabling the first key that answers 401 until a later key serves the same request. |
| `passthrough` | bool | `false` | Plain reverse proxy. The client's own credentials go upstream unchanged. No Reiki token and no key from a chain. A passthrough upstream cannot have `fallback`. |
| `capacity_group` | string | unset | Opaque operator-set label published on this upstream's peer health entry. Never inferred from the upstream name, a key label or a response. 1 to 64 printable ASCII characters, not blank. |
| `account` | string | `"default"` | Operator label for observed passthrough answers from this upstream. It groups rate-limit and quota observations without reading a credential, and follows key rotation. 1 to 64 printable ASCII characters, not blank. |
| `fallback` | table | unset | Where requests go when no key of this upstream can serve them. See below. |
| `forward` | table | unset | Run a command or talk to a socket instead of Reiki's HTTP client. See below. |
| `limits` | table | unset | Outbound shaping for keys of this upstream. See below. |

Model and body rewrites run once, before a key is picked. Failover replays the same bytes. See [providers](providers.md).

Client-supplied `X-Reiki-*` request headers are removed before any upstream transport, including `passthrough` and `forward` routes. Reiki reserves that prefix for headers it adds to its own answers; the values are never logged. A `passthrough` answer is classified and its rate-limit headers are read into the `observed` view under `account`, without storing or publishing the client's credential. See [classification](classification.md#observed-passthrough-headers).

### `[upstream.NAME.fallback]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `upstream` | string | required | Another upstream to try. It must exist and must not be this upstream. A stream that uses this upstream must also hold a key for the target (see below). |
| `model_aliases` | table of strings | `{}` | Applied before the target's own `model_aliases`. `"*"` matches any model. |

Every stream whose chain uses this upstream must also hold a key for the fallback target. Otherwise the config is refused.

### `[upstream.NAME.forward]`

Hands each request to a child command or to a Unix socket that speaks HTTP/1.1. Exactly one of `command` or `socket` must be set.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | string | unset | Program run once per request. The API token and outbound headers go to its stdin, never argv. Not empty. |
| `args` | array of strings | `[]` | Arguments for `command`. |
| `pass_env` | array of strings | `[]` | Environment variables passed to `command`. |
| `socket` | string | unset | Absolute path of a Unix socket, at most 103 bytes. |
| `expect_uid` | integer | unset | User id that must own the socket's peer. Only for `socket`. Required when the socket's directory is writable by group or others and lacks the sticky bit. |
| `max_concurrent` | integer | `16` | Copies of `command` running at once. Further requests get a 502. Only for `command`. At least 1. |
| `streaming` | bool | `false` | Relay the body as it arrives instead of buffering it. |
| `max_stream_bytes` | integer | `67108864` (64 MiB) | Largest streamed body relayed by a `streaming` forwarder. A stream past the cap ends. At least 1. |
| `stream_timeout_secs` | integer | `1800` | Longest total time for a streamed forwarder body after its head. At least 1. |

See [providers](providers.md#forwarding).

### `[upstream.NAME.limits]`

Outbound shaping. Each key of the upstream gets a budget. A key out of budget is skipped. Fields that are not set do not limit anything. An explicit `0` means unlimited, even over a preset value. The table must set a `preset` or at least one limit. Presets are optional named examples, not provider detection: Reiki never chooses one from an upstream name or a response header.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `preset` | string | unset | Starting values from an optional named example: `zen-free` or `openrouter-free`; see [limit presets](#limit-presets). |
| `rpm` | integer | from preset | Requests per minute. |
| `tpm` | integer | from preset | Estimated tokens per minute. |
| `concurrent` | integer | from preset | Requests in flight at once. |
| `daily_requests` | integer | from preset | Requests per day. |
| `daily_tokens` | integer | unset | Tokens per day. |
| `min_rpm` | integer | from preset, else `1` | Floor for the learned requests-per-minute cap. |
| `max_wait_secs` | integer | `20` | Longest a request waits for a slot before Reiki sheds it with a 429. Range 1 to 120. |

See [providers](providers.md#outbound-limits).

## `[keys.NAME]`

One upstream key. The file holds the key; the config holds only its path.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `upstream` | string | required | Upstream this key authenticates to. That upstream must not be `passthrough`. |
| `file` | string | required | Path to the file that holds the key. Read at startup and on reload. The contents are trimmed and must be 8 to 4096 bytes. |
| `quota_resets` | string | unset | When a quota cooldown ends. `daily HH:MM ZONE` or `monthly D HH:MM ZONE`, where `D` is 1 to 28 and `ZONE` is `UTC` or `+HH:MM` / `-HH:MM`. Without it a quota cooldown lasts 5 hours. |

A quota 429 that carries `Retry-After` cools the key for that long. A `quota_resets` schedule takes precedence over both. A schedule also gives peer consumers the absolute reset as `quota_reset_at`; without a schedule or a non-zero `Retry-After`, no reset time is published. See [providers](providers.md).

## `[streams.NAME]`

A stream is a client identity. Its token authenticates the client, and its chain is the keys the client may use, in order.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `token_sha256` | string | required | SHA-256 hex of the client token, 64 characters. Unique across streams. Make it with `reiki hash-token`, from a token of at least 16 characters. |
| `chain` | array of strings | required | Key names from `[keys.*]`, tried in order. Must not be empty. |
| `strategy` | `"sticky"` or `"round_robin"` | `"sticky"` | `sticky` keeps the current key until it cannot serve, then moves to the next usable key. `round_robin` starts each request on the key after the one the last request used. |
| `limits` | table | unset | Per-stream admission limits. See below. |
| `allowed_upstreams` | array of strings | unset | Glob patterns for upstream names this stream may use. Omitted means any; an explicit empty list denies all. `*` and `?` are supported. |
| `allowed_models` | array of strings | unset | Glob patterns for declared model names this stream may use. Omitted means any; an explicit empty list denies all. A request with no determinable model is refused whenever this is set. |
| `weight` | integer | `1` | Relative share of a key's contested rate and concurrency budget when several streams use that key. At least 1. |

The stream name `passthrough` is reserved for unauthenticated passthrough traffic. See [security](security.md).

### `[streams.NAME.limits]`

All fields are optional; an omitted limit is not enforced. Unset `period_zone` means `UTC`. A fixed offset such as `+02:00` or `-05:30` is also accepted.

| Key | Type | Meaning |
| --- | --- | --- |
| `rpm` | integer | Requests admitted per fixed 60-second window. At least 1. |
| `tpm` | integer | Tokens charged per fixed 60-second window, using the request-size estimate until measured usage arrives. |
| `concurrent` | integer | Requests in flight for this stream at once. |
| `daily_requests` | integer | Requests per calendar day in `period_zone`. |
| `daily_tokens` | integer | Tokens per calendar day in `period_zone`. |
| `monthly_requests` | integer | Requests per calendar month in `period_zone`. |
| `monthly_tokens` | integer | Tokens per calendar month in `period_zone`. |
| `period_zone` | string | `UTC` or a fixed `+HH:MM` / `-HH:MM` offset for the daily and monthly windows. |
| `on_store_error` | `"closed"` or `"open"` | Overrides `[gateway] on_store_error` for this stream. |

A token rotation (a changed `token_sha256`) resets that stream's counters on reload; unchanged tokens keep their counters. See [traffic shaping](shaping.md#persisting-across-restarts).

## `[tunnel]`

A CONNECT-only forward proxy. It copies bytes without reading them, unless `intercept` is set.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | string | required | Tunnel listener address. Must differ from the client and admin listeners. |
| `allow` | array of strings | required | Destinations a client may open. Each entry is `host`, `host:port` (port 443 when omitted), `*.host` for any subdomain, or `*`. Must not be empty. |
| `connect_timeout_secs` | integer | `10` | Time allowed to open the upstream connection. At least 1. |
| `allow_loopback` | bool | `false` | Let the tunnel reach loopback addresses. For tests and local development only. |
| `deny_private` | bool | `false` | Refuse private addresses: RFC 1918, shared `100.64.0.0/10`, benchmarking `198.18.0.0/15`, `192.0.0.0/24` and IPv6 unique-local. Local, link-local and metadata addresses are refused regardless of this setting. Use it with a broad `allow` list. |
| `close_idle_on_stop` | bool | `true` | Close tunnels idle for at least one second as soon as a stop begins; active tunnels get the drain and are then cut. |
| `intercept` | table | unset | TLS interception. See below. |

See [tunnel](tunnel.md).

### `[tunnel.intercept]`

Reiki answers as the listed hosts, with certificates signed by a CA that clients trust. It then relays each request like any other.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `ca_cert` | string | required | PEM certificate of the signing CA. |
| `ca_key` | string | required | PEM private key of the signing CA. |
| `leaf_ttl_hours` | integer | `24` | Validity of each minted certificate. Range 1 to 720. |
| `require_auth` | bool | `true` | A CONNECT to a host served from a stream must carry that stream's token. Turn it off only when only trusted clients can reach the tunnel. |
| `ca_key_permissions` | `"error"` or `"warn"` | `"error"` | What to do when the CA key is readable by its group or by others. `error` refuses to load it; `warn` loads it and logs a warning. |
| `hosts` | array of tables | required | Hosts to intercept. Must not be empty. See below. |

Each entry of `hosts` is written as `[[tunnel.intercept.hosts]]`:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `host` | string | required | Lowercase DNS name. Must be covered by `tunnel.allow`. Listed once. |
| `upstream` | string | required | Upstream that receives the requests. If its `base_url` is `https`, its host must equal `host`. |
| `stream` | string | unset | Stream whose key chain serves the requests. Required unless the upstream is `passthrough`; refused when it is. The stream's chain must hold a key for the upstream. |
| `paths` | array of strings | `[]` | Path prefixes relayed as the client sent them, to the upstream's host. Each starts with `/` and has no `?`. Empty keeps only the upstream's base path. |

The CA must cover every intercepted host. Check it with `reiki check`. See [tunnel](tunnel.md#interception).

## `[peers]`

Health sharing between Reiki instances. The table is optional. Without it, or with `enabled = false`, nothing is published or read. Needs the `peers` build feature, which is on by default.

Scalar keys go in `[peers]`. Each `[peers.NAME]` sub-table is a peer to read. `[[peers.tokens]]` entries are read-only tokens that other instances may present.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Master switch. |
| `publish` | bool | `false` | Serve `GET /v1/health` on `listen` to holders of a peer token. Needs `enabled`. |
| `listen` | string | unset | Address of the peer listener. Required when `publish` is on. Must differ from the client, admin and tunnel listeners. Must be a loopback address unless `tls_cert` and `tls_key` are set. |
| `peer_token_sha256` | string | unset | Shorthand for one token named `default`. |
| `tokens` | array of tables | `[]` | `[[peers.tokens]]` entries: `name` and `token_sha256`. Publishing needs at least one token. |
| `allowed_urls` | array of strings | `[]` | Exact base URLs that `[peers.NAME]` entries may use. A trailing `/` is ignored. Empty keeps the usual rule: https, or http on loopback. A URL that points at this instance's own client, admin, tunnel or peer listener is refused. |
| `instance` | string | `"reiki"` | Name published for this instance. Letters, digits, `-`, `_` and `.`, at most 64 characters. |
| `poll_interval_secs` | integer | `30` | Period of the reader's polls. Data older than three periods counts as unknown. Range 1 to 86400. |
| `expose_key_labels` | bool | `false` | Publish per-key labels and states, not only counts per upstream. |
| `expose_host_stats` | bool | `false` | Publish load, memory, disk use and active sessions. |
| `expose_usage` | bool | `true` | Publish per-upstream request and token counts for the last hour, without key or route names. |
| `session_header` | string | `"x-opencode-session"` | Client header whose distinct values count as active sessions. Lowercase. Credential headers such as `authorization` are refused. |
| `session_window_secs` | integer | `300` | How long a session stays active after its last request. Range 10 to 86400. |
| `stream_interval_secs` | integer | `5` | Period of `host` events on `/v1/health/stream`. Range 1 to 3600. |
| `max_subscribers` | integer | `16` | Open stream connections allowed in total. Range 1 to 256. |
| `max_subscribers_per_peer` | integer | `4` | Open stream connections from one client address. Range 1 to `max_subscribers`. |
| `max_subscribers_per_token` | integer | `8` | Open stream connections for one peer token. Range 1 to 256. |
| `max_connections_per_addr` | integer | `4` | Open TCP connections from one address to the peer listener. Range 1 to 256. |
| `cooldown_bucket_secs` | integer | `300` | Published cooldowns round up to a multiple of this. `0` publishes them exactly. Range 0 to 86400. |
| `tls_cert` | string | unset | PEM certificate chain for HTTPS on the peer listener. Set together with `tls_key`. |
| `tls_key` | string | unset | PEM private key for `tls_cert`. Keep it private to its owner. |
| `tls_key_permissions` | `"error"` or `"warn"` | `"error"` | What to do when `tls_key` is readable by its group or by others. |

Peer tokens are named `[a-z0-9-]`, at most 32 characters. Each token's hash must differ from every other token, the admin token and every stream token.

### `[[peers.tokens]]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | required | Lowercase letters, digits and `-`, at most 32 characters. May be logged. |
| `token_sha256` | string | required | SHA-256 hex of the token, 64 characters. |

### `[peers.NAME]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `url` | string | required | Base URL of the peer's listener. `https://`, or `http://` on a loopback host. No credentials, query or fragment. |
| `token_file` | string | required | File that holds the token the peer issued to this instance. |
| `ca_file` | string | required for `https` | PEM CA certificates trusted for this peer only. |

Peer names use letters, digits, `-`, `_` and `.`, at most 64 characters. See [peers](peers.md).

## `[plugins.NAME]`

Each `[plugins.NAME]` table turns on a plugin that is compiled into the binary. A name that is not compiled in is an error at startup. Plugin order is set at build time. See [plugins](plugins.md).

Keys that every plugin accepts:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Set `false` to keep the table and not run the plugin. |
| `on_error` | `"pass_through"` or `"reject"` | plugin-specific | What happens when the plugin fails, including a panic in `on_request`, `new_state` or a response hook. `pass_through` continues without it (and logs a warning when the plugin would otherwise fail closed). `reject` stops the request with a 503. Plugins that mask data reject by default, and so do detectors in `block` mode. |
| `dry_run` | bool | `false` where used | Accepted by every plugin. Plugins that use it validate settings without opening files, sockets or threads; `reiki check` adds `dry_run = true` to every plugin table. |
| `streams` | array of strings | all streams | Run only for requests on these streams. Each name is a configured stream or `passthrough`. |

Plugin-specific keys are documented on each plugin's page:

| Plugin | Table | Page |
| --- | --- | --- |
| Placeholders | `[plugins.placeholders]` | [placeholders](placeholders.md) |
| Request log | `[plugins.requestlog]` | [requestlog](requestlog.md) |
| PHI and NPPI | `[plugins.phi]` | [phi](phi.md) |
| Finance | `[plugins.finance]` | [finance](finance.md) |
| Controlled information | `[plugins.controlled]` | [controlled](controlled.md) |
| Metrics | `[plugins.metrics]` | [monitoring](monitoring.md) |
| Cost | `[plugins.cost]` | [cost](cost.md) |

The `requestlog` plugin records summaries only by default: `body = "summary"` and `capture_bodies = false`. Request, response and error body text requires the explicit `capture_bodies = true` opt-in together with `body = "full"` or `response = "body"`; the opt-in warns at startup and applies to every configured sink. A separate summary-only export for a log collector is configured at `[plugins.requestlog.export]` and can be the plugin's only sink. See [requestlog](requestlog.md#configure) and [summary export](requestlog.md#summary-export).

## Limit presets

`limits.preset` and `accounts.LABEL.limits.preset` take one of these optional example names. Explicit fields override the preset's value.

| Name | `rpm` | `tpm` | `concurrent` | `daily_requests` | `min_rpm` |
| --- | --- | --- | --- | --- | --- |
| `zen-free` | 10 | none | 2 | none | 1 |
| `openrouter-free` | 20 | none | none | 50 | 1 |

These are illustrative starting values shipped in the build, not authoritative limits for any service; check the service's current terms and override any field that matters. The names are opaque configuration keys: no provider name, response body or header selects a preset.

## Hubs, hub and accounts

These tables are configuration only. They exist behind the `hub` build feature, which is not in the default set.

- A binary built without `hub` refuses any config that has these tables.
- A binary built with `hub` validates them. It opens no listener and contacts no hub.

The rules and the reasoning are in [hub](hub.md).

### `[hubs.NAME]`

A remote hub this instance reaches as an edge.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `url` | string | required | Hub base URL. `https://`, or `http://` on a loopback host. |
| `token_file` | string | required | File that holds the raw token the hub issued to this instance. |
| `ca_file` | string | unset | PEM CA certificates trusted for this hub instead of the built-in roots. |
| `accounts` | array of strings | `[]` | Account labels whose traffic goes through this hub. Each must exist in `[accounts.LABEL]`. |
| `timeout_secs` | integer | `300` | Longest one round trip to the hub. Range 1 to 3600. |

### `[hub]` and `[hub.edges.NAME]`

`[hub]` makes this instance a hub. Its edges are the streams it answers.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | string | required | Hub listener address. Must be a loopback IP and port, because no TLS is supported yet. |
| `edges` | table | `{}` | Edge streams this hub answers, one `[hub.edges.NAME]` table each. |

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `token_sha256` | string | required | SHA-256 hex of the token this edge presents. Must differ from every other credential. |
| `allow_from` | array of strings | `[]` | Account labels this edge may send traffic for. |
| `max_concurrent` | integer | `0` | Requests this edge may have in flight. `0` means unlimited. |
| `max_tokens_per_day` | integer | `0` | Tokens this edge may use per day. `0` means unlimited. |

### `[accounts.LABEL]`

One account: its key or keys on one upstream.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `upstream` | string | required | Upstream the account's keys authenticate to. |
| `keys` | array of strings | required | Key names from `[keys.*]`, tried in order. Non-empty. A key belongs to one account only. |
| `shared` | bool | `false` | Whether edges share this account. |
| `limits` | table | unset | Outbound shaping for this account; see below. |

### `[accounts.LABEL.limits]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `rpm` | integer | `0` | Requests per minute. `0` means unlimited. |
| `tpm` | integer | `0` | Estimated tokens per minute. `0` means unlimited. |
| `concurrent` | integer | `0` | Requests in flight. `0` means unlimited. |
| `daily_requests` | integer | `0` | Requests per day. `0` means unlimited. |
| `daily_tokens` | integer | `0` | Tokens per day. `0` means unlimited. |
| `preset` | string | unset | One of `zen-free` or `openrouter-free`; see [limit presets](#limit-presets). |

## Validation

`reiki check` runs the same load as the service. It parses the file, reads every key file, builds each plugin in dry-run mode, checks the intercept CA when interception is configured, and checks the peer TLS certificate when the peer listener is published with TLS. It prints one `ok:` line when all of this passes and exits 1 otherwise.

Rules, by section:

- **File.** Valid TOML. No unknown keys in a Reiki table. Syntax and unknown-key errors give a line and column, and an unknown-key error lists the keys the table accepts. Quoted text is hidden in these messages. Other errors name the setting and the rule.
- **Logging.** `level` is a valid level or filter. `file` is an absolute path. `stderr = false` needs `file`. `file_keep` is at most 100.
- **Service, reload, auth.** Each value is in the range shown above.
- **Server.** Each cap is in range. A per-address cap is at most its total.
- **Listeners.** `admin.listen` and `tunnel.listen` must differ from each other and from `listen`. A publishing peer listener must differ from all three. The hub listener must differ from client, admin and tunnel and be on loopback. A non-loopback peer listener needs TLS; a non-loopback admin listener is refused because it has no TLS option.
- **Upstream.** `base_url` is `https`, or `http` on loopback. `auth_header` is a valid header name. `auth_format` has `{key}` once and makes a valid header value. Model alias keys and values are not empty. Drop-parameter names are not empty. The timeouts and `max_decoded_bytes` are at least 1. `scope_escalate_after` is 2 to 16. Each response phrase list has at most 16 unique phrases of 3 to 120 non-control characters.
- **Upstream limits.** The table sets a preset or a limit. `max_wait_secs` is 1 to 120. The preset name is known.
- **Upstream forward.** Exactly one of `command` or `socket`. `args`, `pass_env` and `max_concurrent` apply only to `command`. `expect_uid` applies only to `socket`. `socket` is an absolute path of at most 103 bytes, in a directory that group and others cannot write unless it has the sticky bit, or with `expect_uid` set. `max_stream_bytes` and `stream_timeout_secs` are at least 1 when set. `send_keep_alive`, `http2` and `title_case_headers` are refused.
- **Upstream fallback.** Not on a passthrough upstream. The target exists, is not this upstream, and has valid aliases.
<<<<<<< ours
- **Keys.** The upstream exists and is not passthrough. `quota_resets` parses. Each key file exists, is readable, is not empty and holds at least 8 bytes. Unless `strict_key_permissions = false`, it must have no group or other read or write bits.
- **Gateway.** `usage_rpm` is 1 to 3600 and `on_store_error` is `closed` or `open`.
- **Streams.** Every chain entry is a known key. The chain is not empty. `token_sha256` is 64 hex characters and unique. The name `passthrough` is not used. `weight` is at least 1. Every limit in `[streams.NAME.limits]` is positive, `period_zone` is `UTC` or a fixed offset, and `on_store_error` is `closed` or `open`. Allowlist patterns use only `*` and `?`. If an upstream falls back, the chain holds a key for the target.
=======
- **Keys.** The upstream exists and is not passthrough. `quota_resets` parses. Each key file exists, is readable, and holds 8 to 4096 bytes. Unless `strict_key_permissions = false`, it must have no group or other read or write bits.
- **Streams.** Every chain entry is a known key. The chain is not empty. `token_sha256` is 64 hex characters and unique. The name `passthrough` is not used. `rate_limit_per_minute` is at least 1. If an upstream falls back, the chain holds a key for the target.
>>>>>>> theirs
- **Admin.** `token_sha256` is 64 hex characters and not the same as a stream token. `listen` is a literal loopback IP and port; hostnames are refused.
- **Stream admin.** Needs `[admin]`; `allowed_listen` contains that literal loopback address. Both hashes are 64 hex characters and distinct from each other, the admin and stream tokens, and the two-party hashes. Environment variable names are valid and differ; file names are relative with the required suffix and differ. Ranges: `max_streams` 1 to 10000, `max_tokens_per_stream` 1 to 100, `default_ttl` at least 1 and no greater than `max_ttl` (at most 2592000), `max_body_bytes` 1024 to 1048576, `lockout_secs` 1 to 86400, and `lockout_threshold` and `rpm` at least 1. Every `allowed_keys` entry names a known key and sets `assignable = true`; each present ceiling is positive, and `default_limits` stay within the ceilings.
- **Tunnel.** `allow` is not empty and every entry is valid. `connect_timeout_secs` is at least 1. Each intercepted host is unique, valid, covered by `allow`, and served by an upstream and stream that match.
- **Peers.** Each numeric setting is in range. `session_header` is a lowercase header that is not a credential header. `instance` is a valid name. Each token name and hash is valid and unique. Each `allowed_urls` entry is a plain base URL without credentials, query or fragment. Each `[peers.NAME]` has a valid URL and `token_file`, matches `allowed_urls` when that list is set, and does not point at one of this instance's literal loopback listeners. An `https` peer has `ca_file`. Publishing needs `listen` and at least one token. `tls_cert` and `tls_key` are both set or both unset. Invalid URLs are reported without echoing them, so credentials in a malformed URL cannot reach logs.
- **Plugins.** The plugin is compiled in. `enabled` is a boolean. `on_error` is `pass_through` or `reject`. Each `streams` entry names a configured stream or `passthrough`. The plugin's own settings are valid. `requestlog` refuses `body = "full"` or `response = "body"` unless `capture_bodies = true`. `cost` needs at least one price table, a shared uppercase currency label, decimal string prices and budgets, and a `report` or `block` value for `on_exceed`.
- **Hub.** Only with the `hub` feature. Names, URLs and token hashes are valid, edge tokens are unique and differ from every other credential, accounts and keys exist, and no key is used by two accounts.
- **Build features.** A `[peers]` table needs the `peers` feature. Any hub table needs the `hub` feature. A `[plugins.cost]` table needs the `plugin-cost` feature; cost is not in the default build.

Startup also fails when a key file cannot be read or is too short. `reiki check` reports the same failures.

See also: [cli](cli.md) for the commands that read this file, and [security](security.md) for the trust boundaries these settings draw.
