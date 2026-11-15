pub mod bucket;
pub mod headers;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use crate::shaper::bucket::{Bucket, Decision, Permit, Window};

/// Limits for a single key: its rate-limit window and the maximum time
/// a caller is willing to wait for a slot.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyLimits {
    pub window: Window,
    pub max_wait: Duration,
}

/// Result of trying to acquire capacity for a shaped key.
#[derive(Debug)]
pub enum Acquire {
    /// The key is not shaped.
    Unshaped,
    /// Capacity granted; the permit releases its concurrency slot on drop.
    Granted(Permit),
    /// Wait time exceeds `max_wait`; the caller should shed or skip the key.
    TooLong(Duration),
    /// The key's daily or monthly window is exhausted until this instant.
    Closed(SystemTime),
    /// A slot will be available after this duration; the caller should sleep.
    Wait(Duration),
}

/// Per-key rate-limit buckets that survive a config reload.
///
/// The map stores the bucket, its window (for reload comparison), and the
/// caller's max wait. The window is kept beside the bucket because
/// `Bucket` does not expose its window publicly.
#[derive(Debug, Default)]
pub struct Shaper {
    map: RwLock<BTreeMap<String, Entry>>,
}

/// A shaped key: its bucket, the window it was built from, and its max wait.
type Entry = (Arc<Bucket>, Window, Duration);

impl Shaper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronizes the shaper's key set to `wanted`.
    ///
    /// - Keys present in `wanted` with an unchanged `Window` keep their existing
    ///   bucket (same Arc), preserving counters and learned caps across reloads.
    /// - Keys with a changed `Window` or new keys get a fresh bucket.
    /// - Keys not in `wanted` are dropped.
    pub fn sync(&self, wanted: &BTreeMap<String, KeyLimits>) {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        let mut next = BTreeMap::new();

        for (key, limits) in wanted {
            let bucket = match map.get(key) {
                Some((bucket, window, _)) if window == &limits.window => Arc::clone(bucket),
                _ => Arc::new(Bucket::new(limits.window.clone())),
            };
            next.insert(
                key.clone(),
                (bucket, limits.window.clone(), limits.max_wait),
            );
        }

        *map = next;
    }

    /// Returns the bucket and its `max_wait` for `key`, or `None` if the key is not shaped.
    pub fn get(&self, key: &str) -> Option<(Arc<Bucket>, Duration)> {
        let map = self.map.read().unwrap_or_else(|e| e.into_inner());
        map.get(key).map(|(b, _, w)| (Arc::clone(b), *w))
    }

    /// Returns true if no keys are currently shaped.
    pub fn is_empty(&self) -> bool {
        let map = self.map.read().unwrap_or_else(|e| e.into_inner());
        map.is_empty()
    }

    /// Tries to acquire capacity for `key` at time `now` with token `estimate`.
    /// Returns an `Acquire` variant describing the outcome.
    pub fn acquire_within(&self, key: &str, now: SystemTime, estimate: u64) -> Acquire {
        let (bucket, max_wait) = match self.get(key) {
            Some(pair) => pair,
            None => return Acquire::Unshaped,
        };

        match bucket.try_acquire(now, estimate) {
            Decision::Granted(permit) => Acquire::Granted(permit),
            Decision::Wait(d) => {
                if d <= max_wait {
                    Acquire::Wait(d)
                } else {
                    Acquire::TooLong(d)
                }
            }
            Decision::Closed(until) => Acquire::Closed(until),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_changed_max_wait_applies_without_resetting_the_bucket() {
        let shaper = Shaper::new();
        let window = bucket::Window {
            rpm: 5,
            ..bucket::Window::default()
        };
        let one = |secs| {
            BTreeMap::from([(
                "k".to_owned(),
                KeyLimits {
                    window: window.clone(),
                    max_wait: Duration::from_secs(secs),
                },
            )])
        };
        shaper.sync(&one(20));
        let (before, _) = shaper.get("k").unwrap();
        shaper.sync(&one(60));
        let (after, wait) = shaper.get("k").unwrap();
        assert!(Arc::ptr_eq(&before, &after), "the bucket is kept");
        assert_eq!(wait, Duration::from_secs(60), "the new max_wait applies");
    }

    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn window(rpm: u32) -> Window {
        Window {
            rpm,
            ..Window::default()
        }
    }

    fn limits(rpm: u32, max_wait_secs: u64) -> KeyLimits {
        KeyLimits {
            window: window(rpm),
            max_wait: Duration::from_secs(max_wait_secs),
        }
    }

    #[test]
    fn sync_keeps_unchanged_bucket_same_arc() {
        let shaper = Shaper::new();
        let mut wanted = BTreeMap::new();
        wanted.insert("key1".to_string(), limits(10, 5));

        shaper.sync(&wanted);
        let (bucket1, _) = shaper.get("key1").unwrap();

        shaper.sync(&wanted);
        let (bucket2, _) = shaper.get("key1").unwrap();

        assert!(
            Arc::ptr_eq(&bucket1, &bucket2),
            "bucket Arc should be identical"
        );
    }

    #[test]
    fn sync_keeps_counters_across_reload() {
        let shaper = Shaper::new();
        let mut wanted = BTreeMap::new();
        wanted.insert("key1".to_string(), limits(2, 5));

        shaper.sync(&wanted);
        let (bucket, _) = shaper.get("key1").unwrap();

        let t0 = at(1000);
        for _ in 0..2 {
            match bucket.try_acquire(t0, 0) {
                Decision::Granted(p) => p.complete(0),
                other => panic!("{other:?}"),
            }
        }
        assert!(matches!(bucket.try_acquire(t0, 0), Decision::Wait(_)));

        shaper.sync(&wanted);
        let (bucket2, _) = shaper.get("key1").unwrap();
        assert!(Arc::ptr_eq(&bucket, &bucket2));
        assert!(matches!(bucket2.try_acquire(t0, 0), Decision::Wait(_)));
    }

    #[test]
    fn sync_replaces_bucket_on_window_change() {
        let shaper = Shaper::new();
        let mut wanted = BTreeMap::new();
        wanted.insert("key1".to_string(), limits(2, 5));

        shaper.sync(&wanted);
        let (bucket1, _) = shaper.get("key1").unwrap();

        wanted.insert("key1".to_string(), limits(5, 5));
        shaper.sync(&wanted);
        let (bucket2, _) = shaper.get("key1").unwrap();

        assert!(
            !Arc::ptr_eq(&bucket1, &bucket2),
            "bucket Arc should be different"
        );
    }

    #[test]
    fn sync_drops_removed_key() {
        let shaper = Shaper::new();
        let mut wanted = BTreeMap::new();
        wanted.insert("key1".to_string(), limits(10, 5));
        wanted.insert("key2".to_string(), limits(10, 5));

        shaper.sync(&wanted);
        assert!(shaper.get("key1").is_some());
        assert!(shaper.get("key2").is_some());

        wanted.remove("key2");
        shaper.sync(&wanted);

        assert!(shaper.get("key1").is_some());
        assert!(shaper.get("key2").is_none());
    }

    #[test]
    fn unshaped_key_returns_unshaped() {
        let shaper = Shaper::new();
        let mut wanted = BTreeMap::new();
        wanted.insert("key1".to_string(), limits(10, 5));
        shaper.sync(&wanted);

        let result = shaper.acquire_within("unknown", at(1000), 0);
        assert!(matches!(result, Acquire::Unshaped));
    }

    #[test]
    fn short_wait_returns_wait() {
        let shaper = Shaper::new();
        let mut wanted = BTreeMap::new();
        wanted.insert("key1".to_string(), limits(1, 60));
        shaper.sync(&wanted);

        let (bucket, _) = shaper.get("key1").unwrap();
        let t0 = at(1000);
        match bucket.try_acquire(t0, 0) {
            Decision::Granted(p) => p.complete(0),
            other => panic!("{other:?}"),
        }

        let result = shaper.acquire_within("key1", t0, 0);
        match result {
            Acquire::Wait(d) => assert!(d > Duration::ZERO && d <= Duration::from_secs(60)),
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    #[test]
    fn too_long_wait_returns_too_long() {
        let shaper = Shaper::new();
        let mut wanted = BTreeMap::new();
        wanted.insert("key1".to_string(), limits(1, 5));
        shaper.sync(&wanted);

        let (bucket, _) = shaper.get("key1").unwrap();
        let t0 = at(1000);
        match bucket.try_acquire(t0, 0) {
            Decision::Granted(p) => p.complete(0),
            other => panic!("{other:?}"),
        }

        let result = shaper.acquire_within("key1", t0, 0);
        match result {
            Acquire::TooLong(d) => assert!(d > Duration::from_secs(5)),
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    #[test]
    fn closed_returns_closed() {
        let shaper = Shaper::new();
        let mut wanted = BTreeMap::new();
        wanted.insert(
            "key1".to_string(),
            KeyLimits {
                window: Window {
                    daily_requests: 1,
                    ..Window::default()
                },
                max_wait: Duration::from_secs(10),
            },
        );
        shaper.sync(&wanted);

        let (bucket, _) = shaper.get("key1").unwrap();
        let t0 = at(1000);
        match bucket.try_acquire(t0, 0) {
            Decision::Granted(p) => p.complete(0),
            other => panic!("{other:?}"),
        }

        let result = shaper.acquire_within("key1", t0, 0);
        assert!(matches!(result, Acquire::Closed(_)));
    }

    #[test]
    fn concurrent_readers_during_sync_no_deadlock() {
        let shaper = Arc::new(Shaper::new());
        let mut wanted = BTreeMap::new();
        wanted.insert("key1".to_string(), limits(10, 5));
        shaper.sync(&wanted);

        let shaper_read = Arc::clone(&shaper);
        let reader = thread::spawn(move || {
            for _ in 0..100 {
                let _ = shaper_read.get("key1");
                let _ = shaper_read.acquire_within("key1", at(1000), 0);
                thread::yield_now();
            }
        });

        let shaper_write = Arc::clone(&shaper);
        let writer = thread::spawn(move || {
            for i in 0..50 {
                let mut w = BTreeMap::new();
                w.insert("key1".to_string(), limits(10 + i, 5));
                shaper_write.sync(&w);
                thread::yield_now();
            }
        });

        reader.join().unwrap();
        writer.join().unwrap();
    }

    #[test]
    fn is_empty() {
        let shaper = Shaper::new();
        assert!(shaper.is_empty());

        let mut wanted = BTreeMap::new();
        wanted.insert("key1".to_string(), limits(10, 5));
        shaper.sync(&wanted);
        assert!(!shaper.is_empty());

        wanted.clear();
        shaper.sync(&wanted);
        assert!(shaper.is_empty());
    }
}
