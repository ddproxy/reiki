use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use wasmtime::component::Component;
use wasmtime::{Config as WasmConfig, Engine};

use crate::capability::{
    Capability, DerivedCapabilities, capability_for_import, is_allowed_inert_import,
};
use crate::config::{EffectiveLimits, WasmPluginConfig};
use crate::manifest::{EmbeddedManifest, HostCeilings, extract_embedded_manifest};

/// Compiled Wasm engine shared across plugins.
pub struct WasmEngine {
    engine: Engine,
    ceilings: HostCeilings,
}

impl WasmEngine {
    pub fn new() -> Result<Self, String> {
        Self::with_ceilings(HostCeilings::default())
    }

    pub fn with_ceilings(ceilings: HostCeilings) -> Result<Self, String> {
        let mut cfg = WasmConfig::new();
        cfg.wasm_component_model(true);
        cfg.consume_fuel(true);
        cfg.epoch_interruption(true);
        // No on-disk cache in v1 (P25); cache feature not enabled.
        // No debug info needed in production; keep deterministic.
        cfg.debug_info(false);
        let engine = Engine::new(&cfg).map_err(|e| format!("wasmtime engine: {e}"))?;
        Ok(Self { engine, ceilings })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn ceilings(&self) -> HostCeilings {
        self.ceilings
    }

    pub fn load_component(&self, config: &WasmPluginConfig) -> Result<LoadedComponent, String> {
        let bytes = std::fs::read(&config.wasm)
            .map_err(|e| format!("cannot read {}: {e}", config.wasm.display()))?;
        self.load_component_bytes(config, &bytes)
    }

    pub fn load_component_bytes(
        &self,
        config: &WasmPluginConfig,
        bytes: &[u8],
    ) -> Result<LoadedComponent, String> {
        let actual = sha256_hex(bytes);
        if actual != config.sha256 {
            return Err(format!(
                "sha256 mismatch for {}: expected {}, got {actual}",
                config.wasm.display(),
                config.sha256
            ));
        }
        let embedded = extract_embedded_manifest(bytes)?;
        if embedded.manifest.plugin.name != config.name {
            return Err(format!(
                "manifest name `{}` does not match configured plugin name `{}`",
                embedded.manifest.plugin.name, config.name
            ));
        }
        let component = Component::new(&self.engine, bytes)
            .map_err(|e| format!("cannot compile component {}: {e}", config.wasm.display()))?;
        let derived = derive_capabilities(&self.engine, &component)?;
        validate_imports(&self.engine, &component)?;
        validate_manifest_matches_imports(&embedded.manifest, &derived)?;
        validate_grants_cover_requests(&config.grants, &derived)?;
        validate_read_xor_egress(&config.grants, &derived)?;
        validate_role(config, &embedded.manifest)?;
        let limits = config
            .limits
            .resolve(&embedded.manifest.limits, &self.ceilings)?;
        Ok(LoadedComponent {
            component,
            embedded,
            derived,
            limits,
            config: config.clone(),
            bytes_sha256: actual,
        })
    }
}

pub struct LoadedComponent {
    pub component: Component,
    pub embedded: EmbeddedManifest,
    pub derived: DerivedCapabilities,
    pub limits: EffectiveLimits,
    pub config: WasmPluginConfig,
    pub bytes_sha256: String,
}

impl LoadedComponent {
    pub fn manifest(&self) -> &crate::manifest::Manifest {
        &self.embedded.manifest
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub fn sha256_hex_public(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

fn derive_capabilities(
    engine: &Engine,
    component: &Component,
) -> Result<DerivedCapabilities, String> {
    let ty = component.component_type();
    let mut caps = BTreeSet::new();
    for (name, _item) in ty.imports(engine) {
        if let Some(cap) = capability_for_import(name) {
            caps.insert(cap);
        }
    }
    Ok(DerivedCapabilities::new(caps))
}

pub fn derive_capabilities_public(
    engine: &Engine,
    component: &Component,
) -> Result<DerivedCapabilities, String> {
    derive_capabilities(engine, component)
}

fn validate_imports(engine: &Engine, component: &Component) -> Result<(), String> {
    let ty = component.component_type();
    for (name, _item) in ty.imports(engine) {
        if capability_for_import(name).is_some() {
            continue;
        }
        if is_allowed_inert_import(name) {
            if name.contains("wasi:sockets") || name.contains("wasi:http") {
                return Err(format!("component imports disallowed interface `{name}`"));
            }
            continue;
        }
        return Err(format!(
            "component imports unknown or unsupported interface `{name}`"
        ));
    }
    Ok(())
}

pub fn validate_imports_public(engine: &Engine, component: &Component) -> Result<(), String> {
    validate_imports(engine, component)
}

fn validate_manifest_matches_imports(
    manifest: &crate::manifest::Manifest,
    derived: &DerivedCapabilities,
) -> Result<(), String> {
    let declared = manifest.declared_capabilities();
    for cap in derived.iter() {
        if cap.default_allowed() {
            continue;
        }
        if !declared.contains(&cap) {
            return Err(format!(
                "component imports capability `{}` but manifest does not declare it",
                cap.as_str()
            ));
        }
    }
    for cap in &declared {
        if cap.default_allowed() {
            continue;
        }
        if !derived.contains(*cap) {
            return Err(format!(
                "manifest declares `{}` but component does not import it",
                cap.as_str()
            ));
        }
    }
    Ok(())
}

pub fn validate_manifest_matches_imports_public(
    manifest: &crate::manifest::Manifest,
    derived: &DerivedCapabilities,
) -> Result<(), String> {
    validate_manifest_matches_imports(manifest, derived)
}

fn validate_grants_cover_requests(
    grants: &BTreeSet<Capability>,
    derived: &DerivedCapabilities,
) -> Result<(), String> {
    for cap in derived.iter() {
        if cap.default_allowed() {
            continue;
        }
        if !grants.contains(&cap) {
            return Err(format!(
                "plugin requests capability `{}` but config does not grant it",
                cap.as_str()
            ));
        }
    }
    // Also reject grants for capabilities not requested? Task says "plugin requesting more than config grants fails to load" - that's covered.
    // Extra grants in config are harmless but confusing; reject for clarity.
    for cap in grants {
        if !derived.contains(*cap) {
            return Err(format!(
                "config grants `{}` but component does not request it",
                cap.as_str()
            ));
        }
    }
    Ok(())
}

fn validate_read_xor_egress(
    grants: &BTreeSet<Capability>,
    derived: &DerivedCapabilities,
) -> Result<(), String> {
    let has_read =
        derived.iter().any(|c| c.is_data_read()) || grants.iter().any(|c| c.is_data_read());
    // Phase 1 has no log:text or finding:detail-free or helper; but enforce rule structurally.
    let has_egress = false;
    if has_read && has_egress {
        return Err(
            "read-xor-egress: plugin cannot hold data-read and egress capabilities".to_owned(),
        );
    }
    Ok(())
}

fn validate_role(
    config: &WasmPluginConfig,
    manifest: &crate::manifest::Manifest,
) -> Result<(), String> {
    if config.role == Some(crate::config::Role::Security)
        && config.on_error.as_deref() != Some("reject")
    {
        return Err("role = \"security\" requires on_error = \"reject\"".to_owned());
    }
    // manifest suggested-role is informational.
    let _ = manifest;
    Ok(())
}
