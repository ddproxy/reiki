# Command line

`reiki` runs the proxy when given no subcommand. The other subcommands check config, read a running instance, or do one job and exit.

```
reiki [--config PATH] [--listen ADDR] [COMMAND [ARGS]]
```

`reiki COMMAND --help` prints the flags for each command. `reiki --version` prints the build version.

## Global options

| Option | Default | Meaning |
| --- | --- | --- |
| `--config PATH` | `/etc/reiki/reiki.toml` | Config file. Read by `check`, server mode, `keys`, `failures`, `reload`, `peers` and `top`. `hash-token`, `ca-init` and `scan` do not read it. |
| `--listen ADDR` | from `listen` in the config | Client listen address. Used by server mode and `check`. Other commands ignore it. |

### Environment

| Variable | Used by | Meaning |
| --- | --- | --- |
| `REIKI_LISTEN` | server mode, `check` | Client listen address. Overridden by `--listen`; overrides `listen` in the config. |
| `REIKI_ADMIN_TOKEN` | `reload`, `keys`, `failures`, `top` | Admin token, when `--token-file` is not given. `top` uses it only to read the local instance's full failure rows. |
| `RUST_LOG` | server mode | Log filter. Overrides `[log] level` at startup and after reload. |

`--listen` and `REIKI_LISTEN` accept an `IP:port` address or a `host:port` name.

## Exit codes

The argument parser exits `2` on a usage error: an unknown flag, or two flags that conflict. Beyond that, each command lists its own exit codes in its section. Codes are not the same across commands. For example, a config that does not parse exits `1` for `check` and `2` for `keys`.

## Server mode

```
reiki --config /etc/reiki/reiki.toml
```

Starts the client listener, and the admin, peer and tunnel listeners when configured. Logs go to stderr and to `[log] file` when set. Runs until SIGTERM or SIGINT. SIGHUP reloads the config.

| Exit | Meaning |
| --- | --- |
| `0` | Stopped cleanly after a signal. |
| `1` | Startup failed: config, key files, plugin build or a listener could not be bound. |

Send SIGTERM for a clean stop. Requests in flight get `[service] shutdown_timeout_secs`. A second signal exits at once. See [deploy](deploy.md#signals-and-lifecycle).

## hash-token

Reads a token from stdin and prints its SHA-256 in hex, for `token_sha256` in `[admin]` or `[streams.NAME]`.

```
reiki hash-token
```

| Behaviour | Detail |
| --- | --- |
| Input | All of stdin. Leading and trailing whitespace is removed. |
| Length | At least 16 characters. A shorter token is refused. |
| Output | 64 lowercase hex characters and a newline. |

Exit codes: `0` on success; `1` when the token is too short or stdin cannot be read.

Example, with a token generated for one client:

```
openssl rand -hex 24 > client.token
reiki hash-token < client.token
```

```
<64 hex characters>
```

Keep `client.token` for the client. Put only the hash in the config.

## check

Loads the config the way the service does, then exits. It does not contact the upstream, an admin listener or a peer.

```
reiki --config /etc/reiki/reiki.toml check
```

| Step | Checked |
| --- | --- |
| Config | Parse and validate every table. See [validation](configuration.md#validation). |
| Keys | Each key file exists, is readable, is not empty and holds at least 8 bytes. |
| Plugins | Each `[plugins.NAME]` builds in dry-run mode. |
| Intercept CA | Key matches, not expired, covers every intercepted host. Prints days left. |
| Peer certificate | `tls_cert` and `tls_key` load. Prints days left. |
| Forwarder socket | Warnings are printed as `warning: ...`. |

Output, for a config with one upstream, one key, one stream and peers off:

```
caps: client 1024 (256 per address), admin 64 (16 per address); loopback exempt from the per-address caps
peers: disabled
ok: 1 upstream(s), 1 key(s), 1 stream(s), listening on 127.0.0.1:4097, admin on 127.0.0.1:4098
```

Exit codes: `0` when every step passes; `1` on any failure, including a config that does not parse.

## scan

Runs the detection plugins over files or stdin. It reports where each identifier is. It never prints the matched text, so its output is safe to share. Use it to tune `kinds` before you switch a plugin from `report` to `block`.

```
reiki scan [--plugin NAME]... [--kinds K1,K2 | --all] [--json] [FILE]...
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--plugin NAME` | every compiled-in plugin | Run only this plugin. Repeat for several. Names: `phi`, `finance`, `controlled`. The former name `gov` is rejected. |
| `--kinds K1,K2` | each plugin's default set | Kinds to look for, comma separated. A kind is matched only by the selected plugins that know it. Conflicts with `--all`. |
| `--all` | off | Look for every kind, including the ones that are off by default. |
| `--json` | off | One JSON object per finding. |
| `FILE...` | stdin | Files to scan. `-` means stdin. |

Kinds per plugin:

| Plugin | Default | With `--all` adds |
| --- | --- | --- |
| `phi` | `ssn`, `card`, `routing`, `account`, `dob`, `medical_id`, `npi`, `icd10`, `column` | `field`, `email`, `phone` |
| `finance` | `iban`, `swift`, `crypto`, `cusip`, `isin`, `pan_sad` | `account`, `sedol` |
| `controlled` | `cui`, `classification`, `dissemination` | `ein`, `itin`, `cage`, `uei`, `edipi`, `eccn` |

Text output, one line per finding:

```
FILE:LINE:COL PLUGIN KIND
```

JSON output, one object per line:

```
{"file":"FILE","line":1,"col":6,"plugin":"phi","kind":"card"}
```

A summary goes to stderr:

```
scanned 1 input(s): 2 finding(s): phi card x1, phi ssn x1
```

Example:

```
reiki scan --plugin phi --kinds ssn,card notes.txt
```

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | No findings. |
| `1` | At least one finding. |
| `2` | Unknown plugin or kind, a file cannot be read, or a usage error. |

A named kind that the selected plugin does not know exits `2`. For example, `--plugin finance --kinds ssn` fails.

## reload

Asks the running instance to reload its config over the admin listener. Posts to `/reload`. Use `--dry-run` to see what a reload would change without applying it. See [upgrades](upgrades.md#reload-vs-restart) and [troubleshooting](troubleshooting.md#reload).

```
reiki reload [--dry-run] [--url URL] [--token-file PATH]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--dry-run` | off | Report the changes only. Apply nothing. |
| `--url URL` | `http://` plus `[admin] listen` from the config | Admin address. An unspecified address such as `0.0.0.0` is sent as `127.0.0.1`. |
| `--token-file PATH` | `REIKI_ADMIN_TOKEN` | File that holds the admin token. Leading and trailing whitespace is removed. |

Output is the JSON answer from `/reload`, printed to stdout for both success and refusal:

```
{"ok":true,"dry_run":false,"generation":1,"keys":1,"streams":1,"changed":[],"restart_needed":[]}
```

Fields: `ok`, `dry_run`, `generation` (successful reloads since startup), `keys` and `streams` (counts after the reload), `changed` (what differs, such as `plugins phi`), `restart_needed` (settings that apply only after a restart).

A failed reload answers `{"ok":false,"error":"..."}` and exits `1`.

Example:

```
reiki reload --dry-run --url http://127.0.0.1:4098 --token-file /etc/reiki/admin.token
```

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | The reload or the preview succeeded. |
| `1` | The admin listener refused the request, or the reload failed. |
| `2` | No admin token, no admin address, config unreadable (when `--url` is not given), or the listener could not be reached. |

## keys

Shows each key's health from the running instance, or checks key files offline. The three forms are mutually exclusive. See [keys and streams](keys.md).

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
| `--enable NAME` | none | Re-enable a key that is out of rotation. Posts to `/v1/keys/enable`. |
| `--check` | off | Check the key files in the config without contacting an instance. Conflicts with the other flags. |

### Health table

Reads `/status` on the admin listener:

```
KEY    UPSTREAM  STATE  COOLDOWN  LAST  STREAMS
zen-a  zen       ok     -         -     client
```

| Column | Meaning |
| --- | --- |
| `KEY` | Key name from `[keys.NAME]`. |
| `UPSTREAM` | Upstream the key authenticates to. |
| `STATE` | `ok`, `cooling` (for the `COOLDOWN` time) or `disabled` (out of rotation until `--enable`). |
| `COOLDOWN` | Seconds left on a cooling key, or `-`. |
| `LAST` | Last upstream status code, or `-`. |
| `STREAMS` | Streams whose chain holds the key. |

### JSON

```
{"keys":[{"name":"zen-a","upstream":"zen","state":"ok","cooldown_seconds":null,"last_status_code":null,"streams":["client"]}]}
```

`cooldown_class` and `scopes` appear when the instance reports them. `scopes` lists cooling route and model pairs, soonest first.

### `--enable`

Prints the admin answer, such as `{"ok":true,"key":"zen-a"}`. A name that does not exist gets `404` and exit `1`.

### `--check`

Reads each key file named in the config. It prints one line per key:

```
zen-a: ok
loose: warning: /path/to/key is group or world readable (mode 0644)
gone: error: /path/to/key does not exist
```

A warning also exits `1`. Problems are listed for every key, not only the first.

Exit codes:

| Mode | `0` | `1` | `2` |
| --- | --- | --- | --- |
| Health table or JSON | Answer received and parsed. | Admin listener answered with an error status, or the answer is not valid `/status` JSON. | No admin token, no admin address, the listener could not be reached, or the answer could not be read. |
| `--enable` | Key re-enabled. | Refused, such as an unknown key (`404`). | As above. |
| `--check` | Every key OK. | Any error or warning. | Config unreadable or does not parse. |

## failures

Shows the running instance's last key failures, from the same in-memory ring as `GET /v1/failures`.

```
reiki failures [--limit N] [--key NAME] [--json] [--url URL] [--token-file PATH]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--limit N` | `20` | Records to show, newest first; 1 to 64. |
| `--key NAME` | all keys | Only failures for this key name. |
| `--json` | off | Print the same JSON array as the endpoint. |
| `--url URL` | `[admin] listen` from the config | Admin address. |
| `--token-file PATH` | `REIKI_ADMIN_TOKEN` | Admin token file. |

The table:

```
TIME               STREAM  KEY    STATUS  CLASS         COOLED  NOTE
2026-11-17 12:00:00  client  zen-a  429     rate_limited  42s     {"error":"rate limited"}
```

| Column | Meaning |
| --- | --- |
| `TIME` | Failure time in UTC. |
| `STREAM` | Stream that made the attempt. |
| `KEY` | Key name from `[keys.NAME]`. |
| `STATUS` | Upstream status code, or `-` for a transport failure. |
| `CLASS` | `rate_limited`, `quota_exhausted`, `access_disabled`, `unauthorized` or `transient`. |
| `COOLED` | Seconds left on the cooldown this failure started, or `-`. |
| `NOTE` | Scrubbed, printable body snippet, cut to the terminal width (`COLUMNS`, default 120). |

Lines are cut to `COLUMNS`, so a narrow terminal keeps the columns aligned. Nothing in the ring is written to disk; a restart starts it empty. `--json` prints an array whether or not `--limit` was given.

Exit codes: `0` on success; `1` when the admin listener refuses the token or answer; `2` when it cannot be reached or no admin token or address is available.

## peers

Reads `/v1/health` on each `[peers.NAME]` and prints what it reports. Each peer's token comes from its `token_file` in the config. See [peers](peers.md).

```
reiki peers [--json] [--watch]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--json` | off | Print JSON. |
| `--watch` | off | Follow each peer's `/v1/health/stream` and print the table on each change. A peer whose stream is down is polled. Runs until interrupted. |

Table output:

```
PEER    INSTANCE  UPSTREAM  STATE  HEALTHY  COOLING  DISABLED  WAIT  AGE
edge-1  edge-pub  zen       ok     1        0        0         -     0s
```

| Column | Meaning |
| --- | --- |
| `PEER` | Name from `[peers.NAME]`. |
| `INSTANCE` | Name the peer publishes, from its `instance` setting. |
| `UPSTREAM` | One row per upstream the peer reports. |
| `STATE` | Per upstream: `ok`, `cooling`, `disabled` or `no_keys`. Per peer: `unknown` when the data is missing or older than three poll intervals, and `no_upstreams` when the peer has none. |
| `HEALTHY`, `COOLING`, `DISABLED` | Key counts for the upstream. |
| `WAIT` | Shortest remaining cooldown, or `-`. |
| `AGE` | Seconds since the peer's data was taken. |

A peer that cannot be reached prints an error line to stderr, for example `reiki: peer `edge-1`: cannot connect: ...`, and shows as `unknown`.

JSON output has a `peers` array with each peer's `name`, `instance`, `state` (`fresh` or `unknown`), `error`, `age_seconds` and `upstreams`.

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | At least one peer gave fresh data. |
| `1` | No peer gave fresh data. |
| `2` | Config unreadable or invalid, no `[peers]` table, `[peers]` not enabled, or no peers listed. |

## top

A live terminal view of every peer's key health. The screen is a peers-by-providers matrix, with the selected peer's detail below it. See [top](top.md) for the screens and columns. This page lists the keys.

```
reiki top [--once] [--interval SECS] [--full] [--provider NAME]
```

Needs the `top` build feature. The feature is not in the default build. Release binaries include it.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--once` | off | Poll each peer once, print plain text and exit. |
| `--interval SECS` | `1` | Seconds between redraws. Values below 1 count as 1. Live mode only. |
| `--full` | off | Start in the per-peer text view instead of the matrix. |
| `--provider NAME` | all providers | Show only this provider's column. An unknown name prints `unknown provider: NAME` to stderr and shows everything. |

One-shot output:

```
Fleet 1/1 fresh   zen 1/1   [req 1h]
          zen       Ver     load  disk  sess
  edge-1  ■ ······  0.10.3  -     -     -
```

Exit codes:

| Mode | `0` | `1` | `2` |
| --- | --- | --- | --- |
| `--once` | At least one peer answered. | No peer answered. | Config problem, as for `peers`. |
| Live | Quit with `q`, `Esc` or Ctrl-C. | Terminal could not be set up or read. | Config problem, or stdout is not a terminal. Use `--once` in scripts. |

### Keys in the live view

Some keys act only in matrix mode, which is the default view. Ctrl-C quits from any screen, including the key list.

| Key | Action |
| --- | --- |
| `q`, `Esc`, Ctrl-C | Quit. |
| `j`, `Down` | Matrix: select the next peer. Full view: scroll down. |
| `k`, `Up` | Matrix: select the previous peer. Full view: scroll up. |
| `PageUp`, `PageDown` | Scroll one screen in the full view. |
| `g`, `Home` | Scroll to the top. |
| `G`, `End` | Scroll to the bottom. |
| `Tab`, `Shift-Tab` | Select the next or previous peer. |
| `Enter`, `e` | Matrix: open or close the selected peer's detail. |
| `v`, `E` | Switch between the matrix and the full text view. |
| `f` | Matrix: cycle the provider filter, through each provider and back to all. |
| `c` | Clear the provider and peer filter. |
| `p` | Pause or resume the screen. The age of the frozen data is shown. |
| `u` | Matrix: switch the gauge between requests and tokens. |
| `w` | Matrix: switch the gauge window between the last hour and the last five minutes. |
| `m` | Matrix: cycle the chart metric. The chart row appears at 28 rows or more. |
| `?` | Show the key list. The next key press closes it and does nothing else. |
| `x` | No action. |

The footer hint line reads `j/k peer`, which is the matrix behaviour above.

Source: `crates/reiki-top/src/ui.rs` and `src/top.rs`.

## ca-init

Creates a certificate authority for `[tunnel.intercept]`. The CA is name-constrained to the hosts you list, so a stolen key cannot sign for anything else.

```
reiki ca-init --dir DIR --host HOST [--host HOST]... [--days N] [--replace]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--dir DIR` | required | Directory that receives `ca.pem` and `ca.key`. Created if missing. |
| `--host HOST` | required, repeatable | A host the CA may sign for. Lowercase DNS name with at least two labels. |
| `--days N` | `365` | Validity in days. Range 1 to 3650. |
| `--replace` | off | Overwrite an existing CA in `--dir`. Clients then need the new `ca.pem`. |

Example:

```
reiki ca-init --dir /etc/reiki/intercept --host api.example.net --days 365
```

```
wrote /etc/reiki/intercept/ca.pem and /etc/reiki/intercept/ca.key; give ca.pem to clients, keep ca.key private
```

`ca.key` is created with mode `0600`. Give clients only `ca.pem`.

Exit codes: `0` on success; `1` on a bad host, a bad `--days` value, or an existing CA without `--replace`.

Then point the config at the files:

```toml
[tunnel.intercept]
ca_cert = "/etc/reiki/intercept/ca.pem"
ca_key = "/etc/reiki/intercept/ca.key"
```

See [tunnel](tunnel.md#interception).
