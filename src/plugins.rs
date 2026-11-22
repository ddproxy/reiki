//! Assembles the compiled-in plugins named in the config into a [`PluginChain`].
//!
//! Plugins are compiled in, never loaded at runtime. [`builtin`] lists the ones
//! this binary contains; their crate features are enabled when reiki is built.
//! A plugin named in `[plugins.*]` but absent from [`builtin`] is a config
//! error, so a typo fails at startup instead of silently doing nothing.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use serde_json::Value as JsonValue;
use toml::Value;

use reiki_plugin::{
    AdminCtx, AdminReply, AdmissionRefusal, AttemptCtx, ChunkCtx, Event, KeyCounts,
    ObservedCooldown, Plugin, PluginError, PluginHost, RequestCtx, ResponseHeadCtx, UpstreamUsage,
};

use crate::config::ConfigError;
use crate::reload::{Stamps, file_stamp, stamps};

/// A plugin admin path in the form the admin router matches: absolute, already normalised.
fn admin_path(path: &str) -> Option<&str> {
    if !path.starts_with('/') || path == "/" || path.ends_with('/') || path.contains('%') {
        return None;
    }
    if path
        .split('/')
        .skip(1)
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return None;
    }
    Some(path)
}

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
#[derive(Debug, Clone)]
pub struct Blocked {
    pub status: StatusCode,
    pub message: String,
}

/// Told to the client when a fail-closed plugin fails and its work cannot be trusted.
pub const PLUGIN_FAILURE_MESSAGE: &str =
    "a fail-closed plugin failed; the request was not processed";

/// The status used for a plugin failure, independent of the error the plugin returned.
pub const PLUGIN_FAILURE_STATUS: StatusCode = StatusCode::SERVICE_UNAVAILABLE;

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
    stamps: Stamps,
    /// Streams the plugin runs on; `None` runs it on every stream.
    streams: Option<Vec<String>>,
}

fn stamps_of(plugin: &dyn Plugin) -> Stamps {
    stamps(&plugin.watched_files())
}

impl Entry {
    /// True when a file the plugin reads differs from what it saw at build time.
    fn files_changed(&self) -> bool {
        self.stamps
            .iter()
            .any(|(path, seen)| file_stamp(path) != *seen)
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

fn is_wasm_table(table: &toml::Table) -> bool {
    table.contains_key("wasm")
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
            )));
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
                )));
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
        | Event::GatewayRejected { stream, .. }
        | Event::GatewayStoreError { stream }
        | Event::Usage { stream, .. }
        | Event::PluginRejected { stream, .. }
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

/// Owned copy of [`RequestMeta`] for a blocking task, which cannot borrow the request.
struct OwnedRequestMeta {
    id: String,
    stream: String,
    upstream: String,
    method: Method,
    path: String,
}

impl<'a> From<&RequestMeta<'a>> for OwnedRequestMeta {
    fn from(meta: &RequestMeta<'a>) -> Self {
        Self {
            id: meta.id.to_owned(),
            stream: meta.stream.to_owned(),
            upstream: meta.upstream.to_owned(),
            method: meta.method.clone(),
            path: meta.path.to_owned(),
        }
    }
}

impl OwnedRequestMeta {
    fn borrowed(&self) -> RequestMeta<'_> {
        RequestMeta {
            id: &self.id,
            stream: &self.stream,
            upstream: &self.upstream,
            method: &self.method,
            path: &self.path,
        }
    }
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
        Self::build_with_host(configured, registry, &PluginHost::default())
    }

    /// Like [`Self::build`], with host facts such as the private state path each
    /// plugin may use. Only the process entry point supplies these facts.
    pub fn build_with_host(
        configured: &BTreeMap<String, toml::Table>,
        registry: &[Registration],
        host: &PluginHost,
    ) -> Result<Self, ConfigError> {
        let mut builtin_entries = Vec::new();
        #[cfg(feature = "plugin-wasm")]
        let mut wasm_entries: Vec<String> = Vec::new();
        for name in configured.keys() {
            if registry.iter().any(|r| r.name == name) {
                continue;
            }
            if is_wasm_table(configured.get(name).unwrap()) {
                #[cfg(feature = "plugin-wasm")]
                {
                    wasm_entries.push(name.clone());
                }
                #[cfg(not(feature = "plugin-wasm"))]
                {
                    return Err(ConfigError::PluginConfig {
                        name: name.clone(),
                        message: "runtime Wasm plugins require a binary built with the `plugin-wasm` feature".to_owned(),
                    });
                }
            } else {
                return Err(ConfigError::UnknownPlugin {
                    name: name.clone(),
                    known: registry.iter().map(|r| r.name.to_string()).collect(),
                });
            }
        }

        for reg in registry {
            let Some(table) = configured.get(reg.name) else {
                continue;
            };
            if !enabled(reg.name, table)? {
                continue;
            }
            let explicit = on_error(reg.name, table)?;
            let streams = streams_of(reg.name, table)?;
            let mut plugin = (reg.build)(table).map_err(|message| ConfigError::PluginConfig {
                name: reg.name.to_string(),
                message,
            })?;
            plugin.configure(host);
            let plugin: Arc<dyn Plugin> = Arc::from(plugin);
            let on_error = resolve_on_error(reg.name, explicit, plugin.as_ref());
            builtin_entries.push(Entry {
                stamps: stamps_of(plugin.as_ref()),
                plugin,
                on_error,
                table: table.clone(),
                streams,
            });
        }

        #[cfg(feature = "plugin-wasm")]
        let wasm_built = Self::build_wasm_entries(configured, &wasm_entries)?;
        #[cfg(not(feature = "plugin-wasm"))]
        let wasm_built: Vec<Entry> = Vec::new();

        let entries = Self::insert_wasm_at_d5_slot(builtin_entries, wasm_built);
        Self::check_admin_paths(&entries)?;
        Ok(Self { entries })
    }

    fn insert_wasm_at_d5_slot(mut builtin: Vec<Entry>, wasm: Vec<Entry>) -> Vec<Entry> {
        if wasm.is_empty() {
            return builtin;
        }
        // D5: runtime Wasm plugins run after every compiled-in request plugin and before
        // every compiled-in response plugin. The fixed inner slot is immediately before the
        // first response-observer builtin (requestlog/metrics/toolpolicy), which places them
        // after placeholders on request and before placeholders on response (masked both ways).
        let pos = builtin
            .iter()
            .position(|e| matches!(e.plugin.name(), "requestlog" | "metrics" | "toolpolicy"))
            .unwrap_or(builtin.len());
        let tail = builtin.split_off(pos);
        builtin.extend(wasm);
        builtin.extend(tail);
        builtin
    }

    #[cfg(feature = "plugin-wasm")]
    fn build_wasm_entries(
        configured: &BTreeMap<String, toml::Table>,
        names: &[String],
    ) -> Result<Vec<Entry>, ConfigError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let engine =
            reiki_wasm::WasmEngine::new().map_err(|message| ConfigError::PluginConfig {
                name: "wasm".to_owned(),
                message,
            })?;
        let engine = Arc::new(engine);
        let mut entries = Vec::new();
        for name in names {
            let Some(table) = configured.get(name) else {
                continue;
            };
            if !enabled(name, table)? {
                continue;
            }
            let cfg =
                reiki_wasm::config::parse_wasm_plugin_config(name, table).map_err(|message| {
                    ConfigError::PluginConfig {
                        name: name.clone(),
                        message,
                    }
                })?;
            let explicit = match cfg.on_error.as_deref() {
                Some("pass_through") => Some(OnError::PassThrough),
                Some("reject") => Some(OnError::Reject),
                _ => None,
            };
            let streams = cfg.streams.clone();
            let plugin =
                reiki_wasm::WasmPlugin::load(Arc::clone(&engine), cfg).map_err(|message| {
                    ConfigError::PluginConfig {
                        name: name.clone(),
                        message,
                    }
                })?;
            let plugin: Arc<dyn Plugin> = Arc::from(plugin);
            let on_error = resolve_on_error(name, explicit, plugin.as_ref());
            entries.push(Entry {
                stamps: stamps_of(plugin.as_ref()),
                plugin,
                on_error,
                table: table.clone(),
                streams,
            });
        }
        Ok(entries)
    }

    #[cfg(feature = "plugin-wasm")]
    fn rebuild_wasm_entries(
        &self,
        configured: &BTreeMap<String, toml::Table>,
        names: &[String],
        build: bool,
        changed: &mut Vec<String>,
        restart_needed: &mut Vec<String>,
    ) -> Result<Vec<Entry>, ConfigError> {
        let mut entries = Vec::new();
        // Handle removals: old wasm plugins not in configured
        for old in &self.entries {
            let is_wasm_old = is_wasm_table(&old.table);
            if !is_wasm_old {
                continue;
            }
            if !configured.contains_key(old.plugin.name()) {
                changed.push(old.plugin.name().to_owned());
            }
        }

        if names.is_empty() {
            return Ok(entries);
        }
        let engine =
            reiki_wasm::WasmEngine::new().map_err(|message| ConfigError::PluginConfig {
                name: "wasm".to_owned(),
                message,
            })?;
        let engine = Arc::new(engine);
        for name in names {
            let Some(table) = configured.get(name) else {
                continue;
            };
            if !enabled(name, table)? {
                // if it existed before, removal already counted above
                continue;
            }
            let old = self
                .entries
                .iter()
                .find(|e| e.plugin.name() == name.as_str() && is_wasm_table(&e.table));
            let new_cfg =
                reiki_wasm::config::parse_wasm_plugin_config(name, table).map_err(|message| {
                    ConfigError::PluginConfig {
                        name: name.clone(),
                        message,
                    }
                })?;
            let streams = new_cfg.streams.clone();
            let explicit = match new_cfg.on_error.as_deref() {
                Some("pass_through") => Some(OnError::PassThrough),
                Some("reject") => Some(OnError::Reject),
                _ => None,
            };

            if let Some(old) = old {
                if same_settings(&old.table, table) && !old.files_changed() {
                    if old.streams != streams {
                        changed.push(name.clone());
                    }
                    entries.push(old.share(streams));
                    continue;
                }
                // Wasm-specific reload narrowing
                let old_cfg = reiki_wasm::config::parse_wasm_plugin_config(name, &old.table).ok();
                let narrowing = if let Some(ref oc) = old_cfg {
                    let old_has_high_or_med = oc.grants.iter().any(|g| g.risk() != "low");
                    reiki_wasm::config::reload_narrowing(oc, &new_cfg, old_has_high_or_med)
                } else {
                    reiki_wasm::ReloadDecision::RestartNeeded(vec!["config".to_owned()])
                };
                match narrowing {
                    reiki_wasm::ReloadDecision::Live => {
                        // Can live-reload: rebuild instance if build requested, or reuse?
                        // live narrowing means safe to rebuild. D6/2.6 says "live reloadable".
                        changed.push(name.clone());
                        if build {
                            let plugin = reiki_wasm::WasmPlugin::load(Arc::clone(&engine), new_cfg)
                                .map_err(|message| ConfigError::PluginConfig {
                                    name: name.clone(),
                                    message,
                                })?;
                            let plugin: Arc<dyn Plugin> = Arc::from(plugin);
                            let on_error = resolve_on_error(name, explicit, plugin.as_ref());
                            entries.push(Entry {
                                stamps: stamps_of(plugin.as_ref()),
                                plugin,
                                on_error,
                                table: table.clone(),
                                streams,
                            });
                        } else {
                            // plan mode: validate by building then drop
                            let _plugin =
                                reiki_wasm::WasmPlugin::load(Arc::clone(&engine), new_cfg)
                                    .map_err(|message| ConfigError::PluginConfig {
                                        name: name.clone(),
                                        message,
                                    })?;
                        }
                    }
                    reiki_wasm::ReloadDecision::RestartNeeded(_reasons) => {
                        restart_needed.push(name.clone());
                        if old.streams != streams {
                            changed.push(name.clone());
                        }
                        entries.push(old.share(streams));
                    }
                }
            } else {
                changed.push(name.clone());
                if build {
                    let plugin = reiki_wasm::WasmPlugin::load(Arc::clone(&engine), new_cfg)
                        .map_err(|message| ConfigError::PluginConfig {
                            name: name.clone(),
                            message,
                        })?;
                    let plugin: Arc<dyn Plugin> = Arc::from(plugin);
                    let on_error = resolve_on_error(name, explicit, plugin.as_ref());
                    entries.push(Entry {
                        stamps: stamps_of(plugin.as_ref()),
                        plugin,
                        on_error,
                        table: table.clone(),
                        streams,
                    });
                } else {
                    // plan: validate by building if it looks like a new plugin? plan builds if old.is_some() in registry path;
                    // for new wasm plugins, build to validate.
                    let _plugin = reiki_wasm::WasmPlugin::load(Arc::clone(&engine), new_cfg)
                        .map_err(|message| ConfigError::PluginConfig {
                            name: name.clone(),
                            message,
                        })?;
                }
            }
        }
        Ok(entries)
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
        #[cfg(feature = "plugin-wasm")]
        let mut wasm_names: Vec<String> = Vec::new();
        for name in configured.keys() {
            if registry.iter().any(|r| r.name == name) {
                continue;
            }
            if is_wasm_table(configured.get(name).unwrap()) {
                #[cfg(feature = "plugin-wasm")]
                {
                    wasm_names.push(name.clone());
                }
                #[cfg(not(feature = "plugin-wasm"))]
                {
                    return Err(ConfigError::PluginConfig {
                        name: name.clone(),
                        message: "runtime Wasm plugins require a binary built with the `plugin-wasm` feature".to_owned(),
                    });
                }
            } else {
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
                    let explicit = on_error(reg.name, table)?;
                    changed.push(reg.name.to_owned());
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
                        let on_error = resolve_on_error(reg.name, explicit, plugin.as_ref());
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

        #[cfg(feature = "plugin-wasm")]
        {
            let wasm = self.rebuild_wasm_entries(
                configured,
                &wasm_names,
                build,
                &mut changed,
                &mut restart_needed,
            )?;
            let entries = Self::insert_wasm_at_d5_slot(entries, wasm);
            if build {
                Self::check_admin_paths(&entries)?;
            }
            Ok(Rebuilt {
                chain: Self { entries },
                changed,
                restart_needed,
            })
        }

        #[cfg(not(feature = "plugin-wasm"))]
        {
            if build {
                Self::check_admin_paths(&entries)?;
            }
            Ok(Rebuilt {
                chain: Self { entries },
                changed,
                restart_needed,
            })
        }
    }

    /// Starts a request's plugin session: this chain and fresh per-request state for each plugin.
    pub fn session(self: &Arc<Self>) -> PluginSession {
        let (states, failed) = self.states_with_failures();
        PluginSession {
            chain: Arc::clone(self),
            states,
            scope: self.scope(None),
            failed,
        }
    }

    /// Which plugins run for a request on `stream`, in chain order.
    fn scope(&self, stream: Option<&str>) -> Vec<bool> {
        self.entries.iter().map(|e| e.runs_for(stream)).collect()
    }

    fn check_admin_paths(entries: &[Entry]) -> Result<(), ConfigError> {
        let mut claimed: Vec<(&str, &str)> = Vec::new();
        for entry in entries {
            for path in entry.plugin.admin_paths() {
                let Some(path) = admin_path(path) else {
                    return Err(ConfigError::PluginConfig {
                        name: entry.plugin.name().to_owned(),
                        message: format!(
                            "admin path {path:?} must start with '/', have no trailing slash, \
                             no '.' or '..' segment and no percent-encoding"
                        ),
                    });
                };
                if crate::admin::is_reserved_path(path) {
                    return Err(ConfigError::PluginConfig {
                        name: entry.plugin.name().to_owned(),
                        message: format!("admin path {path} is served by reiki itself"),
                    });
                }
                if let Some((_, other)) = claimed.iter().find(|(p, _)| *p == path) {
                    return Err(ConfigError::PluginConfig {
                        name: entry.plugin.name().to_owned(),
                        message: format!("admin path {path} is already served by {other}"),
                    });
                }
                claimed.push((path, entry.plugin.name()));
            }
        }
        Ok(())
    }

    /// True when no plugin changes a response body, so `content-length` can stay.
    pub fn is_empty(&self) -> bool {
        !self.entries.iter().any(|e| e.plugin.uses_bodies())
    }

    /// True when any plugin needs the response head or chunks.
    pub fn needs_response_hooks(&self) -> bool {
        self.entries.iter().any(|e| e.plugin.needs_response_hooks())
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
    pub fn on_admin(
        &self,
        path: &str,
        keys: KeyCounts,
        usage: Option<Vec<UpstreamUsage>>,
        observed: Vec<ObservedCooldown>,
    ) -> Option<AdminReply> {
        self.on_admin_with_extra(path, "", keys, usage, observed, String::new())
    }

    /// Like [`Self::on_admin`], with host-supplied metric text for the metrics endpoint.
    pub fn on_admin_with_extra(
        &self,
        path: &str,
        query: &str,
        keys: KeyCounts,
        usage: Option<Vec<UpstreamUsage>>,
        observed: Vec<ObservedCooldown>,
        extra_metrics: String,
    ) -> Option<AdminReply> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.plugin.admin_paths().contains(&path))?;
        let ctx = AdminCtx::new(path, keys)
            .with_query(query)
            .with_usage(usage)
            .with_observed(observed)
            .with_extra_metrics(extra_metrics);
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

    /// Extra Prometheus text from plugins, appended by the host to `/v1/metrics`.
    pub fn metrics_text(&self) -> String {
        self.entries
            .iter()
            .filter(|e| e.runs_for(None))
            .filter_map(|e| guard(e.plugin.name(), "metrics_text", || e.plugin.metrics_text()))
            .collect()
    }

    /// The cost fragment for one stream's own `/v1/usage` answer, if a plugin has one.
    pub fn usage_cost_fragment(&self, stream: &str) -> Option<JsonValue> {
        self.entries
            .iter()
            .filter(|e| e.runs_for(Some(stream)))
            .filter_map(|e| {
                guard(e.plugin.name(), "usage_cost_fragment", || {
                    e.plugin.usage_cost_fragment(stream)
                })
                .flatten()
            })
            .reduce(|mut all, next| {
                if let (Some(object), Some(add)) = (all.as_object_mut(), next.as_object()) {
                    for (key, value) in add {
                        object.insert(key.clone(), value.clone());
                    }
                }
                all
            })
    }

    /// A plugin's admission refusal, still routed through the gateway's refusal path.
    pub fn admission_refusal(&self, stream: &str, now: SystemTime) -> Option<AdmissionRefusal> {
        self.entries
            .iter()
            .filter(|e| e.runs_for(Some(stream)))
            .filter_map(|e| {
                guard(e.plugin.name(), "admission_refusal", || {
                    e.plugin.admission_refusal(stream, now)
                })
                .flatten()
            })
            .next()
    }

    /// JSON objects to merge into the admin status document. A panicking or
    /// malformed hook is skipped so one plugin cannot take status down.
    pub fn status_fragments(&self) -> Vec<JsonValue> {
        self.entries
            .iter()
            .filter_map(|e| {
                guard(e.plugin.name(), "status_fragment", || {
                    e.plugin.status_fragment()
                })
                .flatten()
            })
            .filter(JsonValue::is_object)
            .collect()
    }

    /// Named monotonic counters to publish in the shared health document.
    pub fn health_gauges(&self) -> Vec<(&'static str, u64)> {
        self.entries
            .iter()
            .filter_map(|e| {
                guard(e.plugin.name(), "health_gauge", || e.plugin.health_gauge()).flatten()
            })
            .collect()
    }

    /// Names of the active plugins, for startup logging.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.plugin.name())
    }

    /// Tells each plugin the process is stopping, so a buffering plugin can write out what it holds.
    /// A panicking hook is logged and skipped, so the other plugins still get to shut down.
    pub fn shutdown(&self) {
        for entry in &self.entries {
            let _ = guard(entry.plugin.name(), "on_shutdown", || {
                entry.plugin.on_shutdown();
            });
        }
    }

    /// Every file a plugin in this chain reads, for the watcher.
    pub fn watched_paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .flat_map(|e| e.stamps.iter().map(|(path, _)| path.clone()))
            .collect()
    }

    /// Creates the per-request state for every plugin, in chain order. A panicking hook is
    /// logged and skipped: that plugin gets the same default state as one that implements no
    /// `new_state`, so the request keeps its one-state-per-plugin shape. The flag reports a
    /// panic in a plugin whose failure policy is [`OnError::Reject`].
    fn states_with_failures(&self) -> (Vec<Box<dyn Any + Send>>, bool) {
        let mut failed = false;
        let states = self
            .entries
            .iter()
            .map(
                |e| match guard(e.plugin.name(), "new_state", || e.plugin.new_state()) {
                    Some(state) => state,
                    None => {
                        if e.on_error == OnError::Reject {
                            failed = true;
                        }
                        Box::new(())
                    }
                },
            )
            .collect();
        (states, failed)
    }

    /// Creates states without reporting a failure; request handling uses [`Self::session`],
    /// which fails closed when a fail-closed plugin panics.
    pub fn states(&self) -> Vec<Box<dyn Any + Send>> {
        self.states_with_failures().0
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
                    self.on_event(&Event::PluginRejected {
                        stream: meta.stream,
                        plugin: entry.plugin.name(),
                    });
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
                            self.on_event(&Event::PluginRejected {
                                stream: meta.stream,
                                plugin: entry.plugin.name(),
                            });
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
    /// and skipped; `true` means a plugin whose failure policy is [`OnError::Reject`]
    /// panicked and the response must not be relayed.
    #[allow(clippy::too_many_arguments)]
    pub fn on_response_head(
        &self,
        states: &mut [Box<dyn Any + Send>],
        scope: &[bool],
        key: &str,
        upstream: &str,
        status: &mut StatusCode,
        headers: &mut HeaderMap,
    ) -> bool {
        let mut failed = false;
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
            let mut ctx = ResponseHeadCtx::new(key, &mut *status, &mut *headers, &mut **state)
                .with_upstream(upstream);
            if guard(entry.plugin.name(), "on_response_head", || {
                entry.plugin.on_response_head(&mut ctx);
            })
            .is_none()
                && entry.on_error == OnError::Reject
            {
                failed = true;
            }
        }
        failed
    }

    /// Runs `on_response_chunk` on every plugin, in reverse order, letting each rewrite
    /// `chunk` before the next sees it. A panicking hook is logged and skipped; `true`
    /// means a plugin whose failure policy is [`OnError::Reject`] panicked.
    pub fn on_response_chunk(
        &self,
        states: &mut [Box<dyn Any + Send>],
        scope: &[bool],
        chunk: &mut Bytes,
        is_last: bool,
    ) -> bool {
        let mut failed = false;
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
            if guard(entry.plugin.name(), "on_response_chunk", || {
                entry.plugin.on_response_chunk(&mut ctx);
            })
            .is_none()
                && entry.on_error == OnError::Reject
            {
                failed = true;
            }
        }
        failed
    }
}

/// One request's plugin chain and per-plugin state. It owns the chain it started with, so a reload
/// that swaps the chain mid-request cannot pair the request's state with the wrong plugins.
pub struct PluginSession {
    chain: Arc<PluginChain>,
    states: Vec<Box<dyn Any + Send>>,
    /// Which plugins run for this request, set by `on_request`; only unscoped plugins until then.
    scope: Vec<bool>,
    /// A fail-closed plugin panicked, so its work cannot be trusted for this request.
    failed: bool,
}

impl Default for PluginSession {
    fn default() -> Self {
        Self {
            chain: Arc::new(PluginChain::default()),
            states: Vec::new(),
            scope: Vec::new(),
            failed: false,
        }
    }
}

impl PluginSession {
    /// True when no plugin in this session changes a response body.
    pub fn is_empty(&self) -> bool {
        self.chain.is_empty()
    }

    /// True when any plugin in this session needs the response hooks.
    pub fn needs_response_hooks(&self) -> bool {
        self.chain.needs_response_hooks()
    }

    /// The chain this session runs.
    pub fn chain(&self) -> &Arc<PluginChain> {
        &self.chain
    }

    /// True when a fail-closed plugin panicked while preparing or processing this request.
    pub fn failed(&self) -> bool {
        self.failed
    }

    pub fn on_request(
        &mut self,
        meta: &RequestMeta<'_>,
        headers: &mut HeaderMap,
        body: &mut Bytes,
    ) -> Result<(), Blocked> {
        self.scope = self.chain.scope(Some(meta.stream));
        if self.failed {
            return Err(Blocked {
                status: PLUGIN_FAILURE_STATUS,
                message: PLUGIN_FAILURE_MESSAGE.to_owned(),
            });
        }
        self.chain.on_request(&mut self.states, meta, headers, body)
    }

    /// Same as [`Self::on_request`], but runs the synchronous hooks on Tokio's
    /// blocking pool so a CPU-heavy plugin cannot stall the runtime.
    pub async fn on_request_blocking(
        &mut self,
        meta: &RequestMeta<'_>,
        headers: &mut HeaderMap,
        body: &mut Bytes,
    ) -> Result<(), Blocked> {
        self.scope = self.chain.scope(Some(meta.stream));
        if self.failed {
            return Err(Blocked {
                status: PLUGIN_FAILURE_STATUS,
                message: PLUGIN_FAILURE_MESSAGE.to_owned(),
            });
        }
        if !self.scope.iter().any(|&runs| runs) {
            return Ok(());
        }

        let chain = Arc::clone(&self.chain);
        // The blocking task runs on another thread, where neither the request's tracing span nor
        // this session's ownership survives. Carry the span in explicitly, and poison the session
        // before handing its state away: if this future is cancelled at the await, the moved
        // states never come back and the session must fail closed on any reuse.
        let span = tracing::Span::current();
        self.failed = true;
        let mut states = std::mem::take(&mut self.states);
        let owned_meta = OwnedRequestMeta::from(meta);
        let mut owned_headers = headers.clone();
        let mut owned_body = body.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            let meta = owned_meta.borrowed();
            let result = chain.on_request(&mut states, &meta, &mut owned_headers, &mut owned_body);
            (states, owned_headers, owned_body, result)
        })
        .await;
        match joined {
            Ok((states, returned_headers, returned_body, result)) => {
                self.states = states;
                *headers = returned_headers;
                *body = returned_body;
                self.failed = false;
                result
            }
            Err(_) => {
                // A panic that escaped the chain's per-plugin guard means no part of
                // this request's plugin work can be trusted. Do not forward it.
                self.failed = true;
                self.states = self.chain.states();
                Err(Blocked {
                    status: PLUGIN_FAILURE_STATUS,
                    message: PLUGIN_FAILURE_MESSAGE.to_owned(),
                })
            }
        }
    }

    pub fn on_attempt(&mut self, key: &str, status: Option<u16>, body: &[u8]) {
        self.chain
            .on_attempt(&mut self.states, &self.scope, key, status, body);
    }

    pub fn on_response_head(
        &mut self,
        key: &str,
        upstream: &str,
        status: &mut StatusCode,
        headers: &mut HeaderMap,
    ) -> bool {
        self.failed |= self.chain.on_response_head(
            &mut self.states,
            &self.scope,
            key,
            upstream,
            status,
            headers,
        );
        self.failed
    }

    pub fn on_response_chunk(&mut self, chunk: &mut Bytes, is_last: bool) -> bool {
        self.failed |= self
            .chain
            .on_response_chunk(&mut self.states, &self.scope, chunk, is_last);
        self.failed
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

/// The operator's explicit `on_error`, or `None` when the table does not set one.
fn on_error(name: &str, table: &toml::Table) -> Result<Option<OnError>, ConfigError> {
    match table.get("on_error") {
        None => Ok(None),
        Some(Value::String(value)) => match value.as_str() {
            "pass_through" => Ok(Some(OnError::PassThrough)),
            "reject" => Ok(Some(OnError::Reject)),
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

/// The failure policy for a built plugin: an explicit setting wins; otherwise a plugin that
/// masks data fails closed and every other plugin passes the request through.
fn resolve_on_error(name: &str, explicit: Option<OnError>, plugin: &dyn Plugin) -> OnError {
    match explicit {
        Some(OnError::PassThrough) if plugin.fails_closed() => {
            tracing::warn!(
                plugin = name,
                "on_error = \"pass_through\" lets a request through unmasked when this plugin fails"
            );
            OnError::PassThrough
        }
        Some(policy) => policy,
        None if plugin.fails_closed() => OnError::Reject,
        None => OnError::PassThrough,
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

    #[test]
    fn toolpolicy_is_registered_after_metrics() {
        let registry = builtin();
        let names: Vec<&str> = registry
            .iter()
            .map(|registration| registration.name)
            .collect();
        let metrics = names.iter().position(|name| *name == "metrics");
        let toolpolicy = names.iter().position(|name| *name == "toolpolicy");
        if let (Some(metrics), Some(toolpolicy)) = (metrics, toolpolicy) {
            assert!(metrics < toolpolicy, "compiled plugin order is {names:?}");
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

    struct Masker;
    impl Plugin for Masker {
        fn name(&self) -> &str {
            "masker"
        }
        fn fails_closed(&self) -> bool {
            true
        }
        fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
            Err(PluginError::Skip("cannot mask this body".into()))
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
            Registration {
                name: "masker",
                build: |_| Ok(Box::new(Masker) as Box<dyn Plugin>),
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

    #[cfg(feature = "plugin-metrics")]
    #[test]
    fn a_changed_metrics_table_keeps_its_counters_and_asks_for_a_restart() {
        let registry = vec![Registration {
            name: reiki_metrics::NAME,
            build: reiki_metrics::build,
        }];
        let first = PluginChain::build(&configured("[metrics]\n"), &registry).unwrap();
        first.on_event(&Event::RequestDone {
            stream: "s",
            upstream: "u",
            status: 503,
        });
        let next = first
            .rebuild(&configured("[metrics]\non_error = \"reject\"\n"), &registry)
            .unwrap();
        assert_eq!(next.restart_needed, ["metrics"]);
        assert!(next.changed.is_empty());
        assert!(same_instance(&first, &next.chain, "metrics"));
        let text = next
            .chain
            .on_admin("/metrics", KeyCounts::default(), None, vec![])
            .unwrap()
            .body;
        assert!(text.contains(
            "reiki_requests_total{stream=\"s\",upstream=\"u\",status=\"server_error\"} 1"
        ));
    }

    #[test]
    fn a_rebuild_with_an_unknown_plugin_or_a_bad_table_changes_nothing() {
        let registry = rebuild_registry();
        let first = PluginChain::build(&configured("[append]\n"), &registry).unwrap();
        assert!(first.rebuild(&configured("[nope]\n"), &registry).is_err());
        assert!(
            first
                .rebuild(
                    &configured("[append]\non_error = \"sideways\"\n"),
                    &registry
                )
                .is_err()
        );
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
    fn a_rejection_and_a_skip_that_rejects_are_announced_but_a_pass_through_is_not() {
        use std::sync::Mutex;

        static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

        struct Listener;
        impl Plugin for Listener {
            fn name(&self) -> &str {
                "listener"
            }
            fn on_event(&self, event: &Event<'_>) {
                if let Event::PluginRejected { stream, plugin } = event {
                    SEEN.lock().unwrap().push(format!("{plugin}@{stream}"));
                }
            }
        }
        let mut registry = registry();
        registry.push(Registration {
            name: "listener",
            build: |_| Ok(Box::new(Listener) as Box<dyn Plugin>),
        });
        let run = |source: &str| {
            let chain = PluginChain::build(&configured(source), &registry).unwrap();
            let mut states = chain.states();
            let mut body = Bytes::new();
            request(&chain, &mut states, &mut body)
        };
        assert!(run("[listener]\n[decline]\n").is_ok());
        assert!(
            SEEN.lock().unwrap().is_empty(),
            "a pass-through skip is no rejection"
        );
        assert!(run("[listener]\n[refuse]\n").is_err());
        assert!(run("[listener]\n[decline]\non_error = \"reject\"\n").is_err());
        assert_eq!(*SEEN.lock().unwrap(), ["refuse@default", "decline@default"]);
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
    fn a_panicking_on_shutdown_does_not_stop_the_other_plugins() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static CALLED: AtomicUsize = AtomicUsize::new(0);

        struct PanicShutdown;
        impl Plugin for PanicShutdown {
            fn name(&self) -> &str {
                "panic_shutdown"
            }
            fn on_shutdown(&self) {
                panic!("boom");
            }
        }
        struct After;
        impl Plugin for After {
            fn name(&self) -> &str {
                "after"
            }
            fn on_shutdown(&self) {
                CALLED.fetch_add(1, Ordering::SeqCst);
            }
        }
        let registry = vec![
            Registration {
                name: "panic_shutdown",
                build: |_| Ok(Box::new(PanicShutdown) as Box<dyn Plugin>),
            },
            Registration {
                name: "after",
                build: |_| Ok(Box::new(After) as Box<dyn Plugin>),
            },
        ];
        let chain =
            PluginChain::build(&configured("[panic_shutdown]\n[after]\n"), &registry).unwrap();
        chain.shutdown();
        assert_eq!(CALLED.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_panicking_new_state_yields_a_skippable_default_state() {
        struct PanicState;
        impl Plugin for PanicState {
            fn name(&self) -> &str {
                "panic_state"
            }
            fn new_state(&self) -> Box<dyn Any + Send> {
                panic!("boom");
            }
            fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
                *ctx.body = Bytes::from_static(b"still served");
                Ok(())
            }
        }
        let registry = vec![Registration {
            name: "panic_state",
            build: |_| Ok(Box::new(PanicState) as Box<dyn Plugin>),
        }];
        let chain = PluginChain::build(&configured("[panic_state]\n"), &registry).unwrap();
        let mut states = chain.states();
        assert_eq!(states.len(), 1, "one state per plugin, even after a panic");
        assert!(
            states[0].is::<()>(),
            "the default state lets the other hooks skip as usual"
        );
        let mut body = Bytes::new();
        request(&chain, &mut states, &mut body).unwrap();
        assert_eq!(&body[..], b"still served");
    }

    #[test]
    fn a_fail_closed_plugin_that_panics_in_new_state_blocks_the_request() {
        struct PanicState;
        impl Plugin for PanicState {
            fn name(&self) -> &str {
                "panic_state"
            }
            fn fails_closed(&self) -> bool {
                true
            }
            fn new_state(&self) -> Box<dyn Any + Send> {
                panic!("boom");
            }
        }
        let registry = vec![Registration {
            name: "panic_state",
            build: |_| Ok(Box::new(PanicState) as Box<dyn Plugin>),
        }];
        let chain =
            Arc::new(PluginChain::build(&configured("[panic_state]\n"), &registry).unwrap());
        let mut session = chain.session();
        assert!(session.failed());

        let method = Method::POST;
        let meta = RequestMeta {
            id: "test",
            stream: "default",
            upstream: "zen",
            method: &method,
            path: "/zen/chat",
        };
        let mut body = Bytes::from_static(b"unmasked secret");
        let blocked = session
            .on_request(&meta, &mut HeaderMap::new(), &mut body)
            .unwrap_err();
        assert_eq!(blocked.status, PLUGIN_FAILURE_STATUS);
        assert_eq!(&body[..], b"unmasked secret");

        // The operator can explicitly accept the risk, in which case the request goes on.
        let chain = Arc::new(
            PluginChain::build(
                &configured("[panic_state]\non_error = \"pass_through\"\n"),
                &registry,
            )
            .unwrap(),
        );
        let mut session = chain.session();
        assert!(!session.failed());
        session
            .on_request(&meta, &mut HeaderMap::new(), &mut body)
            .unwrap();
    }

    #[test]
    fn a_response_hook_panic_in_a_fail_closed_plugin_is_reported_to_the_session() {
        struct PanicResponse;
        impl Plugin for PanicResponse {
            fn name(&self) -> &str {
                "panic_response"
            }
            fn fails_closed(&self) -> bool {
                true
            }
            fn needs_response_hooks(&self) -> bool {
                true
            }
            fn on_response_head(&self, _ctx: &mut ResponseHeadCtx<'_>) {
                panic!("boom");
            }
            fn on_response_chunk(&self, _ctx: &mut ChunkCtx<'_>) {
                panic!("boom");
            }
        }
        let registry = vec![Registration {
            name: "panic_response",
            build: |_| Ok(Box::new(PanicResponse) as Box<dyn Plugin>),
        }];
        let chain =
            Arc::new(PluginChain::build(&configured("[panic_response]\n"), &registry).unwrap());
        let mut session = chain.session();
        let method = Method::POST;
        let meta = RequestMeta {
            id: "test",
            stream: "default",
            upstream: "zen",
            method: &method,
            path: "/zen/chat",
        };
        session
            .on_request(&meta, &mut HeaderMap::new(), &mut Bytes::new())
            .unwrap();

        let mut status = StatusCode::OK;
        assert!(session.on_response_head("k", "zen", &mut status, &mut HeaderMap::new()));
        let mut chunk = Bytes::from_static(b"secret");
        assert!(session.on_response_chunk(&mut chunk, true));
        assert_eq!(status, StatusCode::OK);
    }

    /// A process-wide log capture for the blocking-pool span test. Global because blocking
    /// threads do not inherit the thread-local subscriber the rest of this test binary uses.
    #[derive(Clone)]
    struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

    impl LogBuffer {
        fn clear(&self) {
            self.0.lock().unwrap().clear();
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
        type Writer = LogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            LogWriter(Arc::clone(&self.0))
        }
    }

    fn request_logs() -> &'static LogBuffer {
        static LOGS: std::sync::OnceLock<LogBuffer> = std::sync::OnceLock::new();
        LOGS.get_or_init(|| {
            let logs = LogBuffer(Arc::new(std::sync::Mutex::new(Vec::new())));
            let subscriber = tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .without_time()
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("install the request log capture once");
            logs
        })
    }

    #[tokio::test]
    async fn blocking_hooks_keep_the_request_span_in_their_logs() {
        use tracing::Instrument;

        struct Logger;
        impl Plugin for Logger {
            fn name(&self) -> &str {
                "logger"
            }
            fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
                tracing::info!("blocking hook ran");
                Ok(())
            }
        }

        let logs = request_logs();
        logs.clear();
        let registry = vec![Registration {
            name: "logger",
            build: |_| Ok(Box::new(Logger) as Box<dyn Plugin>),
        }];
        let chain = Arc::new(PluginChain::build(&configured("[logger]\n"), &registry).unwrap());
        let mut session = chain.session();
        let method = Method::POST;
        let meta = RequestMeta {
            id: "span-42",
            stream: "default",
            upstream: "zen",
            method: &method,
            path: "/zen/chat",
        };
        let span = tracing::info_span!("req", id = %meta.id);
        async {
            session
                .on_request_blocking(&meta, &mut HeaderMap::new(), &mut Bytes::new())
                .await
                .unwrap();
        }
        .instrument(span)
        .await;

        let text = logs.text();
        let line = text
            .lines()
            .find(|line| line.contains("blocking hook ran"))
            .unwrap_or_else(|| panic!("the blocking hook log is missing: {text}"));
        assert!(
            line.contains("req{id=span-42}"),
            "the hook lost its request span: {line}"
        );
    }

    #[tokio::test]
    async fn a_cancelled_blocking_session_fails_closed_when_reused() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc::{Receiver, channel};
        use std::time::Duration;

        struct Blocker {
            release: Arc<std::sync::Mutex<Receiver<()>>>,
            calls: &'static AtomicUsize,
        }
        impl Plugin for Blocker {
            fn name(&self) -> &str {
                "blocker"
            }
            fn fails_closed(&self) -> bool {
                true
            }
            fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let _ = self.release.lock().unwrap().recv();
                Ok(())
            }
        }

        static RELEASE: std::sync::OnceLock<Arc<std::sync::Mutex<Receiver<()>>>> =
            std::sync::OnceLock::new();
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let (tx, rx) = channel();
        let _release = RELEASE.get_or_init(|| Arc::new(std::sync::Mutex::new(rx)));
        CALLS.store(0, Ordering::SeqCst);
        let registry = vec![Registration {
            name: "blocker",
            build: |_| {
                Ok(Box::new(Blocker {
                    release: Arc::clone(RELEASE.get().unwrap()),
                    calls: &CALLS,
                }) as Box<dyn Plugin>)
            },
        }];
        let chain = Arc::new(PluginChain::build(&configured("[blocker]\n"), &registry).unwrap());
        let mut session = chain.session();
        let method = Method::POST;
        let meta = RequestMeta {
            id: "cancelled",
            stream: "default",
            upstream: "zen",
            method: &method,
            path: "/zen/chat",
        };

        let blocked = tokio::time::timeout(
            Duration::from_millis(50),
            session.on_request_blocking(&meta, &mut HeaderMap::new(), &mut Bytes::new()),
        )
        .await;
        assert!(
            blocked.is_err(),
            "the hook was supposed to still be running"
        );
        let _ = tx.send(());
        tokio::task::yield_now().await;

        let blocked = session
            .on_request(&meta, &mut HeaderMap::new(), &mut Bytes::new())
            .unwrap_err();
        assert_eq!(blocked.status, PLUGIN_FAILURE_STATUS);
        assert_eq!(blocked.message, PLUGIN_FAILURE_MESSAGE);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "the hook ran again");
    }

    #[cfg(feature = "plugin-placeholders")]
    #[tokio::test(flavor = "current_thread")]
    async fn offloaded_request_hooks_do_not_stall_the_runtime() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let registry = builtin();
        if !registry
            .iter()
            .any(|registration| registration.name == "placeholders")
        {
            return;
        }
        let terms: Vec<String> = (0..300).map(|i| format!("secret{i:06}")).collect();
        let source = format!(
            "[placeholders]\nbuiltin = false\nterms = [{}]\n",
            terms
                .iter()
                .map(|term| format!("\"{term}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let chain = Arc::new(PluginChain::build(&configured(&source), &registry).unwrap());
        let mut session = chain.session();

        let mut text = String::from("{\"text\":\"");
        for i in 0..200_000 {
            text.push_str(&terms[i % terms.len()]);
            text.push(' ');
        }
        text.push_str("\"}");
        let mut body = Bytes::from(text);

        let ticks = Arc::new(AtomicUsize::new(0));
        let heartbeat = {
            let ticks = Arc::clone(&ticks);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    ticks.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        let method = Method::POST;
        let meta = RequestMeta {
            id: "test",
            stream: "default",
            upstream: "zen",
            method: &method,
            path: "/zen/chat",
        };
        session
            .on_request_blocking(&meta, &mut HeaderMap::new(), &mut body)
            .await
            .unwrap();
        heartbeat.abort();
        let _ = heartbeat.await;

        assert!(
            ticks.load(Ordering::Relaxed) > 0,
            "the runtime did not tick while the request hooks ran"
        );
        let masked = String::from_utf8(body.to_vec()).unwrap();
        assert!(!masked.contains("secret"), "body was not masked");
        assert!(
            masked.contains("[[REIKI_TERM_"),
            "masked body has no placeholder"
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
    fn a_masking_plugin_fails_closed_unless_the_operator_says_otherwise() {
        let run = |source: &str| {
            let chain = PluginChain::build(&configured(source), &registry()).unwrap();
            let mut states = chain.states();
            let mut body = Bytes::new();
            request(&chain, &mut states, &mut body)
        };
        let blocked = run("[masker]\n").unwrap_err();
        assert_eq!(blocked.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(run("[masker]\non_error = \"pass_through\"\n").is_ok());
        assert!(
            run("[decline]\n").is_ok(),
            "other plugins keep the pass-through default"
        );
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
        chain.on_response_head(&mut states, &scope, "key-a", "u", &mut status, &mut headers);
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

    #[cfg(feature = "plugin-metrics")]
    #[test]
    fn metrics_alone_still_gets_response_hooks_and_keeps_content_length() {
        let registry = vec![Registration {
            name: reiki_metrics::NAME,
            build: reiki_metrics::build,
        }];
        let chain = Arc::new(PluginChain::build(&configured("[metrics]"), &registry).unwrap());
        assert!(chain.is_empty(), "metrics does not rewrite bodies");
        assert!(chain.needs_response_hooks());

        let mut session = chain.session();
        let method = Method::POST;
        let meta = RequestMeta {
            id: "",
            stream: "s",
            upstream: "u",
            method: &method,
            path: "/x",
        };
        session
            .on_request(&meta, &mut HeaderMap::new(), &mut Bytes::new())
            .unwrap();
        let mut status = StatusCode::OK;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, "42".parse().unwrap());
        session.on_response_head("key-a", "u", &mut status, &mut headers);
        assert!(headers.get(CONTENT_LENGTH).is_some());
        let mut body =
            Bytes::from_static(br#"{"usage":{"prompt_tokens":2,"completion_tokens":3}}"#);
        session.on_response_chunk(&mut body, true);

        let reply = chain
            .on_admin("/usage", KeyCounts::default(), None, vec![])
            .unwrap();
        let usage: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(usage["s"]["u"]["input"], 2);
        assert_eq!(usage["s"]["u"]["output"], 3);
        assert_eq!(usage["s"]["u"]["requests"], 1);
    }

    #[cfg(feature = "plugin-metrics")]
    #[test]
    fn metrics_attribute_tokens_to_the_upstream_that_served_the_response() {
        let registry = vec![Registration {
            name: reiki_metrics::NAME,
            build: reiki_metrics::build,
        }];
        let chain = Arc::new(PluginChain::build(&configured("[metrics]"), &registry).unwrap());
        let mut session = chain.session();
        let method = Method::POST;
        let meta = RequestMeta {
            id: "",
            stream: "s",
            upstream: "first",
            method: &method,
            path: "/x",
        };
        session
            .on_request(&meta, &mut HeaderMap::new(), &mut Bytes::new())
            .unwrap();
        let mut status = StatusCode::OK;
        let mut headers = HeaderMap::new();
        session.on_response_head("fallback-key", "fallback", &mut status, &mut headers);
        let mut body =
            Bytes::from_static(br#"{"usage":{"prompt_tokens":5,"completion_tokens":6}}"#);
        session.on_response_chunk(&mut body, true);

        let reply = chain
            .on_admin("/usage", KeyCounts::default(), None, vec![])
            .unwrap();
        let usage: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(usage["s"]["fallback"]["input"], 5);
        assert_eq!(usage["s"]["fallback"]["output"], 6);
        assert!(usage["s"].get("first").is_none(), "{usage}");
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
        assert_eq!(
            chain.on_admin("/b", keys, None, vec![]).unwrap().body,
            "second:3"
        );
        assert!(chain.on_admin("/missing", keys, None, vec![]).is_none());
    }

    #[test]
    fn two_plugins_cannot_claim_one_admin_path_or_reiki_s_own() {
        let clash = admin_chain(vec![admin("a", vec!["/m"]), admin("b", vec!["/m"])]);
        assert!(matches!(clash, Err(ConfigError::PluginConfig { name, .. }) if name == "b"));
        for path in [
            "/status",
            "/reload",
            "/keys/enable",
            "/failures",
            "/v1/status",
            "/v1/reload",
            "/v1/keys/enable",
            "/v1/failures",
            "/v1/openapi.json",
        ] {
            let reserved = admin_chain(vec![admin("a", vec![path])]);
            assert!(
                matches!(reserved, Err(ConfigError::PluginConfig { .. })),
                "{path}"
            );
        }
    }

    #[test]
    fn plugin_admin_paths_must_be_normalised() {
        for path in [
            "metrics",
            "/metrics/",
            "/",
            "/a/../b",
            "/a/./b",
            "/a//b",
            "/metrics%2Fv1",
        ] {
            let result = admin_chain(vec![admin("a", vec![path])]);
            assert!(
                matches!(result, Err(ConfigError::PluginConfig { .. })),
                "{path}"
            );
        }
        assert!(admin_chain(vec![admin("a", vec!["/metrics/v1"])]).is_ok());
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
            scope: reiki_plugin::CooldownScope::Key,
        });
        assert_eq!(count(), 0);
        chain.on_event(&Event::RateLimited { stream: "one" });
        assert_eq!(count(), 1);
        let keys = KeyCounts::default();
        assert!(
            chain.on_admin("/scoped", keys, None, vec![]).is_some(),
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
