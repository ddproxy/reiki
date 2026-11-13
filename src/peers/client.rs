//! `reiki peers`: read each configured peer's `/v1/health` with a client of its own, trusting
//! only that peer's CA, and report anything unanswered or old as unknown.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::{PeerEntry, PeersConfig};

/// Bounds one fetch, connect included.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest `/v1/health` body read; a bigger one is refused, not truncated.
pub const MAX_BODY: usize = 64 * 1024;

/// One upstream as a peer publishes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UpstreamHealth {
    pub name: String,
    pub healthy_keys: u32,
    pub cooling_keys: u32,
    pub disabled_keys: u32,
    #[serde(default)]
    pub soonest_cooldown_seconds: Option<u64>,
    #[serde(default)]
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

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HealthDoc {
    pub(crate) instance: String,
    pub(crate) observed_at: String,
    pub(crate) upstreams: Vec<UpstreamHealth>,
}

/// What `reiki peers` knows about one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerReport {
    pub name: String,
    /// `fresh`, or `unknown` after any failure or when the data is too old.
    pub state: &'static str,
    pub error: Option<String>,
    pub instance: Option<String>,
    /// Seconds between the peer's `observed_at` and this host's clock.
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

/// Seconds since the Unix epoch of an RFC 3339 UTC time such as `2026-11-13T09:30:00Z`.
pub(crate) fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' || bytes[19] != b'Z' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = crate::classify::days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn unix_now(now: SystemTime) -> i64 {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Judges a fetched document against this host's clock: data more than three poll intervals
/// old, or that far in the future, is unknown.
pub(crate) fn assess(name: &str, doc: HealthDoc, poll: Duration, now: SystemTime) -> PeerReport {
    let Some(observed) = parse_rfc3339(&doc.observed_at) else {
        return PeerReport::unknown(name, "observed_at is not an RFC 3339 UTC time".to_owned());
    };
    let limit = i64::try_from(poll.as_secs().saturating_mul(3)).unwrap_or(i64::MAX);
    let age = unix_now(now) - observed;
    if age > limit {
        return PeerReport::unknown(name, format!("stale data, {age}s old"));
    }
    if -age > limit {
        return PeerReport::unknown(
            name,
            format!("observed_at is {}s ahead of this clock", -age),
        );
    }
    PeerReport {
        name: name.to_owned(),
        state: "fresh",
        error: None,
        instance: Some(doc.instance),
        age_seconds: Some(u64::try_from(age.max(0)).unwrap_or(0)),
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

async fn fetch(peer: &PeerEntry) -> Result<HealthDoc, String> {
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
    serde_json::from_slice(&body).map_err(|e| format!("not a /v1/health document: {e}"))
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
                    Ok(doc) => assess(&name, doc, poll, SystemTime::now()),
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

/// The `reiki peers` command: 0 when a peer gave fresh data, 1 when none did, 2 on a config
/// problem.
pub async fn run(peers: Option<&PeersConfig>, json: bool) -> i32 {
    let Some(peers) = peers else {
        eprintln!("reiki: the config has no [peers] table");
        return 2;
    };
    if !peers.enabled {
        eprintln!("reiki: [peers] is not enabled");
        return 2;
    }
    if peers.peers.is_empty() {
        eprintln!("reiki: [peers] lists no peers");
        return 2;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(observed_at: &str) -> HealthDoc {
        HealthDoc {
            instance: "ct-4".to_owned(),
            observed_at: observed_at.to_owned(),
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

    fn at(text: &str) -> SystemTime {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(u64::try_from(parse_rfc3339(text).unwrap()).unwrap())
    }

    #[test]
    fn rfc3339_round_trips_with_the_reload_formatter() {
        for secs in [0u64, 86_399, 1_763_026_200, 4_102_444_800] {
            let time = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
            let text = crate::reload::rfc3339(time);
            assert_eq!(
                parse_rfc3339(&text),
                Some(i64::try_from(secs).unwrap()),
                "{text}"
            );
        }
        for bad in [
            "",
            "2026-11-13 09:30:00Z",
            "2026-11-13T09:30:00",
            "2026-13-13T09:30:00Z",
            "2026-11-13T24:30:00Z",
            "2026-11-13T09:30:00+01:00",
            "+026-11-13T09:30:00Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn data_older_or_newer_than_three_intervals_is_unknown() {
        let poll = Duration::from_secs(30);
        let now = at("2026-11-13T10:00:00Z");
        let fresh = assess("ct-4", doc("2026-11-13T09:58:31Z"), poll, now);
        assert!(fresh.is_fresh());
        assert_eq!(fresh.age_seconds, Some(89));
        assert_eq!(fresh.upstreams.len(), 1);
        let stale = assess("ct-4", doc("2026-11-13T09:58:29Z"), poll, now);
        assert_eq!(stale.state, "unknown");
        assert!(
            stale.upstreams.is_empty(),
            "stale data is never shown as health"
        );
        assert!(stale.error.unwrap().contains("stale"));
        let ahead = assess("ct-4", doc("2026-11-13T10:05:00Z"), poll, now);
        assert_eq!(ahead.state, "unknown");
        let garbled = assess("ct-4", doc("yesterday"), poll, now);
        assert_eq!(garbled.state, "unknown");
    }

    #[test]
    fn the_table_shows_unknown_peers_without_numbers() {
        let poll = Duration::from_secs(30);
        let now = at("2026-11-13T10:00:00Z");
        let reports = vec![
            assess("ct-4", doc("2026-11-13T09:59:58Z"), poll, now),
            PeerReport::unknown("ct-5", "cannot connect".to_owned()),
        ];
        let table = render_table(&reports);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "{table}");
        assert!(lines[0].starts_with("PEER"));
        let ct4: Vec<&str> = lines[1].split_whitespace().collect();
        assert_eq!(
            ct4,
            ["ct-4", "ct-4", "zen", "cooling", "0", "1", "0", "42s", "2s"]
        );
        let ct5: Vec<&str> = lines[2].split_whitespace().collect();
        assert_eq!(ct5, ["ct-5", "-", "-", "unknown", "-", "-", "-", "-", "-"]);
        let json: serde_json::Value =
            serde_json::from_str(&render_json(&reports).unwrap()).unwrap();
        assert_eq!(json["peers"][1]["state"], "unknown");
        assert_eq!(json["peers"][1]["upstreams"], serde_json::json!([]));
        assert_eq!(
            json["peers"][0]["upstreams"][0]["soonest_cooldown_seconds"],
            42
        );
    }
}
