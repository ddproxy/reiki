//! Host stats for `/v1/health` behind `expose_host_stats`: load, cores, memory and disk use of
//! the filesystems Reiki writes to, labelled by role and never by path.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

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

/// One entry per filesystem behind the given `(role, directory)` pairs, roles merged.
fn disks(dirs: &[(&'static str, PathBuf)]) -> Vec<Value> {
    let mut seen: Vec<(Usage, Vec<&'static str>)> = Vec::new();
    for (role, dir) in dirs {
        let Some(found) = usage(dir) else {
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
    seen.into_iter()
        .map(|(u, roles)| {
            json!({
                "roles": roles,
                "total_bytes": u.total,
                "available_bytes": u.available,
                "used_percent": used_percent(u.used, u.available),
            })
        })
        .collect()
}

/// The `host` object of `/v1/health`; a figure the platform cannot give is `null`.
pub(crate) fn stats(dirs: &[(&'static str, PathBuf)]) -> Value {
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
        "disks": disks(dirs),
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
    #[test]
    fn roles_on_one_filesystem_share_an_entry_and_paths_never_appear() {
        let dir = std::env::temp_dir();
        let out = disks(&[
            ("config", dir.clone()),
            ("state", dir.clone()),
            ("log", PathBuf::from("/nonexistent/reiki/dir")),
        ]);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0]["roles"], json!(["config", "state"]));
        assert!(out[0]["total_bytes"].as_u64().unwrap() > 0);
        assert!(!Value::Array(out)
            .to_string()
            .contains(&*dir.to_string_lossy()));
    }
}
