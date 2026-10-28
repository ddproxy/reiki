# Plugins

Plugins add behaviour to the request path without changing Reiki itself, for example rewriting a body or recording requests. They are compiled into the binary; Reiki never loads code at runtime.

Release binaries include the plugins listed in [Bundled plugins](#bundled-plugins). Building from source lets you choose which are compiled in, and in what order.

## Use a plugin

A plugin is configured with a `[plugins.<name>]` table:

```toml
[plugins.requestlog]
file = "/var/lib/reiki/requests.jsonl"
```

- A name that is not compiled into this binary is a startup error, so a typo cannot silently do nothing.
- `enabled = false` turns a configured plugin off.
- `on_error` is `"pass_through"` (default) or `"reject"`. It decides what happens when a plugin reports that it could not apply (a skip): `pass_through` logs it and carries on, `reject` answers the client with 503. A plugin that deliberately rejects a request always rejects, whatever `on_error` says.
- `reiki check` adds `dry_run = true` to every plugin table. A plugin should then validate its settings only, and not open files, sockets or threads, so the check has no side effects and works while Reiki is running.
- Plugin settings are read at startup. A SIGHUP reload checks the plugin tables (an unknown plugin name or a bad setting keeps the old config) and logs a warning when they changed, but does not rebuild plugins, so changing a `[plugins.*]` table needs a restart.
- Plugins run in the order they are registered at build time (see below), not in the order of the config file. Request hooks run in that order; response hooks run in the reverse order, so the plugin that changed the request first sees the response last and can undo its change before the client gets it.

## Bundled plugins

| Plugin | Feature | What it does |
| --- | --- | --- |
| `phi` | `plugin-phi` | Blocks or reports PHI and NPPI identifiers in outbound requests. See [phi](phi.md). |
| `finance` | `plugin-finance` | Blocks or reports financial identifiers in outbound requests. See [finance](finance.md). |
| `gov` | `plugin-gov` | Blocks or reports government markings and label-bound identifiers in outbound requests. See [gov](gov.md). |
| `placeholders` | `plugin-placeholders` | Swaps credentials and listed terms for placeholders on the way out and restores them in responses. See [placeholders](placeholders.md). |
| `requestlog` | `plugin-requestlog` | Logs requests to a file and a live socket. See [requestlog](requestlog.md). |
| `metrics` | `plugin-metrics` | Serves Prometheus metrics at `GET /metrics` on the admin listener. See [monitoring](monitoring.md). |

## Tune a detector with `reiki scan`

`reiki scan` runs the `phi`, `finance` and `gov` detectors over files or stdin without the proxy, so you can see what a plugin would flag before you switch it from `report` to `block`.

```bash
reiki scan export.csv                       # each plugin's default kinds
cat prompt.json | reiki scan --plugin phi   # one plugin, from stdin
reiki scan --kinds ssn,iban dump.sql        # named kinds across plugins
reiki scan --all --json notes.txt           # every kind, one JSON object per finding
```

- Each finding prints as `file:line:col plugin kind`. The matched text is never printed, so the output is safe to paste into a ticket. A summary goes to stderr.
- The exit code is 0 for no findings, 1 for findings and 2 for a usage or read error, so the command also works as a pre-commit or CI check.
- `--kinds` takes labels from any plugin. `--all` adds the kinds that are off by default.
- It reads bytes as UTF-8 text with lossy conversion, the same way the plugins read a request body.

## Choose and order plugins at build time

Cargo features only switch a plugin's code on, and they carry no order. Order comes from `REIKI_PLUGINS`, an ordered, comma-separated list read by `build.rs`:

```sh
# only these two, in this order
REIKI_PLUGINS=placeholders,requestlog cargo build --release --no-default-features \
  --features plugin-placeholders,plugin-requestlog
```

- Unset, every plugin whose feature is enabled is registered in the order of the catalog in `Cargo.toml` (`[[package.metadata.reiki.plugin]]`).
- A name that is not in the catalog, a repeated name, or a plugin whose feature is off fails the build with a message saying what to change.
- A plugin left out of the list is not registered, so naming it in `[plugins.*]` is a startup error.

## What a plugin can see and do

A plugin is trusted code that runs inside the Reiki process.

- Before a key is chosen, `on_request` receives the stream name, the upstream name, the method, the request path, the request headers and the request body. It can change the headers and the body, or reject the request with a status and a message. It runs once per request, so every failover attempt sends the same transformed request.
- It sees the client's own headers, including the client token, and the whole request body. It never receives an upstream key: the key is attached after the hooks have run, and on a keyed upstream any `authorization` header a plugin sets is dropped (on a `passthrough` upstream the client's credentials, including one a plugin sets, are forwarded).
- The request path is passed to plugins, the query string is not.
- `on_attempt` runs after each upstream attempt, including the ones that fail over to the next key. It receives the key's name from the config, the upstream status (`None` when no response arrived) and, for an error response, the start of its body. It never receives a key's secret.
- `on_response_head` receives the name of the key that produced the response, the upstream status and the headers. `on_response_chunk` receives each body chunk and may rewrite it.
- Hooks are synchronous and run on the request's task. A hook that blocks is not interrupted, and there is no timeout. Do slow work, such as file writes, on a thread of your own.
- A panic in a hook is caught and logged with the plugin and hook name, and the request carries on. The hook's earlier changes are kept, and the plugin's own state may be half-updated.
- A panic in `on_request` counts as the plugin skipping the request, so `on_error` decides whether the request goes on or gets a 503. Rust's panic handler also prints the message to stderr, outside the log settings, so do not `unwrap` or `expect` on request data.
- `on_event` receives facts about the relay rather than one request: a finished request (stream, upstream, final status), a failover from one key to the next, a key going on cooldown, and a stream rate-limit rejection. It is not tied to a request's state and carries key names, never secrets.
- `admin_paths` and `on_admin` let a plugin answer a GET on the admin listener, after the caller has been authenticated. Two plugins claiming one path, or a plugin claiming `/status`, is a startup error. The hook receives the current count of available, cooling and out-of-rotation keys.
- A plugin that never touches bodies returns `false` from `uses_bodies`, so configuring it does not change how responses are streamed.
- While a plugin that uses bodies is configured, Reiki streams responses without a `content-length`, so a plugin may change a body's length. HEAD, 204 and 304 keep theirs because they have no body.
- When a body ends normally, a final `on_response_chunk` call with `is_last` set follows the last chunk. It does not happen after an upstream read error or a client disconnect, so treat the per-request state being dropped as the end-of-response signal.
- The request body is limited to 8 MiB before the plugins run and again after they have run; a body a plugin makes larger is refused with 413 and the upstream is not called.

## Write a plugin

A plugin implements the `Plugin` trait from the `reiki-plugin` crate in this repository. The crate is not published; depend on it by path. The error and context types are `#[non_exhaustive]`, so new fields and variants can be added without breaking plugins; Reiki builds the contexts, and tests can use their `new` functions.

```rust
use reiki_plugin::{Plugin, PluginError, RequestCtx};

pub struct Tag;

impl Plugin for Tag {
    fn name(&self) -> &str {
        "tag"
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        ctx.headers.insert("x-tag", http::HeaderValue::from_static("1"));
        Ok(())
    }
}

pub fn build(_table: &toml::Table) -> Result<Box<dyn Plugin>, String> {
    Ok(Box::new(Tag))
}
```

To compile it in:

1. Add the crate to the workspace and as an optional dependency of `reiki`.
2. Add a feature of the same name that enables it, for example `plugin-tag = ["dep:reiki-tag"]`.
3. Add it to the catalog in `Cargo.toml`, in default chain order. The crate must export `NAME` and `build`:

   ```toml
   [[package.metadata.reiki.plugin]]
   name = "tag"
   feature = "plugin-tag"
   crate = "reiki_tag"
   ```

Per-request data goes in the value returned by `Plugin::new_state`, which every hook for that request receives. One plugin instance serves many requests at once, so keep shared data behind its own synchronisation.

Reject a request with `PluginError::Reject(status, message)`; the client receives a JSON error with that status and message. Return `PluginError::Skip(message)` when the plugin does not apply and let `on_error` decide what follows.
