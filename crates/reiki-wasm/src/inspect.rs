use std::path::Path;

use serde::Serialize;

use crate::capability::Capability;
use crate::config::WasmPluginConfig;
use crate::host::WasmEngine;

/// Human-readable report from `reiki plugin inspect`.
#[derive(Debug, Clone, Serialize)]
pub struct InspectReport {
    pub path: String,
    pub sha256: String,
    pub name: String,
    pub version: String,
    pub wit_world: String,
    pub hooks: Vec<String>,
    pub derived_capabilities: Vec<String>,
    pub requested_gated_capabilities: Vec<String>,
    pub default_allowed_capabilities: Vec<String>,
    pub manifest_limits: ManifestLimitsReport,
    pub pin: PinReport,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManifestLimitsReport {
    pub memory_mib: u64,
    pub fuel_per_call: u64,
    pub fuel_per_kib: u64,
    pub deadline_ms: u64,
    pub wall_budget_ms: u64,
    pub max_body_kib: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PinReport {
    pub manifest_present: bool,
    pub embedded_section: String,
}

/// Inspect a component file. No operator config is required; this reports what the component
/// declares and derives. It does not verify an operator sha256 pin because there is no config.
pub fn inspect_component(path: &Path) -> Result<InspectReport, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    inspect_component_bytes(&bytes, path.display().to_string())
}

/// Inspect raw component bytes.
pub fn inspect_component_bytes(
    bytes: &[u8],
    display_path: String,
) -> Result<InspectReport, String> {
    let engine = WasmEngine::new()?;
    let embedded = crate::manifest::extract_embedded_manifest(bytes)?;
    // Compile to derive imports/capabilities. We use a synthetic config-less validation path.
    let component = wasmtime::component::Component::new(engine.engine(), bytes)
        .map_err(|e| format!("cannot compile component: {e}"))?;
    let derived = crate::host::derive_capabilities_public(engine.engine(), &component)?;
    crate::host::validate_imports_public(engine.engine(), &component)?;
    crate::host::validate_manifest_matches_imports_public(&embedded.manifest, &derived)?;

    let mut derived_caps: Vec<String> = derived.iter().map(|c| c.as_str().to_owned()).collect();
    derived_caps.sort();
    let mut requested_gated: Vec<String> = derived
        .iter()
        .filter(|c| !c.default_allowed())
        .map(|c| c.as_str().to_owned())
        .collect();
    requested_gated.sort();
    let mut default_allowed: Vec<String> = Capability::all_default_allowed()
        .iter()
        .map(|c| c.as_str().to_owned())
        .collect();
    default_allowed.sort();

    let sha256 = crate::host::sha256_hex_public(bytes);
    let m = &embedded.manifest;
    Ok(InspectReport {
        path: display_path,
        sha256,
        name: m.plugin.name.clone(),
        version: m.plugin.version.clone(),
        wit_world: m.plugin.wit_world.clone(),
        hooks: m.plugin.hooks.clone(),
        derived_capabilities: derived_caps,
        requested_gated_capabilities: requested_gated,
        default_allowed_capabilities: default_allowed,
        manifest_limits: ManifestLimitsReport {
            memory_mib: m.limits.memory_mib,
            fuel_per_call: m.limits.fuel_per_call,
            fuel_per_kib: m.limits.fuel_per_kib,
            deadline_ms: m.limits.deadline_ms,
            wall_budget_ms: m.limits.wall_budget_ms,
            max_body_kib: m.limits.max_body_kib,
        },
        pin: PinReport {
            manifest_present: true,
            embedded_section: crate::MANIFEST_SECTION.to_owned(),
        },
    })
}

/// Inspect with operator config context (pin verified, effective limits resolved).
pub fn inspect_with_config(
    engine: &WasmEngine,
    config: &WasmPluginConfig,
) -> Result<InspectReport, String> {
    let loaded = engine.load_component(config)?;
    let m = loaded.manifest();
    let mut derived_caps: Vec<String> = loaded
        .derived
        .iter()
        .map(|c| c.as_str().to_owned())
        .collect();
    derived_caps.sort();
    let mut requested_gated: Vec<String> = loaded
        .derived
        .iter()
        .filter(|c| !c.default_allowed())
        .map(|c| c.as_str().to_owned())
        .collect();
    requested_gated.sort();
    let mut default_allowed: Vec<String> = Capability::all_default_allowed()
        .iter()
        .map(|c| c.as_str().to_owned())
        .collect();
    default_allowed.sort();
    Ok(InspectReport {
        path: config.wasm.display().to_string(),
        sha256: loaded.bytes_sha256.clone(),
        name: m.plugin.name.clone(),
        version: m.plugin.version.clone(),
        wit_world: m.plugin.wit_world.clone(),
        hooks: m.plugin.hooks.clone(),
        derived_capabilities: derived_caps,
        requested_gated_capabilities: requested_gated,
        default_allowed_capabilities: default_allowed,
        manifest_limits: ManifestLimitsReport {
            memory_mib: loaded.limits.memory_mib,
            fuel_per_call: loaded.limits.fuel_per_call,
            fuel_per_kib: loaded.limits.fuel_per_kib,
            deadline_ms: loaded.limits.deadline_ms,
            wall_budget_ms: loaded.limits.wall_budget_ms,
            max_body_kib: loaded.limits.max_body_kib,
        },
        pin: PinReport {
            manifest_present: true,
            embedded_section: crate::MANIFEST_SECTION.to_owned(),
        },
    })
}
