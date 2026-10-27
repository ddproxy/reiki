# Monitoring

Reiki can serve a small admin listener with `GET /status`, plus any paths a plugin adds such as `GET /metrics` from the `metrics` plugin. It is off unless you add an `[admin]` table, and it always listens on its own socket, never the client one.

## Configure

```toml
[admin]
listen = "127.0.0.1:4098"                 # must differ from the client `listen`
token_sha256 = "<sha256 of the admin token>"
```

Both fields are required. The admin address is bound at startup, so adding, removing or moving `[admin]` needs a restart; a changed token hash is picked up on reload. Generate the hash with `printf '%s' "$ADMIN_TOKEN" | reiki hash-token`. `reiki check` prints the admin address alongside the client one.

## Authenticate

Send the admin token as a bearer credential: `Authorization: Bearer <token>`. A missing or wrong token gets a bare 401 with no body. The token is hashed and compared in constant time. Anything other than `/status` and a path a configured plugin serves returns 404. The listener serves plain HTTP, so keep it on loopback or put a TLS-terminating proxy in front of it.

## GET /status

With `[tunnel.intercept]` configured, the document also has `intercept.ca_not_after` (Unix seconds) and `intercept.ca_days_left`, so an expiring CA can be alerted on.

JSON with a `keys` array and a `streams` array.

Each key:

- `name`, `upstream`
- `state`: `available`, `cooling` or `out_of_rotation`
- `retry_after_seconds`: present only while `cooling`
- `consecutive_failures`: consecutive transient failures since the last success

Each stream:

- `name`, `strategy` (`sticky` or `round_robin`), `chain`
- `current_key`: the key the stream would prefer next, or `null`
- `rate_limit_per_minute`: the stream's limit, or `null`

The response carries no key values, token hashes or file paths.

## GET /metrics

Served by the `metrics` plugin, so it exists only when the binary includes the plugin and the config has a `[plugins.metrics]` table (an empty table is enough). Without that table the path returns 404.

```toml
[plugins.metrics]
```

Prometheus text exposition, format version 0.0.4. Gauges are sampled at scrape time; counters live in memory, survive a reload and reset when Reiki restarts.

| Metric | Type | Labels |
| --- | --- | --- |
| `reiki_requests_total` | counter | `stream`, `upstream`, `status` |
| `reiki_failovers_total` | counter | `stream`, `key`, `class` |
| `reiki_cooldowns_total` | counter | `key`, `class` |
| `reiki_rate_limit_rejections_total` | counter | `stream` |
| `reiki_keys` | gauge | `state` |

`status` is `success`, `client_error` or `server_error`. `class` is `rate_limited`, `quota_exhausted`, `access_disabled`, `unauthorized` or `transient`. The `reiki_keys` gauge always reports all three states, even at zero.

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/metrics
```
