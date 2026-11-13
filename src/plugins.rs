//! Assembles the compiled-in plugins named in the config into a [`PluginChain`].
//!
//! Plugins are compiled in, never loaded at runtime. [`builtin`] lists the ones
//! this binary contains; their crate features are enabled when reiki is built.
//! A plugin named in `[plugins.*]` but absent from [`builtin`] is a config
//! error, so a typo fails at startup instead of silently doing nothing.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use toml::Value;

use reiki_plugin::{
    AdminCtx, AdminReply, AttemptCtx, ChunkCtx, Event, KeyCounts, Plugin, PluginError, RequestCtx,
    ResponseHeadCtx,
};

use crate::config::ConfigError;

const RESERVED_ADMIN_PATHS: [&str; 1] = ["/status"];

/// What to do when a plugin declines a request with [`PluginError::Skip`].
/// [`PluginError::Reject`] always rejects regardless of this setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    /// Log and continue with the remaining plugins, leaving the request alone.
    PassThrough,
    /// Stop and return 503 to the client.
    Reject,
}

/// A request the chain stopped before it reached an upstream.
#[derive(Debug)]
pub struct Blocked {
    pub status: StatusCode,
    pub message: String,
}

/// One compiled-in plugin and how to build it from its config table.
pub struct Registration {
    /// The `[plugins.<name>]` key this registers.
    pub name: &'static str,
    /// Parses the plugin's table. The table is passed through untouched, so the
    /// plugin sees every key, including `enabled` and `on_error`.
    pub build: fn(&toml::Table) -> Result<Box<dyn Plugin>, String>,
}

struct Entry {
    plugin: Arc<dyn Plugin>,
    on_error: OnError,
    /// The `[plugins.<name>]` table the instance was built from, to tell on a reload whether it changed.
    table: toml::Table,
    /// The files the plugin reads, as they were when it was built.
    stamps: Vec<(PathBuf, Option<FileStamp>)>,
    /// Streams the plugin runs on; `None` runs it on every stream.
    streams: Option<Vec<String>>,
}

/// Enough of a file's identity to notice an edit: its modification time and size.
pub(crate) type FileStamp = (std::time::SystemTime, u64);

pub(crate) fn stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn stamps_of(plugin: &dyn Plugin) -> Vec<(PathBuf, Option<FileStamp>)> {
    plugin
        .watched_files()
        .into_iter()
        .map(|path| {
            let seen = stamp(&path);
            (path, seen)
        })
        .collect()
}

impl Entry {
    /// True when a file the plugin reads differs from what it saw at build time.
    fn files_changed(&self) -> bool {
        self.stamps.iter().any(|(path, seen)| stamp(path) != *seen)
    }

    /// Whether the plugin runs for a request on `stream`; a stream-less event reaches only unscoped plugins.
    fn runs_for(&self, stream: Option<&str>) -> bool {
        match (&self.streams, stream) {
            (None, _) => true,
            (Some(names), Some(stream)) => names.iter().any(|name| name == stream),
            (Some(_), None) => false,
        }
    }

    /// The same plugin instance, for the next chain, limited to `streams`.
    fn share(&self, streams: Option<Vec<String>>) -> Self {
        Self {
            plugin: Arc::clone(&self.plugin),
            on_error: self.on_error,
            table: self.table.clone(),
            stamps: self.stamps.clone(),
            streams,
        }
    }
}

/// The streams a plugin is limited to, or `None` when it runs on every stream.
pub(crate) fn streams_of(
    name: &str,
    table: &toml::Table,
) -> Result<Option<Vec<String>>, ConfigError> {
    let err = |message: &str| ConfigError::PluginConfig {
        name: name.to_owned(),
        message: message.to_owned(),
    };
    let items = match table.get("streams") {
        None => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(other) => {
            return Err(err(&format!(
                "`streams` must be a list of stream names, got {}",
                other.type_str()
            )))
        }
    };
    if items.is_empty() {
        return Err(err("`streams` must name at least one stream"));
    }
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Value::String(stream) if !stream.is_empty() => names.push(stream.clone()),
            Value::String(_) => return Err(err("`streams` must not contain an empty name")),
            other => {
                return Err(err(&format!(
                    "`streams` must list stream names as strings, got {}",
                    other.type_str()
                )))
            }
        }
    }
    Ok(Some(names))
}

/// True when two tables differ only in `streams`, which applies without a rebuild.
fn same_settings(a: &toml::Table, b: &toml::Table) -> bool {
    let without_streams = |table: &toml::Table| {
        let mut table = table.clone();
        table.remove("streams");
        table
    };
    without_streams(a) == without_streams(b)
}

/// The stream an event belongs to; a cooldown is about a key, so it has none.
fn event_stream<'a>(event: &Event<'a>) -> Option<&'a str> {
    match event {
        Event::RequestDone { stream, .. }
        | Event::Failover { stream, .. }
        | Event::RateLimited { stream }
        | Event::Finding { stream, .. } => Some(*stream),
        _ => None,
    }
}

/// A rebuilt chain and what the rebuild changed.
pub struct Rebuilt {
    pub chain: PluginChain,
    /// Plugins that were added, removed or rebuilt with new settings.
    pub changed: Vec<String>,
    /// Plugins whose settings changed but cannot be applied to a running instance.
    pub restart_needed: Vec<String>,
}

/// Who is asking and for what, as the request hooks see it.
pub struct RequestMeta<'a> {
    /// The id this request carries in Reiki's logs.
    pub id: &'a str,
    pub stream: &'a str,
    pub upstream: &'a str,
    pub method: &'a Method,
    /// The request path without its query string.
    pub path: &'a str,
}

/// The configured plugins in chain order, run on every request.
#[derive(Default)]
pub struct PluginChain {
    entries: Vec<Entry>,
}

impl fmt::Debug for PluginChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

impl PluginChain {
    /// Builds the chain from the `[plugins.*]` tables and the compiled-in
    /// registry. Names must be known; `enabled = false` drops a plugin without
    /// it being an error.
    pub fn build(
        configured: &BTreeMap<String, toml::Table>,
        registry: &[Registration],
    ) -> Result<Self, ConfigError> {
        for name in configured.keys() {
            if !registry.iter().any(|r| r.name == name) {
                return Err(ConfigError::UnknownPlugin {
                    name: name.clone(),
                    known: registry.iter().map(|r| r.name.to_string()).collect(),
                });
            }
        }

        let mut entries = Vec::new();
        for reg in registry {
            let Some(table) = configured.get(reg.name) else {
                continue;
            };
            if !enabled(reg.name, table)? {
                continue;
            }
            let on_error = on_error(reg.name, table)?;
            let streams = streams_of(reg.name, table)?;
            let plugin = (reg.build)(table).map_err(|message| ConfigError::PluginConfig {
                name: reg.name.to_string(),
                message,
            })?;
            let plugin: Arc<dyn Plugin> = Arc::from(plugin);
            entries.push(Entry {
                stamps: stamps_of(plugin.as_ref()),
                plugin,
                on_error,
                table: table.clone(),
                streams,
            });
        }
        Self::check_admin_paths(&entries)?;
        Ok(Self { entries })
    }

    /// Builds the next chain for a reload. A plugin whose table did not change keeps its running
    /// instance, so open files, sockets and counters survive. A changed plugin is built again,
    /// unless it reports `live_reload() == false`: then it keeps running and is listed in
    /// `restart_needed`. Requests already in flight keep the chain they started with.
    pub fn rebuild(
        &self,
        configured: &BTreeMap<String, toml::Table>,
        registry: &[Registration],
    ) -> Result<Rebuilt, ConfigError> {
        self.rebuild_with(configured, registry, true)
    }

    /// What [`PluginChain::rebuild`] would change, without building anything: no plugin is
    /// constructed, so nothing opens a file or a socket. The returned chain is empty.
    pub fn plan(
        &self,
        configured: &BTreeMap<String, toml::Table>,
        registry: &[Registration],
    ) -> Result<Rebuilt, ConfigError> {
        self.rebuild_with(configured, registry, false)
    }

    fn rebuild_with(
        &self,
        configured: &BTreeMap<String, toml::Table>,
        registry: &[Registration],
        build: bool,
    ) -> Result<Rebuilt, ConfigError> {
        for name in configured.keys() {
            if !registry.iter().any(|r| r.name == name) {
                return Err(ConfigError::UnknownPlugin {
                    name: name.clone(),
                    known: registry.iter().map(|r| r.name.to_string()).collect(),
                });
            }
        }
        let mut entries = Vec::new();
        let mut changed = Vec::new();
        let mut restart_needed = Vec::new();
        for reg in registry {
            let old = self.entries.iter().find(|e| e.plugin.name() == reg.name);
            let wanted = match configured.get(reg.name) {
                Some(table) if enabled(reg.name, table)? => Some(table),
                _ => None,
            };
            let streams = match wanted {
                Some(table) => streams_of(reg.name, table)?,
                None => None,
            };
            match (old, wanted) {
                (None, None) => {}
                (Some(_), None) => changed.push(reg.name.to_owned()),
                (Some(old), Some(table))
                    if same_settings(&old.table, table) && !old.files_changed() =>
                {
                    if old.streams != streams {
                        changed.push(reg.name.to_owned());
                    }
                    entries.push(old.share(streams));
                }
                (Some(old), Some(_)) if !old.plugin.live_reload() => {
                    restart_needed.push(reg.name.to_owned());
                    if old.streams != streams {
                        changed.push(reg.name.to_owned());
                    }
                    entries.push(old.share(streams));
                }
                (_, Some(table)) => {
                    let on_error = on_error(reg.name, table)?;
                    changed.push(reg.name.to_owned());
                    // A preview builds a plugin that is already running (so it is safe to build
                    // again) and drops it, so a bad file path fails the preview as it would the reload.
                    if build || old.is_some() {
                        let plugin =
                            (reg.build)(table).map_err(|message| ConfigError::PluginConfig {
                                name: reg.name.to_string(),
                                message,
                            })?;
                        let plugin: Arc<dyn Plugin> = Arc::from(plugin);
                        if !build {
                            continue;
                        }
                        entries.push(Entry {
                            stamps: stamps_of(plugin.as_ref()),
                            plugin,
                            on_error,
                            table: table.clone(),
                            streams,
                        });
                    }
                }
            }
        }
        if build {
            Self::check_admin_paths(&entries)?;
        }
        Ok(Rebuilt {
            chain: Self { entries },
            changed,
            restart_needed,
        })
    }

    /// Starts a request's plugin session: this chain and fresh per-request state for each plugin.
    pub fn session(self: &Arc<Self>) -> PluginSession {
        PluginSession {
            chain: Arc::clone(self),
            states: self.states(),
            scope: self.scope(None),
        }
    }

    /// Which plugins run for a request on `stream`, in chain order.
    fn scope(&self, stream: Option<&str>) -> Vec<bool> {
        self.entries.iter().map(|e| e.runs_for(stream)).collect()
    }

    fn check_admin_paths(entries: &[Entry]) -> Result<(), ConfigError> {
        let mut claimed: Vec<(&'static str, &str)> = Vec::new();
        for entry in entries {
            for path in entry.plugin.admin_paths() {
                if let Some((_, other)) = claimed.iter().find(|(p, _)| *p == path) {
                    return Err(ConfigError::PluginConfig {
                        name: entry.plugin.name().to_owned(),
                        message: format!("admin path {path} is already served by {other}"),
                    });
                }
                if RESERVED_ADMIN_PATHS.contains(&path) {
                    return Err(ConfigError::PluginConfig {
                        name: entry.plugin.name().to_owned(),
                        message: format!("admin path {path} is served by reiki itself"),
                    });
                }
                claimed.push((path, entry.plugin.name()));
            }
        }
        Ok(())
    }

    /// True when no plugin touches bodies, so the body path skips the per-request work.
    pub fn is_empty(&self) -> bool {
        !self.entries.iter().any(|e| e.plugin.uses_bodies())
    }

    /// Delivers a relay-level event to every plugin; a panicking hook is logged and skipped.
    pub fn on_event(&self, event: &Event<'_>) {
        let stream = event_stream(event);
        for entry in self.entries.iter().filter(|e| e.runs_for(stream)) {
            let _ = guard(entry.plugin.name(), "on_event", || {
                entry.plugin.on_event(event);
            });
        }
    }

    /// Answers an admin GET from the plugin that claimed `path`.
    pub fn on_admin(&self, path: &str, keys: KeyCounts) -> Option<AdminReply> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.plugin.admin_paths().contains(&path))?;
        let ctx = AdminCtx::new(path, keys);
        guard(entry.plugin.name(), "on_admin", || {
            entry.plugin.on_admin(&ctx)
        })
        .flatten()
    }

    /// Every path an active plugin answers on the admin listener.
    pub fn admin_paths(&self) -> Vec<&'static str> {
        self.entries
            .iter()
            .flat_map(|e| e.plugin.admin_paths())
            .collect()
    }

    /// Names of the active plugins, for startup logging.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.plugin.name())
    }

    /// Every file a plugin in this chain reads, for the watcher.
    pub fn watched_paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .flat_map(|e| e.stamps.iter().map(|(path, _)| path.clone()))
            .collect()
    }

    /// Creates the per-request state for every plugin, in chain order.
    pub fn states(&self) -> Vec<Box<dyn Any + Send>> {
        self.entries.iter().map(|e| e.plugin.new_state()).collect()
    }

    /// Runs `on_request` on every plugin, in order, mutating the request in
    /// place. Stops at the first rejection. A panicking hook counts as a skip.
    pub fn on_request(
        &self,
        states: &mut [Box<dyn Any + Send>],
        meta: &RequestMeta<'_>,
        headers: &mut HeaderMap,
        body: &mut Bytes,
    ) -> Result<(), Blocked> {
        for (entry, state) in self.entries.iter().zip(states.iter_mut()) {
            if !entry.runs_for(Some(meta.stream)) {
                continue;
            }
            let mut ctx = RequestCtx::new(
                meta.stream,
                meta.upstream,
                meta.method,
                meta.path,
                &mut *headers,
                &mut *body,
                &mut **state,
            )
            .with_id(meta.id);
            let result = guard(entry.plugin.name(), "on_request", || {
                entry.plugin.on_request(&mut ctx)
            })
            .unwrap_or_else(|| Err(PluginError::Skip("plugin panicked".to_owned())));
            for found in std::mem::take(&mut ctx.reported) {
                let detail = found.detail.as_deref().and_then(sanitize_detail);
                self.on_event(&Event::Finding {
                    id: meta.id,
                    stream: meta.stream,
                    plugin: entry.plugin.name(),
                    kind: found.kind,
                    detail: detail.as_deref(),
                    count: found.count as u64,
                    blocked: found.blocked,
                });
            }
            match result {
                Ok(()) => {}
                Err(PluginError::Reject(status, message)) => {
                    tracing::warn!(
                        plugin = entry.plugin.name(),
                        status = status.as_u16(),
                        %message,
                        "plugin rejected the request"
                    );
                    return Err(Blocked { status, message });
                }
                Err(other) => {
                    let message = match other {
                        PluginError::Skip(message) => message,
                        unknown => unknown.to_string(),
                    };
                    match entry.on_error {
                        OnError::PassThrough => {
                            tracing::debug!(
                                plugin = entry.plugin.name(),
                                %message,
                                "plugin skipped the request"
                            );
                        }
                        OnError::Reject => {
                            tracing::warn!(
                                plugin = entry.plugin.name(),
                                %message,
                                "plugin skipped the request; rejecting per on_error"
                            );
                            return Err(Blocked {
                                status: StatusCode::SERVICE_UNAVAILABLE,
                                message,
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Runs `on_attempt` on every plugin, in order. A panicking hook is logged
    /// and skipped.
    pub fn on_attempt(
        &self,
        states: &mut [Box<dyn Any + Send>],
        scope: &[bool],
        key: &str,
        status: Option<u16>,
        body: &[u8],
    ) {
        for ((entry, state), runs) in self
            .entries
            .iter()
            .zip(states.iter_mut())
            .zip(scope.iter().copied())
        {
            if !runs {
                continue;
            }
            let mut ctx = AttemptCtx::new(key, status, body, &mut **state);
            let _ = guard(entry.plugin.name(), "on_attempt", || {
                entry.plugin.on_attempt(&mut ctx);
            });
        }
    }

    /// Runs `on_response_head` on every plugin, in reverse order, so the plugin that
    /// touched the request first sees the response last. A panicking hook is logged
    /// and skipped.
    pub fn on_response_head(
        &self,
        states: &mut [Box<dyn Any + Send>],
        scope: &[bool],
        key: &str,
        status: &mut StatusCode,
        headers: &mut HeaderMap,
    ) {
        for ((entry, state), runs) in self
            .entries
            .iter()
            .zip(states.iter_mut())
            .zip(scope.iter().copied())
            .rev()
        {
            if !runs {
                continue;
            }
            let mut ctx = ResponseHeadCtx::new(key, &mut *status, &mut *headers, &mut **state);
            let _ = guard(entry.plugin.name(), "on_response_head", || {
                entry.plugin.on_response_head(&mut ctx);
            });
        }
    }

    /// Runs `on_response_chunk` on every plugin, in reverse order, letting each rewrite
    /// `chunk` before the next sees it. A panicking hook is logged and skipped.
    pub fn on_response_chunk(
        &self,
        states: &mut [Box<dyn Any + Send>],
        scope: &[bool],
        chunk: &mut Bytes,
        is_last: bool,
    ) {
        for ((entry, state), runs) in self
            .entries
            .iter()
            .zip(states.iter_mut())
            .zip(scope.iter().copied())
            .rev()
        {
            if !runs {
                continue;
            }
            let mut ctx = ChunkCtx::new(&mut *chunk, is_last, &mut **state);
            let _ = guard(entry.plugin.name(), "on_response_chunk", || {
                entry.plugin.on_response_chunk(&mut ctx);
            });
        }
    }
}

/// One request's plugin chain and per-plugin state. It owns the chain it started with, so a reload
/// that swaps the chain mid-request cannot pair the request's state with the wrong plugins.
pub struct PluginSession {
    chain: Arc<PluginChain>,
    states: Vec<Box<dyn Any + Send>>,
    /// Which plugins run for this request, set by `on_request`; only unscoped plugins until then.
    scope: Vec<bool>,
}

impl Default for PluginSession {
    fn default() -> Self {
        Self {
            chain: Arc::new(PluginChain::default()),
            states: Vec::new(),
            scope: Vec::new(),
        }
    }
}

impl PluginSession {
    /// True when no plugin in this session touches bodies.
    pub fn is_empty(&self) -> bool {
        self.chain.is_empty()
    }

    /// The chain this session runs.
    pub fn chain(&self) -> &Arc<PluginChain> {
        &self.chain
    }

    pub fn on_request(
        &mut self,
        meta: &RequestMeta<'_>,
        headers: &mut HeaderMap,
        body: &mut Bytes,
    ) -> Result<(), Blocked> {
        self.scope = self.chain.scope(Some(meta.stream));
        self.chain.on_request(&mut self.states, meta, headers, body)
    }

    pub fn on_attempt(&mut self, key: &str, status: Option<u16>, body: &[u8]) {
        self.chain
            .on_attempt(&mut self.states, &self.scope, key, status, body);
    }

    pub fn on_response_head(
        &mut self,
        key: &str,
        status: &mut StatusCode,
        headers: &mut HeaderMap,
    ) {
        self.chain
            .on_response_head(&mut self.states, &self.scope, key, status, headers);
    }

    pub fn on_response_chunk(&mut self, chunk: &mut Bytes, is_last: bool) {
        self.chain
            .on_response_chunk(&mut self.states, &self.scope, chunk, is_last);
    }
}

/// Longest finding detail the host forwards, in bytes.
const DETAIL_MAX_BYTES: usize = 64;

/// Keeps only `[A-Za-z0-9_.:*/-]`, cuts the result at [`DETAIL_MAX_BYTES`] and drops an empty
/// result. The filter runs here, not in each plugin, so no plugin can send free text as a detail.
fn sanitize_detail(raw: &str) -> Option<String> {
    let mut kept: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '*' | '/' | '-'))
        .collect();
    kept.truncate(DETAIL_MAX_BYTES);
    (!kept.is_empty()).then_some(kept)
}

/// Runs a plugin hook, turning a panic into a logged `None` so one faulty plugin cannot take the request task down.
fn guard<T>(plugin: &str, hook: &str, run: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::error!(plugin, hook, "plugin panicked; the hook was abandoned");
            None
        }
    }
}

/// The plugin tables with `dry_run = true` added, for `reiki check`, so plugins validate
/// their settings without opening files or sockets.
pub fn with_dry_run(configured: &BTreeMap<String, toml::Table>) -> BTreeMap<String, toml::Table> {
    configured
        .iter()
        .map(|(name, table)| {
            let mut table = table.clone();
            table.insert("dry_run".to_owned(), Value::Boolean(true));
            (name.clone(), table)
        })
        .collect()
}

fn enabled(name: &str, table: &toml::Table) -> Result<bool, ConfigError> {
    match table.get("enabled") {
        None => Ok(true),
        Some(Value::Boolean(value)) => Ok(*value),
        Some(other) => Err(ConfigError::PluginConfig {
            name: name.to_owned(),
            message: format!("`enabled` must be a boolean, got {}", other.type_str()),
        }),
    }
}

fn on_error(name: &str, table: &toml::Table) -> Result<OnError, ConfigError> {
    match table.get("on_error") {
        None => Ok(OnError::PassThrough),
        Some(Value::String(value)) => match value.as_str() {
            "pass_through" => Ok(OnError::PassThrough),
            "reject" => Ok(OnError::Reject),
            other => Err(ConfigError::PluginConfig {
                name: name.to_owned(),
                message: format!(
                    "`on_error` must be \"pass_through\" or \"reject\", got \"{other}\""
                ),
            }),
        },
        Some(other) => Err(ConfigError::PluginConfig {
            name: name.to_owned(),
            message: format!("`on_error` must be a string, got {}", other.type_str()),
        }),
    }
}

/// The plugins compiled into this binary, in chain order.
///
/// Generated by `build.rs` from `[[package.metadata.reiki.plugin]]` in `Cargo.toml`. Set
/// `REIKI_PLUGINS=a,b` when building to choose which are registered and in what order.
pub fn builtin() -> Vec<Registration> {
    include!(concat!(env!("OUT_DIR"), "/builtin_plugins.rs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::CONTENT_LENGTH;
    use std::any::Any;

    #[test]
    fn placeholders_are_compiled_before_the_request_log() {
        let registry = builtin();
        let names: Vec<&str> = registry
            .iter()
            .map(|registration| registration.name)
            .collect();
        let placeholders = names.iter().position(|name| *name == "placeholders");
        let requestlog = names.iter().position(|name| *name == "requestlog");
        if let (Some(placeholders), Some(requestlog)) = (placeholders, requestlog) {
            assert!(
                placeholders < requestlog,
                "compiled plugin order is {names:?}"
            );
        }
    }

    struct Append(&'static str);
    impl Plugin for Append {
        fn name(&self) -> &str {
            "append"
        }
        fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
            let mut body = ctx.body.to_vec();
            body.extend_from_slice(self.0.as_bytes());
            *ctx.body = Bytes::from(body);
            Ok(())
        }
    }

    struct Swap {
        placeholder: &'static str,
        value: &'static str,
    }
    impl Plugin for Swap {
        fn name(&self) -> &str {
            "swap"
        }
        fn new_state(&self) -> Box<dyn Any + Send> {
            Box::new(Vec::<u8>::new())
        }
        fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
            let text = String::from_utf8_lossy(ctx.body).replace(self.value, self.placeholder);
            *ctx.body = Bytes::from(text);
            Ok(())
        }
        fn on_response_head(&self, ctx: &mut ResponseHeadCtx<'_>) {
            ctx.headers.remove(CONTENT_LENGTH);
        }
        fn on_response_chunk(&self, ctx: &mut ChunkCtx<'_>) {
            let buffer = ctx.state.downcast_mut::<Vec<u8>>().unwrap();
            buffer.extend_from_slice(&ctx.chunk[..]);
            if ctx.is_last {
                let text = String::from_utf8_lossy(buffer).replace(self.placeholder, self.value);
                *ctx.chunk = Bytes::from(text);
                buffer.clear();
            } else {
                *ctx.chunk = Bytes::new();
            }
        }
    }

    struct Decline;
    impl Plugin for Decline {
        fn name(&self) -> &str {
            "decline"
        }
        fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
            Err(PluginError::Skip("no secret for this request".into()))
        }
    }

    struct Refuse;
    impl Plugin for Refuse {
        fn name(&self) -> &str {
            "refuse"
        }
        fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
            Err(PluginError::Reject(
                StatusCode::FORBIDDEN,
                "not allowed".into(),
            ))
        }
    }

    fn registry() -> Vec<Registration> {
        vec![
            Registration {
                name: "swap",
                build: |_| {
                    Ok(Box::new(Swap {
                        placeholder: "<PLACEHOLDER>",
                        value: "s3cr3t",
                    }) as Box<dyn Plugin>)
                },
            },
            Registration {
                name: "append",
                build: |_| Ok(Box::new(Append("-tail")) as Box<dyn Plugin>),
            },
            Registration {
                name: "decline",
                build: |_| Ok(Box::new(Decline) as Box<dyn Plugin>),
            },
            Registration {
                name: "refuse",
                build: |_| Ok(Box::new(Refuse) as Box<dyn Plugin>),
            },
        ]
    }

    fn configured(source: &str) -> BTreeMap<String, toml::Table> {
        toml::from_str(source).unwrap()
    }

    fn request(
        chain: &PluginChain,
        states: &mut [Box<dyn Any + Send>],
        body: &mut Bytes,
    ) -> Result<(), Blocked> {
        let mut headers = HeaderMap::new();
        chain.on_request(
            states,
            &RequestMeta {
                id: "test",
                stream: "default",
                upstream: "zen",
                method: &Method::POST,
                path: "/zen/chat/completions",
            },
            &mut headers,
            body,
        )
    }

    struct Sticky;
    impl Plugin for Sticky {
        fn name(&self) -> &str {
            "sticky"
        }
        fn live_reload(&self) -> bool {
            false
        }
    }

    fn rebuild_registry() -> Vec<Registration> {
        vec![
            Registration {
                name: "append",
                build: |_| Ok(Box::new(Append("-tail")) as Box<dyn Plugin>),
            },
            Registration {
                name: "sticky",
                build: |_| Ok(Box::new(Sticky) as Box<dyn Plugin>),
            },
        ]
    }

    fn same_instance(a: &PluginChain, b: &PluginChain, name: &str) -> bool {
        let find = |c: &PluginChain| {
            c.entries
                .iter()
                .find(|e| e.plugin.name() == name)
                .map(|e| Arc::clone(&e.plugin))
        };
        match (find(a), find(b)) {
            (Some(x), Some(y)) => Arc::ptr_eq(&x, &y),
            _ => false,
        }
    }

    #[test]
    fn a_rebuild_reuses_unchanged_plugins_and_rebuilds_changed_ones() {
        let registry = rebuild_registry();
        let first =
            PluginChain::build(&configured("[append]\nn = 1\n[sticky]\n"), &registry).unwrap();

        let same = first
            .rebuild(&configured("[append]\nn = 1\n[sticky]\n"), &registry)
            .unwrap();
        assert!(same.changed.is_empty() && same.restart_needed.is_empty());
        assert!(same_instance(&first, &same.chain, "append"));
        assert!(same_instance(&first, &same.chain, "sticky"));

        let edited = first
            .rebuild(&configured("[append]\nn = 2\n[sticky]\n"), &registry)
            .unwrap();
        assert_eq!(edited.changed, ["append"]);
        assert!(!same_instance(&first, &edited.chain, "append"), "rebuilt");
        assert!(same_instance(&first, &edited.chain, "sticky"), "untouched");

        let dropped = first.rebuild(&configured("[sticky]\n"), &registry).unwrap();
        assert_eq!(dropped.changed, ["append"]);
        assert_eq!(dropped.chain.names().collect::<Vec<_>>(), ["sticky"]);

        let disabled = first
            .rebuild(
                &configured("[append]\nenabled = false\n[sticky]\n"),
                &registry,
            )
            .unwrap();
        assert_eq!(disabled.changed, ["append"]);

        let added = dropped
            .chain
            .rebuild(&configured("[append]\n[sticky]\n"), &registry)
            .unwrap();
        assert_eq!(added.changed, ["append"]);
        assert_eq!(
            added.chain.names().collect::<Vec<_>>(),
            ["append", "sticky"]
        );
    }

    #[test]
    fn a_plugin_that_cannot_reload_live_keeps_running_and_asks_for_a_restart() {
        let registry = rebuild_registry();
        let first = PluginChain::build(&configured("[sticky]\nsize = 1\n"), &registry).unwrap();
        let next = first
            .rebuild(&configured("[sticky]\nsize = 2\n"), &registry)
            .unwrap();
        assert_eq!(next.restart_needed, ["sticky"]);
        assert!(next.changed.is_empty());
        assert!(
            same_instance(&first, &next.chain, "sticky"),
            "the running instance stays"
        );
    }

    #[test]
    fn a_rebuild_with_an_unknown_plugin_or_a_bad_table_changes_nothing() {
        let registry = rebuild_registry();
        let first = PluginChain::build(&configured("[append]\n"), &registry).unwrap();
        assert!(first.rebuild(&configured("[nope]\n"), &registry).is_err());
        assert!(first
            .rebuild(
                &configured("[append]\non_error = \"sideways\"\n"),
                &registry
            )
            .is_err());
        assert_eq!(first.names().collect::<Vec<_>>(), ["append"]);
    }

    struct Watcher(PathBuf);
    impl Plugin for Watcher {
        fn name(&self) -> &str {
            "watcher"
        }
        fn watched_files(&self) -> Vec<PathBuf> {
            vec![self.0.clone()]
        }
    }

    #[test]
    fn a_changed_watched_file_rebuilds_the_plugin_even_when_its_table_is_the_same() {
        static FILE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        let file = FILE.get_or_init(|| {
            std::env::temp_dir().join(format!("reiki-watch-{}.txt", std::process::id()))
        });
        std::fs::write(file, "one").unwrap();
        let registry = vec![Registration {
            name: "watcher",
            build: |_| Ok(Box::new(Watcher(FILE.get().unwrap().clone())) as Box<dyn Plugin>),
        }];
        let table = configured("[watcher]\n");
        let first = PluginChain::build(&table, &registry).unwrap();

        let same = first.rebuild(&table, &registry).unwrap();
        assert!(same.changed.is_empty(), "nothing changed on disk");
        assert!(same_instance(&first, &same.chain, "watcher"));

        std::fs::write(file, "one and more").unwrap();
        let edited = first.rebuild(&table, &registry).unwrap();
        assert_eq!(edited.changed, ["watcher"]);
        assert!(!same_instance(&first, &edited.chain, "watcher"));

        std::fs::remove_file(file).unwrap();
        let gone = edited.chain.rebuild(&table, &registry).unwrap();
        assert_eq!(gone.changed, ["watcher"], "a deleted file is a change too");
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn a_plan_checks_a_changed_plugin_builds_but_keeps_the_running_one() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static BUILDS: AtomicUsize = AtomicUsize::new(0);
        let registry = vec![Registration {
            name: "append",
            build: |_| {
                BUILDS.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Append("-tail")) as Box<dyn Plugin>)
            },
        }];
        let first = PluginChain::build(&configured("[append]\nn = 1\n"), &registry).unwrap();
        assert_eq!(BUILDS.load(Ordering::SeqCst), 1);

        let planned = first
            .plan(&configured("[append]\nn = 2\n"), &registry)
            .unwrap();
        assert_eq!(planned.changed, ["append"]);
        assert_eq!(
            BUILDS.load(Ordering::SeqCst),
            2,
            "a plan builds to validate, then drops it"
        );
        assert!(planned.chain.entries.is_empty(), "and installs nothing");
        assert!(first.plan(&configured("[nope]\n"), &registry).is_err());
    }

    #[test]
    fn a_session_keeps_the_chain_it_started_with_across_a_rebuild() {
        let registry = rebuild_registry();
        let chain =
            Arc::new(PluginChain::build(&configured("[append]\n[sticky]\n"), &registry).unwrap());
        let mut session = chain.session();
        let next = chain
            .rebuild(&configured("[sticky]\n"), &registry)
            .unwrap()
            .chain;
        assert_eq!(next.names().count(), 1);

        let method = Method::POST;
        let meta = RequestMeta {
            id: "",
            stream: "s",
            upstream: "u",
            method: &method,
            path: "/x",
        };
        let mut headers = HeaderMap::new();
        let mut body = Bytes::from_static(b"body");
        session.on_request(&meta, &mut headers, &mut body).unwrap();
        assert_eq!(
            &body[..],
            b"body-tail",
            "the in-flight request still runs `append`"
        );
        assert!(Arc::ptr_eq(session.chain(), &chain));
    }

    #[test]
    fn a_plugins_findings_reach_every_plugin_as_events() {
        use std::sync::Mutex;

        // Registrations hold plain function pointers, so the listener records into a static.
        static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

        struct Reporter;
        impl Plugin for Reporter {
            fn name(&self) -> &str {
                "reporter"
            }
            fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
                ctx.report("ssn", 2, false);
                Ok(())
            }
        }
        struct Listener;
        impl Plugin for Listener {
            fn name(&self) -> &str {
                "listener"
            }
            fn on_event(&self, event: &Event<'_>) {
                if let Event::Finding {
                    plugin,
                    kind,
                    count,
                    blocked,
                    ..
                } = event
                {
                    SEEN.lock()
                        .unwrap()
                        .push(format!("{plugin} {kind} {count} {blocked}"));
                }
            }
        }
        let registry = vec![
            Registration {
                name: "reporter",
                build: |_| Ok(Box::new(Reporter) as Box<dyn Plugin>),
            },
            Registration {
                name: "listener",
                build: |_| Ok(Box::new(Listener) as Box<dyn Plugin>),
            },
        ];
        let chain = PluginChain::build(&configured("[reporter]\n[listener]\n"), &registry).unwrap();
        let mut states = chain.states();
        let method = Method::POST;
        let meta = RequestMeta {
            id: "",
            stream: "s",
            upstream: "u",
            method: &method,
            path: "/x",
        };
        let mut headers = HeaderMap::new();
        let mut body = Bytes::new();
        chain
            .on_request(&mut states, &meta, &mut headers, &mut body)
            .unwrap();
        assert_eq!(*SEEN.lock().unwrap(), ["reporter ssn 2 false"]);
    }

    #[test]
    fn finding_details_keep_only_label_characters_and_stay_short() {
        assert_eq!(
            sanitize_detail("get_weather.v2:*/x-y").as_deref(),
            Some("get_weather.v2:*/x-y")
        );
        assert_eq!(
            sanitize_detail("bad name\nwith \"quotes\" and ünïcode!").as_deref(),
            Some("badnamewithquotesandncode")
        );
        assert_eq!(sanitize_detail("  \n\"\" "), None);
        assert_eq!(sanitize_detail("").as_deref(), None);
        let long = "a".repeat(100);
        assert_eq!(
            sanitize_detail(&long).map(|s| s.len()),
            Some(DETAIL_MAX_BYTES)
        );
        // Filtering happens before the cut, so spaces do not eat into the 64 bytes.
        let spaced = format!("{} {}", "b".repeat(60), "c".repeat(10));
        assert_eq!(
            sanitize_detail(&spaced).map(|s| s.len()),
            Some(DETAIL_MAX_BYTES)
        );
    }

    #[test]
    fn a_finding_detail_reaches_listeners_sanitised() {
        use std::sync::Mutex;

        static SEEN: Mutex<Vec<Option<String>>> = Mutex::new(Vec::new());

        struct Reporter;
        impl Plugin for Reporter {
            fn name(&self) -> &str {
                "detailer"
            }
            fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
                ctx.report_detail("tool", "get weather\n\"x\"", 1, false);
                ctx.report("tool", 1, false);
                Ok(())
            }
        }
        struct Listener;
        impl Plugin for Listener {
            fn name(&self) -> &str {
                "listener"
            }
            fn on_event(&self, event: &Event<'_>) {
                if let Event::Finding { detail, .. } = event {
                    SEEN.lock().unwrap().push(detail.map(str::to_owned));
                }
            }
        }
        let registry = vec![
            Registration {
                name: "detailer",
                build: |_| Ok(Box::new(Reporter) as Box<dyn Plugin>),
            },
            Registration {
                name: "listener",
                build: |_| Ok(Box::new(Listener) as Box<dyn Plugin>),
            },
        ];
        let chain = PluginChain::build(&configured("[detailer]\n[listener]\n"), &registry).unwrap();
        let mut states = chain.states();
        let method = Method::POST;
        let meta = RequestMeta {
            id: "",
            stream: "s",
            upstream: "u",
            method: &method,
            path: "/x",
        };
        let mut headers = HeaderMap::new();
        let mut body = Bytes::new();
        chain
            .on_request(&mut states, &meta, &mut headers, &mut body)
            .unwrap();
        assert_eq!(
            *SEEN.lock().unwrap(),
            [Some("getweatherx".to_owned()), None]
        );
    }

    #[test]
    fn unknown_plugin_names_are_rejected() {
        let err = PluginChain::build(&configured("[nope]"), &registry()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("nope"), "error was: {text}");
        assert!(text.contains("append"), "error was: {text}");
    }

    #[test]
    fn disabled_plugins_are_dropped() {
        let chain =
            PluginChain::build(&configured("[append]\nenabled = false\n"), &registry()).unwrap();
        assert!(chain.is_empty());
    }

    struct Tag(&'static str);
    impl Plugin for Tag {
        fn name(&self) -> &str {
            "tag"
        }
        fn on_response_chunk(&self, ctx: &mut ChunkCtx<'_>) {
            let mut bytes = ctx.chunk.to_vec();
            bytes.extend_from_slice(self.0.as_bytes());
            *ctx.chunk = Bytes::from(bytes);
        }
    }

    #[test]
    fn response_hooks_run_in_reverse_registry_order() {
        let registry = vec![
            Registration {
                name: "first",
                build: |_| Ok(Box::new(Tag("A"))),
            },
            Registration {
                name: "second",
                build: |_| Ok(Box::new(Tag("B"))),
            },
        ];
        let chain = PluginChain::build(&configured("[first]\n[second]\n"), &registry).unwrap();
        let mut states = chain.states();
        let mut chunk = Bytes::from_static(b"x");
        chain.on_response_chunk(&mut states, &chain.scope(None), &mut chunk, false);
        // The last plugin to see the request sees the response first, so it tags first.
        assert_eq!(&chunk[..], b"xBA");
    }

    #[test]
    fn plugins_run_in_registry_order() {
        // Registry order is swap, append; the config lists them the other way.
        let chain = PluginChain::build(&configured("[append]\n[swap]\n"), &registry()).unwrap();
        assert_eq!(chain.names().collect::<Vec<_>>(), vec!["swap", "append"]);
    }

    #[test]
    fn a_plugin_transforms_the_request_body() {
        let chain = PluginChain::build(&configured("[append]"), &registry()).unwrap();
        let mut states = chain.states();
        let mut body = Bytes::from_static(b"hi");
        request(&chain, &mut states, &mut body).unwrap();
        assert_eq!(&body[..], b"hi-tail");
    }

    #[test]
    fn reject_stops_the_chain_with_its_status() {
        let chain = PluginChain::build(&configured("[append]\n[refuse]\n"), &registry()).unwrap();
        let mut states = chain.states();
        let mut body = Bytes::new();
        let blocked = request(&chain, &mut states, &mut body).unwrap_err();
        assert_eq!(blocked.status, StatusCode::FORBIDDEN);
        assert_eq!(blocked.message, "not allowed");
        // The plugin before it still ran.
        assert_eq!(&body[..], b"-tail");
    }

    #[test]
    fn skip_passes_through_by_default() {
        let chain = PluginChain::build(&configured("[decline]\n[append]\n"), &registry()).unwrap();
        let mut states = chain.states();
        let mut body = Bytes::new();
        request(&chain, &mut states, &mut body).unwrap();
        assert_eq!(&body[..], b"-tail");
    }

    #[test]
    fn skip_rejects_when_on_error_says_so() {
        let chain = PluginChain::build(
            &configured("[decline]\non_error = \"reject\"\n"),
            &registry(),
        )
        .unwrap();
        let mut states = chain.states();
        let mut body = Bytes::new();
        let blocked = request(&chain, &mut states, &mut body).unwrap_err();
        assert_eq!(blocked.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_bad_on_error_value_is_a_config_error() {
        let err = PluginChain::build(
            &configured("[append]\non_error = \"explode\"\n"),
            &registry(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("explode"), "error was: {err}");
    }

    #[test]
    fn response_bytes_split_across_chunks_are_reassembled() {
        let chain = PluginChain::build(&configured("[swap]"), &registry()).unwrap();
        let mut states = chain.states();
        let mut status = StatusCode::OK;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, "31".parse().unwrap());
        let scope = chain.scope(None);
        chain.on_response_head(&mut states, &scope, "key-a", &mut status, &mut headers);
        assert!(headers.get(CONTENT_LENGTH).is_none());

        let payload = b"echo <PLACEHOLDER> end";
        for split in 0..=payload.len() {
            let mut release = Vec::new();
            let (first, second) = payload.split_at(split);
            let mut chunk = Bytes::copy_from_slice(first);
            chain.on_response_chunk(&mut states, &scope, &mut chunk, false);
            release.extend_from_slice(&chunk);
            let mut chunk = Bytes::copy_from_slice(second);
            chain.on_response_chunk(&mut states, &scope, &mut chunk, true);
            release.extend_from_slice(&chunk);
            assert_eq!(release, b"echo s3cr3t end", "split at {split}");
        }
    }

    #[test]
    fn with_dry_run_marks_every_table_and_leaves_the_rest_alone() {
        let tables = configured("[append]\nenabled = true\n[other]\nx = 1\n");
        let dry = with_dry_run(&tables);
        for (name, table) in &dry {
            assert_eq!(table.get("dry_run"), Some(&Value::Boolean(true)), "{name}");
        }
        assert_eq!(dry["append"].get("enabled"), Some(&Value::Boolean(true)));
        assert_eq!(dry["other"].get("x"), Some(&Value::Integer(1)));
        assert!(
            tables["append"].get("dry_run").is_none(),
            "the input must not change"
        );
    }

    struct Admin {
        name: &'static str,
        paths: Vec<&'static str>,
        events: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Plugin for Admin {
        fn name(&self) -> &str {
            self.name
        }
        fn uses_bodies(&self) -> bool {
            false
        }
        fn on_event(&self, _event: &Event<'_>) {
            self.events
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        fn admin_paths(&self) -> Vec<&'static str> {
            self.paths.clone()
        }
        fn on_admin(&self, ctx: &AdminCtx<'_>) -> Option<AdminReply> {
            Some(AdminReply {
                content_type: "text/plain",
                body: format!("{}:{}", self.name, ctx.keys.available),
            })
        }
    }

    fn admin_chain(entries: Vec<Admin>) -> Result<PluginChain, ConfigError> {
        let mut chain = PluginChain::default();
        for plugin in entries {
            chain.entries.push(Entry {
                plugin: Arc::new(plugin),
                on_error: OnError::PassThrough,
                table: toml::Table::new(),
                stamps: Vec::new(),
                streams: None,
            });
        }
        PluginChain::check_admin_paths(&chain.entries)?;
        Ok(chain)
    }

    fn admin(name: &'static str, paths: Vec<&'static str>) -> Admin {
        Admin {
            name,
            paths,
            events: std::sync::Arc::default(),
        }
    }

    #[test]
    fn events_reach_every_plugin_and_admin_paths_go_to_their_owner() {
        let first = admin("first", vec!["/a"]);
        let second = admin("second", vec!["/b"]);
        let seen = std::sync::Arc::clone(&first.events);
        let chain = admin_chain(vec![first, second]).unwrap();
        chain.on_event(&Event::RateLimited { stream: "s" });
        assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 1);
        let keys = KeyCounts {
            available: 3,
            ..KeyCounts::default()
        };
        assert_eq!(chain.on_admin("/b", keys).unwrap().body, "second:3");
        assert!(chain.on_admin("/missing", keys).is_none());
    }

    #[test]
    fn two_plugins_cannot_claim_one_admin_path_or_reiki_s_own() {
        let clash = admin_chain(vec![admin("a", vec!["/m"]), admin("b", vec!["/m"])]);
        assert!(matches!(clash, Err(ConfigError::PluginConfig { name, .. }) if name == "b"));
        let reserved = admin_chain(vec![admin("a", vec!["/status"])]);
        assert!(matches!(reserved, Err(ConfigError::PluginConfig { .. })));
    }

    #[test]
    fn a_plugin_that_skips_bodies_does_not_make_the_chain_a_body_chain() {
        let chain = admin_chain(vec![admin("quiet", vec![])]).unwrap();
        assert!(chain.is_empty());
    }

    fn request_on(chain: &PluginChain, stream: &str, body: &mut Bytes) -> Result<(), Blocked> {
        let mut states = chain.states();
        let method = Method::POST;
        let meta = RequestMeta {
            id: "",
            stream,
            upstream: "zen",
            method: &method,
            path: "/x",
        };
        chain.on_request(&mut states, &meta, &mut HeaderMap::new(), body)
    }

    #[test]
    fn a_plugin_with_streams_runs_only_on_those_streams() {
        let chain = PluginChain::build(&configured("[append]\nstreams = [\"one\"]\n"), &registry())
            .unwrap();
        let mut body = Bytes::from_static(b"hi");
        request_on(&chain, "one", &mut body).unwrap();
        assert_eq!(&body[..], b"hi-tail");
        let mut body = Bytes::from_static(b"hi");
        request_on(&chain, "two", &mut body).unwrap();
        assert_eq!(&body[..], b"hi");
    }

    #[test]
    fn a_plugin_without_streams_runs_on_every_stream() {
        let chain = PluginChain::build(&configured("[append]\n"), &registry()).unwrap();
        for stream in ["one", "two", "passthrough"] {
            let mut body = Bytes::from_static(b"hi");
            request_on(&chain, stream, &mut body).unwrap();
            assert_eq!(&body[..], b"hi-tail", "{stream}");
        }
    }

    #[test]
    fn a_scoped_plugin_runs_its_response_hooks_only_for_its_streams() {
        let registry = vec![Registration {
            name: "tag",
            build: |_| Ok(Box::new(Tag("A"))),
        }];
        let chain = Arc::new(
            PluginChain::build(&configured("[tag]\nstreams = [\"one\"]\n"), &registry).unwrap(),
        );
        let method = Method::POST;
        for (stream, expected) in [("one", &b"xA"[..]), ("two", &b"x"[..])] {
            let mut session = chain.session();
            let meta = RequestMeta {
                id: "",
                stream,
                upstream: "zen",
                method: &method,
                path: "/x",
            };
            session
                .on_request(&meta, &mut HeaderMap::new(), &mut Bytes::new())
                .unwrap();
            let mut chunk = Bytes::from_static(b"x");
            session.on_response_chunk(&mut chunk, true);
            assert_eq!(&chunk[..], expected, "{stream}");
        }
    }

    #[test]
    fn events_reach_a_scoped_plugin_only_for_its_streams() {
        let scoped = admin("scoped", vec!["/scoped"]);
        let seen = Arc::clone(&scoped.events);
        let mut chain = PluginChain::default();
        chain.entries.push(Entry {
            plugin: Arc::new(scoped),
            on_error: OnError::PassThrough,
            table: toml::Table::new(),
            stamps: Vec::new(),
            streams: Some(vec!["one".to_owned()]),
        });
        let count = || seen.load(std::sync::atomic::Ordering::Relaxed);
        chain.on_event(&Event::RateLimited { stream: "two" });
        chain.on_event(&Event::Cooldown {
            key: "k",
            kind: reiki_plugin::FailureKind::Transient,
        });
        assert_eq!(count(), 0);
        chain.on_event(&Event::RateLimited { stream: "one" });
        assert_eq!(count(), 1);
        let keys = KeyCounts::default();
        assert!(
            chain.on_admin("/scoped", keys).is_some(),
            "admin paths ignore streams"
        );
    }

    #[test]
    fn a_live_reload_of_only_streams_keeps_the_running_instance() {
        let registry = rebuild_registry();
        let first = PluginChain::build(&configured("[append]\n[sticky]\n"), &registry).unwrap();
        let next = first
            .rebuild(
                &configured("[append]\nstreams = [\"one\"]\n[sticky]\nstreams = [\"one\"]\n"),
                &registry,
            )
            .unwrap();
        assert!(
            same_instance(&first, &next.chain, "append"),
            "counters survive"
        );
        assert!(
            same_instance(&first, &next.chain, "sticky"),
            "counters survive"
        );
        assert!(next.restart_needed.is_empty());
        assert_eq!(next.changed, ["append", "sticky"]);
        let mut body = Bytes::from_static(b"hi");
        request_on(&next.chain, "two", &mut body).unwrap();
        assert_eq!(&body[..], b"hi", "append no longer runs on stream two");
        let mut body = Bytes::from_static(b"hi");
        request_on(&next.chain, "one", &mut body).unwrap();
        assert_eq!(&body[..], b"hi-tail");

        let edited = first
            .rebuild(
                &configured("[append]\n[sticky]\nsize = 2\nstreams = [\"one\"]\n"),
                &registry,
            )
            .unwrap();
        assert_eq!(edited.restart_needed, ["sticky"]);
        assert!(same_instance(&first, &edited.chain, "sticky"));
    }

    #[test]
    fn a_bad_streams_value_is_a_config_error() {
        for bad in [
            "streams = \"one\"",
            "streams = []",
            "streams = [\"\"]",
            "streams = [1]",
        ] {
            let err = PluginChain::build(&configured(&format!("[append]\n{bad}\n")), &registry())
                .unwrap_err();
            assert!(err.to_string().contains("streams"), "{bad}: {err}");
        }
    }
}
