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

/// Returns true if an import is a known WASI/host inert stub allowed in Phase 1.
pub fn is_allowed_inert_import(import: &str) -> bool {
    // WASI p2 cli/stdio/clocks/random/filesystem/sockets/env may be imported by toolchains.
    // Phase 1: filesystem linked with zero preopens (inert), env/args/stdin empty, stdout/stderr sink.
    // sockets/http not grantable - refused at load.
    if s_is_wasi_socket(import) || s_is_wasi_http(import) {
        return false;
    }
    import.starts_with("wasi:")
        || import.starts_with("[method]")
        || import.starts_with("[resource-drop]")
        || import.starts_with("[constructor]")
        || import.starts_with("[static]")
}

fn s_is_wasi_socket(import: &str) -> bool {
    import.contains("wasi:sockets") || import.contains("wasi:http/outgoing-handler")
}

fn s_is_wasi_http(import: &str) -> bool {
    import.contains("wasi:http")
}
