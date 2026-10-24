# Plugins

Plugins add behaviour to the request path without changing Reiki itself, for example rewriting a body or recording requests. They are compiled into the binary; Reiki never loads code at runtime.

Release binaries include the plugins listed in [Bundled plugins](#bundled-plugins). Building from source lets you choose which are compiled in.

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
- Plugins run in the order they are registered in `src/plugins.rs`, not in the order of the config file.

## Bundled plugins

| Plugin | Feature | What it does |
| --- | --- | --- |
| `requestlog` | `plugin-requestlog` | Logs requests to a file and a live socket. See [requestlog](requestlog.md). |

## What a plugin can see and do

A plugin is trusted code that runs inside the Reiki process.

- Before a key is chosen, `on_request` receives the stream name, the upstream name, the method, the request headers and the request body. It can change the headers and the body, or reject the request with a status and a message. It runs once per request, so every failover attempt sends the same transformed request.
- It sees the client's own headers, including the client token, and the whole request body. It never receives an upstream key: the key is attached after the hooks have run, and any `authorization` header a plugin sets is dropped.
- The request path and query string are not passed to plugins.
- `on_response_head` receives the upstream status and headers. `on_response_chunk` receives each body chunk and may rewrite it.
- Hooks are synchronous and run on the request's task. A hook that blocks is not interrupted, and there is no timeout. Do slow work, such as file writes, on a thread of your own.
- A panic in a hook is caught and logged with the plugin and hook name, and the request carries on. Changes the hook made before it panicked are kept, and the plugin's own state may be left half-updated. A panic in `on_request` counts as the plugin skipping the request, so `on_error` decides whether the request goes on or is answered with 503. Rust's default panic handler also prints the panic message and location to stderr, outside the log settings, so do not `unwrap` or `expect` on request data.
- While any plugin is configured, Reiki streams responses without a `content-length`, so a plugin may change the length of a body. HEAD, 204 and 304 responses keep theirs, as they have no body. When the body ends normally the final `on_response_chunk` call, with `is_last` set, follows the last chunk. It does not happen after an upstream read error or when the client disconnects, so use the per-request state being dropped as the end-of-response signal.
- The request body is limited to 8 MB before the plugins run and again after they have run; a body a plugin makes larger is refused with 413 and the upstream is not called.

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
3. Register it in `builtin()` in `src/plugins.rs`, behind that feature, with its name and its `build` function.

Per-request data goes in the value returned by `Plugin::new_state`, which every hook for that request receives. One plugin instance serves many requests at once, so keep shared data behind its own synchronisation.

Reject a request with `PluginError::Reject(status, message)`; the client receives a JSON error with that status and message. Return `PluginError::Skip(message)` when the plugin does not apply and let `on_error` decide what follows.
