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
}

#[derive(Default)]
struct Slots {
    total: usize,
    per_peer: HashMap<IpAddr, usize>,
}

pub(crate) struct Hub {
    tx: broadcast::Sender<Bytes>,
    slots: Mutex<Slots>,
    ticker: OnceLock<()>,
}

/// Holds one subscriber's place until its forwarding task ends.
struct Slot {
    hub: Arc<Hub>,
    peer: Option<IpAddr>,
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
            if let Some(count) = slots.per_peer.get_mut(&ip) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    slots.per_peer.remove(&ip);
                }
            }
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
        max_total: usize,
        max_per_peer: usize,
    ) -> Result<Slot, Refused> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if slots.total >= max_total {
            return Err(Refused::Full);
        }
        if let Some(ip) = peer {
            let count = slots.per_peer.entry(ip).or_insert(0);
            if *count >= max_per_peer {
                return Err(Refused::PeerFull);
            }
            *count += 1;
        }
        slots.total += 1;
        Ok(Slot {
            hub: Arc::clone(self),
            peer,
        })
    }

    /// Joins the broadcast, then queues `snapshot()` first, so no change between the two is lost.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        peer: Option<IpAddr>,
        limits: (usize, usize),
        heartbeat: Duration,
        snapshot: impl FnOnce() -> Bytes,
    ) -> Result<mpsc::Receiver<Bytes>, Refused> {
        let slot = self.reserve(peer, limits.0, limits.1)?;
        let events = self.tx.subscribe();
        let (out, inbox) = mpsc::channel(SUBSCRIBER_BUFFER);
        let _ = out.try_send(snapshot());
        tokio::spawn(forward(events, out, heartbeat, slot));
        Ok(inbox)
    }

    pub(crate) fn publish(&self, frame: Bytes) {
        let _ = self.tx.send(frame);
    }

    /// Ends every open stream, for a reload that stops publishing or changes the token.
    pub(crate) fn close_all(&self) {
        self.publish(Bytes::new());
    }
}

async fn forward(
    mut events: broadcast::Receiver<Bytes>,
    out: mpsc::Sender<Bytes>,
    heartbeat: Duration,
    slot: Slot,
) {
    let mut beat = tokio::time::interval_at(Instant::now() + heartbeat, heartbeat);
    loop {
        let next = tokio::select! {
            event = events.recv() => match event {
                Ok(frame) if frame.is_empty() => break,
                Ok(frame) => frame,
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
    token: Option<String>,
    upstreams: Vec<(String, Value)>,
    sessions: Option<Value>,
    next_host: Instant,
}

impl Watcher {
    fn new() -> Self {
        Self {
            token: None,
            upstreams: Vec::new(),
            sessions: None,
            next_host: Instant::now(),
        }
    }

    /// The frames that describe what changed since the last call, or `None` to close every stream.
    fn changes(&mut self, app: &App, now: Instant) -> Option<Vec<Bytes>> {
        let snapshot = app.snapshot();
        let peers = snapshot.config.peers.as_ref().filter(|p| p.publishing())?;
        if self.token.as_ref() != peers.peer_token_sha256.as_ref() {
            let first = self.token.is_none();
            self.token.clone_from(&peers.peer_token_sha256);
            if !first {
                return None;
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
        Some(frames)
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
            Some(frames) => frames.into_iter().for_each(|f| hub.publish(f)),
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
        let mut inbox = hub.subscribe(ip(1), (4, 4), HEARTBEAT, snapshot).unwrap();
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
            .subscribe(ip(1), (4, 4), Duration::from_millis(50), snapshot)
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
        let first = hub.subscribe(ip(1), (2, 1), HEARTBEAT, snapshot).unwrap();
        assert_eq!(
            hub.subscribe(ip(1), (2, 1), HEARTBEAT, snapshot)
                .unwrap_err(),
            Refused::PeerFull
        );
        let _second = hub.subscribe(ip(2), (2, 1), HEARTBEAT, snapshot).unwrap();
        assert_eq!(
            hub.subscribe(ip(3), (2, 1), HEARTBEAT, snapshot)
                .unwrap_err(),
            Refused::Full
        );
        drop(first);
        settle(&hub, 1).await;
        assert!(hub.subscribe(ip(1), (2, 1), HEARTBEAT, snapshot).is_ok());
    }

    #[tokio::test]
    async fn a_consumer_that_stops_reading_is_dropped() {
        let hub = Hub::new();
        let mut stalled = hub.subscribe(ip(1), (4, 4), HEARTBEAT, snapshot).unwrap();
        let mut reading = hub.subscribe(ip(2), (4, 4), HEARTBEAT, snapshot).unwrap();
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
        let mut inbox = hub.subscribe(ip(1), (4, 4), HEARTBEAT, snapshot).unwrap();
        hub.close_all();
        assert_eq!(next(&mut inbox).await.unwrap(), snapshot());
        assert_eq!(next(&mut inbox).await, None);
        settle(&hub, 0).await;
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
