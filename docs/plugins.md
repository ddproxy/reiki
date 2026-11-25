# Plugins

Plugins add behaviour to the request path without changing Reiki itself: they can detect identifiers, mask values, rewrite bodies, record requests or price usage. They are compiled into the binary; Reiki never loads native code at runtime. Release binaries include the plugins in [bundled plugins](#bundled-plugins). Building from source chooses the set and order ([build-time order](#choose-and-order-plugins-at-build-time)); to write a plugin crate, see [plugin authoring](plugin-authoring.md).

## Use a plugin

A plugin is configured with a `[plugins.<name>]` table:

```toml
[plugins.requestlog]
file = "/var/lib/reiki/requests.jsonl"
```

- A name not compiled into this binary is a startup error.
- `enabled = false` turns a configured plugin off.
- `streams = ["client-a", "other"]` runs the plugin only for those streams; without it the plugin runs on every stream. Each name must be a configured stream or `passthrough` for unauthenticated traffic. A plugin limited by `streams` receives request, attempt and response hooks only for those streams, and `on_event` only for events naming them; a cooldown event names no stream. `on_admin` is unaffected.
- `streams`, `enabled`, `on_error` and `dry_run` are host keys passed through to the plugin; a plugin should accept and ignore them.
- `on_error` is `pass_through` (default) or `reject`. For a fail-closed plugin such as `placeholders` the default is `reject`. A reported skip or a hook panic follows `on_error`: `pass_through` logs and continues, `reject` answers 503. A deliberate `PluginError::Reject` always rejects.
- `reiki check` adds `dry_run = true`: a plugin should only validate settings, with no files, sockets or threads.
- Reload rebuilds only plugins whose table or watched files changed. A change to `streams` alone applies live without a rebuild. An invalid change keeps the old config; `live_reload() == false` keeps the running instance and reports a restart need. See [reload](troubleshooting.md#reload).
- Hooks run in build-time registration order; response hooks run in reverse, so the plugin that changed the request first sees the response last and can undo its change.

## Bundled plugins

| Plugin | Feature | What it does |
| --- | --- | --- |
| `phi`, `finance`, `controlled` | `plugin-phi`, `plugin-finance`, `plugin-controlled` | Report or block identifiers and markings in outbound request bodies; see [detection plugins](#detection-plugins). |
| `placeholders` | `plugin-placeholders` | Masks credentials and listed terms on the way out and restores them in responses; see [placeholders](#placeholders). |
| `requestlog` | `plugin-requestlog` | Records requests to a file, socket or syslog collector; see [request log](requestlog.md). |
| `metrics` | `plugin-metrics` | Serves Prometheus metrics at `GET /v1/metrics` on the admin listener; see [monitoring](monitoring.md). |
| `toolpolicy` | `plugin-toolpolicy` | Reports or blocks flagged tool calls in model responses; see [tool policy](#tool-policy). |
| `cost` | `plugin-cost` (off by default) | Prices measured token usage from an operator table; see [cost](cost.md). |
| `wasm` runtime | `plugin-wasm` (off by default) | Loads sandboxed Wasm components; see [runtime Wasm plugins](#runtime-wasm-plugins). |

## Detection plugins

`phi`, `finance` and `controlled` check outbound request bodies for structured identifiers and markings. In `block` mode a request with a match is rejected before a key is used; in `report` mode the request goes through and the finding is logged. A match never leaves the plugin: logs and rejections name a kind and a count, never a value. Only the request body is read; headers, the query string and responses are not checked.

```toml
[plugins.phi]
mode = "report"                      # "block" or "report"; required
# kinds = ["ssn", "card", "field"]  # plugin defaults below
# upstreams = ["zen", "other"]      # default: all upstreams
```

| Key | Default | Meaning |
| --- | --- | --- |
| `mode` | required | `block` rejects a matching request with 422; `report` logs and lets it through. |
| `kinds` | plugin-specific | Kinds to look for. |
| `upstreams` | all | Check only requests to these upstreams. |

A body with a `Content-Encoding` other than `identity`, or one that is not valid UTF-8, cannot be read: `block` rejects it with 415 and `report` logs that it was not checked. Each hit writes a `finding` record to the [request log](requestlog.md#findings-from-detection-plugins). In the default build the detectors run before `placeholders` and `requestlog`, so they see the original request and the log sees masked traffic. Run [`reiki scan`](#tune-a-detector-with-reiki-scan) over sample data before choosing a mode.

### PHI and NPPI (`phi`)

Default kinds: all below except `field`, `email` and `phone`.

| Kind | Matches |
| --- | --- |
| `ssn` | `NNN-NN-NNNN` with valid area, group and serial. Nine digits in a row or spaces instead of dashes count only after an `ssn` or `social security` label. |
| `card` | 13 to 19 digits, grouped by single spaces or dashes, with a plausible first digit and valid Luhn checksum. |
| `routing` | Nine digits with a valid ABA checksum after `routing` or `aba`. |
| `account` | Six to 17 digits after `account number`, `account no`, `account #` or `acct`. |
| `dob` | A date after `dob`, `date of birth`, `date_of_birth`, `birth date` or `born on`. |
| `medical_id` | A token with at least four digits after `mrn`, `member id`, `subscriber id`, `policy number` or similar. |
| `npi` | A 10-digit National Provider Identifier with valid checksum after `npi`. |
| `icd10` | An ICD-10 code such as `C50.911` after `icd`, `icd-10` or `diagnosis code`. |
| `column` | A cell under a table header naming an identifying field, such as `first_name`, `address` or `diagnosis`. |
| `field` | Off by default: a field named like an identifying field with a value that reads as data. |
| `email` | Off by default: an email address. |
| `phone` | Off by default: a US phone number with separators or parentheses. |

Table headers are read on the first line after a blank line or a line without that delimiter. Covers delimited text with quoted cells, Markdown tables, SQL `INSERT` with a column list and SQL `COPY ... FROM stdin` blocks. For the `column` kind, at least half of the non-empty cells must be a known column name. A table without a recognised header is checked only by keyword and shape rules; type names, `null` and placeholders are not reported.

### Finance (`finance`)

Default kinds: all below except `account` and `sedol`. This plugin does not price spend; see [cost](cost.md).

| Kind | Matches |
| --- | --- |
| `iban` | Registry country, exact country length and valid mod-97 check; printed spaces between groups allowed. |
| `swift` | 8- or 11-character BIC with ISO country after a `swift`/`bic` label, next to an IBAN, or under a `swift`/`bic` table header. |
| `crypto` | Bitcoin Base58Check, bech32 or bech32m SegWit addresses, or an Ethereum `0x` address; mixed-case Ethereum addresses must satisfy EIP-55. |
| `cusip` | Nine characters with valid check digit after `cusip` or in a `cusip` column. |
| `isin` | Twelve characters with ISO country and Luhn check over the expanded value. |
| `pan_sad` | A Luhn-valid card number next to a `cvv`, `cvc`, `cid` or similar label, or at the start of `%B` track data. |
| `account` | Off by default: six to 17 digits after `account`, `acct`, `beneficiary` or under an `account` header. |
| `sedol` | Off by default: seven characters with valid check digit after a `sedol` label or header. |

Table handling matches the PHI rules above, with headers such as `iban`, `swift`, `bic`, `cusip`, `isin`, `wallet`, `account` and `sedol`. A bare card number is covered by `phi`'s `card` kind, not by `pan_sad`.

### Controlled information (`controlled`)

Default kinds: `cui`, `classification`, `dissemination`.

| Kind | Default | Matches |
| --- | --- | --- |
| `cui` | on | An upper-case `CUI` banner: alone on a line, `CUI//` with a caveat such as `CUI//SP-CTI`, or a `(CUI)` portion marking. |
| `classification` | on | `TOP SECRET`, `SECRET` or `CONFIDENTIAL` followed by a `//` caveat chain; a bare level is not reported. |
| `dissemination` | on | `NOFORN` or `ORCON` in banner or portion-marking shape. |
| `ein` | off | Two digits, hyphen and seven digits after `ein` or `employer identification number`. |
| `itin` | off | `9XX-XX-XXXX` after `itin` or `individual taxpayer identification number`, with historical group ranges. |
| `cage` | off | Five characters from the CAGE alphabet after `cage` or `cage code`. |
| `uei` | off | Twelve characters from the UEI alphabet after `uei` or `unique entity id`. |
| `edipi` | off | Ten digits not starting with zero after `edipi`, `dod id` or `dod_id`. |
| `eccn` | off | A five-character export control number after `eccn`, with optional dot suffix. |

Keyword-bound kinds are off by default because their shapes collide with ordinary codes. A marking in prose, quotes or a JSON string is not a banner.

### What detectors do not cover

- Names, addresses and markings in prose without a field name or banner shape.
- Traffic Reiki cannot read: an opaque CONNECT destination; an allow-list such as `*` lets data leave unchecked. See [tunnel](tunnel.md#trust-boundary).
- Identifiers that are base64-encoded, split across fields or written out in words.
- Pixel and file contents, base64 `image` and `document` blocks, and `thinking`, `signature` and `redacted_thinking` payloads.
- Headers, query strings and responses, and traffic on routes other than the configured `upstreams`.

### Tune a detector with reiki scan

`reiki scan` runs the detectors over files or stdin without the proxy:

```bash
reiki scan export.csv                       # each plugin's default kinds
cat prompt.json | reiki scan --plugin phi   # one plugin, from stdin
reiki scan --kinds ssn,iban dump.sql        # named kinds across plugins
reiki scan --all --json notes.txt           # every kind, one JSON object per finding
```

- Findings print as `file:line:col plugin kind`; matched text is never printed. A summary goes to stderr.
- Exit codes: `0` no findings, `1` findings, `2` usage or read error.
- `--kinds` takes labels from any plugin; `--all` adds kinds that are off by default.
- Input is read as UTF-8 with lossy conversion; the proxy plugins refuse a non-UTF-8 request body. See [cli](cli.md#scan).

## Placeholders

`placeholders` swaps credential-shaped strings and chosen terms for stable placeholders on the way out and restores them in responses. The request log, which runs after it in the default order, records masked traffic.

```toml
[plugins.placeholders]
terms = ["Project Falcon", "Acme Corp"]      # literal terms, any case
terms_file = "/etc/reiki/terms.txt"          # one per line, # for comments
builtin = true                               # also mask credential shapes
rehydrate = true                             # restore originals in responses
max_response_bytes = 8388608                 # longest non-streaming body held
```

| Key | Default | Meaning |
| --- | --- | --- |
| `terms` | none | Literal terms, matched case-insensitively; at least 3 characters, no quote, backslash or control character. The first spelling seen is restored. |
| `terms_file` | none | Absolute path to a newline-delimited term file, re-read when changed on reload. Keep it mode `0600`. |
| `builtin` | true | Also mask common API key prefixes, `Bearer` tokens and JWTs. A PEM private key block is removed and never restored. |
| `rehydrate` | true | Restore originals in responses; with `false` the client sees the placeholders. |
| `restore_tool_args` | false | Also restore credentials inside tool-call arguments; terms are always restored. |
| `max_response_bytes` | 8 MiB | Non-streaming responses larger than this are released unchanged. |

At least one of `terms`, `terms_file` or `builtin` must leave something to mask. A bad value, unknown key or unreadable `terms_file` stops startup; `reiki check` validates the table without reading the file.

A masked value becomes a `[[REIKI_TERM_...]]` or `[[REIKI_SECRET_...]]` marker. The id depends only on the value and a process-lifetime random salt, so a resent history masks to the same ids and the provider's prompt cache keeps working; a reload shares the salt, a restart changes every id. Responses are restored as they stream; JSON bodies are held to the end and only changed string values are rewritten. Reasoning blocks, signatures and base64 `image` or `document` blocks pass byte for byte in both directions. A placeholder split across SSE events is held and completed; an unfinished fragment at stream end is sent as plain text. If the plugin cannot find its per-request state it refuses the request with 503 rather than send unmasked text: `placeholders` fails closed, so the default `on_error` is `reject`. An explicit `pass_through` is an intentional fail-open. See [rollout](#observe-then-enforce) and [Claude Code](clients.md#claude-code).

Limits: this is pattern and list matching, not a data-loss guarantee. Only the request body is scanned as raw UTF-8; headers, paths and query strings are not. Credential detection is heuristic and terms match only exact listed characters (as substrings, case-insensitively). Unscanned blocks and a model that paraphrases instead of repeating a placeholder defeat masking or restoration. A request body with non-`identity` `Content-Encoding` or invalid UTF-8 is refused with 415; a response with non-`identity` encoding is not restored.

## Tool policy

`toolpolicy` inspects tool calls in model responses before the client acts on them. It reads OpenAI Chat (`/chat/completions`) and Anthropic Messages (`/messages`), whole JSON and server-sent events. It is a response-side guard, not a sandbox: a harness can ignore the notice or run tools outside the proxied conversation.

| Rule id | Severity | What matched |
| --- | --- | --- |
| `undeclared_tool` | high | A tool the request did not declare (case-insensitive name match). |
| `unapproved_host` | high | A network-looking call naming a host outside `allow_hosts` and user/system provenance. |
| `proc_environ` | high | A path under `/proc/<pid>/environ` or `/proc/self/environ`. |
| `container_escape` | high | `unshare`, `nsenter`, `chroot` or `mount --bind`. |
| `proxy_bypass` | medium | Proxy variables, `--noproxy`, `*_BASE_URL` overrides or `unset` of proxy settings. |
| `credential_file` | high | Known credential paths such as `~/.ssh`, `.git-credentials`, `.npmrc`, `.aws/credentials` or `/etc/reiki`. |
| `self_protection` | high | Text that would stop, kill, reload or reconfigure Reiki or a protected helper. |

Arguments are checked as written and after shell quote removal. Hosts are read from URLs, bare IPv4/IPv6 addresses, bracketed IPv6 with a port and host-shaped words after a network command; an `allow_hosts` entry is a suffix match, so `example.org` allows `docs.example.org` but not `evil-example.org`. `tool_result` blocks and assistant prose never become provenance. In `block` mode a finding at or above `block_severity` replaces the call with a plain-text notice; the flagged name and arguments are not forwarded.

```toml
[plugins.toolpolicy]
mode = "report"              # "report" (default) or "block"
block_severity = "high"      # "high" (default) or "medium"
allow_hosts = []             # suffix matches; empty by default
check_declared = true
check_hosts = true
check_tripwires = true
journal = "/var/lib/reiki/toolpolicy-findings.jsonl"
journal_max_bytes = 1048576
upstreams = []               # empty means every upstream
```

Bad values fail at startup and name the key. The journal is optional; the plugin holds it and its counters, so changes need a restart. `mode = "block"` fails closed: a hook failure rejects with 503 under the default `on_error`; explicit `pass_through` overrides that and warns at startup.

Text deltas pass through as they arrive; only tool-call fragments are held until arguments are complete. Each call is capped at 64 KiB of arguments and each response at 64 calls. A larger call, invalid JSON arguments, a malformed event or an unparseable recognized request is "unparsed": `block` drops held calls and appends a notice; `report` passes bytes through and increases `reiki_toolpolicy_unparsed_total`. The journal writes mode-`0600` JSON lines with `timestamp`, `stream`, `upstream`, `tool`, `rules`, `severity`, `action` and arguments cut to 2 KiB, with credential-shaped text masked. `GET /v1/toolpolicy/metrics` on the admin listener exports `reiki_toolpolicy_calls_total`, `reiki_toolpolicy_findings_total` and `reiki_toolpolicy_unparsed_total`.

Start in `report` with a journal, watch for false positives, add only genuinely needed hosts to `allow_hosts` and treat a write-capable allowed host as a grant. Then switch to `block` with `block_severity = "high"` and restart; lower it to `medium` only after proxy-bypass findings are understood. The plugin does not stop shell escapes outside its tripwires, encoded payloads it cannot decode, tools that act without a structured call, a client that ignores the notice, hosts mentioned only in tool results, writes to allowed hosts, or bytes already delivered before an unparsed condition was found.

## Observe, then enforce

Add outbound controls in this order: mask known values, watch detections without blocking, scan samples offline, preview the config change, then reject. Each step has a separate proof.

1. Configure `placeholders` (`builtin = true`, `terms` or `terms_file`, `rehydrate = true`) and leave `requestlog` at `body = "summary"` with `capture_bodies = false`.
2. Add each detector with `mode = "report"` and a narrow `upstreams` list where needed. `field`, `email` and `phone` in `phi` match fields and metadata more often than the default kinds.
3. Run `reiki scan --plugin <name> --kinds <list>` over samples; a finding prints no matched text.
4. Preview with `reiki --config FILE reload --dry-run --token-file FILE`. `changed` lists differences, `restart_needed` lists settings waiting for a restart, and a config that fails validation keeps the running state.
5. Switch to `mode = "block"`, preview and reload. A found identifier gets 422 with a `finding` record and no request or response record; an unreadable body gets 415; a plugin skip with `on_error = "reject"` gets 503. Check `/v1/metrics` for `reiki_findings_total` and `reiki_plugin_rejections_total`.
6. Roll back by restoring `mode = "report"` (or removing the table), then preview and reload. Keep the previous config until the proof checks pass.

`report` mode, a narrow `upstreams` list and body-only detection leave traffic unblocked by design. With a detector in `report` mode, leave body capture off or set `body = "none"` so detected identifiers are not written to the request log. See [security](security.md#what-reiki-does-not-protect-against).

## Report findings with a detail

A detection plugin calls `RequestCtx::report(kind, count, blocked)`, or `RequestCtx::report_detail(kind, detail, count, blocked)` for a per-name or per-rule label:

```rust
ctx.report_detail("tool", "get_weather", 1, true);
```

The host keeps only `A-Z`, `a-z`, `0-9`, `_`, `.`, `:`, `*` and `-`, cuts the label at 64 bytes and drops it if nothing remains. This is not a privacy check: a plugin must never pass matched text. `Event::Finding` carries the sanitised `detail`.

## Choose and order plugins at build time

Cargo features switch plugin code on but carry no order. `REIKI_PLUGINS`, read by `build.rs`, is an ordered comma-separated list:

```sh
REIKI_PLUGINS=placeholders,requestlog cargo build --release --no-default-features \
  --features plugin-placeholders,plugin-requestlog
```

- Unset: register every plugin whose feature is enabled, in the catalog order in `Cargo.toml` (`[[package.metadata.reiki.plugin]]`).
- A name that is not in the catalog, a repeated name or a disabled feature fails the build.
- `requestlog` before `placeholders` fails the build. The build warns when no masking plugin precedes `requestlog` or when a detector follows `placeholders`.
- A plugin left out of the list is not registered; naming it in `[plugins.*]` is a startup error.
- The default build registers `phi` before `finance` before `controlled` before `placeholders` before `requestlog`.

## What a plugin can see and do

A plugin is trusted code that runs inside the Reiki process.

- `on_request` runs once per request before key selection, with the stream name, upstream name, method, path, headers and body. It can change headers and body or reject the request; every failover attempt sends the same transformed request. The query string is not passed to plugins.
- It sees the client's own headers, including the client token, and the whole body, but never an upstream key: keys are attached after hooks. On a keyed upstream any `authorization` header a plugin sets is dropped; on `passthrough` it is forwarded.
- `on_attempt` runs after each attempt, including failed ones, with the key's config name, upstream status and the start of an error body, never a key value.
- `on_response_head` and `on_response_chunk` run in reverse order with the serving key name, upstream, status, headers and body chunks. `RequestCtx::id` is a 32-hex-digit id shared with logs.
- Hooks are synchronous. `on_request` runs on Tokio's blocking pool so slow work cannot stall the runtime; other hooks run on the request task with no timeout.
- Panics in request hooks are caught and logged; an `on_request` panic counts as a skip under `on_error`. Panics in `new_state` or response hooks mark the plugin failed for that request: with `reject` the client gets 503, a buffered response is refused and a streamed body is cut. Panics in `on_event`, `on_admin` and `on_shutdown` are only logged; shutdown of other plugins continues. Earlier changes can remain and plugin state can be half-updated.
- `on_event` receives request completion, failover, cooldown, rate-limit rejection, plugin rejection and findings, with key names but never secrets.
- `admin_paths` and `on_admin` serve an authenticated `GET` on the admin listener. Paths must be absolute with no trailing slash, `.`/`..` segment or `%`; reserved admin paths and duplicates are startup errors. `status_fragment` merges JSON into `/v1/status`; `health_gauge` publishes one monotonic count in `/v1/health`.
- A plugin that never changes bodies returns `false` from `uses_bodies` so response streaming and `content-length` stay unchanged. A final `on_response_chunk` with `is_last` follows normal end, upstream error and client disconnect, so make it repeat-safe.
- Request bodies are capped at 8 MiB before and after plugins; a larger body gets 413 and no upstream call.

## Runtime Wasm plugins

Binaries built with `plugin-wasm` load Wasm component plugins from `[plugins.<name>]` tables with `wasm` and `sha256`. Phase 1 exposes request-side hooks, import-derived capabilities, manifest pinning, host-applied JSON edit ops, limits, reload narrowing, inspect and a request-side tool allowlist. Components run in a Wasmtime sandbox with no filesystem, socket or HTTP capability. See [plugin authoring](plugin-authoring.md#runtime-wasm-plugins).

## Write a plugin

The `Plugin` trait, hook timing, contexts, errors, tests, build-time registration and Wasm host are documented in [plugin authoring](plugin-authoring.md).
