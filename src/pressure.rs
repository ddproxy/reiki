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

    /// Attempts and limited attempts in the last hour, saturating rather than wrapping.
    pub(crate) fn totals(&self, upstream: &str, now: Instant) -> (u64, u32) {
        let minute = self.minute(now);
        let map = self
            .upstreams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(series) = map.get(upstream) else {
            return (0, 0);
        };
        let (mut requests, mut limited) = (0u64, 0u32);
        for slot in series.slots.iter().filter(|slot| in_hour(slot, minute)) {
            requests = requests.saturating_add(u64::from(slot.requests));
            limited = limited.saturating_add(slot.limited);
        }
        (requests, limited)
    }

    /// Tokens in the last hour, saturating rather than wrapping.
    pub(crate) fn token_totals(&self, upstream: &str, now: Instant) -> u64 {
        let minute = self.minute(now);
        let map = self
            .upstreams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(series) = map.get(upstream) else {
            return 0;
        };
        series
            .slots
            .iter()
            .filter(|slot| in_hour(slot, minute))
            .fold(0u64, |total, slot| total.saturating_add(slot.tokens))
    }

    /// Twelve five-minute token counts, oldest first and newest last.
    pub(crate) fn token_buckets(&self, upstream: &str, now: Instant) -> [u64; BUCKETS] {
        let minute = self.minute(now);
        let map = self
            .upstreams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(series) = map.get(upstream) else {
            return [0; BUCKETS];
        };
        let mut out = [0u64; BUCKETS];
        for slot in series.slots.iter().filter(|slot| in_hour(slot, minute)) {
            let bucket = bucket_of(slot, minute);
            out[bucket] = out[bucket].saturating_add(slot.tokens);
        }
        out
    }

    /// Twelve five-minute buckets, oldest first and newest last.
    pub(crate) fn buckets(&self, upstream: &str, now: Instant) -> [u32; BUCKETS] {
        let minute = self.minute(now);
        let map = self
            .upstreams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(series) = map.get(upstream) else {
            return [0; BUCKETS];
        };
        let mut out = [0u32; BUCKETS];
        for slot in series.slots.iter().filter(|slot| in_hour(slot, minute)) {
            let bucket = bucket_of(slot, minute);
            out[bucket] = out[bucket].saturating_add(slot.requests);
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
            let (requests, limited) = pressure.totals(name, now);
            UpstreamUsage {
                upstream: name.clone(),
                requests_1h: requests,
                limited_1h: u64::from(limited),
                tokens_1h: pressure.token_totals(name, now),
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
        assert_eq!(counter.totals("zen", at), (5, 1));
        assert_eq!(
            counter.buckets("zen", at),
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
        assert_eq!(counter.totals("zen", at), (2, 0));
        assert_eq!(
            counter.buckets("zen", at),
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
        assert_eq!(counter.totals("zen", later), (0, 0));
        assert_eq!(counter.buckets("zen", later), [0; BUCKETS]);
        assert_eq!(counter.totals("never-seen", t0), (0, 0));
        assert_eq!(counter.buckets("never-seen", t0), [0; BUCKETS]);
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
        assert_eq!(counter.totals("zen", t0), (u64::from(u32::MAX), u32::MAX));
        assert_eq!(counter.buckets("zen", t0)[BUCKETS - 1], u32::MAX);
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
        assert_eq!(counter.totals("zen", t0), (8000, 8000));
        assert_eq!(counter.buckets("zen", t0)[BUCKETS - 1], 8000);
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
        assert_eq!(counter.token_totals("zen", at(59 * 60)), 22);
        let mut expected = [0u64; BUCKETS];
        expected[0] = 10;
        expected[1] = 5;
        expected[BUCKETS - 1] = 7;
        assert_eq!(counter.token_buckets("zen", at(59 * 60)), expected);
        assert_eq!(
            counter.totals("zen", at(59 * 60)),
            (0, 0),
            "requests are separate"
        );
        assert_eq!(
            counter.token_totals("zen", at(60 * 60 + 60)),
            12,
            "minute 0 ages out"
        );

        counter.record_tokens("zen", u64::MAX, t0);
        counter.record_tokens("zen", 1, t0);
        assert_eq!(
            counter.token_totals("zen", t0),
            u64::MAX,
            "saturates, never wraps"
        );
        assert_eq!(counter.token_totals("never-seen", t0), 0);
        assert_eq!(counter.token_buckets("never-seen", t0), [0; BUCKETS]);
    }

    #[test]
    fn a_slot_reused_after_an_hour_starts_its_tokens_again() {
        let counter = PressureCounter::new();
        let t0 = Instant::now();
        counter.record_tokens("zen", 100, t0);
        let later = t0 + std::time::Duration::from_secs(60 * 60);
        counter.record_tokens("zen", 3, later);
        assert_eq!(counter.token_totals("zen", later), 3);
    }
}
