//! Provider-side request and token buckets. Pure: callers pass `now`.
//! rpm and tpm use a sliding grant log (a fixed minute allows 2x at its edge);
//! daily and monthly quotas are fixed UTC windows, as providers bill them.

use std::borrow::Borrow;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reiki_plugin::{civil_from_days, days_from_civil};

const MINUTE: Duration = Duration::from_secs(60);
const DAY: i64 = 86_400;
const MAX_PAUSE: Duration = Duration::from_secs(24 * 3_600);
/// A restored pause or closed window longer than this is not trusted.
const MAX_RESTORED_STATE: Duration = Duration::from_secs(31 * 24 * 3_600);
/// No slot time is computable for concurrency; re-check after this.
const CONCURRENCY_RETRY: Duration = Duration::from_secs(1);
/// Bounds memory when rpm is unlimited.
const LOG_CAP: usize = 4_096;

/// Zero in any limit means unlimited.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub rpm: u32,
    pub tpm: u64,
    pub concurrent: u32,
    pub daily_requests: u32,
    pub daily_tokens: u64,
    pub monthly_requests: u32,
    pub monthly_tokens: u64,
    /// Offset from UTC midnight where daily and monthly windows start.
    pub reset: Duration,
    /// Floor for learned rpm; zero counts as one.
    pub min_rpm: u32,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            rpm: 0,
            tpm: 0,
            concurrent: 0,
            daily_requests: 0,
            daily_tokens: 0,
            monthly_requests: 0,
            monthly_tokens: 0,
            reset: Duration::ZERO,
            min_rpm: 1,
        }
    }
}

#[derive(Debug)]
pub enum Decision {
    Granted(Permit),
    /// Time until the next slot; the caller queues or sheds.
    Wait(Duration),
    /// Daily or monthly window exhausted until this instant.
    Closed(SystemTime),
}

/// The durable part of a bucket: learned cap, the remaining pause and closed
/// time, and the current quota windows. The sliding minute log is omitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketSnapshot {
    /// Learned rpm below the configured ceiling; `None` means the ceiling applies.
    pub learned_rpm: Option<u32>,
    pub pause: Option<Duration>,
    pub closed: Option<Duration>,
    pub daily: Option<QuotaSnapshot>,
    pub monthly: Option<QuotaSnapshot>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuotaSnapshot {
    /// Unix seconds when the window started.
    pub start: i64,
    pub requests: u32,
    pub tokens: u64,
}

impl BucketSnapshot {
    pub fn is_empty(&self) -> bool {
        self.learned_rpm.is_none()
            && self.pause.is_none()
            && self.closed.is_none()
            && self.daily.is_none()
            && self.monthly.is_none()
    }
}

#[derive(Debug)]
pub struct Bucket {
    window: Window,
    state: Arc<Mutex<State>>,
}

/// Holds one concurrency slot until dropped.
#[derive(Debug)]
pub struct Permit {
    state: Arc<Mutex<State>>,
    seq: u64,
    estimate: u64,
    day_start: i64,
    month_start: i64,
}

#[derive(Debug, Default)]
struct State {
    log: VecDeque<Grant>,
    next_seq: u64,
    in_flight: u32,
    /// Zero means the configured rpm applies.
    learned_rpm: u32,
    paused_until: Option<SystemTime>,
    closed_until: Option<SystemTime>,
    /// Quiet period start, gating the slow raise.
    last_event: Option<SystemTime>,
    day: Quota,
    month: Quota,
    /// Latest time seen; a clock that steps back is held here so a window cannot reopen.
    high_water: Option<SystemTime>,
}

#[derive(Debug)]
struct Grant {
    seq: u64,
    at: SystemTime,
    tokens: u64,
}

#[derive(Debug, Default)]
struct Quota {
    start: i64,
    end: i64,
    requests: u32,
    tokens: u64,
}

#[derive(Clone, Copy)]
enum Period {
    Day,
    Month,
}

impl Bucket {
    pub fn new(window: Window) -> Self {
        Self {
            window,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// The facts worth keeping across a restart, expressed relative to `now`.
    pub fn snapshot(&self, now: SystemTime) -> BucketSnapshot {
        let w = &self.window;
        let s = lock(&self.state);
        BucketSnapshot {
            learned_rpm: (s.learned_rpm != 0).then_some(s.learned_rpm),
            pause: remaining(s.paused_until, now),
            closed: remaining(s.closed_until, now),
            daily: quota_snapshot(&s.day, w.daily_requests, w.daily_tokens),
            monthly: quota_snapshot(&s.month, w.monthly_requests, w.monthly_tokens),
        }
    }

    /// Applies a snapshot anchored to `now`. Hostile values are clamped: a pause,
    /// closed time or cap above the configured ceiling never outlasts them.
    pub fn restore(&self, snapshot: impl Borrow<BucketSnapshot>, now: SystemTime) {
        let snapshot = snapshot.borrow();
        let w = &self.window;
        let mut s = lock(&self.state);
        let reset = reset_secs(w);
        let now_secs = secs(now);

        s.learned_rpm = 0;
        if let Some(cap) = snapshot.learned_rpm.filter(|cap| *cap > 0) {
            let cap = cap.max(floor(w));
            s.set_cap(w, if w.rpm > 0 { cap.min(w.rpm) } else { cap });
        }
        s.paused_until = snapshot
            .pause
            .filter(|d| !d.is_zero())
            .map(|d| now + d.min(MAX_PAUSE));
        s.closed_until = snapshot
            .closed
            .filter(|d| !d.is_zero())
            .map(|d| now + d.min(MAX_RESTORED_STATE));

        restore_quota(
            &mut s.day,
            snapshot.daily,
            Period::Day,
            now_secs,
            reset,
            w.daily_requests,
            w.daily_tokens,
        );
        restore_quota(
            &mut s.month,
            snapshot.monthly,
            Period::Month,
            now_secs,
            reset,
            w.monthly_requests,
            w.monthly_tokens,
        );
    }

    pub fn try_acquire(&self, now: SystemTime, estimate: u64) -> Decision {
        let w = &self.window;
        let mut s = lock(&self.state);
        let now = s.high_water.map_or(now, |seen| seen.max(now));
        s.high_water = Some(now);
        let reset = reset_secs(w);
        s.day.roll(Period::Day, secs(now), reset);
        s.month.roll(Period::Month, secs(now), reset);
        s.prune(now);

        if let Some(until) = s.closed_until.filter(|u| *u > now) {
            return Decision::Closed(until);
        }
        if exhausted(&s.day, w.daily_requests, w.daily_tokens, estimate) {
            return Decision::Closed(from_secs(s.day.end));
        }
        if exhausted(&s.month, w.monthly_requests, w.monthly_tokens, estimate) {
            return Decision::Closed(from_secs(s.month.end));
        }
        if let Some(until) = s.paused_until.filter(|u| *u > now) {
            return Decision::Wait(until.duration_since(now).unwrap_or_default());
        }
        if w.concurrent > 0 && s.in_flight >= w.concurrent {
            return Decision::Wait(CONCURRENCY_RETRY);
        }
        let limit = s.rpm_limit(w) as usize;
        let n = s.log.len();
        if limit > 0 && n >= limit {
            return Decision::Wait(expiry(&s.log[n - limit], now));
        }
        if let Some(wait) = s.token_wait(w.tpm, estimate, now) {
            return Decision::Wait(wait);
        }

        s.next_seq += 1;
        let seq = s.next_seq;
        s.log.push_back(Grant {
            seq,
            at: now,
            tokens: estimate,
        });
        while s.log.len() > LOG_CAP.max(w.rpm as usize) {
            s.log.pop_front();
        }
        s.day.charge(estimate);
        s.month.charge(estimate);
        s.in_flight = s.in_flight.saturating_add(1);
        Decision::Granted(Permit {
            state: Arc::clone(&self.state),
            seq,
            estimate,
            day_start: s.day.start,
            month_start: s.month.start,
        })
    }

    /// Confirmed 429: cap rpm at 80 % of the last minute's rate, pause for `retry_after` (max 24 h).
    pub fn note_rate_limited(&self, now: SystemTime, retry_after: Option<Duration>) {
        let w = &self.window;
        let mut s = lock(&self.state);
        s.prune(now);
        let candidate = u32::try_from(s.log.len() * 4 / 5)
            .unwrap_or(u32::MAX)
            .max(floor(w));
        let base = s.rpm_limit(w);
        let next = if base == 0 {
            candidate
        } else {
            candidate.min(base)
        };
        s.set_cap(w, next);
        if let Some(d) = retry_after {
            let until = now + d.min(MAX_PAUSE);
            s.paused_until = Some(s.paused_until.map_or(until, |p| p.max(until)));
        }
        s.last_event = Some(now);
    }

    /// Closes the active daily or monthly window until `until`.
    pub fn note_window_closed(&self, now: SystemTime, until: SystemTime) {
        if until <= now {
            return;
        }
        let mut s = lock(&self.state);
        s.closed_until = Some(s.closed_until.map_or(until, |c| c.max(until)));
    }

    /// One step (5 % of ceiling) per quiet minute; clears the cap at the ceiling.
    pub fn note_success(&self, now: SystemTime) {
        let w = &self.window;
        let mut s = lock(&self.state);
        if s.learned_rpm == 0 {
            return;
        }
        let Some(last) = s.last_event else {
            return;
        };
        if now.duration_since(last).unwrap_or_default() < MINUTE {
            return;
        }
        s.last_event = Some(now);
        if w.rpm == 0 {
            s.learned_rpm = 0;
            return;
        }
        let next = s.learned_rpm.saturating_add((w.rpm / 20).max(1));
        s.set_cap(w, next);
    }

    /// Lowers the rpm cap to `rate`, by at most half per call.
    pub fn lower_to(&self, now: SystemTime, rate: u32) {
        let w = &self.window;
        let mut s = lock(&self.state);
        let current = s.rpm_limit(w);
        if current != 0 && rate >= current {
            return;
        }
        let next = if current == 0 {
            rate
        } else {
            rate.max(current / 2)
        }
        .max(floor(w));
        s.set_cap(w, next);
        s.last_event = Some(now);
    }
}

impl Permit {
    /// Swaps the estimate for the measured count and releases the slot.
    pub fn complete(self, actual_tokens: u64) {
        let mut s = lock(&self.state);
        if let Ok(i) = s.log.binary_search_by_key(&self.seq, |g| g.seq) {
            s.log[i].tokens = actual_tokens;
        }
        s.day.correct(self.day_start, self.estimate, actual_tokens);
        s.month
            .correct(self.month_start, self.estimate, actual_tokens);
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut s = lock(&self.state);
        s.in_flight = s.in_flight.saturating_sub(1);
    }
}

impl State {
    fn prune(&mut self, now: SystemTime) {
        while self.log.front().is_some_and(|g| g.at + MINUTE <= now) {
            self.log.pop_front();
        }
    }

    fn rpm_limit(&self, w: &Window) -> u32 {
        if self.learned_rpm != 0 {
            self.learned_rpm
        } else {
            w.rpm
        }
    }

    /// A cap at or above the ceiling is stored as zero.
    fn set_cap(&mut self, w: &Window, cap: u32) {
        self.learned_rpm = if w.rpm > 0 && cap >= w.rpm { 0 } else { cap };
    }

    fn token_wait(&self, tpm: u64, estimate: u64, now: SystemTime) -> Option<Duration> {
        if tpm == 0 || self.log.is_empty() {
            return None;
        }
        let total = self
            .log
            .iter()
            .fold(0u64, |a, g| a.saturating_add(g.tokens));
        if total.saturating_add(estimate) <= tpm {
            return None;
        }
        let mut rest = total;
        let mut idx = self.log.len() - 1;
        for (i, g) in self.log.iter().enumerate() {
            rest = rest.saturating_sub(g.tokens);
            if rest.saturating_add(estimate) <= tpm {
                idx = i;
                break;
            }
        }
        Some(expiry(&self.log[idx], now))
    }
}

impl Quota {
    fn roll(&mut self, period: Period, t: i64, reset: i64) {
        if t < self.start || t >= self.end {
            let (start, end) = period.bounds(t, reset);
            *self = Quota {
                start,
                end,
                requests: 0,
                tokens: 0,
            };
        }
    }

    fn charge(&mut self, tokens: u64) {
        self.requests = self.requests.saturating_add(1);
        self.tokens = self.tokens.saturating_add(tokens);
    }

    fn correct(&mut self, start: i64, estimate: u64, actual: u64) {
        if self.start == start {
            self.tokens = self.tokens.saturating_sub(estimate).saturating_add(actual);
        }
    }
}

impl Period {
    /// Unix-second bounds `[start, end)` of the window containing `t`.
    fn bounds(self, t: i64, reset: i64) -> (i64, i64) {
        let day = (t - reset).div_euclid(DAY);
        match self {
            Period::Day => (day * DAY + reset, (day + 1) * DAY + reset),
            Period::Month => {
                let (y, m, _) = civil_from_days(day);
                let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                (
                    days_from_civil(y, m, 1) * DAY + reset,
                    days_from_civil(ny, nm, 1) * DAY + reset,
                )
            }
        }
    }
}

fn exhausted(q: &Quota, requests: u32, tokens: u64, estimate: u64) -> bool {
    (requests > 0 && q.requests >= requests)
        || (tokens > 0 && q.tokens > 0 && q.tokens.saturating_add(estimate) > tokens)
}

/// Restores a quota only into the window it was saved from; a later window starts at zero.
fn restore_quota(
    quota: &mut Quota,
    saved: Option<QuotaSnapshot>,
    period: Period,
    now: i64,
    reset: i64,
    requests: u32,
    tokens: u64,
) {
    quota.roll(period, now, reset);
    let Some(saved) = saved.filter(|saved| saved.start == quota.start) else {
        return;
    };
    quota.requests = if requests > 0 {
        saved.requests.min(requests)
    } else {
        saved.requests
    };
    quota.tokens = if tokens > 0 {
        saved.tokens.min(tokens)
    } else {
        saved.tokens
    };
}

fn remaining(until: Option<SystemTime>, now: SystemTime) -> Option<Duration> {
    until
        .and_then(|until| until.duration_since(now).ok())
        .filter(|d| !d.is_zero())
}

/// Only a quota with a configured limit and some use is worth saving.
fn quota_snapshot(q: &Quota, requests: u32, tokens: u64) -> Option<QuotaSnapshot> {
    if (requests == 0 && tokens == 0) || (q.requests == 0 && q.tokens == 0) {
        return None;
    }
    Some(QuotaSnapshot {
        start: q.start,
        requests: q.requests,
        tokens: q.tokens,
    })
}

fn floor(w: &Window) -> u32 {
    w.min_rpm.max(1)
}

fn reset_secs(w: &Window) -> i64 {
    (w.reset.as_secs() % DAY as u64) as i64
}

fn expiry(g: &Grant, now: SystemTime) -> Duration {
    (g.at + MINUTE).duration_since(now).unwrap_or_default()
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

fn from_secs(s: i64) -> SystemTime {
    if s >= 0 {
        UNIX_EPOCH + Duration::from_secs(s as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(s.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i64, m: i64, d: i64, h: i64, min: i64, s: i64) -> SystemTime {
        from_secs(days_from_civil(y, m, d) * DAY + h * 3_600 + min * 60 + s)
    }

    fn plus(t: SystemTime, secs: u64) -> SystemTime {
        t + Duration::from_secs(secs)
    }

    fn rpm(n: u32) -> Bucket {
        Bucket::new(Window {
            rpm: n,
            ..Window::default()
        })
    }

    fn grant(b: &Bucket, now: SystemTime, est: u64) -> Permit {
        match b.try_acquire(now, est) {
            Decision::Granted(p) => p,
            other => panic!("expected grant, got {other:?}"),
        }
    }

    fn wait(b: &Bucket, now: SystemTime, est: u64) -> Duration {
        match b.try_acquire(now, est) {
            Decision::Wait(d) => d,
            other => panic!("expected wait, got {other:?}"),
        }
    }

    fn closed(b: &Bucket, now: SystemTime, est: u64) -> SystemTime {
        match b.try_acquire(now, est) {
            Decision::Closed(u) => u,
            other => panic!("expected closed, got {other:?}"),
        }
    }

    #[test]
    fn rpm_waits_exact_remaining_time() {
        let b = rpm(3);
        let t0 = at(2026, 11, 14, 12, 0, 0);
        grant(&b, t0, 0);
        grant(&b, plus(t0, 10), 0);
        grant(&b, plus(t0, 20), 0);
        assert_eq!(wait(&b, plus(t0, 30), 0), Duration::from_secs(30));
        assert_eq!(wait(&b, plus(t0, 59), 0), Duration::from_secs(1));
        grant(&b, plus(t0, 60), 0);
        assert_eq!(wait(&b, plus(t0, 60), 0), Duration::from_secs(10));
    }

    #[test]
    fn a_clock_that_steps_back_does_not_reopen_a_quota() {
        let w = Window {
            daily_requests: 2,
            ..Window::default()
        };
        let b = Bucket::new(w);
        let t = UNIX_EPOCH + Duration::from_secs(10 * 86_400 + 100);
        let mut permits = Vec::new();
        for _ in 0..2 {
            match b.try_acquire(t, 0) {
                Decision::Granted(p) => permits.push(p),
                other => panic!("{other:?}"),
            }
        }
        let before_midnight = t - Duration::from_secs(200);
        assert!(matches!(
            b.try_acquire(before_midnight, 0),
            Decision::Closed(_)
        ));
        assert!(matches!(b.try_acquire(t, 0), Decision::Closed(_)));
    }

    #[test]
    fn tokens_correct_down_and_up_on_complete() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let b = Bucket::new(Window {
            tpm: 1_000,
            ..Window::default()
        });
        grant(&b, t0, 400).complete(100);
        grant(&b, plus(t0, 1), 900);
        assert_eq!(wait(&b, plus(t0, 2), 1), Duration::from_secs(58));

        let b = Bucket::new(Window {
            tpm: 1_000,
            ..Window::default()
        });
        grant(&b, t0, 100).complete(1_000);
        assert_eq!(wait(&b, plus(t0, 1), 1), Duration::from_secs(59));
    }

    #[test]
    fn oversized_request_only_admitted_into_empty_minute() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let b = Bucket::new(Window {
            tpm: 100,
            ..Window::default()
        });
        grant(&b, t0, 500);
        assert_eq!(wait(&b, plus(t0, 1), 1), Duration::from_secs(59));
        grant(&b, plus(t0, 60), 1);
    }

    #[test]
    fn concurrent_slot_freed_on_drop_and_on_complete() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let b = Bucket::new(Window {
            concurrent: 1,
            ..Window::default()
        });
        let p = grant(&b, t0, 0);
        assert_eq!(wait(&b, t0, 0), CONCURRENCY_RETRY);
        drop(p);
        let p = grant(&b, t0, 0);
        assert_eq!(wait(&b, t0, 0), CONCURRENCY_RETRY);
        p.complete(0);
        grant(&b, t0, 0);
    }

    #[test]
    fn daily_window_rolls_at_utc_midnight() {
        let b = Bucket::new(Window {
            daily_requests: 2,
            ..Window::default()
        });
        let late = at(2026, 11, 14, 23, 59, 58);
        grant(&b, late, 0);
        grant(&b, plus(late, 1), 0);
        assert_eq!(closed(&b, plus(late, 1), 0), at(2026, 11, 15, 0, 0, 0));
        grant(&b, at(2026, 11, 15, 0, 0, 0), 0);
    }

    #[test]
    fn daily_window_honours_reset_offset() {
        let b = Bucket::new(Window {
            daily_requests: 1,
            reset: Duration::from_secs(9 * 3_600),
            ..Window::default()
        });
        grant(&b, at(2026, 11, 14, 8, 0, 0), 0);
        assert_eq!(
            closed(&b, at(2026, 11, 14, 8, 30, 0), 0),
            at(2026, 11, 14, 9, 0, 0)
        );
        grant(&b, at(2026, 11, 14, 9, 0, 0), 0);
    }

    #[test]
    fn daily_tokens_use_corrected_count() {
        let b = Bucket::new(Window {
            daily_tokens: 1_000,
            ..Window::default()
        });
        let t0 = at(2026, 11, 14, 12, 0, 0);
        grant(&b, t0, 600).complete(100);
        grant(&b, plus(t0, 1), 900);
        assert_eq!(closed(&b, plus(t0, 2), 1), at(2026, 11, 15, 0, 0, 0));
    }

    #[test]
    fn monthly_window_rolls_on_the_first() {
        let b = Bucket::new(Window {
            monthly_requests: 1,
            ..Window::default()
        });
        grant(&b, at(2026, 11, 30, 23, 0, 0), 0);
        assert_eq!(
            closed(&b, at(2026, 11, 30, 23, 30, 0), 0),
            at(2026, 12, 1, 0, 0, 0)
        );
        grant(&b, at(2026, 12, 1, 0, 0, 0), 0);
    }

    #[test]
    fn monthly_window_handles_leap_february() {
        let b = Bucket::new(Window {
            monthly_requests: 1,
            ..Window::default()
        });
        grant(&b, at(2028, 2, 29, 12, 0, 0), 0);
        assert_eq!(
            closed(&b, at(2028, 2, 29, 13, 0, 0), 0),
            at(2028, 3, 1, 0, 0, 0)
        );
    }

    #[test]
    fn rate_limit_lowers_rpm_to_eighty_percent() {
        let b = rpm(20);
        let t0 = at(2026, 11, 14, 12, 0, 0);
        for _ in 0..10 {
            grant(&b, t0, 0);
        }
        b.note_rate_limited(t0, None);
        assert_eq!(wait(&b, t0, 0), MINUTE);
        let t1 = plus(t0, 60);
        for _ in 0..8 {
            grant(&b, t1, 0);
        }
        assert_eq!(wait(&b, t1, 0), MINUTE);
    }

    #[test]
    fn rate_limit_respects_floor() {
        let b = Bucket::new(Window {
            rpm: 20,
            min_rpm: 5,
            ..Window::default()
        });
        let t0 = at(2026, 11, 14, 12, 0, 0);
        grant(&b, t0, 0);
        grant(&b, t0, 0);
        b.note_rate_limited(t0, None);
        let t1 = plus(t0, 60);
        for _ in 0..5 {
            grant(&b, t1, 0);
        }
        assert_eq!(wait(&b, t1, 0), MINUTE);
    }

    #[test]
    fn rate_limit_pause_is_capped_at_a_day() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let b = Bucket::new(Window::default());
        b.note_rate_limited(t0, Some(Duration::from_secs(30)));
        assert_eq!(wait(&b, t0, 0), Duration::from_secs(30));

        let b = Bucket::new(Window::default());
        b.note_rate_limited(t0, Some(Duration::from_secs(90_000)));
        assert_eq!(wait(&b, t0, 0), Duration::from_secs(24 * 3_600));
    }

    #[test]
    fn success_raises_slowly_and_never_above_ceiling() {
        let b = rpm(10);
        let t0 = at(2026, 11, 14, 12, 0, 0);
        for _ in 0..10 {
            grant(&b, t0, 0);
        }
        b.note_rate_limited(t0, None);
        b.note_success(plus(t0, 30));

        let t1 = plus(t0, 60);
        b.note_success(t1);
        for _ in 0..9 {
            grant(&b, t1, 0);
        }
        assert_eq!(wait(&b, t1, 0), MINUTE);

        let t2 = plus(t0, 120);
        b.note_success(t2);
        for _ in 0..10 {
            grant(&b, t2, 0);
        }
        assert_eq!(wait(&b, t2, 0), MINUTE);

        let t3 = plus(t0, 300);
        b.note_success(t3);
        for _ in 0..10 {
            grant(&b, t3, 0);
        }
        assert_eq!(wait(&b, t3, 0), MINUTE);
    }

    #[test]
    fn lower_to_halves_at_most_per_response() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let b = rpm(100);
        b.lower_to(t0, 10);
        for _ in 0..50 {
            grant(&b, t0, 0);
        }
        assert_eq!(wait(&b, t0, 0), MINUTE);

        let b = rpm(100);
        b.lower_to(t0, 10);
        b.lower_to(t0, 10);
        for _ in 0..25 {
            grant(&b, t0, 0);
        }
        assert_eq!(wait(&b, t0, 0), MINUTE);

        let b = rpm(100);
        b.lower_to(t0, 10);
        b.lower_to(t0, 60);
        for _ in 0..50 {
            grant(&b, t0, 0);
        }
        assert_eq!(wait(&b, t0, 0), MINUTE);
    }

    #[test]
    fn lower_to_applies_when_unlimited() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let b = Bucket::new(Window::default());
        b.lower_to(t0, 7);
        for _ in 0..7 {
            grant(&b, t0, 0);
        }
        assert_eq!(wait(&b, t0, 0), MINUTE);
    }

    #[test]
    fn zero_limits_are_unlimited() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let b = Bucket::new(Window::default());
        for _ in 0..500 {
            grant(&b, t0, 1_000_000);
        }
    }

    #[test]
    fn window_closed_until_given_instant() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let b = Bucket::new(Window::default());
        b.note_window_closed(t0, plus(t0, 1_800));
        assert_eq!(closed(&b, t0, 0), plus(t0, 1_800));
        grant(&b, plus(t0, 1_800), 0);

        let b = Bucket::new(Window::default());
        b.note_window_closed(t0, t0);
        grant(&b, t0, 0);
    }

    #[test]
    fn snapshot_restores_learned_cap() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let b = rpm(20);
        for _ in 0..10 {
            grant(&b, t0, 0);
        }
        b.note_rate_limited(t0, None);
        let saved = b.snapshot(t0);
        assert_eq!(saved.learned_rpm, Some(8));

        let restored = rpm(20);
        restored.restore(saved, t0);
        assert_eq!(restored.snapshot(t0), saved);
        let t1 = plus(t0, 60);
        for _ in 0..8 {
            grant(&restored, t1, 0);
        }
        assert_eq!(wait(&restored, t1, 0), MINUTE);
    }

    #[test]
    fn snapshot_restores_a_pause() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let b = rpm(10);
        b.note_rate_limited(t0, Some(Duration::from_secs(30)));
        let saved = b.snapshot(t0);
        assert_eq!(saved.pause, Some(Duration::from_secs(30)));

        let restored = rpm(10);
        restored.restore(saved, t0);
        assert_eq!(wait(&restored, t0, 0), Duration::from_secs(30));
        assert_eq!(restored.snapshot(t0), saved);
    }

    #[test]
    fn snapshot_restores_a_closed_daily_window() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let b = Bucket::new(Window {
            daily_requests: 2,
            ..Window::default()
        });
        grant(&b, t0, 0);
        grant(&b, plus(t0, 1), 0);
        let saved = b.snapshot(plus(t0, 2));
        assert_eq!(saved.daily.unwrap().requests, 2);

        let restored = Bucket::new(Window {
            daily_requests: 2,
            ..Window::default()
        });
        restored.restore(saved, plus(t0, 2));
        assert_eq!(closed(&restored, plus(t0, 2), 0), at(2026, 11, 15, 0, 0, 0));
    }

    #[test]
    fn a_restart_in_a_later_window_starts_the_counters_at_zero() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let window = Window {
            daily_requests: 1,
            monthly_requests: 1,
            ..Window::default()
        };
        let b = Bucket::new(window.clone());
        grant(&b, t0, 0);
        let saved = b.snapshot(t0);
        assert!(saved.daily.is_some() && saved.monthly.is_some());

        let later = at(2026, 12, 2, 12, 0, 0);
        let restored = Bucket::new(window);
        restored.restore(saved, later);
        let next = restored.snapshot(later);
        assert_eq!((next.daily, next.monthly), (None, None));
        grant(&restored, later, 0);
    }

    #[test]
    fn hostile_snapshot_values_are_clamped() {
        let t0 = at(2026, 11, 14, 12, 0, 0);
        let window = Window {
            rpm: 10,
            daily_requests: 2,
            daily_tokens: 100,
            monthly_requests: 3,
            monthly_tokens: 200,
            ..Window::default()
        };
        let reset = reset_secs(&window);
        let (day_start, _) = Period::Day.bounds(secs(t0), reset);
        let (month_start, _) = Period::Month.bounds(secs(t0), reset);
        let hostile = BucketSnapshot {
            learned_rpm: Some(u32::MAX),
            pause: Some(Duration::from_secs(90 * 86_400)),
            closed: Some(Duration::from_secs(90 * 86_400)),
            daily: Some(QuotaSnapshot {
                start: day_start,
                requests: u32::MAX,
                tokens: u64::MAX,
            }),
            monthly: Some(QuotaSnapshot {
                start: month_start,
                requests: u32::MAX,
                tokens: u64::MAX,
            }),
        };

        let b = Bucket::new(window);
        b.restore(hostile, t0);
        let saved = b.snapshot(t0);
        // A cap at the ceiling is stored as no learned cap.
        assert_eq!(saved.learned_rpm, None);
        assert_eq!(saved.pause, Some(MAX_PAUSE));
        assert_eq!(saved.closed, Some(MAX_RESTORED_STATE));
        assert_eq!(
            saved.daily,
            Some(QuotaSnapshot {
                start: day_start,
                requests: 2,
                tokens: 100,
            })
        );
        assert_eq!(
            saved.monthly,
            Some(QuotaSnapshot {
                start: month_start,
                requests: 3,
                tokens: 200,
            })
        );
    }

    #[test]
    fn civil_helpers_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2026, 11, 14), (2028, 12, 31)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
    }
}
