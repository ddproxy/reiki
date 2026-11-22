use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::capability::Capability;

/// The embedded manifest custom section name.
pub const SECTION_NAME: &str = "reiki-manifest";

/// TOML manifest embedded in the component as a custom section.
/// The manifest is the human-readable justification; capabilities are derived from imports (D1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub plugin: PluginMeta,
    #[serde(default)]
    pub requests: Requests,
    #[serde(default)]
    pub literals: BTreeMap<String, String>,
    #[serde(default)]
    pub details: Vec<String>,
    #[serde(default, rename = "log-templates")]
    pub log_templates: BTreeMap<String, String>,
    #[serde(default, rename = "reject-templates")]
    pub reject_templates: BTreeMap<String, RejectTemplate>,
    #[serde(default)]
    pub limits: ManifestLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginMeta {
    pub name: String,
    pub version: String,
    #[serde(rename = "wit-world")]
    pub wit_world: String,
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default, rename = "suggested-role")]
    pub suggested_role: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Requests {
    /// "read" or "edit"
    #[serde(default, rename = "request-body")]
    pub request_body: Option<String>,
    /// Declares the read half of an editing plugin. `request-body = "edit"` alone
    /// declares only `request-body:edit`; a component that also calls `body()` needs
    /// this explicit flag. Kept optional so existing manifests parse unchanged.
    #[serde(default, rename = "request-body-read")]
    pub request_body_read: bool,
    #[serde(default, rename = "finding-detail")]
    pub finding_detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RejectTemplate {
    pub status: u16,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestLimits {
    #[serde(default = "default_memory_mib")]
    pub memory_mib: u64,
    #[serde(default = "default_fuel_per_call")]
    pub fuel_per_call: u64,
    #[serde(default = "default_fuel_per_kib")]
    pub fuel_per_kib: u64,
    #[serde(default = "default_deadline_ms")]
    pub deadline_ms: u64,
    #[serde(default = "default_wall_budget_ms")]
    pub wall_budget_ms: u64,
    #[serde(default = "default_max_body_kib")]
    pub max_body_kib: u64,
}

fn default_memory_mib() -> u64 {
    16
}
fn default_fuel_per_call() -> u64 {
    10_000_000
}
fn default_fuel_per_kib() -> u64 {
    200_000
}
fn default_deadline_ms() -> u64 {
    250
}
fn default_wall_budget_ms() -> u64 {
    1_000
}
fn default_max_body_kib() -> u64 {
    1024
}

impl Default for ManifestLimits {
    fn default() -> Self {
        Self {
            memory_mib: default_memory_mib(),
            fuel_per_call: default_fuel_per_call(),
            fuel_per_kib: default_fuel_per_kib(),
            deadline_ms: default_deadline_ms(),
            wall_budget_ms: default_wall_budget_ms(),
            max_body_kib: default_max_body_kib(),
        }
    }
}

/// Host ceilings (Phase 1 starting points).
#[derive(Debug, Clone, Copy)]
pub struct HostCeilings {
    pub memory_mib: u64,
    pub fuel_per_call: u64,
    pub fuel_per_kib: u64,
    pub deadline_ms: u64,
    pub wall_budget_ms: u64,
    pub max_body_kib: u64,
}

impl Default for HostCeilings {
    fn default() -> Self {
        Self {
            memory_mib: 128,
            fuel_per_call: 2_000_000_000,
            fuel_per_kib: 20_000_000,
            deadline_ms: 2_000,
            wall_budget_ms: 10_000,
            max_body_kib: 8192,
        }
    }
}

impl Manifest {
    pub fn parse(toml_str: &str) -> Result<Self, String> {
        let manifest: Manifest =
            toml::from_str(toml_str).map_err(|e| format!("invalid manifest TOML: {e}"))?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), String> {
        if self.plugin.name.trim().is_empty() {
            return Err("plugin.name must not be empty".to_owned());
        }
        if self.plugin.version.trim().is_empty() {
            return Err("plugin.version must not be empty".to_owned());
        }
        if self.plugin.wit_world.trim().is_empty() {
            return Err("plugin.wit-world must not be empty".to_owned());
        }
        for hook in &self.plugin.hooks {
            match hook.as_str() {
                "on-request" => {}
                other => return Err(format!("unknown or unsupported hook `{other}` in phase 1")),
            }
        }
        if let Some(mode) = &self.requests.request_body {
            match mode.as_str() {
                "read" | "edit" => {}
                other => {
                    return Err(format!(
                        "requests.request-body must be \"read\" or \"edit\", got \"{other}\""
                    ));
                }
            }
        }
        if let Some(mode) = &self.requests.finding_detail {
            match mode.as_str() {
                "vocab" => {}
                "free" => {
                    return Err(
                        "requests.finding-detail = \"free\" is not supported in phase 1".to_owned(),
                    );
                }
                other => {
                    return Err(format!(
                        "requests.finding-detail must be \"vocab\", got \"{other}\""
                    ));
                }
            }
        }
        for (id, literal) in &self.literals {
            if id.trim().is_empty() {
                return Err("literals key must not be empty".to_owned());
            }
            if literal.len() > 64 {
                return Err(format!("literal `{id}` is longer than 64 bytes"));
            }
            if literal
                .chars()
                .any(|c| matches!(c, '{' | '}' | '[' | ']' | '"' | '\\'))
            {
                return Err(format!("literal `{id}` contains a forbidden character"));
            }
        }
        if self.details.len() > 1024 {
            return Err("details vocab has more than 1024 entries".to_owned());
        }
        for d in &self.details {
            if d.is_empty() || d.len() > 64 {
                return Err("each detail vocab entry must be 1-64 bytes".to_owned());
            }
        }
        if self.log_templates.len() > 64 {
            return Err("log-templates has more than 64 entries".to_owned());
        }
        for (id, tpl) in &self.log_templates {
            if id.is_empty() || tpl.len() > 200 {
                return Err(format!(
                    "log template `{id}` must have id 1+ bytes and text <= 200 bytes"
                ));
            }
        }
        if self.reject_templates.len() > 64 {
            return Err("reject-templates has more than 64 entries".to_owned());
        }
        for (id, tpl) in &self.reject_templates {
            if id.is_empty() || tpl.message.len() > 200 {
                return Err(format!(
                    "reject template `{id}` must have id 1+ bytes and message <= 200 bytes"
                ));
            }
            if !matches!(tpl.status, 400 | 403 | 413 | 422 | 451) {
                return Err(format!(
                    "reject template `{id}` has status {}; allowed: 400,403,413,422,451",
                    tpl.status
                ));
            }
        }
        if self.limits.memory_mib == 0
            || self.limits.deadline_ms == 0
            || self.limits.wall_budget_ms == 0
        {
            return Err("manifest limits must be positive".to_owned());
        }
        Ok(())
    }

    /// Capabilities declared by manifest requests (human-readable justification).
    pub fn declared_capabilities(&self) -> BTreeSet<Capability> {
        let mut caps = BTreeSet::new();
        // default allowed always present implicitly in manifest view? No - manifest declares requests.
        // We include defaults as implicit baseline, and add gated ones from requests.
        caps.insert(Capability::RequestMeta);
        caps.insert(Capability::Finding);
        caps.insert(Capability::Log);
        caps.insert(Capability::Reject);
        caps.insert(Capability::ClockMonotonic);
        caps.insert(Capability::Random);
        caps.insert(Capability::RequestState);
        match self.requests.request_body.as_deref() {
            Some("read") => {
                caps.insert(Capability::RequestBodyRead);
            }
            Some("edit") => {
                caps.insert(Capability::RequestBodyEdit);
            }
            _ => {}
        }
        if self.requests.request_body_read {
            caps.insert(Capability::RequestBodyRead);
        }
        if self.requests.finding_detail.as_deref() == Some("vocab") {
            caps.insert(Capability::FindingDetailVocab);
        }
        caps
    }
}

/// Manifest parsed from a component, along with its raw bytes.
#[derive(Debug, Clone)]
pub struct EmbeddedManifest {
    pub manifest: Manifest,
    pub raw_toml: String,
}

/// Extract the embedded reiki-manifest custom section from a Wasm component.
/// Phase 1: supports component custom sections in the component binary.
/// We scan the wasm binary for a custom section named `reiki-manifest`.
pub fn extract_embedded_manifest(bytes: &[u8]) -> Result<EmbeddedManifest, String> {
    let raw_toml = extract_custom_section(bytes, SECTION_NAME)?
        .ok_or_else(|| format!("missing `{SECTION_NAME}` custom section in component"))?;
    let raw_toml = String::from_utf8(raw_toml)
        .map_err(|e| format!("`{SECTION_NAME}` section is not UTF-8: {e}"))?;
    let manifest = Manifest::parse(&raw_toml)?;
    Ok(EmbeddedManifest { manifest, raw_toml })
}

/// Minimal wasm custom section extractor. Works for core wasm modules and components
/// (components are wasm modules with component preamble in version; custom sections
/// appear at top-level module/component section encoding). We parse sections generically.
fn extract_custom_section(bytes: &[u8], name: &str) -> Result<Option<Vec<u8>>, String> {
    if bytes.len() < 8 {
        return Err("file is too small to be a wasm component/module".to_owned());
    }
    if &bytes[0..4] != b"\0asm" {
        return Err("file does not start with \\0asm".to_owned());
    }
    // version: module = 0x01 00 00 00, component = 0x0d 00 01 00 (wasm component model)
    let mut pos = 8;
    // For component model, there may be a component layer; custom sections are id 0.
    let mut found = None;
    while pos < bytes.len() {
        let section_id = bytes[pos];
        pos += 1;
        let (section_len, len_bytes) = read_leb128_u32(&bytes[pos..])
            .map_err(|e| format!("invalid section length at {pos}: {e}"))?;
        pos += len_bytes;
        let end = pos
            .checked_add(section_len as usize)
            .ok_or_else(|| "section length overflow".to_owned())?;
        if end > bytes.len() {
            return Err("section extends past end of file".to_owned());
        }
        if section_id == 0 {
            // custom section: name (name-len + bytes) then payload
            let (name_len, name_len_bytes) = read_leb128_u32(&bytes[pos..end])
                .map_err(|e| format!("invalid custom section name length: {e}"))?;
            let name_start = pos + name_len_bytes;
            let name_end = name_start
                .checked_add(name_len as usize)
                .ok_or_else(|| "custom section name overflow".to_owned())?;
            if name_end > end {
                return Err("custom section name extends past section".to_owned());
            }
            let sec_name = std::str::from_utf8(&bytes[name_start..name_end])
                .map_err(|e| format!("custom section name is not UTF-8: {e}"))?;
            if sec_name == name {
                if found.is_some() {
                    return Err(format!("more than one `{name}` custom section"));
                }
                found = Some(bytes[name_end..end].to_vec());
            }
        }
        pos = end;
    }
    Ok(found)
}

fn read_leb128_u32(bytes: &[u8]) -> Result<(u32, usize), String> {
    let mut result: u32 = 0;
    let mut shift = 0u32;
    for (i, &b) in bytes.iter().enumerate().take(5) {
        let low = u32::from(b & 0x7f);
        result |= low << shift;
        if b & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
    }
    Err("leb128 too long".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_validates() {
        let toml = r#"
details = ["rule.a"]

[plugin]
name = "test"
version = "0.1.0"
wit-world = "reiki:plugin/plugin@1.0.0"
hooks = ["on-request"]

[requests]
request-body = "read"
finding-detail = "vocab"

[literals]
auto = "auto"

[log-templates]
ok = "ok"

[reject-templates.bad]
status = 403
message = "blocked"
"#;
        let m = Manifest::parse(toml).unwrap();
        assert_eq!(m.plugin.name, "test");
        assert!(
            m.declared_capabilities()
                .contains(&Capability::RequestBodyRead)
        );
        assert!(
            m.declared_capabilities()
                .contains(&Capability::FindingDetailVocab)
        );
    }

    #[test]
    fn edit_manifest_can_declare_body_read_and_edit() {
        let toml = r#"
[plugin]
name = "edit"
version = "0.1.0"
wit-world = "reiki:plugin/plugin@1.0.0"
[requests]
request-body = "edit"
request-body-read = true
"#;
        let m = Manifest::parse(toml).unwrap();
        let caps = m.declared_capabilities();
        assert!(caps.contains(&Capability::RequestBodyRead));
        assert!(caps.contains(&Capability::RequestBodyEdit));
    }

    #[test]
    fn duplicate_manifest_sections_are_refused() {
        fn section(name: &str, payload: &[u8]) -> Vec<u8> {
            let mut body = vec![name.len() as u8];
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(payload);
            let mut out = vec![0, body.len() as u8];
            out.extend(body);
            out
        }
        let mut bytes = b"\0asm\r\0\x01\0".to_vec();
        bytes.extend(section(SECTION_NAME, b"first"));
        bytes.extend(section(SECTION_NAME, b"second"));
        let err = extract_embedded_manifest(&bytes).unwrap_err();
        assert!(err.contains("more than one"), "{err}");
    }

    #[test]
    fn reject_template_status_is_limited() {
        let toml = r#"
[plugin]
name = "t"
version = "0.1.0"
wit-world = "reiki:plugin/plugin@1.0.0"

[reject-templates.nope]
status = 500
message = "no"
"#;
        let err = Manifest::parse(toml).unwrap_err();
        assert!(err.contains("status"));
    }
}
