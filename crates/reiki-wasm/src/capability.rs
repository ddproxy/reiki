use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Phase 1 capabilities. Names are the stable operator-facing strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    RequestMeta,
    RequestBodyRead,
    RequestBodyEdit,
    Finding,
    FindingDetailVocab,
    Log,
    Reject,
    ClockMonotonic,
    Random,
    RequestState,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RequestMeta => "request-meta",
            Self::RequestBodyRead => "request-body:read",
            Self::RequestBodyEdit => "request-body:edit",
            Self::Finding => "finding",
            Self::FindingDetailVocab => "finding:detail-vocab",
            Self::Log => "log",
            Self::Reject => "reject",
            Self::ClockMonotonic => "clock:monotonic",
            Self::Random => "random",
            Self::RequestState => "request-state",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "request-meta" => Some(Self::RequestMeta),
            "request-body:read" => Some(Self::RequestBodyRead),
            "request-body:edit" => Some(Self::RequestBodyEdit),
            "finding" => Some(Self::Finding),
            "finding:detail-vocab" => Some(Self::FindingDetailVocab),
            "log" => Some(Self::Log),
            "reject" => Some(Self::Reject),
            "clock:monotonic" => Some(Self::ClockMonotonic),
            "random" => Some(Self::Random),
            "request-state" => Some(Self::RequestState),
            _ => None,
        }
    }

    /// Default-allowed capabilities are linked for every plugin.
    pub fn default_allowed(self) -> bool {
        matches!(
            self,
            Self::RequestMeta
                | Self::Finding
                | Self::Log
                | Self::Reject
                | Self::ClockMonotonic
                | Self::Random
                | Self::RequestState
        )
    }

    pub fn risk(self) -> &'static str {
        match self {
            Self::RequestMeta
            | Self::Finding
            | Self::FindingDetailVocab
            | Self::Log
            | Self::Reject
            | Self::ClockMonotonic
            | Self::Random
            | Self::RequestState => "low",
            Self::RequestBodyRead | Self::RequestBodyEdit => "med",
        }
    }

    /// Phase 1: capabilities that read request data (read-xor-egress rule).
    pub fn is_data_read(self) -> bool {
        matches!(self, Self::RequestBodyRead | Self::RequestBodyEdit)
    }

    /// No phase-1 capability can send data off the host. Keeping this an explicit
    /// classifier means the read-xor-egress check starts enforcing as soon as an
    /// egress capability is introduced.
    pub fn is_egress(self) -> bool {
        match self {
            Self::RequestMeta
            | Self::RequestBodyRead
            | Self::RequestBodyEdit
            | Self::Finding
            | Self::FindingDetailVocab
            | Self::Log
            | Self::Reject
            | Self::ClockMonotonic
            | Self::Random
            | Self::RequestState => false,
        }
    }

    pub fn all_default_allowed() -> &'static [Capability] {
        &[
            Self::RequestMeta,
            Self::Finding,
            Self::Log,
            Self::Reject,
            Self::ClockMonotonic,
            Self::Random,
            Self::RequestState,
        ]
    }
}

/// Capability set derived from component imports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DerivedCapabilities {
    caps: BTreeSet<Capability>,
}

impl DerivedCapabilities {
    pub fn new(caps: BTreeSet<Capability>) -> Self {
        Self { caps }
    }

    pub fn contains(&self, cap: Capability) -> bool {
        self.caps.contains(&cap)
    }

    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.caps.iter().copied()
    }

    pub fn as_set(&self) -> &BTreeSet<Capability> {
        &self.caps
    }

    pub fn only_default_allowed(&self) -> bool {
        self.caps.iter().all(|c| c.default_allowed())
    }
}

/// Map a component import name to a Phase 1 capability, if known.
/// The host derives capabilities from imports (D1); this is the mapping.
pub fn capability_for_import(import: &str) -> Option<Capability> {
    // Match by interface name in the import path.
    // wasmtime component imports look like "reiki:plugin/request-body-read@1.0.0"
    // or interface names depending on API. We normalize by checking contained names.
    let s = import;
    if s.contains("request-meta") {
        Some(Capability::RequestMeta)
    } else if s.contains("request-body-read") {
        Some(Capability::RequestBodyRead)
    } else if s.contains("request-body-edit") {
        Some(Capability::RequestBodyEdit)
    } else if s.contains("finding-detail-vocab") {
        Some(Capability::FindingDetailVocab)
    } else if s.contains("/finding@") || s.ends_with("/finding") {
        Some(Capability::Finding)
    } else if s.contains("/log@") || s.ends_with("/log") {
        Some(Capability::Log)
    } else if s.contains("/reject@") || s.ends_with("/reject") {
        Some(Capability::Reject)
    } else if s.contains("/clock@") || s.ends_with("/clock") {
        Some(Capability::ClockMonotonic)
    } else if s.contains("/random@") || s.ends_with("/random") {
        Some(Capability::Random)
    } else if s.contains("request-state") {
        Some(Capability::RequestState)
    } else if s.contains("/types") || s.ends_with("types") {
        // types interface is not a capability gate by itself
        None
    } else {
        None
    }
}

/// Returns true for host imports that carry no capability gate. The generated guest
/// bindings import `reiki:plugin/types` for the shared variant/record definitions;
/// it is part of the WIT world but is not a grantable capability.
pub fn is_allowed_non_capability_import(import: &str) -> bool {
    // Wasmtime exposes the `request` resource imported from request-meta as a
    // bare name in ComponentType::imports. It is part of the request-meta
    // capability already derived from the interface import.
    import.starts_with("reiki:plugin/types@")
        || import == "reiki:plugin/types"
        || import == "request"
}

/// Returns true if an import would give the guest a filesystem or network
/// interface. These interfaces are not in any grant table: a Phase 1 plugin
/// must not be able to open a path or open a socket, regardless of the WASI
/// context configuration.
pub fn is_forbidden_wasi_interface(import: &str) -> bool {
    import.starts_with("wasi:filesystem/")
        || import.starts_with("wasi:sockets/")
        || import.starts_with("wasi:http/")
        || import == "wasi:filesystem"
        || import == "wasi:sockets"
        || import == "wasi:http"
}

/// Returns true if an import is a known WASI/host inert stub allowed in Phase 1.
pub fn is_allowed_inert_import(import: &str) -> bool {
    // WASI p2 cli/stdio/clocks/random/env may be imported by toolchains. The
    // host context has no preopens, no env, no args and no inherited stdio.
    // Filesystem and network interfaces are refused before this check.
    if is_forbidden_wasi_interface(import) {
        return false;
    }
    import.starts_with("wasi:")
        || import.starts_with("[method]")
        || import.starts_with("[resource-drop]")
        || import.starts_with("[constructor]")
        || import.starts_with("[static]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_phase1_capability_is_egress() {
        for cap in [
            Capability::RequestMeta,
            Capability::RequestBodyRead,
            Capability::RequestBodyEdit,
            Capability::Finding,
            Capability::FindingDetailVocab,
            Capability::Log,
            Capability::Reject,
            Capability::ClockMonotonic,
            Capability::Random,
            Capability::RequestState,
        ] {
            assert!(!cap.is_egress(), "{cap:?}");
        }
    }

    #[test]
    fn generated_types_import_is_allowed_but_not_a_capability() {
        let import = "reiki:plugin/types@1.0.0";
        assert!(is_allowed_non_capability_import(import));
        assert!(capability_for_import(import).is_none());
        assert!(!is_allowed_inert_import(import));
        assert!(is_allowed_non_capability_import("request"));
        assert!(!is_allowed_non_capability_import("wasi:sockets"));
    }

    #[test]
    fn filesystem_and_network_interfaces_are_forbidden() {
        for import in [
            "wasi:filesystem/types@0.2.12",
            "wasi:filesystem/preopens@0.2.12",
            "wasi:sockets/tcp@0.2.12",
            "wasi:sockets/ip-name-lookup@0.2.12",
            "wasi:http/outgoing-handler@0.2.12",
        ] {
            assert!(is_forbidden_wasi_interface(import), "{import}");
            assert!(!is_allowed_inert_import(import), "{import}");
        }
        assert!(!is_forbidden_wasi_interface(
            "wasi:clocks/monotonic-clock@0.2.12"
        ));
        assert!(is_allowed_inert_import(
            "wasi:clocks/monotonic-clock@0.2.12"
        ));
    }
}
