//! Active sessions per stream: distinct client session ids seen inside a sliding window, kept
//! only as salted hashes in a bounded table.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Ids tracked per stream; past this the least recently seen id is dropped.
const MAX_PER_STREAM: usize = 4096;
/// Streams tracked at once, so a reload that renames streams cannot grow the table without end.
const MAX_STREAMS: usize = 256;
/// Longer values are not session ids worth counting.
const MAX_ID_LEN: usize = 256;

pub(crate) struct Sessions {
    /// Salts the id hash per process, so the table holds nothing that matches an id elsewhere.
    salt: [u8; 32],
    seen: Mutex<HashMap<String, HashMap<u64, Instant>>>,
}

impl Sessions {
    pub(crate) fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(std::process::id().to_le_bytes());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        hasher.update(now.as_nanos().to_le_bytes());
        hasher.update(format!("{:p}", &hasher).as_bytes());
        Self {
            salt: hasher.finalize().into(),
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn hash(&self, stream: &str, id: &str) -> u64 {
        let digest = Sha256::new()
            .chain_update(self.salt)
            .chain_update(stream.as_bytes())
            .chain_update([0])
            .chain_update(id.as_bytes())
            .finalize();
        let mut first = [0u8; 8];
        first.copy_from_slice(&digest[..8]);
        u64::from_le_bytes(first)
    }

    /// Records one client request on `stream` carrying session `id`; repeats only refresh it.
    pub(crate) fn observe(&self, stream: &str, id: &str, window: Duration, now: Instant) {
        let id = id.trim();
        if id.is_empty() || id.len() > MAX_ID_LEN {
            return;
        }
        let hash = self.hash(stream, id);
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if !seen.contains_key(stream) && seen.len() >= MAX_STREAMS {
            seen.retain(|_, ids| {
                ids.retain(|_, at| now.saturating_duration_since(*at) < window);
                !ids.is_empty()
            });
            if seen.len() >= MAX_STREAMS {
                return;
            }
        }
        let ids = seen.entry(stream.to_owned()).or_default();
        if !ids.contains_key(&hash) && ids.len() >= MAX_PER_STREAM {
            ids.retain(|_, at| now.saturating_duration_since(*at) < window);
            if ids.len() >= MAX_PER_STREAM {
                if let Some(oldest) = ids.iter().min_by_key(|(_, at)| **at).map(|(h, _)| *h) {
                    ids.remove(&oldest);
                }
            }
        }
        ids.insert(hash, now);
    }

    /// Active session counts per stream, in name order, dropping ids that left the window.
    pub(crate) fn counts(&self, window: Duration, now: Instant) -> BTreeMap<String, usize> {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.retain(|_, ids| {
            ids.retain(|_, at| now.saturating_duration_since(*at) < window);
            !ids.is_empty()
        });
        seen.iter()
            .map(|(stream, ids)| (stream.clone(), ids.len()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(300);

    #[test]
    fn repeats_count_once_and_ids_expire_after_the_window() {
        let sessions = Sessions::new();
        let t0 = Instant::now();
        for _ in 0..5 {
            sessions.observe("ct", "ses_1", WINDOW, t0);
        }
        sessions.observe("ct", "ses_2", WINDOW, t0 + Duration::from_secs(100));
        sessions.observe("other", "ses_1", WINDOW, t0);
        let counts = sessions.counts(WINDOW, t0 + Duration::from_secs(100));
        assert_eq!(counts["ct"], 2);
        assert_eq!(counts["other"], 1);
        let later = sessions.counts(WINDOW, t0 + Duration::from_secs(350));
        assert_eq!(later.get("ct"), Some(&1), "ses_1 left the window");
        assert_eq!(later.get("other"), None);
    }

    #[test]
    fn a_repeat_refreshes_the_window() {
        let sessions = Sessions::new();
        let t0 = Instant::now();
        sessions.observe("ct", "ses_1", WINDOW, t0);
        sessions.observe("ct", "ses_1", WINDOW, t0 + Duration::from_secs(250));
        assert_eq!(
            sessions.counts(WINDOW, t0 + Duration::from_secs(400))["ct"],
            1
        );
    }

    #[test]
    fn empty_and_oversized_ids_are_ignored() {
        let sessions = Sessions::new();
        let t0 = Instant::now();
        sessions.observe("ct", "  ", WINDOW, t0);
        sessions.observe("ct", &"x".repeat(MAX_ID_LEN + 1), WINDOW, t0);
        assert!(sessions.counts(WINDOW, t0).is_empty());
    }

    #[test]
    fn the_table_stays_bounded() {
        let sessions = Sessions::new();
        let t0 = Instant::now();
        for n in 0..MAX_PER_STREAM + 10 {
            let at = t0 + Duration::from_millis(n as u64);
            sessions.observe("ct", &format!("ses_{n}"), WINDOW, at);
        }
        let now = t0 + Duration::from_secs(10);
        assert_eq!(sessions.counts(WINDOW, now)["ct"], MAX_PER_STREAM);
        for n in 0..MAX_STREAMS + 5 {
            sessions.observe(&format!("s{n}"), "id", WINDOW, now);
        }
        assert!(sessions.counts(WINDOW, now).len() <= MAX_STREAMS);
    }

    #[test]
    fn the_table_holds_hashes_not_ids() {
        let sessions = Sessions::new();
        sessions.observe("ct", "ses_secret_value", WINDOW, Instant::now());
        let seen = sessions.seen.lock().unwrap();
        let text = format!("{seen:?}");
        assert!(!text.contains("ses_secret_value"), "{text}");
    }
}
