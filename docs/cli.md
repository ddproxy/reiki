# Command line

`reiki` runs the proxy with no subcommand. Other subcommands check config, read a running instance or do one job and exit.

```
reiki [--config PATH] [--listen ADDR] [COMMAND [ARGS]]
```

`reiki COMMAND --help` prints the flags for each command; `reiki --version` prints the build version. The argument parser exits `2` for a usage error, such as an unknown or conflicting flag. Individual commands add their own codes.

## Global options

| Option | Default | Meaning |
| --- | --- | --- |
| `--config PATH` | `/etc/reiki/reiki.toml` | Config file. Read by server mode, `check`, `keys`, `failures`, `reload`, `streams`, `peers`, `top` and `cost` (when `--url` is omitted). `hash-token`, `ca-init` and `scan` do not read it. |
| `--listen ADDR` | `listen` in the config | Client listen address. Used by server mode and `check`; other commands ignore it. |

### Environment

| Variable | Used by | Meaning |
| --- | --- | --- |
| `REIKI_LISTEN` | server mode, `check` | Client address. Overridden by `--listen`; overrides `listen` in the config. |
| `REIKI_ADMIN_TOKEN` | `reload`, `keys`, `failures`, `top`, `cost` | Admin token when `--token-file` is not given. `top` uses it only for full local failure rows. `streams` uses `REIKI_STREAM_ADMIN_TOKEN` instead. |
| `REIKI_ADMIN_READ_TOKEN` | `top` | Optional read-only admin token. Preferred over `REIKI_ADMIN_TOKEN`; failure rows then omit body snippets. Mutating commands do not read it. |
| `RUST_LOG` | server mode | Log filter. Overrides `[log] level` at startup and after reload. |

`--listen` and `REIKI_LISTEN` accept `IP:port` or `host:port`.

## Exit codes

`0` is success and `2` is an argument-parser usage error unless a command says otherwise. A config that does not parse exits `1` for `check` and `2` for `keys`. Codes are not uniform across commands; each section below lists them.

## Server mode

```
reiki --config /etc/reiki/reiki.toml
```

Starts the client listener and the admin, peer and tunnel listeners when configured. Logs go to stderr and to `[log] file` when set. Runs until SIGTERM or SIGINT; SIGHUP reloads the config.

| Exit | Meaning |
| --- | --- |
| `0` | Clean stop after a signal. |
| `1` | Startup failed: config, key files, plugin build or listener bind. |
| `143` / `130` | Second stop signal during drain: SIGTERM / SIGINT. |

SIGTERM starts a clean stop; in-flight requests get `[service] shutdown_timeout_secs`. See [deploy](deploy.md).

## hash-token

Reads a token from stdin and prints its SHA-256 in hex, for `token_sha256` in `[admin]` or `[streams.NAME]`.

```
reiki hash-token
```

| Behaviour | Detail |
| --- | --- |
| Input | All of stdin, trimmed of leading and trailing whitespace. |
| Length | At least 16 characters; a shorter token is refused. |
| Output | 64 lowercase hex characters and a newline. |

Exit `0` on success, `1` when the token is too short or stdin cannot be read.

```sh
openssl rand -hex 24 > client.token
reiki hash-token < client.token
```

Keep `client.token` for the client; put only the hash in the config. See [keys](keys.md).

## check

Loads the config the way the service does, then exits. It does not contact an upstream, admin listener or peer.

```
reiki --config /etc/reiki/reiki.toml check
```

| Step | Checked |
| --- | --- |
| Config | Parse and validate every table. See [configuration](configuration.md). |
| Keys | Each key file exists, is readable, is not empty and holds at least 8 bytes. |
| Plugins | Each `[plugins.NAME]` builds in dry-run mode. |
| Intercept CA | Key matches, is not expired and covers every intercepted host; prints days left. |
| Peer certificate | `tls_cert` and `tls_key` load; prints days left. |
| Forwarder socket | Warnings are printed as `warning: ...`. |

Output for one upstream, one key, one stream and peers off:

```text
caps: client 1024 (256 per address), admin 64 (16 per address); loopback exempt from the per-address caps
peers: disabled
ok: 1 upstream(s), 1 key(s), 1 stream(s), listening on 127.0.0.1:4097, admin on 127.0.0.1:4098
```

Exit `0` when every step passes, `1` on any failure.

## scan

Runs detection plugins over files or stdin and reports where each identifier is. It never prints the matched text. Use it to tune `kinds` before switching a plugin from `report` to `block`; see [plugins](plugins.md).

```
reiki scan [--plugin NAME]... [--kinds K1,K2 | --all] [--json] [FILE]...
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--plugin NAME` | every compiled-in detector | Run only this plugin. Repeat for several: `phi`, `finance`, `controlled`. |
| `--kinds K1,K2` | each plugin's default set | Comma-separated kinds. Conflicts with `--all`. |
| `--all` | off | Look for every kind, including those off by default. |
| `--json` | off | One JSON object per finding. |
| `FILE...` | stdin | Files to scan; `-` means stdin. |

| Plugin | Default | With `--all` adds |
| --- | --- | --- |
| `phi` | `ssn`, `card`, `routing`, `account`, `dob`, `medical_id`, `npi`, `icd10`, `column` | `field`, `email`, `phone` |
| `finance` | `iban`, `swift`, `crypto`, `cusip`, `isin`, `pan_sad` | `account`, `sedol` |
| `controlled` | `cui`, `classification`, `dissemination` | `ein`, `itin`, `cage`, `uei`, `edipi`, `eccn` |

Text output is `FILE:LINE:COL PLUGIN KIND`; JSON output is one object per line, for example `{"file":"FILE","line":1,"col":6,"plugin":"phi","kind":"card"}`. A summary goes to stderr, for example `scanned 1 input(s): 2 finding(s): phi card x1, phi ssn x1`.

```
reiki scan --plugin phi --kinds ssn,card notes.txt
```

Exit `0` with no findings, `1` with at least one, `2` for an unknown plugin or kind, an unreadable file or a usage error.

## reload

Asks the running instance to reload its config over the admin listener (`POST /v1/reload`). `--dry-run` reports changes without applying them. See [monitoring](monitoring.md) and [troubleshooting](troubleshooting.md).

```
reiki reload [--dry-run] [--url URL] [--token-file PATH]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--dry-run` | off | Report changes only. |
| `--url URL` | `http://` plus `[admin] listen` | Admin address. An unspecified address such as `0.0.0.0` is sent as `127.0.0.1`. |
| `--token-file PATH` | `REIKI_ADMIN_TOKEN` | File holding the admin token; whitespace is trimmed. |

Output is the JSON answer from `/v1/reload` for both success and refusal:

```json
{"dry_run":false,"generation":1,"keys":1,"streams":1,"changed":[],"restart_needed":[]}
```

`generation` counts successful reloads; `keys` and `streams` are post-reload counts; `changed` and `restart_needed` list settings that differ or need a restart. A refusal uses the admin error envelope.

```
reiki reload --dry-run --url http://127.0.0.1:4098 --token-file /etc/reiki/admin.token
```

Exit `0` when the reload or preview succeeds, `1` when the admin listener refuses or the reload fails, `2` when no token or address is available, the config is unreadable or the listener cannot be reached.

## keys

Shows key health from a running instance or checks key files offline. The three forms are mutually exclusive; see [keys](keys.md).

```
reiki keys [--json] [--url URL] [--token-file PATH]
reiki keys --enable NAME [--url URL] [--token-file PATH]
reiki keys --check
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--json` | off | Print JSON instead of a table. Conflicts with `--check` and `--enable`. |
| `--url URL` | `[admin] listen` from the config | Admin address. |
| `--token-file PATH` | `REIKI_ADMIN_TOKEN` | Admin token file. |
| `--enable NAME` | none | Re-enable a key that is out of rotation (`POST /v1/keys/enable`). |
| `--check` | off | Check the config's key files without contacting an instance. Conflicts with the other flags. |

### Health table

Reads `/v1/status`:

```text
KEY    UPSTREAM  STATE  COOLDOWN  LAST  STREAMS
zen-a  zen       ok     -         -     client
```

| Column | Meaning |
| --- | --- |
| `KEY` | Name from `[keys.NAME]`. |
| `UPSTREAM` | Upstream the key authenticates to. |
| `STATE` | `ok`, `cooling` for the remaining cooldown, or `disabled` until enabled. |
| `COOLDOWN` | Seconds left, or `-`. |
| `LAST` | Last upstream status code, or `-`. |
| `STREAMS` | Streams whose chain holds the key. |

JSON adds `cooldown_class` when reported and always shows `scopes`, sorted by soonest expiry. `--enable` prints the admin answer, such as `{"key":"zen-a"}`; an unknown name gets `404` and exit `1`. `--check` prints one line per key, for example `zen-a: ok`, a permission warning or a missing-file error. Problems are listed for every key, and a warning also exits `1`.

| Mode | `0` | `1` | `2` |
| --- | --- | --- | --- |
| Table or JSON | Answer received and parsed. | Admin error or invalid `/v1/status` JSON. | No token or address, unreachable listener, unreadable answer. |
| `--enable` | Key re-enabled. | Refused, such as unknown key (`404`). | As above. |
| `--check` | Every file clean. | Any error or warning. | Config unreadable or invalid. |

## streams

Manages opt-in dynamic streams when `[stream_admin]` is configured. The token comes from `--token-file` or the configured `token_env` (default `REIKI_STREAM_ADMIN_TOKEN`), never from an argument. See [configuration](configuration.md) and the [API examples](README.md).

```
reiki streams [--url URL] [--token-file PATH] [--reason TEXT] [--idempotency-key KEY] COMMAND
```

| Command | Meaning |
| --- | --- |
| `list` | List dynamic streams. |
| `show NAME` | Show one stream and its token ids. |
| `create NAME KEY [--ttl SECS]` | Create a stream with an assignable key; print the first token once. |
| `update NAME [--key KEY] [--rpm N] [--tpm N]` | Change the assignable key or per-stream ceilings. |
| `revoke NAME` | Revoke every token on the stream. |
| `token add NAME [--ttl SECS]` | Add a token; print it once. |
| `token revoke NAME TOKEN_ID` | Revoke one token by id from `show`. |
| `audit verify FILE` | Verify a JSONL audit log, its hash chain and the sibling `audit.head` sidecar offline. |
| `audit head FILE [--n N]` | Print the first `N` audit records, default 10, offline. |

`--reason` is required for every mutation and is written to the audit log. `--idempotency-key` is generated when omitted. With two-party confirmation, the second raw token is read from `confirm_env` and sent as `X-Confirm-Token`. `audit verify` and `audit head` need no running instance or token and require the JSONL file to exist; verification refuses a non-empty chain without its sidecar. Exit `0` on success, `1` when the API refuses a request, `2` for a usage, config, transport or audit-verification error. There is no CLI delete command and no audit HTTP route. See [monitoring](monitoring.md) and [dynamic streams](dynamic-streams.md).

## failures

Shows the running instance's recent key failures, from the same in-memory ring as `GET /v1/failures`.

```
reiki failures [--limit N] [--key NAME] [--stream NAME] [--json] [--url URL] [--token-file PATH]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--limit N` | `20` | Newest records to show, 1 to 64. |
| `--key NAME` | all keys | Only this key name. |
| `--stream NAME` | all streams | Only this stream name. |
| `--json` | off | Print the failure envelope with `failures`, `failures_total` and `failures_truncated`. |
| `--url URL` | `[admin] listen` from the config | Admin address. |
| `--token-file PATH` | `REIKI_ADMIN_TOKEN` | Admin token file. |

```text
TIME                 STREAM  KEY    STATUS  CLASS         ACTION      COOLED  NOTE
YYYY-MM-DD HH:MM:SS  client  zen-a  429     rate_limited  cooldown    42s     {"error":"rate limited"}
```

| Column | Meaning |
| --- | --- |
| `TIME` | Failure time in UTC. |
| `STREAM` | Stream that made the attempt. |
| `KEY` | Key name from `[keys.NAME]`. |
| `STATUS` | Upstream status, or `-` for a transport failure. |
| `CLASS` | `rate_limited`, `quota_exhausted`, `access_disabled`, `unauthorized`, `transient` or `request_error`. |
| `ACTION` | `cooldown`, `rotate` or `pass-through` from the failover walk. |
| `COOLED` | Seconds left on the cooldown this failure started, or `-`. |
| `NOTE` | Scrubbed, printable body snippet, cut to 200 bytes and terminal width (`COLUMNS`, default 120). |

Lines are cut to `COLUMNS`. Nothing in the ring is written to disk; a restart starts it empty. The ring holds `failure_ring_size` entries (`200` by default; `0` disables). `--json` prints the envelope whether or not `--limit` was given. Exit `0` on success, `1` when the admin listener refuses the token or answer, `2` when it cannot be reached or no token or address is available.

## cost

Exports the `cost` plugin's estimate report. The command exists only in a binary built with `--features plugin-cost`.

```
reiki cost export --period YYYY-MM --format csv|json [--url URL] [--token-file PATH]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--period PERIOD` | current UTC month | `YYYY-MM`, `YYYY-MM-DD` or `YYYY-MM-DDTHH`. |
| `--format` | `json` | `csv` or `json`. |
| `--url URL` | `[admin] listen` from the config | Admin address. |
| `--token-file PATH` | `REIKI_ADMIN_TOKEN` | Admin token file; never an argument. |

The body is printed unchanged; see [cost](cost.md). Exit `0` on success, `1` when the admin listener refuses the answer or the plugin is absent, `2` for a bad period, missing token or address, or a transport error.

## peers

Reads `/v1/health` on each `[peers.NAME]` and prints what it reports, using each peer's `token_file`. See [provider health](provider-health.md).

```
reiki peers [--json] [--watch]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--json` | off | Print JSON. |
| `--watch` | off | Follow `/v1/health/stream` and redraw on each change; runs until interrupted. |

```text
PEER    INSTANCE  UPSTREAM  STATE  HEALTHY  COOLING  DISABLED  WAIT  AGE
edge-1  edge-pub  zen       ok     1        0        0         -     0s
```

| Column | Meaning |
| --- | --- |
| `PEER` | Name from `[peers.NAME]`. |
| `INSTANCE` | Name the peer publishes. |
| `UPSTREAM` | One row per reported upstream. |
| `STATE` | `ok`, `cooling`, `disabled` or `no_keys`; `unknown` for stale or missing data, `no_upstreams` when none are reported. |
| `HEALTHY`, `COOLING`, `DISABLED` | Key counts for the upstream. |
| `WAIT` | Shortest remaining cooldown, or `-`. |
| `AGE` | Seconds since the peer's data was taken. |

An unreachable peer prints an error line to stderr and shows as `unknown`. JSON has a `peers` array with `name`, `instance`, `state`, `error`, `age_seconds` and `upstreams`. Exit `0` when at least one peer is fresh, `1` when none is, `2` for a config problem or no peers listed.

## top

A live terminal view of peer key health: a peers-by-providers matrix with the selected peer's detail below it. `t` opens a read-only local Streams screen. See [top](top.md) for the screens; this page lists the keys. Needs the `top` build feature; release binaries include it.

```
reiki top [--once] [--interval SECS] [--full] [--provider NAME]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--once` | off | Poll each peer once, print plain text and exit. |
| `--interval SECS` | `1` | Seconds between redraws; values below 1 count as 1. Live mode only. |
| `--full` | off | Start in the per-peer text view. |
| `--provider NAME` | all providers | Show only this provider's column. |

```text
Fleet 1/1 fresh   zen 1/1   [req 1h]
          zen       Ver     load  disk  sess
  edge-1  ■ ······  0.10.3  -     -     -
```

| Mode | `0` | `1` | `2` |
| --- | --- | --- | --- |
| `--once` | At least one peer answered. | No peer answered. | Config problem, as for `peers`. |
| Live | Quit with `q`, `Esc` or Ctrl-C. | Terminal could not be set up or read. | Config problem or stdout is not a terminal; use `--once` in scripts. |

Keys in the live view: `q`/`Esc`/Ctrl-C quit; `j`/`k` select a peer in matrix mode or scroll in full view; `PageUp`/`PageDown` scroll a screen; `g`/`Home` and `G`/`End` jump; `Tab`/`Shift-Tab` change peer; `Enter`/`e` open or close detail; `v`/`E` switch views; `f` cycles the provider filter; `c` clears filters; `p` pauses; `u` switches the gauge between requests and tokens; `w` switches the window between one hour and five minutes; `m` cycles the chart metric; `?` shows the key list. `x` does nothing. The matrix footer reads `q quit  j/k  Enter detail  f filter  u metric  w window  m chart  s stack  a all  v full  ? keys`; the full view uses `j/k scroll`.

## ca-init

Creates a certificate authority for `[tunnel.intercept]`. The CA is name-constrained to the listed hosts.

```
reiki ca-init --dir DIR --host HOST [--host HOST]... [--days N] [--replace]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--dir DIR` | required | Directory that receives `ca.pem` and `ca.key`; created if missing. |
| `--host HOST` | required, repeatable | Lowercase DNS name with at least two labels. |
| `--days N` | `365` | Validity in days, 1 to 3650. |
| `--replace` | off | Overwrite an existing CA; clients then need the new `ca.pem`. |

```
reiki ca-init --dir /etc/reiki/intercept --host api.example.net --days 365
```

`ca.key` is mode `0600`; give clients only `ca.pem`. Exit `0` on success, `1` for a bad host, a bad `--days` or an existing CA without `--replace`. Then point the config at the files:

```toml
[tunnel.intercept]
ca_cert = "/etc/reiki/intercept/ca.pem"
ca_key = "/etc/reiki/intercept/ca.key"
```

See [tunnel](tunnel.md).

## plugin inspect

Only builds with `plugin-wasm` have this subcommand. It reads a Wasm component without starting a proxy and prints its manifest, requested and derived capabilities and configured limits.

```
reiki plugin inspect FILE [--json]
```

`FILE` is a required `.wasm` path; `--json` prints the report as JSON instead of text lines such as `name: ...` and `pin: ...`. It does not read the config or contact an admin listener. Exit `0` for a readable component, `1` for an unreadable file or invalid component, `2` for a usage error. See [plugin authoring](plugin-authoring.md).
