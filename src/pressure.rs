//! In-memory per-upstream attempt and token counts for the last hour.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use reiki_plugin::UpstreamUsage;

use crate::config::Config;

/// Minutes in the window.
const MINUTES: usize = 60;
/// Five-minute buckets published to a reader.
pub(crate) const BUCKETS: usize = 12;
/// One-minute counts published to a reader, the ten minutes ending at the current one.
pub(crate) const MINUTE_SLOTS: usize = 10;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Slot {
    minute: u64,
    requests: u32,
    limited: u32,
    tokens: u64,
}

#[derive(Debug, Clone)]
struct Series {
    slots: [Slot; MINUTES],
}

impl Default for Series {
    fn default() -> Self {
        Self {
            slots: [Slot::default(); MINUTES],
        }
    }
}

#[derive(Debug)]
pub(crate) struct PressureCounter {
    started: Instant,
    upstreams: Mutex<HashMap<String, Series>>,
}

/// The last-hour request and token figures for one upstream, read under a single lock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PressureSnapshot {
    pub(crate) requests_1h: u64,
    pub(crate) limited_1h: u32,
    pub(crate) tokens_1h: u64,
    pub(crate) buckets: [u32; BUCKETS],
    pub(crate) token_buckets: [u64; BUCKETS],
    pub(crate) minutes: [u32; MINUTE_SLOTS],
    pub(crate) token_minutes: [u32; MINUTE_SLOTS],
}

impl PressureCounter {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            upstreams: Mutex::new(HashMap::new()),
        }
    }

    /// Counts one attempt. `now` only advances the minute ring; a clock that goes backwards is
    /// clamped, never trusted to move a bucket.
    pub(crate) fn record(&self, upstream: &str, limited: bool, now: Instant) {
        let minute = self.minute(now);
        let mut map = self
            .upstreams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(series) = series_mut(&mut map, upstream) else {
            return;
        };
        let slot = slot_at(series, minute);
        slot.requests = slot.requests.saturating_add(1);
        if limited {
            slot.limited = slot.limited.saturating_add(1);
        }
    }

    /// Adds a response's input and output tokens to the minute `now` falls in.
    pub(crate) fn record_tokens(&self, upstream: &str, tokens: u64, now: Instant) {
        let minute = self.minute(now);
        let mut map = self
            .upstreams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(series) = series_mut(&mut map, upstream) else {
            return;
        };
        let slot = slot_at(series, minute);
        slot.tokens = slot.tokens.saturating_add(tokens);
    }

    /// All last-hour figures for `upstream`, read under one lock and at the same minute.
    pub(crate) fn snapshot(&self, upstream: &str, now: Instant) -> PressureSnapshot {
        let minute = self.minute(now);
        let map = self
            .upstreams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(series) = map.get(upstream) else {
            return PressureSnapshot::default();
        };
        let mut out = PressureSnapshot::default();
        for slot in series.slots.iter().filter(|slot| in_hour(slot, minute)) {
            out.requests_1h = out.requests_1h.saturating_add(u64::from(slot.requests));
            out.limited_1h = out.limited_1h.saturating_add(slot.limited);
            out.tokens_1h = out.tokens_1h.saturating_add(slot.tokens);
            let bucket = bucket_of(slot, minute);
            out.buckets[bucket] = out.buckets[bucket].saturating_add(slot.requests);
            out.token_buckets[bucket] = out.token_buckets[bucket].saturating_add(slot.tokens);
        }
        // Looked up by minute: an empty slot still carries minute 0, so scanning the slots would
        // let an unused one overwrite a real minute 0.
        for (position, (requests, tokens)) in out
            .minutes
            .iter_mut()
            .zip(out.token_minutes.iter_mut())
            .enumerate()
        {
            let Some(wanted) = minute.checked_sub((MINUTE_SLOTS - 1 - position) as u64) else {
                continue;
            };
            let slot = &series.slots[(wanted % MINUTES as u64) as usize];
            if slot.minute == wanted {
                *requests = slot.requests;
                *tokens = u32::try_from(slot.tokens).unwrap_or(u32::MAX);
            }
        }
        out
    }

    fn minute(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started).as_secs() / 60
    }
}

/// The last-hour figures of every upstream the peer document lists, in config order, for an
/// admin scrape. Passthrough upstreams are left out, as they are there.
pub(crate) fn upstream_usage(
    config: &Config,
    pressure: &PressureCounter,
    now: Instant,
) -> Vec<UpstreamUsage> {
    config
        .upstream
        .iter()
        .filter(|(_, upstream)| !upstream.passthrough)
        .map(|(name, _)| {
            let snapshot = pressure.snapshot(name, now);
            UpstreamUsage {
                upstream: name.clone(),
                requests_1h: snapshot.requests_1h,
                limited_1h: u64::from(snapshot.limited_1h),
                tokens_1h: snapshot.tokens_1h,
            }
        })
        .collect()
}

/// The upstream's series, created on first use. Looked up before inserting so the hot path
/// allocates only the first time.
fn series_mut<'a>(map: &'a mut HashMap<String, Series>, upstream: &str) -> Option<&'a mut Series> {
    if !map.contains_key(upstream) {
        map.insert(upstream.to_owned(), Series::default());
    }
    map.get_mut(upstream)
}

/// The slot for `minute`, emptied first when it still holds an earlier minute.
fn slot_at(series: &mut Series, minute: u64) -> &mut Slot {
    let slot = &mut series.slots[(minute % MINUTES as u64) as usize];
    if slot.minute != minute {
        *slot = Slot {
            minute,
            ..Slot::default()
        };
    }
    slot
}

/// True when the slot's minute falls in the hour that ends at `minute`.
fn in_hour(slot: &Slot, minute: u64) -> bool {
    slot.minute <= minute && minute - slot.minute < MINUTES as u64
}

/// The five-minute bucket, oldest first, that a slot in the hour ending at `minute` belongs to.
fn bucket_of(slot: &Slot, minute: u64) -> usize {
    BUCKETS - 1 - ((minute - slot.minute) / 5) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn minutes_roll_over_into_oldest_first_five_minute_buckets() {
        let counter = PressureCounter::new();
        let t0 = Instant::now();
        for minute in [0, 5, 10, 55] {
            counter.record(
                "zen",
                false,
                t0 + std::time::Duration::from_secs(minute * 60),
            );
        }
        counter.record("zen", true, t0 + std::time::Duration::from_secs(55 * 60));
        let at = t0 + std::time::Duration::from_secs(59 * 60);
        let snapshot = counter.snapshot("zen", at);
        assert_eq!((snapshot.requests_1h, snapshot.limited_1h), (5, 1));
        assert_eq!(
            counter.snapshot("zen", at).buckets,
            [1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 2],
            "minutes 0, 5 and 10 fall into their own five-minute buckets"
        );
    }

    #[test]
    fn a_minute_boundary_does_not_leak_into_the_next_bucket() {
        let counter = PressureCounter::new();
        let t0 = Instant::now();
        counter.record("zen", false, t0);
        counter.record("zen", false, t0 + std::time::Duration::from_secs(59));
        let at = t0 + std::time::Duration::from_secs(60);
        let snapshot = counter.snapshot("zen", at);
        assert_eq!((snapshot.requests_1h, snapshot.limited_1h), (2, 0));
        assert_eq!(
            counter.snapshot("zen", at).buckets,
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2],
            "both seconds are in the newest five-minute bucket"
        );
    }

    #[test]
    fn an_idle_hour_reads_all_zero() {
        let counter = PressureCounter::new();
        let t0 = Instant::now();
        counter.record("zen", true, t0);
        let later = t0 + std::time::Duration::from_secs(60 * 60);
        let snapshot = counter.snapshot("zen", later);
        assert_eq!((snapshot.requests_1h, snapshot.limited_1h), (0, 0));
        assert_eq!(snapshot.buckets, [0; BUCKETS]);
        let snapshot = counter.snapshot("never-seen", t0);
        assert_eq!((snapshot.requests_1h, snapshot.limited_1h), (0, 0));
        assert_eq!(snapshot.buckets, [0; BUCKETS]);
    }

    #[test]
    fn counts_saturate_instead_of_wrapping() {
        let counter = PressureCounter::new();
        let t0 = Instant::now();
        counter.record("zen", true, t0);
        {
            let mut map = counter.upstreams.lock().unwrap();
            let series = map.get_mut("zen").unwrap();
            let slot = &mut series.slots[0];
            slot.requests = u32::MAX;
            slot.limited = u32::MAX;
        }
        counter.record("zen", true, t0);
        let snapshot = counter.snapshot("zen", t0);
        assert_eq!(
            (snapshot.requests_1h, snapshot.limited_1h),
            (u64::from(u32::MAX), u32::MAX)
        );
        assert_eq!(snapshot.buckets[BUCKETS - 1], u32::MAX);
    }

    #[test]
    fn concurrent_increments_are_all_counted() {
        let counter = Arc::new(PressureCounter::new());
        let t0 = Instant::now();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        counter.record("zen", true, t0);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let snapshot = counter.snapshot("zen", t0);
        assert_eq!((snapshot.requests_1h, snapshot.limited_1h), (8000, 8000));
        assert_eq!(snapshot.buckets[BUCKETS - 1], 8000);
    }

    #[test]
    fn tokens_roll_over_like_requests_and_saturate() {
        let counter = PressureCounter::new();
        let t0 = Instant::now();
        let at = |secs: u64| t0 + std::time::Duration::from_secs(secs);
        counter.record_tokens("zen", 10, t0);
        counter.record_tokens("zen", 5, at(5 * 60));
        counter.record_tokens("zen", 7, at(59 * 60));
        counter.record_tokens("other", 1, t0);
        assert_eq!(counter.snapshot("zen", at(59 * 60)).tokens_1h, 22);
        let mut expected = [0u64; BUCKETS];
        expected[0] = 10;
        expected[1] = 5;
        expected[BUCKETS - 1] = 7;
        assert_eq!(counter.snapshot("zen", at(59 * 60)).token_buckets, expected);
        let snapshot = counter.snapshot("zen", at(59 * 60));
        assert_eq!(
            (snapshot.requests_1h, snapshot.limited_1h),
            (0, 0),
            "requests are separate"
        );
        assert_eq!(
            counter.snapshot("zen", at(60 * 60 + 60)).tokens_1h,
            12,
            "minute 0 ages out"
        );

        counter.record_tokens("zen", u64::MAX, t0);
        counter.record_tokens("zen", 1, t0);
        assert_eq!(
            counter.snapshot("zen", t0).tokens_1h,
            u64::MAX,
            "saturates, never wraps"
        );
        assert_eq!(counter.snapshot("never-seen", t0).tokens_1h, 0);
        assert_eq!(
            counter.snapshot("never-seen", t0).token_buckets,
            [0; BUCKETS]
        );
    }

    #[test]
    fn a_slot_reused_after_an_hour_starts_its_tokens_again() {
        let counter = PressureCounter::new();
        let t0 = Instant::now();
        counter.record_tokens("zen", 100, t0);
        let later = t0 + std::time::Duration::from_secs(60 * 60);
        counter.record_tokens("zen", 3, later);
        assert_eq!(counter.snapshot("zen", later).tokens_1h, 3);
    }

    #[test]
    fn minutes_are_the_last_ten_one_minute_counts_oldest_first() {
        let counter = PressureCounter::new();
        let t0 = Instant::now();
        let at = |minute: u64| t0 + std::time::Duration::from_secs(minute * 60);
        counter.record("zen", false, at(2));
        counter.record("zen", false, at(3));
        counter.record("zen", true, at(3));
        counter.record("zen", false, at(12));
        counter.record("zen", false, at(12));
        counter.record("zen", false, at(12));
        counter.record_tokens("zen", 5, at(3));
        counter.record_tokens("zen", u64::from(u32::MAX) + 9, at(12));
        assert_eq!(
            counter.snapshot("zen", at(12)).minutes,
            [2, 0, 0, 0, 0, 0, 0, 0, 0, 3],
            "minute 2 is outside the ten minutes ending at minute 12"
        );
        assert_eq!(
            counter.snapshot("zen", at(12)).token_minutes[9],
            u32::MAX,
            "tokens saturate to u32"
        );
        assert_eq!(counter.snapshot("zen", at(12)).token_minutes[0], 5);
        assert_eq!(counter.snapshot("zen", at(70)).minutes, [0; MINUTE_SLOTS]);
        assert_eq!(
            counter.snapshot("never-seen", at(12)).minutes,
            [0; MINUTE_SLOTS]
        );
        assert_eq!(
            counter.snapshot("never-seen", at(12)).token_minutes,
            [0; MINUTE_SLOTS]
        );
    }

    #[test]
    fn an_unused_slot_never_hides_minute_zero() {
        let counter = PressureCounter::new();
        let t0 = Instant::now();
        counter.record("zen", false, t0);
        let at_nine = t0 + std::time::Duration::from_secs(9 * 60);
        assert_eq!(
            counter.snapshot("zen", at_nine).minutes,
            [1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            "minute 0 is the oldest of the ten, and empty slots draw nothing"
        );
    }
}
