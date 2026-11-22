//! Wasm plugin host for Reiki.
//!
//! Phase 1: `on-request`, default-allowed capability set, `request-body:read|edit`,
//! `finding:detail-vocab`, embedded manifest pinning, inspect, limits and trap matrix,
//! and request-side tool allowlist integration hooks.

mod capability;
pub mod config;
mod host;
mod inspect;
mod manifest;
mod plugin;

pub use capability::Capability;
pub use config::{
    EffectiveLimits, LimitsOverride, ReloadDecision, Role, WasmPluginConfig, is_sha256_hex,
    is_wasm_plugin_table, parse_wasm_plugin_config, reload_narrowing,
};
pub use host::WasmEngine;
pub use inspect::{InspectReport, inspect_component, inspect_component_bytes, inspect_with_config};
pub use manifest::{EmbeddedManifest, HostCeilings, Manifest, ManifestLimits};
pub use plugin::WasmPlugin;

pub const WIT_WORLD: &str = "reiki:plugin/plugin@1.0.0";
pub const MANIFEST_SECTION: &str = "reiki-manifest";
