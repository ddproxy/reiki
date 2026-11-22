use std::collections::BTreeSet;
use std::path::PathBuf;

use reiki_plugin::HOST_KEYS;
use toml::Value;

use crate::capability::Capability;
use crate::manifest::{HostCeilings, ManifestLimits};

/// Operator config for a runtime Wasm plugin (`[plugins.<name>]`).
#[derive(Debug, Clone)]
pub struct WasmPluginConfig {
    pub name: String,
    pub wasm: PathBuf,
    pub sha256: String,
    pub role: Option<Role>,
    pub on_error: Option<String>,
    pub streams: Option<Vec<String>>,
    pub grants: BTreeSet<Capability>,
    pub confirm: Option<String>,
    pub limits: LimitsOverride,
    pub config: Value,
    pub dry_run: bool,
    /// Request-side tool allowlist: when set, only these tool names may be declared in
    /// recognized request formats. Reuses `reiki-toolpolicy` request parsing.
    pub tool_allowlist: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Security,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "security" => Some(Self::Security),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LimitsOverride {
    pub memory_mib: Option<u64>,
    pub fuel_per_call: Option<u64>,
    pub fuel_per_kib: Option<u64>,
    pub deadline_ms: Option<u64>,
    pub wall_budget_ms: Option<u64>,
    pub max_body_kib: Option<u64>,
}

impl LimitsOverride {
    fn from_table(table: &toml::Table) -> Result<Self, String> {
        let mut out = Self::default();
        for (k, v) in table {
            let n = match v {
                Value::Integer(i) if *i > 0 => *i as u64,
                _ => return Err(format!("`limits.{k}` must be a positive integer")),
            };
            match k.as_str() {
                "memory-mib" | "memory_mib" => out.memory_mib = Some(n),
                "fuel-per-call" | "fuel_per_call" => out.fuel_per_call = Some(n),
                "fuel-per-kib" | "fuel_per_kib" => out.fuel_per_kib = Some(n),
                "deadline-ms" | "deadline_ms" => out.deadline_ms = Some(n),
                "wall-budget-ms" | "wall_budget_ms" => out.wall_budget_ms = Some(n),
                "max-body-kib" | "max_body_kib" => out.max_body_kib = Some(n),
                other => return Err(format!("unknown limits key `{other}`")),
            }
        }
        Ok(out)
    }

    /// Apply overrides to manifest limits; overrides may only lower. Host ceilings cap both.
    pub fn resolve(
        &self,
        manifest: &ManifestLimits,
        ceilings: &HostCeilings,
    ) -> Result<EffectiveLimits, String> {
        let resolve =
            |name: &str, base: u64, ov: Option<u64>, ceiling: u64| -> Result<u64, String> {
                let v = ov.unwrap_or(base);
                if ov.is_some() && v > base {
                    return Err(format!(
                        "`limits.{name}` may only lower the manifest value ({base})"
                    ));
                }
                if v > ceiling {
                    return Err(format!("`limits.{name}` exceeds host ceiling ({ceiling})"));
                }
                if v == 0 {
                    return Err(format!("`limits.{name}` must be positive"));
                }
                Ok(v)
            };
        Ok(EffectiveLimits {
            memory_mib: resolve(
                "memory-mib",
                manifest.memory_mib,
                self.memory_mib,
                ceilings.memory_mib,
            )?,
            fuel_per_call: resolve(
                "fuel-per-call",
                manifest.fuel_per_call,
                self.fuel_per_call,
                ceilings.fuel_per_call,
            )?,
            fuel_per_kib: resolve(
                "fuel-per-kib",
                manifest.fuel_per_kib,
                self.fuel_per_kib,
                ceilings.fuel_per_kib,
            )?,
            deadline_ms: resolve(
                "deadline-ms",
                manifest.deadline_ms,
                self.deadline_ms,
                ceilings.deadline_ms,
            )?,
            wall_budget_ms: resolve(
                "wall-budget-ms",
                manifest.wall_budget_ms,
                self.wall_budget_ms,
                ceilings.wall_budget_ms,
            )?,
            max_body_kib: resolve(
                "max-body-kib",
                manifest.max_body_kib,
                self.max_body_kib,
                ceilings.max_body_kib,
            )?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveLimits {
    pub memory_mib: u64,
    pub fuel_per_call: u64,
    pub fuel_per_kib: u64,
    pub deadline_ms: u64,
    pub wall_budget_ms: u64,
    pub max_body_kib: u64,
}

impl EffectiveLimits {
    pub fn fuel_for_body(&self, body_len: usize) -> u64 {
        let kib = (body_len as u64).div_ceil(1024);
        self.fuel_per_call
            .saturating_add(kib.saturating_mul(self.fuel_per_kib))
    }
}

const KNOWN_KEYS: &[&str] = &[
    "wasm",
    "sha256",
    "role",
    "on_error",
    "streams",
    "grants",
    "confirm",
    "signature",
    "limits",
    "config",
    "files",
    "enabled",
    "dry_run",
    "tool_allowlist",
    "tool-allowlist",
];

pub fn is_wasm_plugin_table(table: &toml::Table) -> bool {
    table.contains_key("wasm")
}

pub fn parse_wasm_plugin_config(
    name: &str,
    table: &toml::Table,
) -> Result<WasmPluginConfig, String> {
    for key in table.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) && !HOST_KEYS.contains(&key.as_str()) {
            return Err(format!("unknown setting `{key}`"));
        }
    }
    let wasm = match table.get("wasm") {
        Some(Value::String(s)) if !s.trim().is_empty() => PathBuf::from(s),
        Some(other) => return Err(format!("`wasm` must be a non-empty string, got {other}")),
        None => return Err("missing `wasm`".to_owned()),
    };
    let sha256 = match table.get("sha256") {
        Some(Value::String(s)) if is_sha256_hex(s) => s.to_ascii_lowercase(),
        Some(Value::String(s)) => {
            return Err(format!("`sha256` must be 64 hex characters, got \"{s}\""));
        }
        Some(other) => return Err(format!("`sha256` must be a string, got {other}")),
        None => return Err("missing `sha256`".to_owned()),
    };
    let role = match table.get("role") {
        None => None,
        Some(Value::String(s)) => Some(
            Role::parse(s).ok_or_else(|| format!("`role` must be \"security\", got \"{s}\""))?,
        ),
        Some(other) => return Err(format!("`role` must be a string, got {other}")),
    };
    let on_error = match table.get("on_error") {
        None => None,
        Some(Value::String(s)) if s == "pass_through" || s == "reject" => Some(s.clone()),
        Some(Value::String(s)) => {
            return Err(format!(
                "`on_error` must be \"pass_through\" or \"reject\", got \"{s}\""
            ));
        }
        Some(other) => return Err(format!("`on_error` must be a string, got {other}")),
    };
    if role == Some(Role::Security) && matches!(on_error.as_deref(), Some("pass_through")) {
        return Err("`role = \"security\"` requires `on_error = \"reject\"`".to_owned());
    }
    let streams = match table.get("streams") {
        None => None,
        Some(Value::Array(items)) => {
            if items.is_empty() {
                return Err("`streams` must name at least one stream".to_owned());
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(s) if !s.is_empty() => out.push(s.clone()),
                    Value::String(_) => {
                        return Err("`streams` must not contain an empty name".to_owned());
                    }
                    other => return Err(format!("`streams` must contain strings, got {other}")),
                }
            }
            Some(out)
        }
        Some(other) => {
            return Err(format!(
                "`streams` must be an array of strings, got {other}"
            ));
        }
    };
    let grants = parse_grants(table.get("grants"))?;
    let confirm = match table.get("confirm") {
        None => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(other) => return Err(format!("`confirm` must be a non-empty string, got {other}")),
    };
    let limits = match table.get("limits") {
        None => LimitsOverride::default(),
        Some(Value::Table(t)) => LimitsOverride::from_table(t)?,
        Some(other) => return Err(format!("`limits` must be a table, got {other}")),
    };
    let config = table
        .get("config")
        .cloned()
        .unwrap_or(Value::Table(toml::Table::new()));
    let dry_run = match table.get("dry_run") {
        None => false,
        Some(Value::Boolean(b)) => *b,
        Some(other) => return Err(format!("`dry_run` must be a boolean, got {other}")),
    };
    let tool_allowlist = parse_tool_allowlist(table)?;
    if table.contains_key("signature") {
        return Err("`signature` is not supported in phase 1".to_owned());
    }
    if table.contains_key("files") {
        return Err("`files` is not supported in phase 1".to_owned());
    }
    Ok(WasmPluginConfig {
        name: name.to_owned(),
        wasm,
        sha256,
        role,
        on_error,
        streams,
        grants,
        confirm,
        limits,
        config,
        dry_run,
        tool_allowlist,
    })
}

fn parse_grants(value: Option<&Value>) -> Result<BTreeSet<Capability>, String> {
    let Some(value) = value else {
        return Ok(BTreeSet::new());
    };
    let Value::Array(items) = value else {
        return Err(format!("`grants` must be an array of strings, got {value}"));
    };
    let mut out = BTreeSet::new();
    for item in items {
        let Value::String(s) = item else {
            return Err(format!("`grants` must contain strings, got {item}"));
        };
        let cap = Capability::parse(s).ok_or_else(|| format!("unknown grant `{s}`"))?;
        // Phase 1 supported grants
        match cap {
            Capability::RequestBodyRead
            | Capability::RequestBodyEdit
            | Capability::FindingDetailVocab => {}
            Capability::RequestMeta
            | Capability::Finding
            | Capability::Log
            | Capability::Reject
            | Capability::ClockMonotonic
            | Capability::Random
            | Capability::RequestState => {
                return Err(format!(
                    "grant `{s}` is default-allowed and must not be listed"
                ));
            }
        }
        if !out.insert(cap) {
            return Err(format!("duplicate grant `{s}`"));
        }
    }
    Ok(out)
}

fn parse_tool_allowlist(table: &toml::Table) -> Result<Option<Vec<String>>, String> {
    let value = match (table.get("tool_allowlist"), table.get("tool-allowlist")) {
        (None, None) => return Ok(None),
        (Some(v), None) | (None, Some(v)) => v,
        (Some(_), Some(_)) => {
            return Err("`tool_allowlist` and `tool-allowlist` cannot both be set".to_owned());
        }
    };
    let Value::Array(items) = value else {
        return Err(format!(
            "`tool_allowlist` must be an array of strings, got {value}"
        ));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Value::String(s) if !s.is_empty() => {
                if out.iter().any(|e: &String| e == s) {
                    return Err(format!("duplicate tool_allowlist entry `{s}`"));
                }
                out.push(s.clone());
            }
            Value::String(_) => {
                return Err("`tool_allowlist` must not contain an empty string".to_owned());
            }
            other => {
                return Err(format!(
                    "`tool_allowlist` must contain strings, got {other}"
                ));
            }
        }
    }
    Ok(Some(out))
}

pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Compare old->new config for reload narrowing (2.6).
/// Returns (changed_settings, restart_needed, error).
/// This is phase-1 scoped.
pub fn reload_narrowing(
    old: &WasmPluginConfig,
    new: &WasmPluginConfig,
    old_has_high_or_med_grants: bool,
) -> ReloadDecision {
    let mut restart = Vec::new();
    if (old.wasm != new.wasm || old.sha256 != new.sha256)
        && new.grants.iter().any(|g| g.risk() != "low")
    {
        restart.push("wasm".to_owned());
    }
    if old.name != new.name {
        restart.push("name".to_owned());
    }
    // grants: remove is live narrowing, add is restart
    for g in &new.grants {
        if !old.grants.contains(g) {
            restart.push(format!("grant:{}", g.as_str()));
        }
    }
    // streams: remove live, add live for low-only, restart for med/high (D6)
    match (&old.streams, &new.streams) {
        (Some(old_s), Some(new_s)) => {
            for s in new_s {
                if !old_s.iter().any(|o| o == s)
                    && (old_has_high_or_med_grants || new.grants.iter().any(|g| g.risk() != "low"))
                {
                    restart.push("streams:add".to_owned());
                    break;
                }
            }
        }
        (Some(_), None) => {
            // widening to all streams
            if old_has_high_or_med_grants || new.grants.iter().any(|g| g.risk() != "low") {
                restart.push("streams:add".to_owned());
            }
        }
        (None, Some(_)) => { /* narrowing */ }
        (None, None) => {}
    }
    // limits: lower live, raise restart
    macro_rules! check_limit {
        ($field:ident, $label:literal) => {
            match (old.limits.$field, new.limits.$field) {
                (Some(o), Some(n)) if n > o => restart.push(format!("limits.{}", $label)),
                (None, Some(_)) => {} // setting an explicit lower/equal override is ok? lowering only vs manifest; but raising relative to old effective may restart. Keep simple: treat as potential restart.
                (Some(_), None) => restart.push(format!("limits.{}", $label)), // removing override may raise effective
                _ => {}
            }
        };
    }
    check_limit!(memory_mib, "memory-mib");
    check_limit!(fuel_per_call, "fuel-per-call");
    check_limit!(fuel_per_kib, "fuel-per-kib");
    check_limit!(deadline_ms, "deadline-ms");
    check_limit!(wall_budget_ms, "wall-budget-ms");
    check_limit!(max_body_kib, "max-body-kib");
    // role/on_error: pass_through->reject or add security live; reverse restart
    match (old.role, new.role) {
        (None, Some(Role::Security)) => {}
        (Some(Role::Security), None) => restart.push("role".to_owned()),
        _ => {}
    }
    if let (Some("reject"), Some("pass_through")) =
        (old.on_error.as_deref(), new.on_error.as_deref())
    {
        restart.push("on_error".to_owned());
    }
    // tool_allowlist: removing allowlist or adding entries is widening (restart); tightening is live.
    match (&old.tool_allowlist, &new.tool_allowlist) {
        (None, None) => {}
        (Some(_), None) => restart.push("tool_allowlist".to_owned()),
        (None, Some(_)) => {}
        (Some(old_list), Some(new_list)) => {
            for tool in new_list {
                if !old_list.iter().any(|o| o == tool) {
                    restart.push("tool_allowlist".to_owned());
                    break;
                }
            }
        }
    }
    if !restart.is_empty() {
        return ReloadDecision::RestartNeeded(restart);
    }
    // A security plugin's operator config or confirmation cannot be swapped under a
    // running instance; neither can a confirmation be removed from any plugin.
    let security = old.role == Some(Role::Security) || new.role == Some(Role::Security);
    if old.config != new.config && security {
        restart.push("config".to_owned());
    }
    if old.confirm != new.confirm && (security || old.confirm.is_some()) {
        restart.push("confirm".to_owned());
    }
    if !restart.is_empty() {
        return ReloadDecision::RestartNeeded(restart);
    }
    ReloadDecision::Live
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadDecision {
    Live,
    RestartNeeded(Vec<String>),
}
