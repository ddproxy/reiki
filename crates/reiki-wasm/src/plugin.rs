use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::StatusCode;
use reiki_plugin::{Plugin as HostPlugin, PluginError, RequestCtx};
use serde_json::Value as JsonValue;
use wasmtime::component::{HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Store, StoreLimits, StoreLimitsBuilder, Trap};

use crate::capability::Capability;
use crate::config::{EffectiveLimits, WasmPluginConfig};
use crate::host::{EPOCH_TICK_MS, LoadedComponent, WasmEngine};
use crate::manifest::Manifest;

wasmtime::component::bindgen!({
    world: "plugin",
    path: "wit",
    with: {
        "reiki:plugin/request-meta@1.0.0.request": HostRequest,
    },
    imports: { default: trappable },
});

use reiki::plugin::types;

pub struct HostRequest {
    pub stream: String,
    pub upstream: String,
    pub method: String,
    pub path: String,
    pub body: Bytes,
}

#[derive(Debug, Clone)]
enum TrapKind {
    Fuel,
    Memory,
    Epoch,
    Other(String),
}

struct HostState {
    table: ResourceTable,
    wasi: wasmtime_wasi::WasiCtx,
    limits: StoreLimits,
    manifest: Manifest,
    grants: BTreeSet<Capability>,
    config_toml: Vec<u8>,
    start: Instant,
    deadline: Duration,
    request: Option<HostRequest>,
    request_state: BTreeMap<String, Vec<u8>>,
    findings: Vec<reiki_plugin::Reported>,
    poisoned: bool,
    trap: Option<TrapKind>,
    log_count: u32,
    finding_count: u32,
}

impl HostState {
    fn check_deadline(&mut self) -> Result<(), types::DenyReason> {
        if self.start.elapsed() > self.deadline {
            self.poisoned = true;
            self.trap = Some(TrapKind::Epoch);
            return Err(types::DenyReason::LimitExceeded);
        }
        Ok(())
    }
}

impl wasmtime_wasi::WasiView for HostState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

fn classify_trap(err: &wasmtime::Error) -> TrapKind {
    // Wasmtime wraps traps in context, so the top-level error is not always the
    // Trap itself. Search the whole cause chain before falling back to text.
    for cause in err.chain() {
        if let Some(trap) = cause.downcast_ref::<Trap>() {
            return match *trap {
                Trap::OutOfFuel => TrapKind::Fuel,
                Trap::Interrupt => TrapKind::Epoch,
                _ => TrapKind::Other(trap.to_string()),
            };
        }
    }
    let msg = err
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    let lower = msg.to_ascii_lowercase();
    if lower.contains("out of fuel") {
        TrapKind::Fuel
    } else if lower.contains("interrupt") || lower.contains("epoch") {
        TrapKind::Epoch
    } else if lower.contains("forcing trap when growing memory")
        || (lower.contains("memory")
            && (lower.contains("grow")
                || lower.contains("limit")
                || lower.contains("maximum")
                || lower.contains("size")))
    {
        TrapKind::Memory
    } else {
        TrapKind::Other(msg)
    }
}

fn trap_status(kind: &TrapKind) -> StatusCode {
    match kind {
        TrapKind::Fuel | TrapKind::Epoch | TrapKind::Memory => StatusCode::SERVICE_UNAVAILABLE,
        TrapKind::Other(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

fn trap_message(kind: &TrapKind) -> &'static str {
    match kind {
        TrapKind::Fuel => "wasm plugin exceeded fuel limit",
        TrapKind::Memory => "wasm plugin exceeded memory limit",
        TrapKind::Epoch => "wasm plugin exceeded deadline",
        TrapKind::Other(_) => "wasm plugin trapped",
    }
}

/// A loaded runtime Wasm plugin implementing the host Plugin trait.
pub struct WasmPlugin {
    name: String,
    engine: Arc<WasmEngine>,
    component: wasmtime::component::Component,
    manifest: Manifest,
    limits: EffectiveLimits,
    grants: BTreeSet<Capability>,
    config: WasmPluginConfig,
    role_security: bool,
    linker: Linker<HostState>,
}

impl WasmPlugin {
    pub fn load(engine: Arc<WasmEngine>, config: WasmPluginConfig) -> Result<Self, String> {
        let loaded = engine.load_component(&config)?;
        Self::from_loaded(engine, loaded)
    }

    pub fn from_loaded(engine: Arc<WasmEngine>, loaded: LoadedComponent) -> Result<Self, String> {
        let grants = loaded.config.grants.clone();
        let manifest = loaded.manifest().clone();
        let limits = loaded.limits;
        let name = loaded.config.name.clone();
        let role_security = loaded.config.role == Some(crate::config::Role::Security);
        let config = loaded.config.clone();
        let component = loaded.component;

        let mut linker = Linker::<HostState>::new(engine.engine());
        // Inert WASI p2 - no preopens, empty env/args, stdio sink.
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| format!("link wasi: {e}"))?;

        // Host interfaces
        reiki::plugin::types::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| format!("link types: {e}"))?;
        reiki::plugin::request_meta::add_to_linker::<HostState, HasSelf<HostState>>(
            &mut linker,
            |s| s,
        )
        .map_err(|e| format!("link request-meta: {e}"))?;
        reiki::plugin::request_body_read::add_to_linker::<HostState, HasSelf<HostState>>(
            &mut linker,
            |s| s,
        )
        .map_err(|e| format!("link request-body-read: {e}"))?;
        reiki::plugin::request_body_edit::add_to_linker::<HostState, HasSelf<HostState>>(
            &mut linker,
            |s| s,
        )
        .map_err(|e| format!("link request-body-edit: {e}"))?;
        reiki::plugin::finding::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| format!("link finding: {e}"))?;
        reiki::plugin::finding_detail_vocab::add_to_linker::<HostState, HasSelf<HostState>>(
            &mut linker,
            |s| s,
        )
        .map_err(|e| format!("link finding-detail-vocab: {e}"))?;
        reiki::plugin::log::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| format!("link log: {e}"))?;
        reiki::plugin::reject::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| format!("link reject: {e}"))?;
        reiki::plugin::clock::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| format!("link clock: {e}"))?;
        reiki::plugin::random::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| format!("link random: {e}"))?;
        reiki::plugin::request_state::add_to_linker::<HostState, HasSelf<HostState>>(
            &mut linker,
            |s| s,
        )
        .map_err(|e| format!("link request-state: {e}"))?;

        let plugin = Self {
            name,
            engine,
            component,
            manifest,
            limits,
            grants,
            config,
            role_security,
            linker,
        };
        plugin.run_init()?;
        Ok(plugin)
    }

    fn make_state(&self) -> HostState {
        let limits = StoreLimitsBuilder::new()
            .memory_size((self.limits.memory_mib * 1024 * 1024) as usize)
            // Without this, memory.grow returns -1 and the guest allocator aborts with
            // `unreachable`, which the host cannot tell apart from a guest trap.
            .trap_on_grow_failure(true)
            .build();
        // Inert WASI p2: no preopens, empty env/args, stdio sink.
        let wasi = wasmtime_wasi::WasiCtxBuilder::new().build();
        let deadline = Duration::from_millis(self.limits.deadline_ms);
        let config_toml = toml::to_string(&self.config.config)
            .unwrap_or_default()
            .into_bytes();
        HostState {
            table: ResourceTable::new(),
            wasi,
            limits,
            manifest: self.manifest.clone(),
            grants: self.grants.clone(),
            config_toml,
            start: Instant::now(),
            deadline,
            request: None,
            request_state: BTreeMap::new(),
            findings: Vec::new(),
            poisoned: false,
            trap: None,
            log_count: 0,
            finding_count: 0,
        }
    }

    fn make_store(&self, body_len: usize) -> Store<HostState> {
        let mut store = Store::new(self.engine.engine(), self.make_state());
        store.limiter(|s| &mut s.limits);
        let fuel = self.limits.fuel_for_body(body_len);
        let _ = store.set_fuel(fuel);
        store.set_epoch_deadline(Self::budget_ticks(&self.limits));
        store.epoch_deadline_trap();
        store
    }

    /// Ticks until a store traps, derived from the tightest configured time budget.
    /// The engine ticks every 10 ms, so this also bounds a pure-Wasm loop that
    /// never calls a checked host function.
    fn budget_ticks(limits: &EffectiveLimits) -> u64 {
        limits
            .deadline_ms
            .min(limits.wall_budget_ms)
            .div_ceil(EPOCH_TICK_MS)
            .max(1)
    }

    fn run_init(&self) -> Result<(), String> {
        let mut store = self.make_store(0);
        let config_bytes = toml::to_string(&self.config.config)
            .unwrap_or_default()
            .into_bytes();
        let instance = self
            .linker
            .instantiate(&mut store, &self.component)
            .map_err(|e| format!("instantiate {}: {e}", self.name))?;
        let plugin =
            Plugin::new(&mut store, &instance).map_err(|e| format!("bind {}: {e}", self.name))?;
        let result = plugin
            .call_init(&mut store, &config_bytes)
            .map_err(|e| format!("init {}: {e}", self.name, e = self.format_err(&store, e)))?;
        if let Some(kind) = store.data().trap.clone() {
            return Err(format!("init {}: {}", self.name, trap_message(&kind)));
        }
        match result {
            Ok(()) => Ok(()),
            Err(msg) => Err(format!(
                "init {} failed: {}",
                self.name,
                sanitize_reason(&msg)
            )),
        }
    }

    fn format_err(&self, _store: &Store<HostState>, err: wasmtime::Error) -> String {
        let kind = classify_trap(&err);
        match kind {
            TrapKind::Fuel => "fuel exhausted".to_owned(),
            TrapKind::Epoch => "deadline exceeded".to_owned(),
            TrapKind::Memory => "memory limit exceeded".to_owned(),
            TrapKind::Other(m) => m,
        }
    }

    fn on_request_impl(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        let body_len = ctx.body.len();
        if body_len > (self.limits.max_body_kib as usize * 1024) {
            return Err(self.fail_closed_error("request body exceeds wasm max-body-kib"));
        }
        self.enforce_tool_allowlist(ctx)?;

        let mut store = self.make_store(body_len);
        store.data_mut().request = Some(HostRequest {
            stream: ctx.stream.to_owned(),
            upstream: ctx.upstream.to_owned(),
            method: ctx.method.as_str().to_owned(),
            path: ctx.path.to_owned(),
            body: ctx.body.clone(),
        });

        let instance = match self.linker.instantiate(&mut store, &self.component) {
            Ok(i) => i,
            Err(e) => return Err(self.trap_to_error(&store, e)),
        };
        let bindings = match Plugin::new(&mut store, &instance) {
            Ok(p) => p,
            Err(e) => return Err(self.trap_to_error(&store, e)),
        };

        let config_bytes = store.data().config_toml.clone();
        let init_res = match bindings.call_init(&mut store, &config_bytes) {
            Ok(r) => r,
            Err(e) => return Err(self.trap_to_error(&store, e)),
        };
        if let Some(kind) = store.data().trap.clone() {
            return Err(self.trap_kind_to_error(&kind));
        }
        if let Err(msg) = init_res {
            return Err(self.fail_closed_error(&format!("wasm init: {msg}")));
        }

        let req_res = store.data_mut().table.push(HostRequest {
            stream: ctx.stream.to_owned(),
            upstream: ctx.upstream.to_owned(),
            method: ctx.method.as_str().to_owned(),
            path: ctx.path.to_owned(),
            body: ctx.body.clone(),
        });
        let req_handle = match req_res {
            Ok(h) => h,
            Err(e) => return Err(self.fail_closed_error(&format!("resource table: {e}"))),
        };

        let call_res = match bindings.call_on_request(&mut store, req_handle) {
            Ok(r) => r,
            Err(e) => return Err(self.trap_to_error(&store, e)),
        };
        // A deadline reached inside a host call poisons the store without raising a
        // Wasmtime trap. Do not let a guest that ignores the deny reason pass.
        if let Some(kind) = store.data().trap.clone() {
            return Err(self.trap_kind_to_error(&kind));
        }

        // Collect findings
        let findings = std::mem::take(&mut store.data_mut().findings);
        for f in findings {
            ctx.reported.push(f);
        }

        let (verdict, edit) = match call_res {
            Ok(pair) => pair,
            Err(msg) => return Err(self.fail_closed_error(&format!("wasm on-request: {msg}"))),
        };

        if let Some(edit) = edit {
            if !self.grants.contains(&Capability::RequestBodyEdit) {
                return Err(self.fail_closed_error(
                    "wasm plugin returned edits without request-body:edit grant",
                ));
            }
            self.apply_edits(ctx, &edit, &mut store)?;
        }

        match verdict {
            types::Verdict::Pass => Ok(()),
            types::Verdict::Skip(msg) => {
                let msg = sanitize_reason(&msg);
                Err(PluginError::Skip(msg))
            }
            types::Verdict::Reject((status, template_id)) => {
                let (status, message) = self.resolve_reject(status, &template_id)?;
                Err(PluginError::Reject(status, message))
            }
        }
    }

    fn enforce_tool_allowlist(&self, ctx: &RequestCtx<'_>) -> Result<(), PluginError> {
        let Some(allowlist) = &self.config.tool_allowlist else {
            return Ok(());
        };
        match reiki_toolpolicy::parse::parse_request(ctx.path, ctx.body.as_ref()) {
            reiki_toolpolicy::parse::RequestParse::Unrelated => Ok(()),
            reiki_toolpolicy::parse::RequestParse::Unparsed(_) => {
                // An unparseable request on a tool route cannot be checked; the
                // allowlist must reject it regardless of the plugin's error mode.
                Err(PluginError::Reject(
                    StatusCode::FORBIDDEN,
                    "request-side tool allowlist could not parse recognized request".to_owned(),
                ))
            }
            reiki_toolpolicy::parse::RequestParse::Recognized { declared, .. } => {
                for tool in &declared {
                    let allowed = allowlist.iter().any(|a| a.eq_ignore_ascii_case(tool));
                    if !allowed {
                        let sanitized = sanitize_tool_name(tool);
                        tracing::warn!(
                            plugin = %self.name,
                            tool = %sanitized,
                            "request-side tool allowlist rejected declared tool"
                        );
                        return Err(PluginError::Reject(
                            StatusCode::FORBIDDEN,
                            format!(
                                "request-side tool allowlist rejected declared tool: {sanitized}"
                            ),
                        ));
                    }
                }
                Ok(())
            }
        }
    }

    fn trap_to_error(&self, _store: &Store<HostState>, err: wasmtime::Error) -> PluginError {
        let kind = classify_trap(&err);
        self.trap_kind_to_error(&kind)
    }

    fn trap_kind_to_error(&self, kind: &TrapKind) -> PluginError {
        tracing::warn!(plugin = %self.name, kind = ?kind, "wasm plugin trapped");
        if self.role_security || self.config.on_error.as_deref() == Some("reject") {
            PluginError::Reject(trap_status(kind), trap_message(kind).to_owned())
        } else {
            PluginError::Skip(trap_message(kind).to_owned())
        }
    }

    fn fail_closed_error(&self, msg: &str) -> PluginError {
        let msg = sanitize_reason(msg);
        tracing::warn!(plugin = %self.name, "{msg}");
        let reject = self.role_security
            || self.config.on_error.as_deref() == Some("reject")
            || self.fails_closed();
        fail_closed_result(&msg, reject)
    }

    fn resolve_reject(
        &self,
        status: u16,
        template_id: &str,
    ) -> Result<(StatusCode, String), PluginError> {
        let Some(tpl) = self.manifest.reject_templates.get(template_id) else {
            return Err(self.fail_closed_error(&format!("unknown reject template `{template_id}`")));
        };
        if tpl.status != status {
            return Err(self.fail_closed_error(&format!(
                "reject template `{template_id}` status mismatch: plugin returned {status}, manifest has {}",
                tpl.status
            )));
        }
        let status = StatusCode::from_u16(status)
            .map_err(|_| self.fail_closed_error(&format!("invalid reject status {status}")))?;
        Ok((status, tpl.message.clone()))
    }

    fn apply_edits(
        &self,
        ctx: &mut RequestCtx<'_>,
        edit: &types::RequestEdit,
        store: &mut Store<HostState>,
    ) -> Result<(), PluginError> {
        apply_edits_inner(
            ctx,
            edit,
            &self.manifest,
            || {
                let state = store.data_mut();
                if state.poisoned {
                    return Err(self.fail_closed_error("wasm plugin poisoned during edit"));
                }
                if state.check_deadline().is_err() {
                    return Err(self.trap_kind_to_error(&TrapKind::Epoch));
                }
                Ok(())
            },
            |msg| self.fail_closed_error(msg),
        )
    }
}

const MAX_EDIT_OPS: usize = 256;

fn apply_edits_inner<C, F>(
    ctx: &mut RequestCtx<'_>,
    edit: &types::RequestEdit,
    manifest: &Manifest,
    mut check: C,
    fail: F,
) -> Result<(), PluginError>
where
    C: FnMut() -> Result<(), PluginError>,
    F: Fn(&str) -> PluginError,
{
    if edit.ops.is_empty() {
        return Ok(());
    }
    if edit.ops.len() > MAX_EDIT_OPS {
        return Err(fail("wasm edit has too many operations"));
    }
    check()?;
    let mut value: JsonValue = serde_json::from_slice(ctx.body.as_ref())
        .map_err(|_| fail("request body is not valid JSON for wasm edit"))?;
    for op in &edit.ops {
        check()?;
        apply_edit_op(&mut value, op, manifest).map_err(|e| fail(&e))?;
    }
    let new_body = serde_json::to_vec(&value)
        .map_err(|e| fail(&format!("edited body is not serializable: {e}")))?;
    *ctx.body = Bytes::from(new_body);
    Ok(())
}

fn fail_closed_result(msg: &str, reject: bool) -> PluginError {
    let msg = sanitize_reason(msg);
    if reject {
        PluginError::Reject(
            StatusCode::SERVICE_UNAVAILABLE,
            "wasm plugin failed".to_owned(),
        )
    } else {
        PluginError::Skip(msg.to_owned())
    }
}

fn sanitize_reason(msg: &str) -> String {
    let out: String = msg.chars().filter(|c| !c.is_control()).take(200).collect();
    if out.trim().is_empty() {
        "skipped".to_owned()
    } else {
        out
    }
}

fn sanitize_tool_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/'))
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_owned()
    } else {
        cleaned
    }
}

fn apply_edit_op(
    value: &mut JsonValue,
    op: &types::EditOp,
    manifest: &Manifest,
) -> Result<(), String> {
    match op {
        types::EditOp::Remove(ptr) => {
            json_remove(value, ptr)?;
        }
        types::EditOp::Set((ptr, lit)) => {
            let lit_val = resolve_literal(manifest, lit)?;
            json_set(value, ptr, lit_val)?;
        }
        types::EditOp::Mask((ptr, lit)) => {
            let lit_val = resolve_literal(manifest, lit)?;
            json_set(value, ptr, lit_val)?;
        }
    }
    Ok(())
}

fn resolve_literal(manifest: &Manifest, id: &str) -> Result<JsonValue, String> {
    let Some(lit) = manifest.literals.get(id) else {
        return Err(format!("unknown literal `{id}`"));
    };
    Ok(JsonValue::String(lit.clone()))
}

fn json_remove(root: &mut JsonValue, pointer: &str) -> Result<(), String> {
    let segments = parse_pointer(pointer)?;
    if segments.is_empty() {
        return Err("cannot remove root document".to_owned());
    }
    let (parent_segs, last) = segments.split_at(segments.len() - 1);
    let parent =
        json_get_mut(root, parent_segs).ok_or_else(|| format!("path not found: {pointer}"))?;
    match parent {
        JsonValue::Object(map) => {
            if map.remove(&last[0]).is_none() {
                return Err(format!("path not found: {pointer}"));
            }
        }
        JsonValue::Array(arr) => {
            let idx = parse_index(&last[0], arr.len(), true)?;
            if idx >= arr.len() {
                return Err(format!("path not found: {pointer}"));
            }
            arr.remove(idx);
        }
        _ => return Err(format!("path not found: {pointer}")),
    }
    Ok(())
}

fn json_set(root: &mut JsonValue, pointer: &str, value: JsonValue) -> Result<(), String> {
    let segments = parse_pointer(pointer)?;
    if segments.is_empty() {
        *root = value;
        return Ok(());
    }
    let (parent_segs, last) = segments.split_at(segments.len() - 1);
    let parent =
        json_get_mut(root, parent_segs).ok_or_else(|| format!("path not found: {pointer}"))?;
    match parent {
        JsonValue::Object(map) => {
            map.insert(last[0].clone(), value);
        }
        JsonValue::Array(arr) => {
            let idx = parse_index(&last[0], arr.len(), false)?;
            if last[0] == "-" || idx == arr.len() {
                arr.push(value);
            } else if idx < arr.len() {
                arr[idx] = value;
            } else {
                return Err(format!("path not found: {pointer}"));
            }
        }
        _ => return Err(format!("path not found: {pointer}")),
    }
    Ok(())
}

fn parse_pointer(pointer: &str) -> Result<Vec<String>, String> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    if !pointer.starts_with('/') {
        return Err(format!("invalid JSON pointer `{pointer}`"));
    }
    pointer[1..]
        .split('/')
        .map(|seg| {
            let mut out = String::with_capacity(seg.len());
            let mut chars = seg.chars();
            while let Some(c) = chars.next() {
                if c == '~' {
                    match chars.next() {
                        Some('0') => out.push('~'),
                        Some('1') => out.push('/'),
                        _ => return Err(format!("invalid JSON pointer escape in `{pointer}`")),
                    }
                } else {
                    out.push(c);
                }
            }
            Ok(out)
        })
        .collect()
}

fn parse_index(seg: &str, len: usize, for_remove: bool) -> Result<usize, String> {
    if seg == "-" {
        if for_remove {
            return Err("cannot remove array append index `-`".to_owned());
        }
        return Ok(len);
    }
    if seg.starts_with('+') {
        return Err(format!("invalid array index `{seg}`"));
    }
    if seg.len() > 1 && seg.starts_with('0') {
        return Err(format!("invalid array index `{seg}`"));
    }
    let idx: usize = seg
        .parse()
        .map_err(|_| format!("invalid array index `{seg}`"))?;
    if idx > len || (for_remove && idx == len) {
        return Err(format!("array index out of bounds `{seg}`"));
    }
    Ok(idx)
}

fn json_get_mut<'a>(root: &'a mut JsonValue, segments: &[String]) -> Option<&'a mut JsonValue> {
    let mut cur = root;
    for seg in segments {
        cur = match cur {
            JsonValue::Object(map) => map.get_mut(seg)?,
            JsonValue::Array(arr) => {
                let idx = parse_index(seg, arr.len(), true).ok()?;
                arr.get_mut(idx)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

// Host trait implementations

impl reiki::plugin::types::Host for HostState {}
impl reiki::plugin::request_meta::Host for HostState {}

impl reiki::plugin::request_meta::HostRequest for HostState {
    fn stream_name(&mut self, rep: Resource<HostRequest>) -> Result<String, wasmtime::Error> {
        let r = self.table.get(&rep)?;
        Ok(r.stream.clone())
    }
    fn upstream(&mut self, rep: Resource<HostRequest>) -> Result<String, wasmtime::Error> {
        let r = self.table.get(&rep)?;
        Ok(r.upstream.clone())
    }
    fn method(&mut self, rep: Resource<HostRequest>) -> Result<String, wasmtime::Error> {
        let r = self.table.get(&rep)?;
        Ok(r.method.clone())
    }
    fn path(&mut self, rep: Resource<HostRequest>) -> Result<String, wasmtime::Error> {
        let r = self.table.get(&rep)?;
        Ok(r.path.clone())
    }
    fn drop(&mut self, rep: Resource<HostRequest>) -> wasmtime::Result<()> {
        let _ = self.table.delete(rep)?;
        Ok(())
    }
}

impl reiki::plugin::request_body_read::Host for HostState {
    fn body(
        &mut self,
        req: Resource<HostRequest>,
    ) -> Result<Result<Vec<u8>, types::DenyReason>, wasmtime::Error> {
        if !self.grants.contains(&Capability::RequestBodyRead)
            && !self.grants.contains(&Capability::RequestBodyEdit)
        {
            return Ok(Err(types::DenyReason::NotGranted));
        }
        if self.poisoned {
            return Ok(Err(types::DenyReason::Poisoned));
        }
        if self.check_deadline().is_err() {
            return Ok(Err(types::DenyReason::LimitExceeded));
        }
        let r = self.table.get(&req)?;
        Ok(Ok(r.body.to_vec()))
    }
}

impl reiki::plugin::request_body_edit::Host for HostState {
    fn enabled(&mut self) -> Result<bool, wasmtime::Error> {
        Ok(self.grants.contains(&Capability::RequestBodyEdit))
    }
}

impl reiki::plugin::finding::Host for HostState {
    fn report(&mut self, f: types::Finding) -> Result<(), wasmtime::Error> {
        if self.poisoned || self.check_deadline().is_err() {
            return Ok(());
        }
        self.finding_count += 1;
        if self.finding_count > 256 {
            self.poisoned = true;
            return Ok(());
        }
        // Sanitize kind and detail
        let kind = sanitize_label(&f.kind, 64);
        if kind.is_empty() {
            return Ok(());
        }
        let detail = f.detail.and_then(|d| {
            if self.grants.contains(&Capability::FindingDetailVocab) {
                if !self.manifest.details.iter().any(|v| v == &d) {
                    // invalid vocab detail - poison? or drop; safer to drop detail
                    return None;
                }
                Some(d)
            } else {
                // default-allowed finding can still carry detail? spec says finding:detail-vocab gated
                // If not granted, drop detail
                None
            }
        });
        self.findings.push(reiki_plugin::Reported {
            kind: intern_kind(kind),
            detail,
            count: f.count as usize,
            blocked: f.blocked,
        });
        Ok(())
    }
}

const MAX_INTERNED_KINDS: usize = 1024;
const UNKNOWN_KIND: &str = "wasm.unknown";

fn intern_kind(kind: String) -> &'static str {
    static KINDS: OnceLock<Mutex<BTreeSet<&'static str>>> = OnceLock::new();
    let kinds = KINDS.get_or_init(|| Mutex::new(BTreeSet::new()));
    let mut kinds = kinds.lock().unwrap_or_else(|e| e.into_inner());
    intern_kind_in(&mut kinds, kind, MAX_INTERNED_KINDS)
}

fn intern_kind_in(kinds: &mut BTreeSet<&'static str>, kind: String, limit: usize) -> &'static str {
    if let Some(interned) = kinds.get(kind.as_str()) {
        return interned;
    }
    if kinds.len() >= limit {
        return UNKNOWN_KIND;
    }
    let interned: &'static str = Box::leak(kind.into_boxed_str());
    kinds.insert(interned);
    interned
}

fn sanitize_label(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '*' | '/' | '-'))
        .take(max)
        .collect()
}

impl reiki::plugin::finding_detail_vocab::Host for HostState {
    fn allowed(&mut self, detail: String) -> Result<bool, wasmtime::Error> {
        if !self.grants.contains(&Capability::FindingDetailVocab) {
            return Ok(false);
        }
        Ok(self.manifest.details.iter().any(|d| d == &detail))
    }
}

impl reiki::plugin::log::Host for HostState {
    fn log(
        &mut self,
        level: u8,
        template_id: String,
        args: Vec<u32>,
    ) -> Result<(), wasmtime::Error> {
        if self.poisoned || self.check_deadline().is_err() {
            return Ok(());
        }
        self.log_count += 1;
        if self.log_count > 256 {
            self.poisoned = true;
            return Ok(());
        }
        let Some(tpl) = self.manifest.log_templates.get(&template_id) else {
            self.poisoned = true;
            return Ok(());
        };
        let rendered = render_template(tpl, &args);
        match level {
            0 => tracing::error!(plugin = "wasm", template = %template_id, "{rendered}"),
            1 => tracing::warn!(plugin = "wasm", template = %template_id, "{rendered}"),
            2 => tracing::info!(plugin = "wasm", template = %template_id, "{rendered}"),
            3 => tracing::debug!(plugin = "wasm", template = %template_id, "{rendered}"),
            _ => tracing::trace!(plugin = "wasm", template = %template_id, "{rendered}"),
        }
        Ok(())
    }
}

fn render_template(tpl: &str, args: &[u32]) -> String {
    let mut out = String::new();
    let mut chars = tpl.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut num = String::new();
            while let Some(d) = chars.peek() {
                if d.is_ascii_digit() {
                    num.push(*d);
                    chars.next();
                } else {
                    break;
                }
            }
            if chars.peek() == Some(&'}') {
                chars.next();
                if let Ok(idx) = num.parse::<usize>() {
                    if let Some(v) = args.get(idx) {
                        out.push_str(&v.to_string());
                    } else {
                        out.push_str("{?}");
                    }
                } else {
                    out.push('{');
                    out.push_str(&num);
                    out.push('}');
                }
            } else {
                out.push('{');
                out.push_str(&num);
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl reiki::plugin::reject::Host for HostState {
    fn has(&mut self, template_id: String) -> Result<bool, wasmtime::Error> {
        Ok(self.manifest.reject_templates.contains_key(&template_id))
    }
}

impl reiki::plugin::clock::Host for HostState {
    fn now_ms(&mut self) -> Result<u64, wasmtime::Error> {
        if self.poisoned {
            return Ok(0);
        }
        Ok(self.start.elapsed().as_millis() as u64)
    }
}

impl reiki::plugin::random::Host for HostState {
    fn fill(&mut self, len: u32) -> Result<Vec<u8>, wasmtime::Error> {
        let len = len.min(1024) as usize;
        let mut buf = vec![0u8; len];
        random_fill(&mut buf).map_err(csprng_error)?;
        Ok(buf)
    }
    fn next_u64(&mut self) -> Result<u64, wasmtime::Error> {
        let mut buf = [0u8; 8];
        random_fill(&mut buf).map_err(csprng_error)?;
        Ok(u64::from_le_bytes(buf))
    }
}

fn random_fill(buf: &mut [u8]) -> Result<(), String> {
    getrandom::fill(buf).map_err(|e| format!("host CSPRNG unavailable: {e}"))
}

fn csprng_error(msg: String) -> wasmtime::Error {
    wasmtime::Error::new(std::io::Error::other(msg))
}

impl reiki::plugin::request_state::Host for HostState {
    fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, wasmtime::Error> {
        if self.poisoned || self.check_deadline().is_err() {
            return Ok(None);
        }
        Ok(self.request_state.get(&key).cloned())
    }
    fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), wasmtime::Error> {
        if self.poisoned || self.check_deadline().is_err() {
            return Ok(());
        }
        if key.len() > 256 || value.len() > 16 * 1024 {
            self.poisoned = true;
            return Ok(());
        }
        if self.request_state.len() >= 64 && !self.request_state.contains_key(&key) {
            self.poisoned = true;
            return Ok(());
        }
        self.request_state.insert(key, value);
        Ok(())
    }
}

impl HostPlugin for WasmPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn fails_closed(&self) -> bool {
        self.role_security || self.config.on_error.as_deref() == Some("reject")
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        self.on_request_impl(ctx)
    }

    fn uses_bodies(&self) -> bool {
        self.grants.contains(&Capability::RequestBodyRead)
            || self.grants.contains(&Capability::RequestBodyEdit)
    }

    fn live_reload(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_pointer_remove_and_set() {
        let mut v: JsonValue = serde_json::from_str(r#"{"a":{"b":[1,2,3]},"x":"y"}"#).unwrap();
        json_remove(&mut v, "/a/b/1").unwrap();
        assert_eq!(v["a"]["b"], serde_json::json!([1, 3]));
        json_set(&mut v, "/x", serde_json::json!("z")).unwrap();
        assert_eq!(v["x"], "z");
    }

    #[test]
    fn render_template_replaces_args() {
        assert_eq!(render_template("hit {0} of {1}", &[2, 5]), "hit 2 of 5");
    }

    #[test]
    fn epoch_deadline_uses_the_tightest_budget() {
        let limits = EffectiveLimits {
            memory_mib: 16,
            fuel_per_call: 10_000_000,
            fuel_per_kib: 200_000,
            deadline_ms: 250,
            wall_budget_ms: 1_000,
            max_body_kib: 1024,
        };
        assert_eq!(WasmPlugin::budget_ticks(&limits), 25);
        let limits = EffectiveLimits {
            wall_budget_ms: 100,
            ..limits
        };
        assert_eq!(WasmPlugin::budget_ticks(&limits), 10);
    }

    #[test]
    fn finding_kinds_are_interned_with_a_bounded_table() {
        let mut kinds = BTreeSet::new();
        let first = intern_kind_in(&mut kinds, "wasm.rule".to_owned(), 2);
        assert_eq!(first, intern_kind_in(&mut kinds, "wasm.rule".to_owned(), 2));
        intern_kind_in(&mut kinds, "wasm.edit".to_owned(), 2);
        assert_eq!(
            intern_kind_in(&mut kinds, "wasm.other".to_owned(), 2),
            UNKNOWN_KIND
        );
    }

    #[test]
    fn guest_messages_are_sanitized_before_a_skip() {
        let err = fail_closed_result("line\n\x1b[31mreason", false);
        match err {
            PluginError::Skip(msg) => assert_eq!(msg, "line[31mreason"),
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn edit_ops_are_capped_and_checked_before_each_op() {
        let manifest = Manifest::parse(
            r#"
[plugin]
name = "t"
version = "0.1.0"
wit-world = "reiki:plugin/plugin@1.0.0"
"#,
        )
        .unwrap();
        let method = http::Method::POST;
        let mut headers = http::HeaderMap::new();
        let mut body = Bytes::from_static(br#"{"a":1}"#);
        let mut state = ();
        let mut ctx = RequestCtx::new("s", "u", &method, "/x", &mut headers, &mut body, &mut state);

        let too_many = types::RequestEdit {
            ops: (0..=MAX_EDIT_OPS)
                .map(|_| types::EditOp::Remove("/a".to_owned()))
                .collect(),
        };
        let mut checks = 0;
        let result = apply_edits_inner(
            &mut ctx,
            &too_many,
            &manifest,
            || {
                checks += 1;
                Ok(())
            },
            |msg| PluginError::Skip(msg.to_owned()),
        );
        assert!(matches!(result, Err(PluginError::Skip(msg)) if msg.contains("too many")));
        assert_eq!(checks, 0);

        let three = types::RequestEdit {
            ops: (0..3)
                .map(|_| types::EditOp::Remove("/a".to_owned()))
                .collect(),
        };
        let mut checks = 0;
        let result = apply_edits_inner(
            &mut ctx,
            &three,
            &manifest,
            || {
                checks += 1;
                if checks == 3 {
                    Err(PluginError::Skip("poisoned".to_owned()))
                } else {
                    Ok(())
                }
            },
            |msg| PluginError::Skip(msg.to_owned()),
        );
        assert!(result.is_err());
        assert_eq!(checks, 3);
        assert_eq!(&body[..], br#"{"a":1}"#);
    }

    #[test]
    fn random_fill_uses_the_host_csprng() {
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        random_fill(&mut first).unwrap();
        random_fill(&mut second).unwrap();
        assert_ne!(first, second);
    }
}
