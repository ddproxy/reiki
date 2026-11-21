//! `reiki peers`: read each configured peer's `/v1/health` with a client of its own, trusting
//! only that peer's CA, and report anything unanswered or old as unknown.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::config::{PeerEntry, PeersConfig};

/// Bounds one fetch, connect included.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest `/v1/health` body read; a bigger one is refused, not truncated.
pub const MAX_BODY: usize = 64 * 1024;

/// One upstream as a peer publishes it. The wire shape is validated by [`UpstreamWire`]; this is
/// only the part `reiki peers` displays.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamHealth {
    pub name: String,
    pub healthy_keys: u32,
    pub cooling_keys: u32,
    pub disabled_keys: u32,
    pub soonest_cooldown_seconds: Option<u64>,
    pub last_status_class: Option<String>,
}

impl UpstreamHealth {
    /// `ok` while a key is usable, else what keeps the upstream out.
    pub fn state(&self) -> &'static str {
        if self.healthy_keys > 0 {
            "ok"
        } else if self.cooling_keys > 0 {
            "cooling"
        } else if self.disabled_keys > 0 {
            "disabled"
        } else {
            "no_keys"
        }
    }
}

/// The current `/v1/health` wire shape. Every required figure must be present; fields a
/// publisher omits by configuration are optional, and unknown fields are an error.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct HealthDocWire {
    instance: String,
    reiki_version: String,
    observed_at: String,
    age_seconds: u64,
    upstreams: Vec<UpstreamWire>,
    failures: BTreeMap<String, u64>,
    #[serde(default)]
    host: Option<Value>,
    #[serde(default)]
    sessions: Option<Value>,
}

/// An `Option` that must be present in JSON; `null` is still a valid value. Serde would otherwise
/// accept a missing `Option` field as `None`, making an absent field read as `null`.
#[allow(dead_code)]
#[derive(Debug)]
struct RequiredNullable<T>(Option<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for RequiredNullable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // `Option::deserialize` would also accept serde's missing-field deserializer, making an
        // absent field look like `null`. Reading a `Value` first keeps absence an error.
        let value = Value::deserialize(deserializer)?;
        let inner = if value.is_null() {
            None
        } else {
            Some(T::deserialize(value).map_err(D::Error::custom)?)
        };
        Ok(Self(inner))
    }
}

/// One upstream in the current wire shape, including fields this reader does not display.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct UpstreamWire {
    name: String,
    healthy_keys: u32,
    cooling_keys: u32,
    disabled_keys: u32,
    cooling_combinations: u32,
    soonest_cooldown_seconds: RequiredNullable<u64>,
    last_status_class: RequiredNullable<String>,
    #[serde(default)]
    keys: Option<Value>,
    #[serde(default)]
    requests_1h: Option<Value>,
    #[serde(default)]
    limited_1h: Option<Value>,
    #[serde(default)]
    request_buckets: Option<Value>,
    #[serde(default)]
    tokens_1h: Option<Value>,
    #[serde(default)]
    token_buckets: Option<Value>,
    #[serde(default)]
    request_minutes: Option<Value>,
    #[serde(default)]
    token_minutes: Option<Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct HealthDoc {
    pub(crate) instance: String,
    pub(crate) observed_at: String,
    pub(crate) age_seconds: u64,
    pub(crate) upstreams: Vec<UpstreamHealth>,
}

impl From<UpstreamWire> for UpstreamHealth {
    fn from(wire: UpstreamWire) -> Self {
        Self {
            name: wire.name,
            healthy_keys: wire.healthy_keys,
            cooling_keys: wire.cooling_keys,
            disabled_keys: wire.disabled_keys,
            soonest_cooldown_seconds: wire.soonest_cooldown_seconds.0,
            last_status_class: wire.last_status_class.0,
        }
    }
}

/// Keeps an uncontrolled peer version out of logs and tables, as in `reiki top`.
fn check_version(text: &str) -> Result<(), String> {
    let clean: String = text
        .chars()
        .filter(|c| (' '..='~').contains(c))
        .take(32)
        .collect();
    if clean.is_empty() {
        return Err("reiki_version has no printable characters".to_owned());
    }
    Ok(())
}

impl<'de> Deserialize<'de> for HealthDoc {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = HealthDocWire::deserialize(deserializer)?;
        check_version(&wire.reiki_version).map_err(D::Error::custom)?;
        Ok(Self {
            instance: wire.instance,
            observed_at: wire.observed_at,
            age_seconds: wire.age_seconds,
            upstreams: wire.upstreams.into_iter().map(Into::into).collect(),
        })
    }
}

/// What `reiki peers` knows about one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerReport {
    pub name: String,
    /// `fresh`, or `unknown` after any failure or when the data is too old.
    pub state: &'static str,
    pub error: Option<String>,
    pub instance: Option<String>,
    /// How old the data is: the age the peer reported plus the time since this host received it.
    pub age_seconds: Option<u64>,
    /// Empty unless `state` is `fresh`, so old data never reads as healthy.
    pub upstreams: Vec<UpstreamHealth>,
}

impl PeerReport {
    pub(crate) fn unknown(name: &str, error: String) -> Self {
        Self {
            name: name.to_owned(),
            state: "unknown",
            error: Some(error),
            instance: None,
            age_seconds: None,
            upstreams: Vec::new(),
        }
    }

    pub fn is_fresh(&self) -> bool {
        self.state == "fresh"
    }
}

/// Judges a document received just now: data more than three poll intervals old is unknown. The
/// age comes from the current wire shape, so the two clocks do not have to agree.
pub(crate) fn assess(name: &str, doc: HealthDoc, poll: Duration) -> PeerReport {
    let limit = poll.as_secs().saturating_mul(3);
    let age = doc.age_seconds;
    if age > limit {
        return PeerReport::unknown(name, format!("stale data, {age}s old"));
    }
    PeerReport {
        name: name.to_owned(),
        state: "fresh",
        error: None,
        instance: Some(doc.instance),
        age_seconds: Some(age),
        upstreams: doc.upstreams,
    }
}

/// Refuses a token file that its group or others can read.
#[cfg(unix)]
fn check_mode(path: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("cannot read token_file: {e}"))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "token_file can be read by group or others (mode {:o}); chmod 600 it",
            mode & 0o777
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_mode(_path: &str) -> Result<(), String> {
    Ok(())
}

pub(crate) fn read_token(peer: &PeerEntry) -> Result<String, String> {
    check_mode(&peer.token_file)?;
    let token = std::fs::read_to_string(&peer.token_file)
        .map_err(|e| format!("cannot read token_file: {e}"))?
        .trim()
        .to_owned();
    if token.is_empty() {
        return Err("token_file is empty".to_owned());
    }
    Ok(token)
}

/// A client for one peer: built-in roots off, only the peer's CA trusted, no proxy, no redirect.
pub(crate) fn client_for(
    peer: &PeerEntry,
    timeout: Option<Duration>,
) -> Result<reqwest::Client, String> {
    let https = peer.url.starts_with("https://");
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(FETCH_TIMEOUT)
        .no_proxy()
        .https_only(https);
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(ca_file) = &peer.ca_file {
        let pem = std::fs::read(ca_file).map_err(|e| format!("cannot read ca_file: {e}"))?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|e| format!("ca_file is not PEM certificates: {e}"))?;
        if certs.is_empty() {
            return Err("ca_file holds no certificate".to_owned());
        }
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }
    builder
        .build()
        .map_err(|e| format!("cannot build the peer client: {e}"))
}

pub(crate) fn endpoint(peer: &PeerEntry, path: &str) -> String {
    format!("{}{path}", peer.url.trim_end_matches('/'))
}

/// Why an answer that is not a 200 was refused.
pub(crate) fn refusal(status: reqwest::StatusCode) -> String {
    if status.is_redirection() {
        format!("redirect refused ({status})")
    } else if status == reqwest::StatusCode::UNAUTHORIZED {
        "the peer refused the token (401)".to_owned()
    } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        "the peer is throttling this address (429)".to_owned()
    } else {
        format!("the peer answered {status}")
    }
}

/// The full chain of a transport error, which is where a TLS or connect cause hides.
pub(crate) fn describe(error: &reqwest::Error) -> String {
    let mut text = if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        "cannot connect".to_owned()
    } else {
        "request failed".to_owned()
    };
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let _ = write!(text, ": {cause}");
        source = cause.source();
    }
    text
}

async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BODY as u64)
    {
        return Err(format!("the body is over {} KiB", MAX_BODY / 1024));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| describe(&e))? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(format!("the body is over {} KiB", MAX_BODY / 1024));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The peer's `/v1/health` body, capped and still unparsed.
pub(crate) async fn fetch_raw(peer: &PeerEntry) -> Result<String, String> {
    let token = read_token(peer)?;
    let client = client_for(peer, Some(FETCH_TIMEOUT))?;
    let response = client
        .get(endpoint(peer, "/v1/health"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| describe(&e))?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(refusal(response.status()));
    }
    let body = read_capped(response).await?;
    String::from_utf8(body).map_err(|e| format!("not a /v1/health document: {e}"))
}

async fn fetch(peer: &PeerEntry) -> Result<HealthDoc, String> {
    serde_json::from_str(&fetch_raw(peer).await?)
        .map_err(|e| format!("not a /v1/health document: {e}"))
}

/// One report per configured peer, fetched concurrently.
pub async fn poll(peers: &PeersConfig) -> Vec<PeerReport> {
    let poll = Duration::from_secs(peers.poll_interval_secs);
    let tasks: Vec<_> = peers
        .peers
        .iter()
        .map(|(name, peer)| {
            let (name, peer) = (name.clone(), peer.clone());
            tokio::spawn(async move {
                match fetch(&peer).await {
                    Ok(doc) => assess(&name, doc, poll),
                    Err(error) => PeerReport::unknown(&name, error),
                }
            })
        })
        .collect();
    let mut reports = Vec::with_capacity(tasks.len());
    for (task, name) in tasks.into_iter().zip(peers.peers.keys()) {
        reports.push(
            task.await
                .unwrap_or_else(|_| PeerReport::unknown(name, "the fetch task failed".to_owned())),
        );
    }
    reports
}

fn dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".to_owned())
}

/// The table `reiki peers` prints: one row per peer upstream, one `unknown` row per peer
/// without fresh data.
/// Control characters and the invisible format characters that reorder or hide text. A peer
/// chooses its instance and upstream names, and they go to a terminal.
fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
        )
}

pub fn render_table(reports: &[PeerReport]) -> String {
    let headers = [
        "PEER", "INSTANCE", "UPSTREAM", "STATE", "HEALTHY", "COOLING", "DISABLED", "WAIT", "AGE",
    ];
    let mut rows: Vec<Vec<String>> = Vec::new();
    for report in reports {
        if !report.is_fresh() || report.upstreams.is_empty() {
            rows.push(vec![
                report.name.clone(),
                dash(report.instance.clone()),
                "-".to_owned(),
                if report.is_fresh() {
                    "no_upstreams"
                } else {
                    "unknown"
                }
                .to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                dash(report.age_seconds.map(|a| format!("{a}s"))),
            ]);
            continue;
        }
        for upstream in &report.upstreams {
            rows.push(vec![
                report.name.clone(),
                dash(report.instance.clone()),
                upstream.name.clone(),
                upstream.state().to_owned(),
                upstream.healthy_keys.to_string(),
                upstream.cooling_keys.to_string(),
                upstream.disabled_keys.to_string(),
                dash(upstream.soonest_cooldown_seconds.map(|s| format!("{s}s"))),
                dash(report.age_seconds.map(|a| format!("{a}s"))),
            ]);
        }
    }
    // A peer chooses its instance and upstream names, and they go to a terminal.
    for cell in rows.iter_mut().flatten() {
        if cell.chars().any(unsafe_char) {
            *cell = cell
                .chars()
                .map(|c| if unsafe_char(c) { '?' } else { c })
                .collect();
        }
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let line = |out: &mut String, cells: &[String]| {
        let mut text = String::new();
        for (index, (cell, width)) in cells.iter().zip(&widths).enumerate() {
            if index + 1 == cells.len() {
                text.push_str(cell);
            } else {
                let _ = write!(text, "{cell:<width$}  ");
            }
        }
        out.push_str(text.trim_end());
        out.push('\n');
    };
    line(
        &mut out,
        &headers.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>(),
    );
    for row in &rows {
        line(&mut out, row);
    }
    out
}

pub fn render_json(reports: &[PeerReport]) -> Result<String, String> {
    let doc = serde_json::json!({ "peers": reports });
    serde_json::to_string_pretty(&doc)
        .map(|text| text + "\n")
        .map_err(|e| e.to_string())
}

/// The `[peers]` config a reader command needs, or the reason it cannot run. The message is
/// shared by `reiki peers` and `reiki top`.
pub(crate) fn ready(peers: Option<&PeersConfig>) -> Result<&PeersConfig, String> {
    let Some(peers) = peers else {
        return Err("the config has no [peers] table".to_owned());
    };
    if !peers.enabled {
        return Err("[peers] is not enabled".to_owned());
    }
    if peers.peers.is_empty() {
        return Err("[peers] lists no peers".to_owned());
    }
    Ok(peers)
}

/// The `reiki peers` command: 0 when a peer gave fresh data, 1 when none did, 2 on a config
/// problem.
pub async fn run(peers: Option<&PeersConfig>, json: bool, follow: bool) -> i32 {
    let peers = match ready(peers) {
        Ok(peers) => peers,
        Err(e) => {
            eprintln!("reiki: {e}");
            return 2;
        }
    };
    if follow {
        return watch(peers, json, None, |text| {
            use std::io::Write as _;
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(text.as_bytes());
            let _ = stdout.flush();
        })
        .await;
    }
    let reports = poll(peers).await;
    print_reports(&reports, json)
}

pub(crate) fn print_reports(reports: &[PeerReport], json: bool) -> i32 {
    for report in reports {
        if let Some(error) = &report.error {
            eprintln!("reiki: peer `{}`: {error}", report.name);
        }
    }
    let output = if json {
        render_json(reports)
    } else {
        Ok(render_table(reports))
    };
    match output {
        Ok(text) => print!("{text}"),
        Err(e) => {
            eprintln!("reiki: {e}");
            return 1;
        }
    }
    i32::from(!reports.iter().any(PeerReport::is_fresh))
}

/// A stream that sends nothing, not even a heartbeat, for this long is given up.
const STREAM_IDLE: Duration = Duration::from_secs(40);

/// Largest number of upstreams a watched peer's document may grow to through key events.
/// A snapshot may be larger, but no key event adds an upstream past this cap.
const MAX_KEY_UPSTREAMS: usize = 256;

/// Largest number of items one `feed` may emit. A comment is three bytes and one item, so
/// without this cap one chunk could enqueue tens of thousands of them.
const MAX_SSE_ITEMS_PER_FEED: usize = 1024;

/// One parsed Server-Sent Events item.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SseItem {
    Event { event: String, data: String },
    Comment,
}

/// Splits a byte stream into SSE items, refusing any event or line over `cap` bytes.
pub(crate) struct SseParser {
    cap: usize,
    line: Vec<u8>,
    event: String,
    data: String,
}

impl SseParser {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            line: Vec::new(),
            event: String::new(),
            data: String::new(),
        }
    }

    fn has_room(items: &[SseItem]) -> Result<(), String> {
        if items.len() >= MAX_SSE_ITEMS_PER_FEED {
            return Err(format!(
                "too many SSE items in one chunk (cap {MAX_SSE_ITEMS_PER_FEED})"
            ));
        }
        Ok(())
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SseItem>, String> {
        let mut items = Vec::new();
        for &byte in bytes {
            if byte != b'\n' {
                self.line.push(byte);
                if self.line.len() + self.data.len() > self.cap {
                    return Err(format!("an event is over {} KiB", self.cap / 1024));
                }
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line =
                String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(&line)).into_owned();
            if line.is_empty() {
                if !self.data.is_empty() || !self.event.is_empty() {
                    Self::has_room(&items)?;
                    items.push(SseItem::Event {
                        event: std::mem::take(&mut self.event),
                        data: std::mem::take(&mut self.data),
                    });
                }
            } else if line.starts_with(':') {
                Self::has_room(&items)?;
                items.push(SseItem::Comment);
            } else {
                let (field, value) = line.split_once(':').unwrap_or((line.as_str(), ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "event" => value.clone_into(&mut self.event),
                    "data" => {
                        // The per-line check above sees raw bytes; lossy UTF-8 decoding can make
                        // the decoded value longer than the bytes it came from. Bound the
                        // accumulated event explicitly, including the separating newline.
                        let added = value.len() + usize::from(!self.data.is_empty());
                        if self.data.len() + added > self.cap {
                            return Err(format!("an event is over {} KiB", self.cap / 1024));
                        }
                        if !self.data.is_empty() {
                            self.data.push('\n');
                        }
                        self.data.push_str(value);
                    }
                    _ => {}
                }
            }
        }
        Ok(items)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyEvent {
    observed_at: String,
    age_seconds: u64,
    upstream: UpstreamWire,
}

enum Watched {
    Report(PeerReport),
    /// A frame arrived, a heartbeat included, so the data is still current.
    Alive,
}

/// Applies one `key` event to a watched document. Returns false when the event is dropped
/// because it would grow the upstream table past [`MAX_KEY_UPSTREAMS`]; the drop is counted
/// and the first one warns, so a peer cannot grow the view without bound.
fn apply_key_event(doc: &mut HealthDoc, key: KeyEvent, drops: &mut u64) -> bool {
    let existing = doc
        .upstreams
        .iter()
        .position(|u| u.name == key.upstream.name);
    if existing.is_none() && doc.upstreams.len() >= MAX_KEY_UPSTREAMS {
        *drops += 1;
        if *drops == 1 {
            tracing::warn!(
                dropped = *drops,
                cap = MAX_KEY_UPSTREAMS,
                "a peer key event would add too many upstreams; dropping it"
            );
        }
        return false;
    }
    doc.observed_at = key.observed_at;
    doc.age_seconds = key.age_seconds;
    let upstream = key.upstream.into();
    match existing {
        Some(index) => doc.upstreams[index] = upstream,
        None => doc.upstreams.push(upstream),
    }
    true
}

/// Opens one peer's event stream with the same pinned client, token and checks as a poll.
pub(crate) async fn open_stream(peer: &PeerEntry) -> Result<reqwest::Response, String> {
    let token = read_token(peer)?;
    let client = client_for(peer, None)?;
    let request = client
        .get(endpoint(peer, "/v1/health/stream"))
        .bearer_auth(token)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send();
    let response = match tokio::time::timeout(FETCH_TIMEOUT, request).await {
        Err(_) => return Err("timed out".to_owned()),
        Ok(Err(e)) => return Err(describe(&e)),
        Ok(Ok(response)) => response,
    };
    if response.status() != reqwest::StatusCode::OK {
        return Err(refusal(response.status()));
    }
    let streams = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    if !streams {
        return Err("the peer does not stream".to_owned());
    }
    Ok(response)
}

/// One read from a live stream: data, or the end of it.
pub(crate) enum Chunk {
    Data(Vec<u8>),
    Ended,
}

/// Largest one read from a live stream. One event may be at most [`MAX_BODY`] bytes, so a
/// chunk this size can still carry a whole one.
const MAX_STREAM_CHUNK: usize = MAX_BODY;

fn capped_chunk(chunk: &[u8]) -> Result<Chunk, String> {
    if chunk.len() > MAX_STREAM_CHUNK {
        return Err(format!(
            "a stream chunk is over {} KiB",
            MAX_STREAM_CHUNK / 1024
        ));
    }
    Ok(Chunk::Data(chunk.to_vec()))
}

pub(crate) async fn next_chunk(response: &mut reqwest::Response) -> Result<Chunk, String> {
    match tokio::time::timeout(STREAM_IDLE, response.chunk()).await {
        Err(_) => Err("the stream went quiet".to_owned()),
        Ok(Err(e)) => Err(describe(&e)),
        Ok(Ok(None)) => Ok(Chunk::Ended),
        Ok(Ok(Some(chunk))) => capped_chunk(&chunk),
    }
}

/// Follows one peer's event stream, sending a report per change; returns why it stopped.
async fn follow_stream(
    name: &str,
    peer: &PeerEntry,
    poll: Duration,
    tx: &mpsc::Sender<Watched>,
) -> String {
    let mut response = match open_stream(peer).await {
        Ok(response) => response,
        Err(e) => return e,
    };
    let mut parser = SseParser::new(MAX_BODY);
    let mut doc: Option<HealthDoc> = None;
    let mut dropped_keys = 0u64;
    loop {
        let chunk = match next_chunk(&mut response).await {
            Ok(Chunk::Data(chunk)) => chunk,
            Ok(Chunk::Ended) => return "the stream ended".to_owned(),
            Err(e) => return e,
        };
        let items = match parser.feed(&chunk) {
            Ok(items) => items,
            Err(e) => return e,
        };
        for item in items {
            let changed = match item {
                SseItem::Comment => false,
                SseItem::Event { event, data } => match event.as_str() {
                    "snapshot" => match serde_json::from_str::<HealthDoc>(&data) {
                        Ok(snapshot) => {
                            doc = Some(snapshot);
                            true
                        }
                        Err(e) => return format!("not a /v1/health snapshot: {e}"),
                    },
                    "key" => match (serde_json::from_str::<KeyEvent>(&data), doc.as_mut()) {
                        (Ok(key), Some(doc)) => apply_key_event(doc, key, &mut dropped_keys),
                        (Err(e), _) => return format!("not a key event: {e}"),
                        (Ok(_), None) => return "a key event came before the snapshot".to_owned(),
                    },
                    _ => false,
                },
            };
            let message = match (&doc, changed) {
                (Some(doc), true) => Watched::Report(assess(name, doc.clone(), poll)),
                _ => Watched::Alive,
            };
            if tx.send(message).await.is_err() {
                return "stopped".to_owned();
            }
        }
    }
}

/// Follows one peer for as long as the watch runs: its stream, and a poll whenever that fails.
async fn follow(name: String, peer: PeerEntry, poll: Duration, tx: mpsc::Sender<Watched>) {
    loop {
        let reason = follow_stream(&name, &peer, poll, &tx).await;
        let report = match fetch(&peer).await {
            Ok(doc) => assess(&name, doc, poll),
            Err(error) => PeerReport::unknown(&name, format!("stream: {reason}; poll: {error}")),
        };
        if tx.send(Watched::Report(report)).await.is_err() || tx.is_closed() {
            return;
        }
        tokio::time::sleep(poll).await;
    }
}

/// `reiki peers --watch`: prints the table (or one JSON line) on each change; with
/// `max_updates` it stops after that many and returns the exit status `run` would.
pub async fn watch(
    peers: &PeersConfig,
    json: bool,
    max_updates: Option<usize>,
    mut out: impl FnMut(&str),
) -> i32 {
    let poll = Duration::from_secs(peers.poll_interval_secs);
    let names: Vec<String> = peers.peers.keys().cloned().collect();
    let mut inboxes = Vec::new();
    let mut tasks = Vec::new();
    for (name, peer) in &peers.peers {
        let (tx, rx) = mpsc::channel(64);
        tasks.push(tokio::spawn(follow(name.clone(), peer.clone(), poll, tx)));
        inboxes.push(rx);
    }
    let (merged_tx, mut merged) = mpsc::channel::<(usize, Watched)>(64);
    for (index, mut inbox) in inboxes.into_iter().enumerate() {
        let merged_tx = merged_tx.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(message) = inbox.recv().await {
                if merged_tx.send((index, message)).await.is_err() {
                    return;
                }
            }
        }));
    }
    drop(merged_tx);
    let mut reports: Vec<PeerReport> = names
        .iter()
        .map(|name| PeerReport::unknown(name, "no answer yet".to_owned()))
        .collect();
    let mut received: Vec<Option<std::time::Instant>> = vec![None; names.len()];
    let limit = poll.saturating_mul(3);
    let mut expiry = tokio::time::interval(poll.min(Duration::from_secs(5)));
    let mut updates = 0usize;
    let status = loop {
        let mut changed = false;
        tokio::select! {
            message = merged.recv() => match message {
                None => break i32::from(!reports.iter().any(PeerReport::is_fresh)),
                Some((index, Watched::Alive)) => {
                    if reports[index].is_fresh() {
                        received[index] = Some(std::time::Instant::now());
                    }
                }
                Some((index, Watched::Report(report))) => {
                    received[index] = report.is_fresh().then(std::time::Instant::now);
                    reports[index] = report;
                    changed = true;
                }
            },
            _ = expiry.tick() => {
                for (report, seen) in reports.iter_mut().zip(&mut received) {
                    let reported = Duration::from_secs(report.age_seconds.unwrap_or(0));
                    if seen.is_some_and(|at| reported + at.elapsed() > limit) {
                        *report = PeerReport::unknown(&report.name, format!("no data for over {}s", limit.as_secs()));
                        *seen = None;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            continue;
        }
        let mut shown = reports.clone();
        for (report, seen) in shown.iter_mut().zip(&received) {
            if let Some(at) = seen {
                let reported = report.age_seconds.unwrap_or(0);
                report.age_seconds = Some(reported.saturating_add(at.elapsed().as_secs()));
            }
        }
        let text = if json {
            serde_json::to_string(&serde_json::json!({ "peers": shown }))
                .map_or_else(|e| e.to_string(), |t| t + "\n")
        } else {
            render_table(&shown) + "\n"
        };
        out(&text);
        updates += 1;
        if max_updates.is_some_and(|max| updates >= max) {
            break i32::from(!reports.iter().any(PeerReport::is_fresh));
        }
    };
    for task in tasks {
        task.abort();
    }
    status
}

#[cfg(test)]
mod tests {
    #[test]
    fn control_characters_from_a_peer_are_shown_as_question_marks() {
        let mut report = PeerReport::unknown("peer-a", "down".to_owned());
        report.instance = Some("evil\u{1b}[2Jname".to_owned());
        let table = render_table(&[report]);
        assert!(!table.contains('\u{1b}'), "{table:?}");
        assert!(table.contains("evil?[2Jname"), "{table:?}");
    }

    use super::*;

    #[test]
    fn sse_frames_split_on_blank_lines_across_chunks() {
        let mut parser = SseParser::new(1024);
        let mut items = parser.feed(b"event: snap").unwrap();
        assert!(items.is_empty());
        items.extend(
            parser
                .feed(b"shot\ndata: {\"a\":1}\n\n: heartbeat\n\nevent: key\r\ndata: x\r\n")
                .unwrap(),
        );
        items.extend(parser.feed(b"data: y\n\n").unwrap());
        assert_eq!(
            items,
            [
                SseItem::Event {
                    event: "snapshot".into(),
                    data: "{\"a\":1}".into()
                },
                SseItem::Comment,
                SseItem::Event {
                    event: "key".into(),
                    data: "x\ny".into()
                },
            ]
        );
        let mut small = SseParser::new(8);
        assert!(
            small.feed(b"data: 0123456789").is_err(),
            "an endless line is refused"
        );
    }

    #[test]
    fn sse_data_accumulation_is_capped_after_lossy_decode() {
        // Each invalid byte becomes a three-byte replacement character, so the raw-byte
        // line check alone lets the decoded event grow past the cap.
        let mut parser = SseParser::new(12);
        let error = parser
            .feed(b"data: \xff\xff\xff\xff\xff\xff\n")
            .unwrap_err();
        assert!(error.contains("over"), "{error}");

        // Ordinary multi-line data still assembles normally under the same cap.
        let mut parser = SseParser::new(16);
        let items = parser.feed(b"data: 1234\ndata: 5678\n\n").unwrap();
        assert_eq!(
            items,
            [SseItem::Event {
                event: String::new(),
                data: "1234\n5678".into()
            }]
        );
    }

    #[test]
    fn sse_feed_caps_the_items_from_one_chunk() {
        let at_cap = ":x\n".repeat(MAX_SSE_ITEMS_PER_FEED);
        let items = SseParser::new(MAX_BODY).feed(at_cap.as_bytes()).unwrap();
        assert_eq!(items.len(), MAX_SSE_ITEMS_PER_FEED);
        // One more comment in the same chunk is refused, not added to the vector.
        let over_cap = ":x\n".repeat(MAX_SSE_ITEMS_PER_FEED + 1);
        let error = SseParser::new(MAX_BODY)
            .feed(over_cap.as_bytes())
            .unwrap_err();
        assert!(error.contains("too many SSE items"), "{error}");
    }

    #[test]
    fn a_stream_chunk_is_capped() {
        assert!(capped_chunk(&vec![0; MAX_STREAM_CHUNK]).is_ok());
        let error = match capped_chunk(&vec![0; MAX_STREAM_CHUNK + 1]) {
            Ok(_) => panic!("an oversized chunk was accepted"),
            Err(error) => error,
        };
        assert!(error.contains("over"), "{error}");
    }

    fn doc(observed_at: &str) -> HealthDoc {
        HealthDoc {
            instance: "peer-a".to_owned(),
            observed_at: observed_at.to_owned(),
            age_seconds: 0,
            upstreams: vec![UpstreamHealth {
                name: "zen".to_owned(),
                healthy_keys: 0,
                cooling_keys: 1,
                disabled_keys: 0,
                soonest_cooldown_seconds: Some(42),
                last_status_class: Some("4xx".to_owned()),
            }],
        }
    }

    #[test]
    fn key_events_stop_growing_the_upstream_table_at_the_cap() {
        let upstream = |name: &str| UpstreamHealth {
            name: name.to_owned(),
            healthy_keys: 1,
            cooling_keys: 0,
            disabled_keys: 0,
            soonest_cooldown_seconds: None,
            last_status_class: None,
        };
        let wire = |name: &str| UpstreamWire {
            name: name.to_owned(),
            healthy_keys: 1,
            cooling_keys: 0,
            disabled_keys: 0,
            cooling_combinations: 0,
            soonest_cooldown_seconds: RequiredNullable(None),
            last_status_class: RequiredNullable(None),
            keys: None,
            requests_1h: None,
            limited_1h: None,
            request_buckets: None,
            tokens_1h: None,
            token_buckets: None,
            request_minutes: None,
            token_minutes: None,
        };
        let mut watched = doc("2026-11-13T09:59:58Z");
        for n in 1..MAX_KEY_UPSTREAMS {
            watched.upstreams.push(upstream(&format!("u{n}")));
        }
        assert_eq!(watched.upstreams.len(), MAX_KEY_UPSTREAMS);
        let mut drops = 0;
        let new = KeyEvent {
            observed_at: "2026-11-13T10:00:00Z".to_owned(),
            age_seconds: 0,
            upstream: wire("one-too-many"),
        };
        assert!(!apply_key_event(&mut watched, new, &mut drops));
        assert_eq!(drops, 1);
        assert_eq!(watched.upstreams.len(), MAX_KEY_UPSTREAMS);
        assert_eq!(watched.observed_at, "2026-11-13T09:59:58Z");

        let mut known = wire("zen");
        known.healthy_keys = 7;
        let update = KeyEvent {
            observed_at: "2026-11-13T10:00:00Z".to_owned(),
            age_seconds: 0,
            upstream: known,
        };
        assert!(apply_key_event(&mut watched, update, &mut drops));
        assert_eq!(drops, 1, "an update to a known upstream is not a drop");
        assert_eq!(watched.upstreams[0].healthy_keys, 7);
        assert_eq!(watched.observed_at, "2026-11-13T10:00:00Z");
    }

    #[test]
    fn a_reported_age_decides_freshness_whatever_the_clocks_say() {
        let poll = Duration::from_secs(30);
        let aged = |observed_at: &str, age: u64| HealthDoc {
            age_seconds: age,
            ..doc(observed_at)
        };
        for skewed in ["2026-11-13T07:00:00Z", "2026-11-13T13:00:00Z"] {
            let report = assess("peer-a", aged(skewed, 0), poll);
            assert!(report.is_fresh(), "a peer clock off by hours: {report:?}");
            assert_eq!(report.age_seconds, Some(0));
        }
        let report = assess("peer-a", aged("2026-11-13T10:00:00Z", 90), poll);
        assert!(report.is_fresh());
        assert_eq!(report.age_seconds, Some(90));
        let report = assess("peer-a", aged("2026-11-13T10:00:00Z", 91), poll);
        assert_eq!(report.state, "unknown", "too old by its own account");
        assert!(report.error.unwrap().contains("stale"));
        let report = assess("peer-a", aged("not a time", 0), poll);
        assert!(report.is_fresh(), "observed_at is not parsed for freshness");
    }

    #[test]
    fn data_older_than_three_intervals_is_unknown() {
        let poll = Duration::from_secs(30);
        let aged = |age: u64| HealthDoc {
            age_seconds: age,
            ..doc("2026-11-13T10:00:00Z")
        };
        let fresh = assess("peer-a", aged(90), poll);
        assert!(fresh.is_fresh());
        assert_eq!(fresh.age_seconds, Some(90));
        assert_eq!(fresh.upstreams.len(), 1);
        let stale = assess("peer-a", aged(91), poll);
        assert_eq!(stale.state, "unknown");
        assert!(
            stale.upstreams.is_empty(),
            "stale data is never shown as health"
        );
        assert!(stale.error.unwrap().contains("stale"));
    }

    #[test]
    fn the_current_wire_shape_is_required() {
        type Mutation = Box<dyn Fn(&mut serde_json::Value)>;
        let base = || {
            serde_json::json!({
                "instance": "peer-a",
                "reiki_version": "0.12.0",
                "observed_at": "2026-11-13T10:00:00Z",
                "age_seconds": 0,
                "failures": {},
                "upstreams": [{
                    "name": "zen",
                    "healthy_keys": 1,
                    "cooling_keys": 0,
                    "disabled_keys": 0,
                    "cooling_combinations": 0,
                    "soonest_cooldown_seconds": null,
                    "last_status_class": null,
                }],
            })
        };
        serde_json::from_value::<HealthDoc>(base()).expect("the current document parses");

        let config_gated = {
            let mut doc = base();
            doc["host"] = serde_json::json!({"cores": 8});
            doc["sessions"] = serde_json::json!({"active_sessions": 2});
            doc["upstreams"][0]["keys"] = serde_json::json!([]);
            doc["upstreams"][0]["requests_1h"] = serde_json::json!(1);
            doc["upstreams"][0]["limited_1h"] = serde_json::json!(0);
            doc["upstreams"][0]["request_buckets"] = serde_json::json!(vec![0; 12]);
            doc["upstreams"][0]["tokens_1h"] = serde_json::json!(0);
            doc["upstreams"][0]["token_buckets"] = serde_json::json!(vec![0; 12]);
            doc["upstreams"][0]["request_minutes"] = serde_json::json!(vec![0; 10]);
            doc["upstreams"][0]["token_minutes"] = serde_json::json!(vec![0; 10]);
            doc
        };
        serde_json::from_value::<HealthDoc>(config_gated)
            .expect("fields published by configuration are accepted");

        let cases: Vec<(&str, Mutation)> = vec![
            (
                "reiki_version",
                Box::new(|doc: &mut serde_json::Value| {
                    doc.as_object_mut().unwrap().remove("reiki_version");
                }) as Box<dyn Fn(&mut serde_json::Value)>,
            ),
            (
                "age_seconds",
                Box::new(|doc: &mut serde_json::Value| {
                    doc.as_object_mut().unwrap().remove("age_seconds");
                }),
            ),
            (
                "failures",
                Box::new(|doc: &mut serde_json::Value| {
                    doc.as_object_mut().unwrap().remove("failures");
                }),
            ),
            (
                "cooling_combinations",
                Box::new(|doc: &mut serde_json::Value| {
                    doc["upstreams"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("cooling_combinations");
                }),
            ),
            (
                "soonest_cooldown_seconds",
                Box::new(|doc: &mut serde_json::Value| {
                    doc["upstreams"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("soonest_cooldown_seconds");
                }),
            ),
            (
                "last_status_class",
                Box::new(|doc: &mut serde_json::Value| {
                    doc["upstreams"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("last_status_class");
                }),
            ),
            (
                "unknown field",
                Box::new(|doc: &mut serde_json::Value| {
                    doc["future_field"] = serde_json::json!(1);
                }),
            ),
        ];
        for (name, mutate) in cases {
            let mut doc = base();
            mutate(&mut doc);
            let error = serde_json::from_value::<HealthDoc>(doc)
                .expect_err(&format!("{name} is not accepted"))
                .to_string();
            assert!(error.contains(name), "the error names {name}: {error}");
        }
    }

    #[test]
    fn key_events_need_the_current_shape() {
        let current = r#"{"observed_at":"2026-11-13T10:00:00Z","age_seconds":0,
            "upstream":{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
            "cooling_combinations":0,"soonest_cooldown_seconds":null,"last_status_class":null}}"#;
        serde_json::from_str::<KeyEvent>(current).expect("the current key event parses");
        for old in [
            r#"{"observed_at":"t","upstream":{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,"cooling_combinations":0,"soonest_cooldown_seconds":null,"last_status_class":null}}"#,
            r#"{"observed_at":"t","age_seconds":0,"upstream":{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,"soonest_cooldown_seconds":null,"last_status_class":null}}"#,
        ] {
            assert!(
                serde_json::from_str::<KeyEvent>(old).is_err(),
                "{old} is not a current key event"
            );
        }
    }

    #[test]
    fn the_table_shows_unknown_peers_without_numbers() {
        let poll = Duration::from_secs(30);
        let two_seconds_old = HealthDoc {
            age_seconds: 2,
            ..doc("2026-11-13T09:59:58Z")
        };
        let reports = vec![
            assess("peer-a", two_seconds_old, poll),
            PeerReport::unknown("peer-b", "cannot connect".to_owned()),
        ];
        let table = render_table(&reports);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "{table}");
        assert!(lines[0].starts_with("PEER"));
        let peer_a: Vec<&str> = lines[1].split_whitespace().collect();
        assert_eq!(
            peer_a,
            ["peer-a", "peer-a", "zen", "cooling", "0", "1", "0", "42s", "2s"]
        );
        let peer_b: Vec<&str> = lines[2].split_whitespace().collect();
        assert_eq!(
            peer_b,
            ["peer-b", "-", "-", "unknown", "-", "-", "-", "-", "-"]
        );
        let json: serde_json::Value =
            serde_json::from_str(&render_json(&reports).unwrap()).unwrap();
        assert_eq!(json["peers"][1]["state"], "unknown");
        assert_eq!(json["peers"][1]["upstreams"], serde_json::json!([]));
        assert_eq!(
            json["peers"][0]["upstreams"][0]["soonest_cooldown_seconds"],
            42
        );
    }

    #[test]
    fn bidi_overrides_and_zero_width_marks_from_a_peer_are_shown_as_question_marks() {
        let mut report = PeerReport::unknown("peer-a", "down".to_owned());
        report.instance = Some("a\u{202E}b\u{200B}c".to_owned());
        let table = render_table(&[report]);
        assert!(
            !table.contains('\u{202E}') && !table.contains('\u{200B}'),
            "{table:?}"
        );
        assert!(table.contains("a?b?c"), "{table:?}");
    }
}
