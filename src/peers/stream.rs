//! `GET /v1/health/stream`: a snapshot on connect, then change events from one bounded
//! broadcast; a subscriber that falls behind is dropped instead of buffered.

use std::collections::HashMap;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Bytes;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

use crate::proxy::App;

pub(crate) const STREAM_PATH: &str = "/v1/health/stream";
/// How often the publisher looks for changes to send.
const TICK: Duration = Duration::from_secs(1);
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(15);
/// Events held for the slowest subscriber before it counts as lagging.
const BROADCAST_CAPACITY: usize = 64;
/// Frames queued for one connection before it counts as a slow consumer.
const SUBSCRIBER_BUFFER: usize = 32;
const HEARTBEAT_FRAME: &[u8] = b": heartbeat\n\n";

/// Why a new subscriber was turned away.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// `max_subscribers` are connected.
    Full,
    /// `max_subscribers_per_peer` are connected from this address.
    PeerFull,
    /// `max_subscribers_per_token` are connected with this peer token.
    TokenFull,
}

/// The subscriber caps: in total, per client address and per peer token.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) total: usize,
    pub(crate) per_peer: usize,
    pub(crate) per_token: usize,
}

#[derive(Default)]
struct Slots {
    total: usize,
    per_peer: HashMap<IpAddr, usize>,
    per_token: HashMap<String, usize>,
}

/// What goes through the broadcast.
#[derive(Clone)]
enum Event {
    Frame(Bytes),
    /// Ends the streams opened with these tokens, or every stream for `None`.
    Close(Option<Arc<[String]>>),
}

pub(crate) struct Hub {
    tx: broadcast::Sender<Event>,
    slots: Mutex<Slots>,
    ticker: OnceLock<()>,
}

/// Holds one subscriber's place until its forwarding task ends.
struct Slot {
    hub: Arc<Hub>,
    peer: Option<IpAddr>,
    /// Name of the peer token the stream was opened with.
    token: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut slots = self
            .hub
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        slots.total = slots.total.saturating_sub(1);
        if let Some(ip) = self.peer {
            release(&mut slots.per_peer, &ip);
        }
        release(&mut slots.per_token, &self.token);
    }
}

fn release<K: std::hash::Hash + Eq>(counts: &mut HashMap<K, usize>, key: &K) {
    if let Some(count) = counts.get_mut(key) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            counts.remove(key);
        }
    }
}

/// An SSE frame; `data` is compact JSON, so it holds no newline.
pub(crate) fn frame(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

impl Hub {
    pub(crate) fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            tx,
            slots: Mutex::new(Slots::default()),
            ticker: OnceLock::new(),
        })
    }

    #[cfg(test)]
    fn subscribers(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .total
    }

    fn reserve(
        self: &Arc<Self>,
        peer: Option<IpAddr>,
        token: &str,
        limits: Limits,
    ) -> Result<Slot, Refused> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if slots.total >= limits.total {
            return Err(Refused::Full);
        }
        if peer.is_some_and(|ip| slots.per_peer.get(&ip).copied().unwrap_or(0) >= limits.per_peer) {
            return Err(Refused::PeerFull);
        }
        if slots.per_token.get(token).copied().unwrap_or(0) >= limits.per_token {
            return Err(Refused::TokenFull);
        }
        if let Some(ip) = peer {
            *slots.per_peer.entry(ip).or_insert(0) += 1;
        }
        *slots.per_token.entry(token.to_owned()).or_insert(0) += 1;
        slots.total += 1;
        Ok(Slot {
            hub: Arc::clone(self),
            peer,
            token: token.to_owned(),
        })
    }

    /// Joins the broadcast, then queues `snapshot()` first, so no change between the two is lost.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        peer: Option<IpAddr>,
        token: &str,
        limits: Limits,
        heartbeat: Duration,
        snapshot: impl FnOnce() -> Bytes,
    ) -> Result<mpsc::Receiver<Bytes>, Refused> {
        let slot = self.reserve(peer, token, limits)?;
        let events = self.tx.subscribe();
        let (out, inbox) = mpsc::channel(SUBSCRIBER_BUFFER);
        let _ = out.try_send(snapshot());
        tokio::spawn(forward(events, out, heartbeat, slot));
        Ok(inbox)
    }

    pub(crate) fn publish(&self, frame: Bytes) {
        let _ = self.tx.send(Event::Frame(frame));
    }

    /// Ends every open stream, for a reload that stops publishing.
    pub(crate) fn close_all(&self) {
        let _ = self.tx.send(Event::Close(None));
    }

    /// Ends the streams opened with any of `tokens`, for a reload that removes or changes them.
    pub(crate) fn close_tokens(&self, tokens: Vec<String>) {
        let _ = self.tx.send(Event::Close(Some(tokens.into())));
    }
}

async fn forward(
    mut events: broadcast::Receiver<Event>,
    out: mpsc::Sender<Bytes>,
    heartbeat: Duration,
    slot: Slot,
) {
    let mut beat = tokio::time::interval_at(Instant::now() + heartbeat, heartbeat);
    loop {
        let next = tokio::select! {
            event = events.recv() => match event {
                Ok(Event::Frame(frame)) => frame,
                Ok(Event::Close(None)) => break,
                Ok(Event::Close(Some(tokens))) if tokens.contains(&slot.token) => break,
                Ok(Event::Close(Some(_))) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    tracing::debug!(peer = ?slot.peer, "peer stream fell behind the broadcast; dropping it");
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = beat.tick() => Bytes::from_static(HEARTBEAT_FRAME),
            () = out.closed() => break,
        };
        if out.try_send(next).is_err() {
            tracing::debug!(peer = ?slot.peer, "peer stream is not reading; dropping it");
            break;
        }
    }
    drop(slot);
}

/// The body of a stream response: frames from one subscriber's queue.
pub(crate) struct Frames(pub(crate) mpsc::Receiver<Bytes>);

impl futures_core::Stream for Frames {
    type Item = Result<Bytes, std::convert::Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx).map(|frame| frame.map(Ok))
    }
}

/// An upstream entry without its countdowns, so a ticking cooldown is not a change.
fn fingerprint(entry: &Value) -> Value {
    let mut entry = entry.clone();
    entry["soonest_cooldown_seconds"] = Value::Null;
    if let Some(keys) = entry.get_mut("keys").and_then(Value::as_array_mut) {
        for key in keys {
            key["cooldown_seconds"] = Value::Null;
        }
    }
    entry
}

/// What the ticker last told subscribers.
struct Watcher {
    /// The peer tokens as (name, hash) when the ticker last looked; `None` before the first look.
    tokens: Option<Vec<(String, String)>>,
    upstreams: Vec<(String, Value)>,
    sessions: Option<Value>,
    next_host: Instant,
}

impl Watcher {
    fn new() -> Self {
        Self {
            tokens: None,
            upstreams: Vec::new(),
            sessions: None,
            next_host: Instant::now(),
        }
    }

    /// What changed since the last call, or `None` to close every stream.
    fn changes(&mut self, app: &App, now: Instant) -> Option<Vec<Event>> {
        let snapshot = app.snapshot();
        let peers = snapshot.config.peers.as_ref().filter(|p| p.publishing())?;
        let tokens: Vec<(String, String)> = peers
            .tokens
            .iter()
            .map(|t| (t.name.clone(), t.token_sha256.to_ascii_lowercase()))
            .collect();
        let mut events = Vec::new();
        if let Some(before) = self.tokens.replace(tokens.clone()) {
            // A token whose hash changed is as gone as a removed one.
            let gone: Vec<String> = before
                .into_iter()
                .filter(|pair| !tokens.contains(pair))
                .map(|(name, _)| name)
                .collect();
            if !gone.is_empty() {
                events.push(Event::Close(Some(gone.into())));
            }
        }
        let observed_at = crate::reload::rfc3339(app.wall_now());
        let upstreams = {
            let health = app.health().lock().unwrap_or_else(PoisonError::into_inner);
            super::health::upstreams(
                &snapshot.config,
                &health,
                std::time::Instant::now(),
                peers.expose_key_labels,
            )
        };
        let current: Vec<(String, Value)> = upstreams
            .iter()
            .map(|entry| {
                (
                    entry["name"].as_str().unwrap_or_default().to_owned(),
                    fingerprint(entry),
                )
            })
            .collect();
        let mut frames = Vec::new();
        let same_names = current.len() == self.upstreams.len()
            && current.iter().zip(&self.upstreams).all(|(a, b)| a.0 == b.0);
        if !same_names && !self.upstreams.is_empty() {
            frames.push(frame("snapshot", &super::health::document(app, peers)));
        } else if same_names {
            for (entry, ((_, now_print), (_, then_print))) in
                upstreams.iter().zip(current.iter().zip(&self.upstreams))
            {
                if now_print != then_print {
                    frames.push(frame(
                        "key",
                        &json!({"observed_at": observed_at, "upstream": entry}),
                    ));
                }
            }
        }
        self.upstreams = current;
        if peers.expose_host_stats {
            let sessions = super::health::sessions(app, peers);
            if self.sessions.as_ref() != Some(&sessions) {
                if self.sessions.is_some() {
                    frames.push(frame(
                        "sessions",
                        &json!({"observed_at": observed_at, "sessions": sessions}),
                    ));
                }
                self.sessions = Some(sessions);
            }
            if now >= self.next_host {
                let host = super::host::stats(&super::health::disk_dirs(app, &snapshot.config));
                frames.push(frame(
                    "host",
                    &json!({"observed_at": observed_at, "host": host}),
                ));
                self.next_host = now + Duration::from_secs(peers.stream_interval_secs);
            }
        } else {
            self.sessions = None;
        }
        events.extend(frames.into_iter().map(Event::Frame));
        Some(events)
    }
}

/// Starts the change ticker once, on the first subscriber, and primes it with the current state.
pub(crate) fn ensure_ticker(app: &Arc<App>, hub: &Arc<Hub>) {
    hub.ticker.get_or_init(|| {
        let mut watcher = Watcher::new();
        // Primed before the first snapshot is built, so the first tick reports only real changes.
        let _ = watcher.changes(app, Instant::now());
        let (app, hub) = (Arc::downgrade(app), Arc::clone(hub));
        tokio::spawn(tick(app, hub, watcher));
    });
}

async fn tick(app: Weak<App>, hub: Arc<Hub>, mut watcher: Watcher) {
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(app) = app.upgrade() else {
            return;
        };
        match watcher.changes(&app, Instant::now()) {
            Some(events) => events.into_iter().for_each(|event| match event {
                Event::Frame(frame) => hub.publish(frame),
                Event::Close(Some(tokens)) => hub.close_tokens(tokens.to_vec()),
                Event::Close(None) => hub.close_all(),
            }),
            None => {
                hub.close_all();
                watcher = Watcher::new();
                let _ = watcher.changes(&app, Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> Option<IpAddr> {
        Some(IpAddr::from([192, 0, 2, last]))
    }

    fn limits(total: usize, per_peer: usize) -> Limits {
        Limits {
            total,
            per_peer,
            per_token: total,
        }
    }

    fn snapshot() -> Bytes {
        frame("snapshot", &json!({"x": 1}))
    }

    async fn next(inbox: &mut mpsc::Receiver<Bytes>) -> Option<Bytes> {
        tokio::time::timeout(Duration::from_secs(2), inbox.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn a_subscriber_gets_the_snapshot_then_published_events() {
        let hub = Hub::new();
        let mut inbox = hub
            .subscribe(ip(1), "default", limits(4, 4), HEARTBEAT, snapshot)
            .unwrap();
        hub.publish(frame("key", &json!({"n": 1})));
        assert_eq!(next(&mut inbox).await.unwrap(), snapshot());
        assert_eq!(
            next(&mut inbox).await.unwrap(),
            Bytes::from("event: key\ndata: {\"n\":1}\n\n")
        );
    }

    #[tokio::test]
    async fn heartbeats_arrive_while_nothing_changes() {
        let hub = Hub::new();
        let mut inbox = hub
            .subscribe(
                ip(1),
                "default",
                limits(4, 4),
                Duration::from_millis(50),
                snapshot,
            )
            .unwrap();
        assert_eq!(next(&mut inbox).await.unwrap(), snapshot());
        assert_eq!(
            next(&mut inbox).await.unwrap(),
            Bytes::from_static(HEARTBEAT_FRAME)
        );
        assert_eq!(
            next(&mut inbox).await.unwrap(),
            Bytes::from_static(HEARTBEAT_FRAME)
        );
    }

    async fn settle(hub: &Hub, expected: usize) {
        for _ in 0..100 {
            if hub.subscribers() == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(hub.subscribers(), expected);
    }

    #[tokio::test]
    async fn caps_apply_in_total_and_per_address_and_free_on_disconnect() {
        let hub = Hub::new();
        let first = hub
            .subscribe(ip(1), "default", limits(2, 1), HEARTBEAT, snapshot)
            .unwrap();
        assert_eq!(
            hub.subscribe(ip(1), "default", limits(2, 1), HEARTBEAT, snapshot)
                .unwrap_err(),
            Refused::PeerFull
        );
        let _second = hub
            .subscribe(ip(2), "default", limits(2, 1), HEARTBEAT, snapshot)
            .unwrap();
        assert_eq!(
            hub.subscribe(ip(3), "default", limits(2, 1), HEARTBEAT, snapshot)
                .unwrap_err(),
            Refused::Full
        );
        drop(first);
        settle(&hub, 1).await;
        assert!(hub
            .subscribe(ip(1), "default", limits(2, 1), HEARTBEAT, snapshot)
            .is_ok());
    }

    #[tokio::test]
    async fn a_consumer_that_stops_reading_is_dropped() {
        let hub = Hub::new();
        let mut stalled = hub
            .subscribe(ip(1), "default", limits(4, 4), HEARTBEAT, snapshot)
            .unwrap();
        let mut reading = hub
            .subscribe(ip(2), "default", limits(4, 4), HEARTBEAT, snapshot)
            .unwrap();
        for n in 0..SUBSCRIBER_BUFFER + 5 {
            hub.publish(frame("key", &json!({"n": n})));
            // Keep one subscriber current so only the stalled one overflows.
            assert!(next(&mut reading).await.is_some());
        }
        settle(&hub, 1).await;
        let mut got = 0;
        while stalled.recv().await.is_some() {
            got += 1;
        }
        assert_eq!(
            got, SUBSCRIBER_BUFFER,
            "it got what fit, then its stream ended"
        );
    }

    #[tokio::test]
    async fn close_all_ends_every_stream() {
        let hub = Hub::new();
        let mut inbox = hub
            .subscribe(ip(1), "default", limits(4, 4), HEARTBEAT, snapshot)
            .unwrap();
        hub.close_all();
        assert_eq!(next(&mut inbox).await.unwrap(), snapshot());
        assert_eq!(next(&mut inbox).await, None);
        settle(&hub, 0).await;
    }

    #[tokio::test]
    async fn the_per_token_cap_counts_streams_of_one_token_across_addresses() {
        let hub = Hub::new();
        let caps = Limits {
            total: 8,
            per_peer: 8,
            per_token: 2,
        };
        let first = hub
            .subscribe(ip(1), "ct-5", caps, HEARTBEAT, snapshot)
            .unwrap();
        let _second = hub
            .subscribe(ip(2), "ct-5", caps, HEARTBEAT, snapshot)
            .unwrap();
        assert_eq!(
            hub.subscribe(ip(3), "ct-5", caps, HEARTBEAT, snapshot)
                .unwrap_err(),
            Refused::TokenFull
        );
        let _other = hub
            .subscribe(ip(3), "ct-6", caps, HEARTBEAT, snapshot)
            .expect("another token has its own count");
        drop(first);
        settle(&hub, 2).await;
        assert!(hub
            .subscribe(ip(3), "ct-5", caps, HEARTBEAT, snapshot)
            .is_ok());
    }

    #[tokio::test]
    async fn closing_a_token_ends_only_its_streams() {
        let hub = Hub::new();
        let mut gone = hub
            .subscribe(ip(1), "ct-5", limits(4, 4), HEARTBEAT, snapshot)
            .unwrap();
        let mut kept = hub
            .subscribe(ip(1), "ct-6", limits(4, 4), HEARTBEAT, snapshot)
            .unwrap();
        hub.close_tokens(vec!["ct-5".to_owned()]);
        hub.publish(frame("key", &json!({"n": 1})));
        assert_eq!(next(&mut gone).await.unwrap(), snapshot());
        assert_eq!(next(&mut gone).await, None);
        assert_eq!(next(&mut kept).await.unwrap(), snapshot());
        assert_eq!(
            next(&mut kept).await.unwrap(),
            Bytes::from("event: key\ndata: {\"n\":1}\n\n")
        );
        settle(&hub, 1).await;
    }

    #[test]
    fn a_ticking_cooldown_is_not_a_change() {
        let a = json!({"name": "zen", "cooling_keys": 1, "soonest_cooldown_seconds": 40,
                       "keys": [{"name": "k", "state": "cooling", "cooldown_seconds": 40}]});
        let b = json!({"name": "zen", "cooling_keys": 1, "soonest_cooldown_seconds": 39,
                       "keys": [{"name": "k", "state": "cooling", "cooldown_seconds": 39}]});
        assert_eq!(fingerprint(&a), fingerprint(&b));
        let c = json!({"name": "zen", "cooling_keys": 2, "soonest_cooldown_seconds": 39});
        assert_ne!(fingerprint(&a), fingerprint(&c));
    }
}
