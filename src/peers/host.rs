//! Host stats for `/v1/health` behind `expose_host_stats`: load, cores, memory and disk use of
//! the filesystems Reiki writes to, labelled by role and never by path.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{json, Value};

/// How long the filesystem queries of one answer may take before their roles read as unavailable.
const DISK_TIMEOUT: Duration = Duration::from_secs(2);

/// A filesystem query: `statvfs` in production, a stand-in in tests.
type Probe = fn(&Path) -> Option<Usage>;

/// Directories whose query has not returned. A hung mount gets no second blocking thread until
/// the first one comes back, so repeated answers cannot pile threads up behind it.
static IN_FLIGHT: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// The 1, 5 and 15 minute load averages from `/proc/loadavg` text.
fn parse_loadavg(text: &str) -> Option<[f64; 3]> {
    let mut fields = text.split_whitespace().map(str::parse::<f64>);
    let one = fields.next()?.ok()?;
    let five = fields.next()?.ok()?;
    let fifteen = fields.next()?.ok()?;
    Some([one, five, fifteen])
}

/// Memory and swap in bytes from `/proc/meminfo` text, whose values are in KiB.
fn parse_meminfo(text: &str) -> Option<Value> {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            let kib = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            Some(kib.saturating_mul(1024))
        })
    };
    Some(json!({
        "total_bytes": field("MemTotal")?,
        "available_bytes": field("MemAvailable")?,
        "swap_total_bytes": field("SwapTotal")?,
        "swap_free_bytes": field("SwapFree")?,
    }))
}

#[derive(Clone)]
struct Usage {
    fsid: u64,
    total: u64,
    available: u64,
    used: u64,
}

#[cfg(unix)]
fn usage(dir: &Path) -> Option<Usage> {
    let vfs = rustix::fs::statvfs(dir).ok()?;
    let unit = if vfs.f_frsize > 0 {
        vfs.f_frsize
    } else {
        vfs.f_bsize
    };
    Some(Usage {
        fsid: vfs.f_fsid,
        total: vfs.f_blocks.saturating_mul(unit),
        available: vfs.f_bavail.saturating_mul(unit),
        used: vfs
            .f_blocks
            .saturating_sub(vfs.f_bfree)
            .saturating_mul(unit),
    })
}

#[cfg(not(unix))]
fn usage(_dir: &Path) -> Option<Usage> {
    None
}

/// Percent of the space a non-root user can reach that is in use, as `df` reports it.
fn used_percent(used: u64, available: u64) -> f64 {
    let reachable = used.saturating_add(available);
    if reachable == 0 {
        return 0.0;
    }
    let tenths = (used as f64 * 1000.0 / reachable as f64).round();
    tenths / 10.0
}

/// Runs `probe` for each directory on the blocking pool, all at once, and waits at most
/// `timeout` for them together; `None` for a directory that failed, timed out or is still stuck.
async fn probe_all(
    dirs: &[(&'static str, PathBuf)],
    probe: Probe,
    timeout: Duration,
) -> Vec<Option<Usage>> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut unique: Vec<&PathBuf> = Vec::new();
    for (_, dir) in dirs {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    let mut pending = Vec::new();
    for dir in &unique {
        let started = {
            let mut in_flight = IN_FLIGHT.lock().unwrap_or_else(PoisonError::into_inner);
            in_flight
                .get_or_insert_with(HashSet::new)
                .insert((*dir).clone())
        };
        if !started {
            pending.push(None);
            continue;
        }
        let dir = (*dir).clone();
        pending.push(Some(tokio::task::spawn_blocking(move || {
            let found = probe(&dir);
            if let Some(in_flight) = IN_FLIGHT
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
            {
                in_flight.remove(&dir);
            }
            found
        })));
    }
    let mut found = Vec::new();
    for task in pending {
        found.push(match task {
            Some(task) => tokio::time::timeout_at(deadline, task)
                .await
                .ok()
                .and_then(Result::ok)
                .flatten(),
            None => None,
        });
    }
    dirs.iter()
        .map(|(_, dir)| {
            let index = unique.iter().position(|d| *d == dir)?;
            found[index].clone()
        })
        .collect()
}

/// One entry per filesystem behind the given `(role, directory)` pairs, roles merged; a role
/// whose filesystem could not be queried gets an entry of its own with `null` figures.
async fn disks(dirs: &[(&'static str, PathBuf)], probe: Probe, timeout: Duration) -> Vec<Value> {
    let found = probe_all(dirs, probe, timeout).await;
    let mut seen: Vec<(Usage, Vec<&'static str>)> = Vec::new();
    let mut unavailable: Vec<&'static str> = Vec::new();
    for ((role, _), found) in dirs.iter().zip(found) {
        let Some(found) = found else {
            if !unavailable.contains(role) {
                unavailable.push(role);
            }
            continue;
        };
        match seen
            .iter_mut()
            .find(|(u, _)| u.fsid == found.fsid && u.total == found.total)
        {
            Some((_, roles)) => {
                if !roles.contains(role) {
                    roles.push(role);
                }
            }
            None => seen.push((found, vec![role])),
        }
    }
    let mut out: Vec<Value> = seen
        .into_iter()
        .map(|(u, roles)| {
            json!({
                "roles": roles,
                "total_bytes": u.total,
                "available_bytes": u.available,
                "used_percent": used_percent(u.used, u.available),
            })
        })
        .collect();
    out.extend(unavailable.into_iter().map(|role| {
        json!({"roles": [role], "total_bytes": null, "available_bytes": null, "used_percent": null})
    }));
    out
}

/// The `host` object of `/v1/health`; a figure the platform cannot give is `null`.
pub(crate) async fn stats(dirs: &[(&'static str, PathBuf)]) -> Value {
    let load = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .as_deref()
        .and_then(parse_loadavg);
    let memory = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .as_deref()
        .and_then(parse_meminfo);
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    json!({
        "load_average": load,
        "cores": cores,
        "memory": memory,
        "disks": disks(dirs, usage, DISK_TIMEOUT).await,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loadavg_and_meminfo_parse() {
        assert_eq!(
            parse_loadavg("0.52 0.58 0.59 1/1234 5678\n"),
            Some([0.52, 0.58, 0.59])
        );
        assert_eq!(parse_loadavg("garbage"), None);
        assert_eq!(parse_loadavg(""), None);
        let meminfo = "MemTotal:       16000000 kB\nMemFree:  1 kB\nMemAvailable:    8000000 kB\n\
                       SwapCached: 0 kB\nSwapTotal:       2000000 kB\nSwapFree:        1000000 kB\n";
        assert_eq!(
            parse_meminfo(meminfo).unwrap(),
            json!({
                "total_bytes": 16_000_000u64 * 1024,
                "available_bytes": 8_000_000u64 * 1024,
                "swap_total_bytes": 2_000_000u64 * 1024,
                "swap_free_bytes": 1_000_000u64 * 1024,
            })
        );
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None, "all four or none");
    }

    #[test]
    fn used_percent_matches_df() {
        assert_eq!(used_percent(0, 0), 0.0);
        assert_eq!(used_percent(1, 3), 25.0);
        assert_eq!(used_percent(2, 1), 66.7);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn roles_on_one_filesystem_share_an_entry_and_paths_never_appear() {
        let dir = std::env::temp_dir();
        let out = disks(
            &[
                ("config", dir.clone()),
                ("state", dir.clone()),
                ("log", PathBuf::from("/nonexistent/reiki/dir")),
            ],
            usage,
            DISK_TIMEOUT,
        )
        .await;
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0]["roles"], json!(["config", "state"]));
        assert!(out[0]["total_bytes"].as_u64().unwrap() > 0);
        assert_eq!(
            out[1],
            json!({"roles": ["log"], "total_bytes": null, "available_bytes": null, "used_percent": null}),
            "a directory that cannot be queried is unavailable"
        );
        assert!(!Value::Array(out)
            .to_string()
            .contains(&*dir.to_string_lossy()));
    }

    fn slow(dir: &Path) -> Option<Usage> {
        if dir.ends_with("hung") {
            std::thread::sleep(Duration::from_millis(1_500));
        }
        Some(Usage {
            fsid: 7,
            total: 100,
            available: 40,
            used: 60,
        })
    }

    #[tokio::test]
    async fn a_hung_filesystem_reads_as_unavailable_and_is_not_queried_twice() {
        let dirs = [
            ("state", PathBuf::from("/probe-test/ok")),
            ("log", PathBuf::from("/probe-test/hung")),
        ];
        let started = std::time::Instant::now();
        let out = disks(&dirs, slow, Duration::from_millis(200)).await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "it did not wait for the hung one"
        );
        assert_eq!(out[0]["roles"], json!(["state"]));
        assert_eq!(out[0]["used_percent"], 60.0);
        assert_eq!(out[1]["roles"], json!(["log"]));
        assert!(out[1]["total_bytes"].is_null());
        let again = probe_all(&dirs[1..], slow, Duration::from_secs(5)).await;
        assert!(again[0].is_none(), "still stuck, so not started again");
        tokio::time::sleep(Duration::from_millis(1_600)).await;
        let later = probe_all(&dirs[1..], slow, Duration::from_secs(5)).await;
        assert!(
            later[0].is_some(),
            "queried again once the first call returned"
        );
    }

    #[tokio::test]
    async fn host_stats_run_off_the_runtime_thread() {
        // A current-thread runtime: an inline blocking query would stall this timer too.
        let ticks = tokio::spawn(async {
            let mut n = 0;
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                n += 1;
            }
            n
        });
        let dirs = [("config", PathBuf::from("/probe-test/stats/hung"))];
        let out = disks(&dirs, slow, Duration::from_millis(400)).await;
        assert!(out[0]["total_bytes"].is_null());
        assert_eq!(ticks.await.unwrap(), 5);
    }
}
