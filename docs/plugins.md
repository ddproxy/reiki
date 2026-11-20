# Plugins

Plugins add behaviour to the request path without changing Reiki itself, for example rewriting a body or recording requests. They are compiled into the binary; Reiki never loads code at runtime.

Release binaries include the plugins listed in [Bundled plugins](#bundled-plugins). Building from source lets you choose which are compiled in, and in what order. To write or build a plugin crate, see [plugin authoring](plugin-authoring.md).

## Use a plugin

A plugin is configured with a `[plugins.<name>]` table:

```toml
[plugins.requestlog]
file = "/var/lib/reiki/requests.jsonl"
```

- A name that is not compiled into this binary is a startup error, so a typo cannot silently do nothing.
- `enabled = false` turns a configured plugin off.
- `streams = ["client-a", "other"]` runs the plugin only for requests on those streams. Without it the plugin runs on every stream. Each name must be a configured stream, or `passthrough` for unauthenticated passthrough traffic. Any other name is a startup error that names the plugin and the stream.
- A plugin limited by `streams` gets its request, attempt and response hooks only for those streams, and its `on_event` calls only for events that name one of them. A cooldown event names no stream, so only plugins without `streams` receive it. `on_admin` is not affected.
- `streams` is a host key, like `enabled`, `on_error` and `dry_run`. The host uses it for scoping and passes the whole table to the plugin. A plugin should accept and ignore host keys.
- `on_error` is `"pass_through"` or `"reject"`. The default is `"pass_through"` unless the plugin fails closed (it returns `true` from `Plugin::fails_closed`, as `placeholders` does because a failure would send the data unmasked), in which case the default is `"reject"`. Setting `"pass_through"` on such a plugin is allowed and logs a warning when that plugin is built. It decides what happens when a plugin reports that it could not apply (a skip): `pass_through` logs it and carries on, `reject` answers the client with 503. A plugin that deliberately rejects a request always rejects, whatever `on_error` says.
- `reiki check` adds `dry_run = true` to every plugin table. A plugin should then validate its settings only, and not open files, sockets or threads, so the check has no side effects and works while Reiki is running.
- A reload rebuilds only the plugins whose table changed or whose watched files changed, and keeps the rest running, with their counters and files. A change to `streams` alone applies live and does not rebuild the plugin, so its counters survive. An unknown plugin name or a bad setting keeps the old config. A plugin with `live_reload() == false` keeps its running file or socket and is reported as needing a restart. See [upgrades](upgrades.md#reload-vs-restart) and [reload](troubleshooting.md#reload).
- Plugins run in the order they are registered at build time (see below), not in the order of the config file. Request hooks run in that order; response hooks run in the reverse order, so the plugin that changed the request first sees the response last and can undo its change before the client gets it.

## Bundled plugins

| Plugin | Feature | What it does |
| --- | --- | --- |
| `phi` | `plugin-phi` | Blocks or reports PHI and NPPI identifiers in outbound requests. See [phi](phi.md). |
| `finance` | `plugin-finance` | Blocks or reports financial identifiers in outbound requests. See [finance](finance.md). |
| `controlled` | `plugin-controlled` | Blocks or reports controlled-information markings (CUI, classification) and label-bound identifiers in outbound requests. See [controlled](controlled.md). |
| `placeholders` | `plugin-placeholders` | Swaps credentials and listed terms for placeholders on the way out and restores them in responses. See [placeholders](placeholders.md). |
| `requestlog` | `plugin-requestlog` | Logs requests to a file and a live socket. See [requestlog](requestlog.md). |
| `metrics` | `plugin-metrics` | Serves Prometheus metrics at `GET /metrics` on the admin listener. See [monitoring](monitoring.md). |

## Tune a detector with `reiki scan`

`reiki scan` runs the `phi`, `finance` and `controlled` detectors over files or stdin without the proxy, so you can see what a plugin would flag before you switch it from `report` to `block`.

```bash
reiki scan export.csv                       # each plugin's default kinds
cat prompt.json | reiki scan --plugin phi   # one plugin, from stdin
reiki scan --kinds ssn,iban dump.sql        # named kinds across plugins
reiki scan --all --json notes.txt           # every kind, one JSON object per finding
```

- Each finding prints as `file:line:col plugin kind`. The matched text is never printed, so the output is safe to paste into a ticket. A summary goes to stderr.
- The exit code is 0 for no findings, 1 for findings and 2 for a usage or read error, so the command also works as a pre-commit or CI check.
- `--kinds` takes labels from any plugin. `--all` adds the kinds that are off by default.
- It reads bytes as UTF-8 text with lossy conversion. The proxy plugins refuse a request body that is not valid UTF-8 instead.

## Report findings with a detail

A detection plugin reports a finding with `RequestCtx::report(kind, count, blocked)`. A plugin that reports per name or per rule, such as a tool allowlist or a lint rule, can add a short label with `RequestCtx::report_detail(kind, detail, count, blocked)`:

```rust
ctx.report_detail("tool", "get_weather", 1, true);
```

The host checks the label before any listener sees it:

- It keeps only the characters `A-Z`, `a-z`, `0-9`, `_`, `.`, `:`, `*`, `/` and `-`. Other characters are removed, not replaced, so a space, a newline or a quote never gets through.
- It cuts the result at 64 bytes.
- It drops the label when nothing is left.

The filter is not a privacy check. A plugin must still never pass matched text, even text that fits the character set.

`Event::Finding` has a `detail: Option<&str>` field with the sanitised label. Plugins that match with `..` need no change.

## Choose and order plugins at build time

Cargo features only switch a plugin's code on, and they carry no order. Order comes from `REIKI_PLUGINS`, an ordered, comma-separated list read by `build.rs`:

```sh
# only these two, in this order
REIKI_PLUGINS=placeholders,requestlog cargo build --release --no-default-features \
  --features plugin-placeholders,plugin-requestlog
```

- Unset, every plugin whose feature is enabled is registered in the order of the catalog in `Cargo.toml` (`[[package.metadata.reiki.plugin]]`). The catalog must be a non-empty array of tables with unique names and non-empty `name`, `feature` and `crate` strings. `crate` is used as a Rust identifier in generated code.
- A name that is not in the catalog, a repeated name, or a plugin whose feature is off fails the build with a message saying what to change.
- Putting `requestlog` before `placeholders` fails the build. The build also warns when `requestlog` has no masking plugin before it, or when `phi`, `finance` or `controlled` follows `placeholders`, because either order weakens what the later plugin can see or mask.
- A plugin left out of the list is not registered, so naming it in `[plugins.*]` is a startup error.

## What a plugin can see and do

A plugin is trusted code that runs inside the Reiki process.

- Before a key is chosen, `on_request` receives the stream name, the upstream name, the method, the request path, the request headers and the request body. It can change the headers and the body, or reject the request with a status and a message. It runs once per request, so every failover attempt sends the same transformed request.
- It sees the client's own headers, including the client token, and the whole request body. It never receives an upstream key: the key is attached after the hooks have run, and on a keyed upstream any `authorization` header a plugin sets is dropped (on a `passthrough` upstream the client's credentials, including one a plugin sets, are forwarded).
- The request path is passed to plugins, the query string is not.
- `on_attempt` runs after each upstream attempt, including the ones that fail over to the next key. It receives the key's name from the config, the upstream status (`None` when no response arrived) and, for an error response, the start of its body. It never receives a key's secret.
- `on_response_head` receives the name of the key that produced the response, the upstream that served it (the fallback upstream after a failover), the upstream status and the headers. `on_response_chunk` receives each body chunk and may rewrite it. `RequestCtx::id` is a 32-hex-digit request id shared with the logs.
- Hooks are synchronous and run on the request's task. A hook that blocks is not interrupted, and there is no timeout. Do slow work, such as file writes, on a thread of your own.
- A panic in `on_request`, `on_attempt`, `on_response_head`, `on_response_chunk`, `on_event`, `on_admin`, `new_state` or `on_shutdown` is caught and logged with the plugin and hook name, and the process carries on. The hook's earlier changes are kept, and the plugin's own state may be half-updated. A `new_state` panic gives that plugin the default `()` state for the request; an `on_shutdown` panic does not stop the other plugins from shutting down.
- A panic in `on_request` counts as the plugin skipping the request, so `on_error` decides whether the request goes on or gets a 503. Rust's panic handler also prints the message to stderr, outside the log settings, so do not `unwrap` or `expect` on request data.
- `on_event` receives facts about the relay rather than one request: a finished request (stream, upstream, final status), a failover from one key to the next, a key going on cooldown, a stream rate-limit rejection, and a request a plugin rejected (`Event::PluginRejected`, which the `metrics` plugin counts). It also receives `Event::Finding` for each finding a plugin reported with `RequestCtx::report` or `report_detail`. It is not tied to a request's state and carries key names, never secrets.
- `admin_paths` and `on_admin` let a plugin answer a GET on the admin listener, after the caller has been authenticated. A path must be absolute with no trailing slash, `.`/`..` segment or `%`. A duplicate path, or one Reiki serves itself (`/status`, `/failures`, `/reload`, `/keys/enable`, their `/v1` forms, `/v1/openapi.json`), is a startup error. The hook receives the current count of available, cooling and out-of-rotation keys.
- A plugin that never changes bodies returns `false` from `uses_bodies`, so configuring it does not change how responses are streamed. An observer returns `false` there and `true` from `needs_response_hooks`.
- While a plugin that uses bodies is configured, Reiki streams responses without a `content-length`, so a plugin may change a body's length. HEAD, 204 and 304 keep theirs because they have no body.
- A final `on_response_chunk` call with `is_last` set follows the last chunk. It also happens on an upstream stream error and when a client disconnects, so make the call safe to repeat.
- The request body is limited to 8 MiB before the plugins run and again after they have run; a body a plugin makes larger is refused with 413 and the upstream is not called.

## Write a plugin

The `Plugin` trait, hook timing, contexts, errors, tests and build-time registration are documented in [plugin authoring](plugin-authoring.md). Plugins are compiled into the binary; Reiki never loads code at runtime.
