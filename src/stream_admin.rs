//! Opt-in dynamic stream management: bounded state, a hash-chained audit log, and strict
//! mutations. Nothing in this module runs unless `[stream_admin]` is configured.
//!
//! Secrets are never stored here: only SHA-256 hashes of tokens and the configured actor id.
//! Raw token values are returned to the caller exactly once and are never logged.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{
    Config, DynamicStreamMeta, DynamicToken, StreamAdminConfig, StreamConfig, StreamLimits,
};
use crate::proxy::App;

const STATE_VERSION: u32 = 1;
const ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Caps the state file so a hand-edited file cannot exhaust memory, and bounds writes.
const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;
const MAX_AUDIT_LINE: usize = 64 * 1024;
const MAX_IDEMPOTENCY_ENTRIES: usize = 1024;
const MAX_REASON_BYTES: usize = 200;
const ID_MAX: usize = 64;
const NAME_MAX: usize = 64;

/// A public, fixed refusal. No request data is interpolated into any of these messages.
#[derive(Debug, Clone, Copy)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: &'static str,
}

impl ApiError {
    pub(crate) const BAD_REQUEST: Self = Self {
        status: StatusCode::BAD_REQUEST,
        message: "invalid stream management request",
    };
    pub(crate) const NOT_FOUND: Self = Self {
        status: StatusCode::NOT_FOUND,
        message: "no such dynamic stream or token",
    };
    pub(crate) const CONFLICT: Self = Self {
        status: StatusCode::CONFLICT,
        message: "the request conflicts with existing state",
    };
    pub(crate) const LIMIT: Self = Self {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        message: "the request exceeds a configured ceiling",
    };
    pub(crate) const STORE: Self = Self {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "stream management is unavailable; the audit log or state file could not be written",
    };
    pub(crate) const DISABLED: Self = Self {
        status: StatusCode::NOT_FOUND,
        message: "stream management is disabled",
    };
}

/// What a management request wants to change. The reason and idempotency key travel separately
/// so they can be audited without becoming part of the mutable state.
#[derive(Debug, Clone)]
pub enum Mutation {
    Create {
        name: String,
        key: String,
        ttl: Option<u64>,
        limits: Option<StreamLimits>,
    },
    Update {
        name: String,
        key: Option<String>,
        limits: Option<StreamLimits>,
    },
    AddToken {
        name: String,
        ttl: Option<u64>,
    },
    RevokeToken {
        name: String,
        token_id: String,
    },
    RevokeAll {
        name: String,
    },
    Delete {
        name: String,
    },
}

impl Mutation {
    fn action(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create",
            Self::Update { .. } => "update",
            Self::AddToken { .. } => "token.add",
            Self::RevokeToken { .. } => "token.revoke",
            Self::RevokeAll { .. } => "revoke",
            Self::Delete { .. } => "delete",
        }
    }

    pub(crate) fn stream(&self) -> &str {
        match self {
            Self::Create { name, .. }
            | Self::Update { name, .. }
            | Self::AddToken { name, .. }
            | Self::RevokeToken { name, .. }
            | Self::RevokeAll { name, .. }
            | Self::Delete { name, .. } => name,
        }
    }

    fn fingerprint(&self, reason: &str) -> String {
        let parts = match self {
            Self::Create {
                name,
                key,
                ttl,
                limits,
            } => format!(
                "create|{name}|{key}|{}|{}",
                ttl.unwrap_or(0),
                limits_text(limits.as_ref())
            ),
            Self::Update { name, key, limits } => {
                format!(
                    "update|{name}|{}|{}",
                    key.as_deref().unwrap_or(""),
                    limits_text(limits.as_ref())
                )
            }
            Self::AddToken { name, ttl } => format!("token.add|{name}|{}", ttl.unwrap_or(0)),
            Self::RevokeToken { name, token_id } => format!("token.revoke|{name}|{token_id}"),
            Self::RevokeAll { name } => format!("revoke|{name}"),
            Self::Delete { name } => format!("delete|{name}"),
        };
        sha256_hex(&format!("{parts}|{}", sanitize_reason(reason)))
    }
}

fn limits_text(limits: Option<&StreamLimits>) -> String {
    let Some(limits) = limits else {
        return "-".to_owned();
    };
    format!(
        "{:?}/{:?}/{:?}/{:?}/{:?}/{:?}/{:?}",
        limits.rpm,
        limits.tpm,
        limits.concurrent,
        limits.daily_requests,
        limits.daily_tokens,
        limits.monthly_requests,
        limits.monthly_tokens
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredState {
    version: u32,
    streams: Vec<StoredStream>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredStream {
    name: String,
    key: String,
    chain: Vec<String>,
    limits: StoredLimits,
    tokens: Vec<StoredToken>,
    created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct StoredLimits {
    #[serde(default)]
    rpm: Option<u32>,
    #[serde(default)]
    tpm: Option<u64>,
    #[serde(default)]
    concurrent: Option<u32>,
    #[serde(default)]
    daily_requests: Option<u32>,
    #[serde(default)]
    daily_tokens: Option<u64>,
    #[serde(default)]
    monthly_requests: Option<u32>,
    #[serde(default)]
    monthly_tokens: Option<u64>,
}

impl StoredLimits {
    fn from_config(limits: Option<&StreamLimits>) -> Self {
        let blank = StreamLimits {
            rpm: None,
            tpm: None,
            concurrent: None,
            daily_requests: None,
            daily_tokens: None,
            monthly_requests: None,
            monthly_tokens: None,
            period_zone: None,
            on_store_error: None,
        };
        let limits = limits.unwrap_or(&blank);
        Self {
            rpm: limits.rpm,
            tpm: limits.tpm,
            concurrent: limits.concurrent,
            daily_requests: limits.daily_requests,
            daily_tokens: limits.daily_tokens,
            monthly_requests: limits.monthly_requests,
            monthly_tokens: limits.monthly_tokens,
        }
    }

    fn to_config(&self) -> StreamLimits {
        StreamLimits {
            rpm: self.rpm,
            tpm: self.tpm,
            concurrent: self.concurrent,
            daily_requests: self.daily_requests,
            daily_tokens: self.daily_tokens,
            monthly_requests: self.monthly_requests,
            monthly_tokens: self.monthly_tokens,
            period_zone: None,
            on_store_error: None,
        }
    }

    fn changes(&self, other: &Self) -> Vec<AuditChange> {
        let pairs = [
            (
                "limits.rpm",
                self.rpm.map(u64::from),
                other.rpm.map(u64::from),
            ),
            ("limits.tpm", self.tpm, other.tpm),
            (
                "limits.concurrent",
                self.concurrent.map(u64::from),
                other.concurrent.map(u64::from),
            ),
            (
                "limits.daily_requests",
                self.daily_requests.map(u64::from),
                other.daily_requests.map(u64::from),
            ),
            ("limits.daily_tokens", self.daily_tokens, other.daily_tokens),
            (
                "limits.monthly_requests",
                self.monthly_requests.map(u64::from),
                other.monthly_requests.map(u64::from),
            ),
            (
                "limits.monthly_tokens",
                self.monthly_tokens,
                other.monthly_tokens,
            ),
        ];
        pairs
            .into_iter()
            .filter(|(_, from, to)| from != to)
            .map(|(field, from, to)| AuditChange::number(field, from, to))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredToken {
    id: String,
    hash: String,
    expires_at: Option<u64>,
    created_at: u64,
}

#[derive(Debug, Clone)]
struct LiveToken {
    id: String,
    hash: String,
    expires_at: Option<u64>,
    created_at: u64,
}

#[derive(Debug, Clone)]
struct LiveStream {
    key: String,
    chain: Vec<String>,
    limits: StoredLimits,
    tokens: Vec<LiveToken>,
    created_at: u64,
}

impl LiveStream {
    fn to_stored(&self, name: &str) -> StoredStream {
        StoredStream {
            name: name.to_owned(),
            key: self.key.clone(),
            chain: self.chain.clone(),
            limits: self.limits.clone(),
            tokens: self
                .tokens
                .iter()
                .map(|token| StoredToken {
                    id: token.id.clone(),
                    hash: token.hash.clone(),
                    expires_at: token.expires_at,
                    created_at: token.created_at,
                })
                .collect(),
            created_at: self.created_at,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct StateData {
    streams: BTreeMap<String, LiveStream>,
    /// Hashes configured or already assigned, so a generated token can never be reused.
    hashes: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuditChange {
    pub field: String,
    pub from: String,
    pub to: String,
}

impl AuditChange {
    fn new(field: &str, from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            field: field.to_owned(),
            from: sanitize_reason(&from.into()),
            to: sanitize_reason(&to.into()),
        }
    }

    fn number(field: &str, from: Option<u64>, to: Option<u64>) -> Self {
        Self::new(
            field,
            from.map_or_else(|| "-".to_owned(), |value| value.to_string()),
            to.map_or_else(|| "-".to_owned(), |value| value.to_string()),
        )
    }
}

/// One append-only audit record. `hash` covers every other field, in this exact order, and
/// `prev` links the record to its predecessor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuditRecord {
    pub seq: u64,
    pub time: String,
    pub actor: String,
    pub source: String,
    pub action: String,
    pub stream: String,
    pub outcome: String,
    pub reason: String,
    pub changes: Vec<AuditChange>,
    pub prev: String,
    pub hash: String,
}

#[derive(Debug, Serialize)]
struct AuditBody<'a> {
    seq: u64,
    time: &'a str,
    actor: &'a str,
    source: &'a str,
    action: &'a str,
    stream: &'a str,
    outcome: &'a str,
    reason: &'a str,
    changes: &'a [AuditChange],
    prev: &'a str,
}

impl AuditRecord {
    fn digest(body: &AuditBody<'_>) -> String {
        let bytes = serde_json::to_vec(body).expect("serializing audit fields cannot fail");
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        to_hex(&hasher.finalize())
    }

    fn verify(&self) -> Result<(), String> {
        let body = AuditBody {
            seq: self.seq,
            time: &self.time,
            actor: &self.actor,
            source: &self.source,
            action: &self.action,
            stream: &self.stream,
            outcome: &self.outcome,
            reason: &self.reason,
            changes: &self.changes,
            prev: &self.prev,
        };
        if !constant_time_hex(&Self::digest(&body), &self.hash) {
            return Err("hash mismatch".to_owned());
        }
        if !is_sha256_hex(&self.hash) || !is_sha256_hex(&self.prev) && self.seq != 1 {
            return Err("invalid hash shape".to_owned());
        }
        let source_ok = is_sha256_hex(&self.source)
            || self.source == "unknown"
            || self.source.parse::<IpAddr>().is_ok();
        if !is_sha256_hex(&self.actor) || !source_ok {
            return Err("invalid actor or source".to_owned());
        }
        if !matches!(self.outcome.as_str(), "success" | "denied" | "failed") {
            return Err("invalid outcome".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug)]
struct AuditLog {
    path: PathBuf,
    /// Serializes appends so concurrent denials and mutations cannot fork the chain.
    write: Mutex<()>,
    /// A small atomic sidecar holding the last sequence number and hash. The chain itself proves
    /// that a middle record was not edited, inserted or removed; without a separately stored head
    /// it cannot prove that the last record was not deleted.
    head_path: PathBuf,
}

#[derive(Debug, Clone)]
struct AuditTail {
    seq: u64,
    hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditHead {
    seq: u64,
    hash: String,
}

impl AuditLog {
    fn new(path: PathBuf) -> Self {
        let head_path = path.with_extension("head");
        Self {
            path,
            write: Mutex::new(()),
            head_path,
        }
    }

    fn read_head(&self) -> Result<Option<AuditTail>, String> {
        let text = match std::fs::read_to_string(&self.head_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("cannot read the audit head".to_owned()),
        };
        if text.len() > 1024 {
            return Err("the audit head is too large".to_owned());
        }
        let head: AuditHead =
            serde_json::from_str(&text).map_err(|_| "the audit head is invalid".to_owned())?;
        if !is_sha256_hex(&head.hash) {
            return Err("the audit head is invalid".to_owned());
        }
        Ok(Some(AuditTail {
            seq: head.seq,
            hash: head.hash,
        }))
    }

    fn write_head(&self, tail: &AuditTail) -> Result<(), String> {
        let text = serde_json::to_string(&AuditHead {
            seq: tail.seq,
            hash: tail.hash.clone(),
        })
        .map_err(|_| "serializing the audit head failed".to_owned())?;
        crate::state::write_atomically(&self.head_path, &text)
            .map_err(|_| "cannot write the audit head".to_owned())
    }

    /// Reads the whole chain in order. A truncated, edited, inserted or deleted record is a
    /// hard error: seq must start at 1 and increase by one, and each prev must match.
    fn read(&self) -> Result<Vec<AuditRecord>, String> {
        // A missing log is an empty chain, so the first append can create it. An existing head
        // still has to match the (empty) chain, and the CLI checks that the file exists before
        // verifying or inspecting it.
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return if self.read_head()?.is_some() {
                    Err("audit log missing but audit head exists".to_owned())
                } else {
                    Ok(Vec::new())
                };
            }
            Err(error) => return Err(format!("open audit log: {error}")),
        };
        let lines = BufReader::new(file).lines();
        let mut records = Vec::new();
        let mut prev = ZERO_HASH.to_owned();
        for (index, line) in lines.enumerate() {
            let line_no = index + 1;
            let expected_seq = index as u64 + 1;
            let line = line.map_err(|e| format!("audit line {line_no}: {e}"))?;
            if line.len() > MAX_AUDIT_LINE {
                return Err(format!("audit line {line_no}: record too long"));
            }
            if line.trim().is_empty() {
                return Err(format!("audit line {line_no}: blank record"));
            }
            let record: AuditRecord = serde_json::from_str(&line)
                .map_err(|_| format!("audit line {line_no}: invalid record"))?;
            record
                .verify()
                .map_err(|e| format!("audit line {line_no}: {e}"))?;
            if record.seq != expected_seq {
                return Err(format!("audit line {line_no}: sequence gap"));
            }
            if record.prev != prev {
                return Err(format!("audit line {line_no}: broken chain"));
            }
            prev = record.hash.clone();
            records.push(record);
        }
        if let Some(head) = self.read_head()? {
            let last = records.last().map_or(
                AuditTail {
                    seq: 0,
                    hash: ZERO_HASH.to_owned(),
                },
                |record| AuditTail {
                    seq: record.seq,
                    hash: record.hash.clone(),
                },
            );
            if head.seq != last.seq || !constant_time_hex(&head.hash, &last.hash) {
                return Err("audit head mismatch: records were removed or truncated".to_owned());
            }
        } else if !records.is_empty() {
            return Err(
                "audit head missing: records were removed or the sidecar was lost".to_owned(),
            );
        }
        Ok(records)
    }

    fn tail(&self) -> Result<AuditTail, String> {
        let records = self.read()?;
        Ok(records.last().map_or(
            AuditTail {
                seq: 0,
                hash: ZERO_HASH.to_owned(),
            },
            |record| AuditTail {
                seq: record.seq,
                hash: record.hash.clone(),
            },
        ))
    }

    fn append(&self, mut record: AuditRecord) -> Result<AuditRecord, String> {
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        let tail = self.tail()?;
        record.seq = tail.seq.saturating_add(1);
        record.prev = tail.hash;
        let body = AuditBody {
            seq: record.seq,
            time: &record.time,
            actor: &record.actor,
            source: &record.source,
            action: &record.action,
            stream: &record.stream,
            outcome: &record.outcome,
            reason: &record.reason,
            changes: &record.changes,
            prev: &record.prev,
        };
        record.hash = AuditRecord::digest(&body);
        let line =
            serde_json::to_string(&record).map_err(|_| "serializing audit failed".to_owned())?;
        if line.len() > MAX_AUDIT_LINE {
            return Err("audit record too long".to_owned());
        }
        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true).mode(0o600);
        let mut file = options
            .open(&self.path)
            .map_err(|e| format!("cannot open audit log: {e}"))?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("cannot write audit log: {e}"))?;
        self.write_head(&AuditTail {
            seq: record.seq,
            hash: record.hash.clone(),
        })?;
        Ok(record)
    }
}

#[derive(Debug, Default)]
struct Idempotent {
    fingerprint: String,
    response: serde_json::Value,
}

#[derive(Debug, Default)]
struct IdempotencyStore {
    entries: HashMap<String, Idempotent>,
    order: std::collections::VecDeque<String>,
}

impl IdempotencyStore {
    fn key(source: IpAddr, idempotency: &str) -> String {
        format!("{source}|{idempotency}")
    }

    fn get(&self, key: &str, fingerprint: &str) -> Result<Option<serde_json::Value>, ApiError> {
        let Some(entry) = self.entries.get(key) else {
            return Ok(None);
        };
        if entry.fingerprint != fingerprint {
            return Err(ApiError::CONFLICT);
        }
        Ok(Some(entry.response.clone()))
    }

    fn insert(&mut self, key: String, fingerprint: String, mut response: serde_json::Value) {
        if let Some(object) = response.as_object_mut() {
            object.remove("token");
        }
        let replaced = self
            .entries
            .insert(
                key.clone(),
                Idempotent {
                    fingerprint,
                    response,
                },
            )
            .is_some();
        if !replaced {
            self.order.push_back(key);
        }
        while self.order.len() > MAX_IDEMPOTENCY_ENTRIES {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
    }
}

#[derive(Debug, Default)]
struct StreamAdminMetricValues {
    actions: BTreeMap<&'static str, u64>,
    dynamic_streams: u64,
    expiring_soon: u64,
}

#[derive(Debug, Default)]
pub(crate) struct StreamAdminMetrics {
    inner: Mutex<StreamAdminMetricValues>,
}

impl StreamAdminMetrics {
    pub(crate) fn record_action(&self, result: &'static str) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        *inner.actions.entry(result).or_insert(0) += 1;
    }

    pub(crate) fn set_gauges(&self, streams: u64, expiring: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.dynamic_streams = streams;
        inner.expiring_soon = expiring;
    }

    pub(crate) fn render(&self) -> String {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = String::new();
        out.push_str("# HELP reiki_stream_admin_actions_total Management actions by result.\n");
        out.push_str("# TYPE reiki_stream_admin_actions_total counter\n");
        for (result, count) in &inner.actions {
            let _ = writeln!(
                out,
                "reiki_stream_admin_actions_total{{result=\"{result}\"}} {count}"
            );
        }
        out.push_str("# HELP reiki_dynamic_streams Active dynamic streams.\n");
        out.push_str("# TYPE reiki_dynamic_streams gauge\n");
        let _ = writeln!(out, "reiki_dynamic_streams {}", inner.dynamic_streams);
        out.push_str(
            "# HELP reiki_stream_tokens_expiring_soon Unexpired tokens expiring within one hour.\n",
        );
        out.push_str("# TYPE reiki_stream_tokens_expiring_soon gauge\n");
        let _ = writeln!(
            out,
            "reiki_stream_tokens_expiring_soon {}",
            inner.expiring_soon
        );
        out
    }
}

/// The runtime owner of dynamic stream state. Created only when `[stream_admin]` is configured.
pub struct Manager {
    config: StreamAdminConfig,
    state_path: PathBuf,
    audit: AuditLog,
    state: Mutex<StateData>,
    /// Serializes the audit append + state replace so two mutations cannot write the same base.
    mutation: Mutex<()>,
    idempotency: Mutex<IdempotencyStore>,
    denied_at: Mutex<HashMap<IpAddr, Instant>>,
    app: OnceLock<Weak<App>>,
    state_error: RwLock<Option<String>>,
    token_seq: AtomicU64,
    metrics: Arc<StreamAdminMetrics>,
}

impl Manager {
    pub fn new(config: &StreamAdminConfig, state_dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            config: config.clone(),
            state_path: state_dir.join(&config.state_file),
            audit: AuditLog::new(state_dir.join(&config.audit_log)),
            state: Mutex::new(StateData::default()),
            mutation: Mutex::new(()),
            idempotency: Mutex::new(IdempotencyStore::default()),
            denied_at: Mutex::new(HashMap::new()),
            app: OnceLock::new(),
            state_error: RwLock::new(None),
            token_seq: AtomicU64::new(0),
            metrics: Arc::new(StreamAdminMetrics::default()),
        })
    }

    pub(crate) fn attach(self: &Arc<Self>, app: Weak<App>) {
        let _ = self.app.set(app);
    }

    pub(crate) fn metrics(&self) -> Arc<StreamAdminMetrics> {
        Arc::clone(&self.metrics)
    }

    pub(crate) fn config(&self) -> &StreamAdminConfig {
        &self.config
    }

    /// The live snapshot already contains this manager's dynamic streams. State validation and
    /// mutation must compare against static config only, or a previously applied stream would
    /// look like a colliding configured stream and every later mutation would fail.
    fn static_config(config: &Config) -> Config {
        let mut config = config.clone();
        config.streams.retain(|_, stream| stream.dynamic.is_none());
        config
    }

    fn app(&self) -> Option<Arc<App>> {
        self.app.get()?.upgrade()
    }

    /// Reads the state file, applying nothing if it is invalid. Static streams keep working;
    /// dynamic streams are dropped, which fails closed rather than trusting edited state.
    pub fn reload_from_disk(self: &Arc<Self>) {
        let _guard = self.mutation.lock().unwrap_or_else(PoisonError::into_inner);
        let loaded = self.read_state();
        match loaded {
            Ok(state) => {
                *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
                *self
                    .state_error
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = None;
            }
            Err(error) => {
                *self.state.lock().unwrap_or_else(PoisonError::into_inner) = StateData::default();
                *self
                    .state_error
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = Some(error);
            }
        }
        self.apply();
    }

    pub(crate) fn state_error(&self) -> Option<String> {
        self.state_error
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn read_state(&self) -> Result<StateData, String> {
        let text = match std::fs::read_to_string(&self.state_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StateData::default());
            }
            Err(_) => return Err("the dynamic stream state file could not be read".to_owned()),
        };
        if text.len() > MAX_STATE_BYTES {
            return Err("the dynamic stream state file is too large".to_owned());
        }
        let stored: StoredState = serde_json::from_str(&text)
            .map_err(|_| "the dynamic stream state file is invalid".to_owned())?;
        if stored.version != STATE_VERSION {
            return Err("the dynamic stream state file has an unsupported version".to_owned());
        }
        if stored.streams.len() > usize::try_from(self.config.max_streams).unwrap_or(usize::MAX) {
            return Err("the dynamic stream state file has too many streams".to_owned());
        }
        let app = self
            .app()
            .ok_or_else(|| "the runtime is starting".to_owned())?;
        let config = Self::static_config(&app.snapshot().config);
        let mut state = StateData::default();
        let mut names = BTreeSet::new();
        for stream in stored.streams {
            if !valid_name(&stream.name)
                || config.streams.contains_key(&stream.name)
                || !names.insert(stream.name.clone())
            {
                return Err(
                    "the dynamic stream state file has an invalid or colliding stream".to_owned(),
                );
            }
            if !config.keys.get(&stream.key).is_some_and(|_| true)
                || !self.config.allowed_keys.contains_key(&stream.key)
            {
                return Err("the dynamic stream state file assigns an unknown key".to_owned());
            }
            if stream.chain != [stream.key.clone()] {
                return Err("the dynamic stream state file has an invalid chain".to_owned());
            }
            if !limits_within(&stream.limits.to_config(), &self.config.ceilings) {
                return Err("the dynamic stream state file exceeds a ceiling".to_owned());
            }
            if stream.tokens.is_empty()
                || stream.tokens.len()
                    > usize::try_from(self.config.max_tokens_per_stream).unwrap_or(usize::MAX)
            {
                return Err("the dynamic stream state file has an invalid token count".to_owned());
            }
            let now = unix_now();
            let mut tokens = Vec::new();
            for token in stream.tokens {
                if !valid_id(&token.id) || !is_sha256_hex(&token.hash) {
                    return Err("the dynamic stream state file has an invalid token".to_owned());
                }
                if token.expires_at.is_some_and(|expires| expires <= now) {
                    continue;
                }
                if !state.hashes.insert(token.hash.to_ascii_lowercase()) {
                    return Err("the dynamic stream state file repeats a token hash".to_owned());
                }
                tokens.push(LiveToken {
                    id: token.id,
                    hash: token.hash.to_ascii_lowercase(),
                    expires_at: token.expires_at,
                    created_at: token.created_at,
                });
            }
            if tokens.is_empty() {
                continue;
            }
            state.streams.insert(
                stream.name.clone(),
                LiveStream {
                    key: stream.key,
                    chain: stream.chain,
                    limits: stream.limits,
                    tokens,
                    created_at: stream.created_at,
                },
            );
        }
        self.check_config_hashes(&config, &mut state)?;
        Ok(state)
    }

    fn check_config_hashes(&self, config: &Config, state: &mut StateData) -> Result<(), String> {
        let mut hashes = BTreeSet::new();
        for stream in config.streams.values() {
            hashes.insert(stream.token_sha256.to_ascii_lowercase());
        }
        if let Some(admin) = &config.admin {
            hashes.insert(admin.token_sha256.to_ascii_lowercase());
        }
        hashes.insert(self.config.token_sha256.to_ascii_lowercase());
        hashes.insert(self.config.actor_id_sha256.to_ascii_lowercase());
        for hash in &self.config.require_two_party {
            hashes.insert(hash.to_ascii_lowercase());
        }
        for stream in state.streams.values() {
            for token in &stream.tokens {
                if hashes.contains(&token.hash) {
                    return Err("a dynamic token reuses a configured credential".to_owned());
                }
            }
        }
        state.hashes.extend(hashes);
        Ok(())
    }

    /// Rebuilds the snapshot with the current static config plus loaded dynamic streams.
    pub(crate) fn apply(self: &Arc<Self>) {
        let Some(app) = self.app() else {
            return;
        };
        let config = Self::static_config(&app.snapshot().config);
        let streams = self.build_streams(&config);
        let count = streams.len() as u64;
        app.apply_stream_config(config, streams);
        let now = unix_now();
        let mut expiring = 0_u64;
        for stream in self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .streams
            .values()
        {
            for token in &stream.tokens {
                if token
                    .expires_at
                    .is_some_and(|expires| expires > now && expires.saturating_sub(now) <= 3600)
                {
                    expiring += 1;
                }
            }
        }
        self.metrics.set_gauges(count, expiring);
    }

    fn build_streams(&self, config: &Config) -> Vec<(String, StreamConfig)> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .streams
            .iter()
            .map(|(name, stream)| {
                let token_ids = stream.tokens.iter().map(|t| t.id.clone()).collect();
                let dynamic_tokens = stream
                    .tokens
                    .iter()
                    .map(|token| DynamicToken {
                        id: token.id.clone(),
                        token_sha256: token.hash.clone(),
                        expires_at: token.expires_at,
                    })
                    .collect();
                (
                    name.clone(),
                    StreamConfig {
                        token_sha256: stream
                            .tokens
                            .first()
                            .map_or_else(|| ZERO_HASH.to_owned(), |t| t.hash.clone()),
                        chain: stream.chain.clone(),
                        strategy: crate::config::StrategyConfig::Sticky,
                        limits: Some(stream.limits.to_config()),
                        allowed_upstreams: None,
                        allowed_models: None,
                        weight: 1,
                        dynamic: Some(DynamicStreamMeta {
                            key: stream.key.clone(),
                            token_ids,
                            created_at: stream.created_at,
                        }),
                        dynamic_tokens,
                    },
                )
            })
            .filter(|(name, _)| !config.streams.contains_key(name))
            .collect()
    }

    /// Audits and applies a mutation. The audit record is fsynced before the state file is
    /// replaced; if the state write fails, a second `failed` record is appended and nothing in
    /// memory changes.
    pub(crate) fn mutate(
        self: &Arc<Self>,
        source: IpAddr,
        idempotency: &str,
        reason: &str,
        mutation: &Mutation,
    ) -> Result<serde_json::Value, ApiError> {
        let _guard = self.mutation.lock().unwrap_or_else(PoisonError::into_inner);
        let fingerprint = mutation.fingerprint(reason);
        let key = IdempotencyStore::key(source, idempotency);
        if let Some(replay) = self
            .idempotency
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key, &fingerprint)?
        {
            return Ok(replay);
        }
        let Some(app) = self.app() else {
            return Err(ApiError::DISABLED);
        };
        let config = Self::static_config(&app.snapshot().config);
        let clean_reason = sanitize_reason(reason);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        self.check_config_hashes(&config, &mut state)
            .map_err(|_| ApiError::STORE)?;
        let now = unix_now();
        let (response, changes) = self.apply_mutation(&config, &mut state, mutation, now)?;
        let stored = StoredState {
            version: STATE_VERSION,
            streams: state
                .streams
                .iter()
                .map(|(name, stream)| stream.to_stored(name))
                .collect(),
        };
        let text = serde_json::to_string(&stored).map_err(|_| ApiError::STORE)?;
        if text.len() > MAX_STATE_BYTES {
            return Err(ApiError::LIMIT);
        }
        let record = AuditRecord {
            seq: 0,
            time: utc_now(),
            actor: self.config.actor_id_sha256.to_ascii_lowercase(),
            source: source.to_string(),
            action: mutation.action().to_owned(),
            stream: sanitize_reason(mutation.stream()),
            outcome: "success".to_owned(),
            reason: clean_reason.clone(),
            changes: changes.clone(),
            prev: ZERO_HASH.to_owned(),
            hash: ZERO_HASH.to_owned(),
        };
        if self.audit.append(record).is_err() {
            self.record_action("failed");
            return Err(ApiError::STORE);
        }
        if crate::state::write_atomically(&self.state_path, &text).is_err() {
            let failed = AuditRecord {
                seq: 0,
                time: utc_now(),
                actor: self.config.actor_id_sha256.to_ascii_lowercase(),
                source: source.to_string(),
                action: mutation.action().to_owned(),
                stream: sanitize_reason(mutation.stream()),
                outcome: "failed".to_owned(),
                reason: "state_write_failed".to_owned(),
                changes: vec![AuditChange::new("state.write", "ok", "failed")],
                prev: ZERO_HASH.to_owned(),
                hash: ZERO_HASH.to_owned(),
            };
            let _ = self.audit.append(failed);
            self.record_action("failed");
            return Err(ApiError::STORE);
        }
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
        self.idempotency
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, fingerprint, response.clone());
        self.apply();
        self.record_action("success");
        Ok(response)
    }

    fn apply_mutation(
        &self,
        config: &Config,
        state: &mut StateData,
        mutation: &Mutation,
        now: u64,
    ) -> Result<(serde_json::Value, Vec<AuditChange>), ApiError> {
        match mutation {
            Mutation::Create {
                name,
                key,
                ttl,
                limits,
            } => {
                if !valid_name(name)
                    || config.streams.contains_key(name)
                    || state.streams.contains_key(name)
                {
                    return Err(ApiError::CONFLICT);
                }
                if state.streams.len()
                    >= usize::try_from(self.config.max_streams).unwrap_or(usize::MAX)
                {
                    return Err(ApiError::LIMIT);
                }
                if !self.config.allowed_keys.contains_key(key) || !config.keys.contains_key(key) {
                    return Err(ApiError::BAD_REQUEST);
                }
                let limits = StoredLimits::from_config(limits.as_ref());
                if !limits_within(&limits.to_config(), &self.config.ceilings) {
                    return Err(ApiError::LIMIT);
                }
                let (raw, hash) = self.new_token()?;
                if state.hashes.contains(&hash) {
                    return Err(ApiError::CONFLICT);
                }
                let expires_at = self.expiry(*ttl, now)?;
                let id = token_id(&hash);
                let mut changes = vec![
                    AuditChange::new("stream", "-", name.as_str()),
                    AuditChange::new("key", "-", key.as_str()),
                    AuditChange::new("chain", "-", key.as_str()),
                    AuditChange::new("tokens.count", "0", "1"),
                    AuditChange::new("token.id", "-", id.as_str()),
                ];
                changes.extend(limits.changes(&StoredLimits::from_config(None)));
                state.hashes.insert(hash.clone());
                state.streams.insert(
                    name.clone(),
                    LiveStream {
                        key: key.clone(),
                        chain: vec![key.clone()],
                        limits,
                        tokens: vec![LiveToken {
                            id: id.clone(),
                            hash,
                            expires_at,
                            created_at: now,
                        }],
                        created_at: now,
                    },
                );
                let response = serde_json::json!({
                    "name": name,
                    "key": key,
                    "token": raw,
                    "token_id": id,
                    "expires_at": expires_at,
                });
                Ok((response, changes))
            }
            Mutation::Update { name, key, limits } => {
                let stream = state.streams.get_mut(name).ok_or(ApiError::NOT_FOUND)?;
                if config.streams.contains_key(name) {
                    return Err(ApiError::CONFLICT);
                }
                let mut changes = Vec::new();
                if let Some(key) = key {
                    if !self.config.allowed_keys.contains_key(key) || !config.keys.contains_key(key)
                    {
                        return Err(ApiError::BAD_REQUEST);
                    }
                    if stream.key != *key {
                        changes.push(AuditChange::new("key", &stream.key, key));
                        changes.push(AuditChange::new("chain", &stream.key, key));
                        stream.key = key.clone();
                        stream.chain = vec![key.clone()];
                    }
                }
                if let Some(limits) = limits {
                    let next = StoredLimits::from_config(Some(limits));
                    if !limits_within(&next.to_config(), &self.config.ceilings) {
                        return Err(ApiError::LIMIT);
                    }
                    changes.extend(stream.limits.changes(&next));
                    stream.limits = next;
                }
                if changes.is_empty() {
                    return Err(ApiError::BAD_REQUEST);
                }
                Ok((serde_json::json!({"name": name, "updated": true}), changes))
            }
            Mutation::AddToken { name, ttl } => {
                let stream = state.streams.get_mut(name).ok_or(ApiError::NOT_FOUND)?;
                if config.streams.contains_key(name) {
                    return Err(ApiError::CONFLICT);
                }
                if stream.tokens.len()
                    >= usize::try_from(self.config.max_tokens_per_stream).unwrap_or(usize::MAX)
                {
                    return Err(ApiError::LIMIT);
                }
                let (raw, hash) = self.new_token()?;
                if state.hashes.contains(&hash) {
                    return Err(ApiError::CONFLICT);
                }
                let expires_at = self.expiry(*ttl, now)?;
                let id = token_id(&hash);
                let changes = vec![
                    AuditChange::new(
                        "tokens.count",
                        stream.tokens.len().to_string(),
                        (stream.tokens.len() + 1).to_string(),
                    ),
                    AuditChange::new("token.id", "-", &id),
                ];
                state.hashes.insert(hash.clone());
                stream.tokens.push(LiveToken {
                    id: id.clone(),
                    hash,
                    expires_at,
                    created_at: now,
                });
                Ok((
                    serde_json::json!({
                        "name": name,
                        "token": raw,
                        "token_id": id,
                        "expires_at": expires_at,
                    }),
                    changes,
                ))
            }
            Mutation::RevokeToken { name, token_id } => {
                let stream = state.streams.get_mut(name).ok_or(ApiError::NOT_FOUND)?;
                if config.streams.contains_key(name) {
                    return Err(ApiError::CONFLICT);
                }
                let Some(index) = stream.tokens.iter().position(|token| token.id == *token_id)
                else {
                    return Err(ApiError::NOT_FOUND);
                };
                let removed = stream.tokens.remove(index);
                state.hashes.remove(&removed.hash);
                let changes = vec![
                    AuditChange::new(
                        "tokens.count",
                        (stream.tokens.len() + 1).to_string(),
                        stream.tokens.len().to_string(),
                    ),
                    AuditChange::new("token.id", token_id, "-"),
                ];
                Ok((
                    serde_json::json!({"name": name, "revoked": token_id}),
                    changes,
                ))
            }
            Mutation::RevokeAll { name } => {
                let stream = state.streams.get_mut(name).ok_or(ApiError::NOT_FOUND)?;
                if config.streams.contains_key(name) {
                    return Err(ApiError::CONFLICT);
                }
                if stream.tokens.is_empty() {
                    return Err(ApiError::NOT_FOUND);
                }
                let before = stream.tokens.len();
                for token in &stream.tokens {
                    state.hashes.remove(&token.hash);
                }
                stream.tokens.clear();
                let changes = vec![
                    AuditChange::new("tokens.count", before.to_string(), "0"),
                    AuditChange::new("tokens.all", "active", "revoked"),
                ];
                Ok((serde_json::json!({"name": name, "revoked": "all"}), changes))
            }
            Mutation::Delete { name } => {
                let stream = state.streams.get(name).ok_or(ApiError::NOT_FOUND)?;
                if config.streams.contains_key(name) {
                    return Err(ApiError::CONFLICT);
                }
                for token in &stream.tokens {
                    state.hashes.remove(&token.hash);
                }
                state.streams.remove(name);
                Ok((
                    serde_json::json!({"name": name, "deleted": true}),
                    vec![AuditChange::new("stream", name, "-")],
                ))
            }
        }
    }

    fn expiry(&self, ttl: Option<u64>, now: u64) -> Result<Option<u64>, ApiError> {
        let ttl = ttl.unwrap_or(self.config.default_ttl);
        if ttl == 0 || ttl > self.config.max_ttl {
            return Err(ApiError::LIMIT);
        }
        Ok(Some(now.saturating_add(ttl)))
    }

    fn new_token(&self) -> Result<(String, String), ApiError> {
        let mut bytes = [0_u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut bytes))
            .map_err(|_| ApiError::STORE)?;
        let raw = format!("rsk_{}", to_hex(&bytes));
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        let hash = to_hex(&hasher.finalize());
        let _ = self.token_seq.fetch_add(1, Ordering::Relaxed);
        Ok((raw, hash))
    }

    fn record_action(&self, result: &'static str) {
        self.metrics.record_action(result);
    }

    /// Denied attempts are audited only once per source per lockout window, so a flood cannot
    /// grow the audit log without bound or hide a real mutation behind noise.
    pub(crate) fn note_denied(
        &self,
        source: IpAddr,
        action: &'static str,
        stream: &str,
        reason: &'static str,
    ) {
        self.record_action("denied");
        let now = Instant::now();
        let mut seen = self
            .denied_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let recently = seen.get(&source).is_some_and(|at| {
            now.duration_since(*at) < Duration::from_secs(self.config.lockout_secs)
        });
        if recently {
            return;
        }
        seen.insert(source, now);
        drop(seen);
        let record = AuditRecord {
            seq: 0,
            time: utc_now(),
            actor: ZERO_HASH.to_owned(),
            source: source.to_string(),
            action: action.to_owned(),
            stream: sanitize_reason(stream),
            outcome: "denied".to_owned(),
            reason: reason.to_owned(),
            changes: Vec::new(),
            prev: ZERO_HASH.to_owned(),
            hash: ZERO_HASH.to_owned(),
        };
        let _ = self.audit.append(record);
    }

    pub(crate) fn note_failed(
        &self,
        source: IpAddr,
        action: &'static str,
        stream: &str,
        reason: &'static str,
    ) {
        self.record_action("failed");
        let record = AuditRecord {
            seq: 0,
            time: utc_now(),
            actor: ZERO_HASH.to_owned(),
            source: source.to_string(),
            action: action.to_owned(),
            stream: sanitize_reason(stream),
            outcome: "failed".to_owned(),
            reason: reason.to_owned(),
            changes: Vec::new(),
            prev: ZERO_HASH.to_owned(),
            hash: ZERO_HASH.to_owned(),
        };
        let _ = self.audit.append(record);
    }

    /// Read-only list of dynamic streams, with no hashes or token values.
    pub(crate) fn list(&self) -> Vec<serde_json::Value> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .streams
            .iter()
            .map(|(name, stream)| {
                serde_json::json!({
                    "name": name,
                    "key": stream.key,
                    "chain": stream.chain,
                    "limits": limits_json(&stream.limits),
                    "tokens": stream.tokens.iter().map(|token| serde_json::json!({
                        "id": token.id,
                        "expires_at": token.expires_at,
                        "created_at": token.created_at,
                    })).collect::<Vec<_>>(),
                    "created_at": stream.created_at,
                })
            })
            .collect()
    }

    pub(crate) fn show(&self, name: &str) -> Option<serde_json::Value> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let stream = state.streams.get(name)?;
        Some(serde_json::json!({
            "name": name,
            "key": stream.key,
            "chain": stream.chain,
            "limits": limits_json(&stream.limits),
            "tokens": stream.tokens.iter().map(|token| serde_json::json!({
                "id": token.id,
                "expires_at": token.expires_at,
                "created_at": token.created_at,
            })).collect::<Vec<_>>(),
            "created_at": stream.created_at,
        }))
    }
}

fn limits_json(limits: &StoredLimits) -> serde_json::Value {
    serde_json::json!({
        "rpm": limits.rpm,
        "tpm": limits.tpm,
        "concurrent": limits.concurrent,
        "daily_requests": limits.daily_requests,
        "daily_tokens": limits.daily_tokens,
        "monthly_requests": limits.monthly_requests,
        "monthly_tokens": limits.monthly_tokens,
    })
}

fn require_audit_file(path: &Path) -> Result<(), String> {
    if path.exists() {
        Ok(())
    } else {
        Err("audit log does not exist".to_owned())
    }
}

/// Verifies a whole audit file. Returns the number of records on success.
pub fn verify_audit(path: &Path) -> Result<usize, String> {
    require_audit_file(path)?;
    AuditLog::new(path.to_owned())
        .read()
        .map(|records| records.len())
}

/// Reads the first `n` audit records for operator inspection.
pub fn audit_head(path: &Path, n: usize) -> Result<Vec<AuditRecord>, String> {
    require_audit_file(path)?;
    let mut records = AuditLog::new(path.to_owned()).read()?;
    records.truncate(n);
    Ok(records)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= ID_MAX
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn token_id(hash: &str) -> String {
    hash.chars().take(16).collect()
}

fn limits_within(limits: &StreamLimits, ceilings: &crate::config::StreamCeilings) -> bool {
    for (value, ceiling) in [
        (limits.rpm.map(u64::from), ceilings.rpm.map(u64::from)),
        (limits.tpm, ceilings.tpm),
        (
            limits.concurrent.map(u64::from),
            ceilings.concurrent.map(u64::from),
        ),
        (
            limits.daily_requests.map(u64::from),
            ceilings.daily_requests.map(u64::from),
        ),
        (limits.daily_tokens, ceilings.daily_tokens),
        (
            limits.monthly_requests.map(u64::from),
            ceilings.monthly_requests.map(u64::from),
        ),
        (limits.monthly_tokens, ceilings.monthly_tokens),
    ] {
        if let (Some(value), Some(ceiling)) = (value, ceiling)
            && value > ceiling
        {
            return false;
        }
    }
    true
}

/// Removes material that could be a token or a bearer header before any caller-controlled text is
/// written to an audit record. Operator reasons and field values stay readable when they contain
/// none of these forms.
pub fn sanitize_reason(reason: &str) -> String {
    let mut out = String::new();
    let bytes = reason.as_bytes();
    let mut index = 0;
    while index < bytes.len() && out.len() < MAX_REASON_BYTES {
        let byte = bytes[index];
        if byte.is_ascii_hexdigit() {
            let mut end = index;
            while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                end += 1;
            }
            if end - index >= 32 {
                out.push_str("[redacted]");
                index = end;
                continue;
            }
        }
        let lower = reason[index..].to_ascii_lowercase();
        if lower.starts_with("bearer ") || lower.starts_with("sk-") || lower.starts_with("rsk_") {
            out.push_str("[redacted]");
            break;
        }
        let ch = reason[index..].chars().next().expect("byte boundary");
        if ch.is_control() {
            out.push(' ');
        } else {
            out.push(ch);
        }
        index += ch.len_utf8();
    }
    out
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn constant_time_hex(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

pub(crate) fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    to_hex(&hasher.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn utc_now() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::from_unix_timestamp(i64::try_from(unix_now()).unwrap_or(i64::MAX))
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_reasons_are_bounded_and_scrubbed() {
        assert!(valid_name("tenant_a-1"));
        assert!(!valid_name("../etc"));
        assert!(!valid_name("with space"));
        let cleaned = sanitize_reason("rotate 0123456789abcdef0123456789abcdef Bearer abc");
        assert!(cleaned.contains("rotate"));
        assert!(!cleaned.contains("Bearer"));
        assert!(!cleaned.contains("0123456789abcdef"));
        let change = AuditChange::new("stream", "-", "tenant rsk_deadbeef");
        assert!(!change.to.contains("rsk_"));
    }

    #[test]
    fn audit_change_numbers_are_field_named() {
        let changes = StoredLimits::from_config(None).changes(&StoredLimits {
            rpm: Some(30),
            ..StoredLimits::from_config(None)
        });
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].field, "limits.rpm");
        assert_eq!(changes[0].from, "-");
        assert_eq!(changes[0].to, "30");
    }

    fn sample_record(action: &str, stream: &str) -> AuditRecord {
        AuditRecord {
            seq: 0,
            time: "2026-01-01T00:00:00Z".to_owned(),
            actor: ZERO_HASH.to_owned(),
            source: "127.0.0.1".to_owned(),
            action: action.to_owned(),
            stream: stream.to_owned(),
            outcome: "success".to_owned(),
            reason: "setup".to_owned(),
            changes: Vec::new(),
            prev: ZERO_HASH.to_owned(),
            hash: ZERO_HASH.to_owned(),
        }
    }

    struct Dir(PathBuf);

    impl Dir {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "reiki-stream-audit-{label}-{}-{}",
                std::process::id(),
                std::sync::atomic::AtomicUsize::new(0).fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn build_chain(dir: &Dir, count: usize) -> PathBuf {
        let path = dir.path("audit.jsonl");
        let log = AuditLog::new(path.clone());
        for index in 0..count {
            log.append(sample_record("create", &format!("s{index}")))
                .unwrap();
        }
        path
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_tail_sidecar_detects_tail_deletion_and_truncation() {
        let dir = Dir::new("tail");
        let path = build_chain(&dir, 3);
        assert_eq!(verify_audit(&path).unwrap(), 3);
        assert_eq!(audit_head(&path, 2).unwrap().len(), 2);

        let mut shortened = lines(&path);
        shortened.pop();
        std::fs::write(&path, shortened.join("\n") + "\n").unwrap();
        assert!(
            verify_audit(&path).is_err(),
            "deleting the last record must fail"
        );

        let dir = Dir::new("truncate");
        let path = build_chain(&dir, 2);
        let mut lines = lines(&path);
        let last = lines.pop().unwrap();
        lines.push(last[..last.len() - 4].to_owned());
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        assert!(verify_audit(&path).is_err());
    }

    #[test]
    fn inserted_deleted_and_edited_middle_records_are_detected() {
        let dir = Dir::new("middle");
        let path = build_chain(&dir, 3);
        let mut edited = lines(&path);
        let mut record: AuditRecord = serde_json::from_str(&edited[1]).unwrap();
        record.reason = "tampered".to_owned();
        edited[1] = serde_json::to_string(&record).unwrap();
        std::fs::write(&path, edited.join("\n") + "\n").unwrap();
        assert!(verify_audit(&path).is_err(), "an edited record must fail");

        let dir = Dir::new("insert");
        let path = build_chain(&dir, 2);
        let mut inserted = lines(&path);
        let copy = inserted[0].clone();
        inserted.insert(1, copy);
        std::fs::write(&path, inserted.join("\n") + "\n").unwrap();
        assert!(verify_audit(&path).is_err(), "an inserted record must fail");

        let dir = Dir::new("delete");
        let path = build_chain(&dir, 3);
        let mut deleted = lines(&path);
        deleted.remove(1);
        std::fs::write(&path, deleted.join("\n") + "\n").unwrap();
        assert!(
            verify_audit(&path).is_err(),
            "a deleted middle record must fail"
        );
    }

    #[test]
    fn a_missing_or_tampered_audit_head_is_detected() {
        let dir = Dir::new("head");
        let path = build_chain(&dir, 2);
        let head = path.with_extension("head");
        std::fs::remove_file(&head).unwrap();
        assert!(verify_audit(&path).is_err());

        let dir = Dir::new("head-tamper");
        let path = build_chain(&dir, 2);
        let head = path.with_extension("head");
        std::fs::write(&head, "{\"seq\":1,\"hash\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}").unwrap();
        assert!(verify_audit(&path).is_err());
    }

    #[test]
    fn metrics_use_fixed_labels_and_never_echo_a_token() {
        let metrics = StreamAdminMetrics::default();
        metrics.record_action("denied");
        metrics.record_action("success");
        metrics.set_gauges(2, 1);
        let text = metrics.render();
        assert!(text.contains("reiki_stream_admin_actions_total{result=\"denied\"} 1"));
        assert!(text.contains("reiki_dynamic_streams 2"));
        assert!(text.contains("reiki_stream_tokens_expiring_soon 1"));
        assert!(!text.contains("rsk_"));
    }

    #[test]
    fn a_edited_audit_record_is_detected() {
        let record = AuditRecord {
            seq: 1,
            time: "2026-01-01T00:00:00Z".to_owned(),
            actor: ZERO_HASH.to_owned(),
            source: "127.0.0.1".to_owned(),
            action: "create".to_owned(),
            stream: "a".to_owned(),
            outcome: "success".to_owned(),
            reason: "setup".to_owned(),
            changes: Vec::new(),
            prev: ZERO_HASH.to_owned(),
            hash: ZERO_HASH.to_owned(),
        };
        let body = AuditBody {
            seq: record.seq,
            time: &record.time,
            actor: &record.actor,
            source: &record.source,
            action: &record.action,
            stream: &record.stream,
            outcome: &record.outcome,
            reason: &record.reason,
            changes: &record.changes,
            prev: &record.prev,
        };
        let mut edited = record.clone();
        edited.hash = AuditRecord::digest(&body);
        assert!(edited.verify().is_ok());
        edited.reason = "tampered".to_owned();
        assert!(edited.verify().is_err());
    }
}
