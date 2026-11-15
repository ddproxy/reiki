//! The process lifecycle: the stop request that every listener waits on, the signals that make
//! it, and a check of the open-file limit.
//!
//! SIGTERM and SIGINT start a clean stop: listeners stop accepting, requests in flight get the
//! drain time, then state and logs are flushed and the process exits 0. A second signal exits at
//! once. SIGHUP is the reload and lives in `main`.

use std::sync::OnceLock;

use tokio::sync::watch;

/// A stop request that any number of waiters can see. The process has one (see [`request_stop`]);
/// tests make their own so one test's stop does not end another's server.
pub struct StopFlag(watch::Sender<bool>);

impl StopFlag {
    pub fn new() -> Self {
        Self(watch::channel(false).0)
    }

    pub fn request(&self) {
        self.0.send_replace(true);
    }

    pub fn requested(&self) -> bool {
        *self.0.borrow()
    }

    /// Completes once a stop is requested; completes at once if it already was.
    pub async fn wait(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|requested| *requested).await;
    }
}

impl Default for StopFlag {
    fn default() -> Self {
        Self::new()
    }
}

fn process_stop() -> &'static StopFlag {
    static STOP: OnceLock<StopFlag> = OnceLock::new();
    STOP.get_or_init(StopFlag::new)
}

/// Asks every listener and background task to wind down.
pub fn request_stop() {
    process_stop().request();
}

/// Whether a stop has been requested.
pub fn stop_requested() -> bool {
    process_stop().requested()
}

/// Completes once a stop is requested; completes at once if it already was.
pub async fn stopped() {
    process_stop().wait().await;
}

/// Turns SIGTERM and SIGINT into a stop request. A second one of either exits at once with the
/// shell's `128 + signal` status, so an operator is never stuck behind a long drain.
#[cfg(unix)]
pub fn spawn_signal_handler() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::warn!("cannot install the stop signal handlers");
        return;
    };
    tokio::spawn(async move {
        let first = tokio::select! {
            _ = term.recv() => 15,
            _ = int.recv() => 2,
        };
        tracing::info!(signal = first, "stop requested; draining");
        crate::notify::stopping();
        request_stop();
        let second = tokio::select! {
            _ = term.recv() => 15,
            _ = int.recv() => 2,
        };
        tracing::warn!(signal = second, "second stop signal; exiting now");
        std::process::exit(128 + second);
    });
}

#[cfg(not(unix))]
pub fn spawn_signal_handler() {
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("stop requested; draining");
        request_stop();
        let _ = tokio::signal::ctrl_c().await;
        std::process::exit(130);
    });
}

/// The soft limit on open files, from `/proc/self/limits`. `None` where that file is absent.
pub fn open_files_soft_limit() -> Option<u64> {
    parse_open_files(&std::fs::read_to_string("/proc/self/limits").ok()?)
}

pub(crate) fn parse_open_files(text: &str) -> Option<u64> {
    let rest = text
        .lines()
        .find_map(|line| line.strip_prefix("Max open files"))?;
    let soft = rest.split_whitespace().next()?;
    if soft == "unlimited" {
        Some(u64::MAX)
    } else {
        soft.parse().ok()
    }
}

/// Smallest soft limit that leaves room for the listener caps plus upstream connections.
pub const RECOMMENDED_OPEN_FILES: u64 = 4096;

/// Logs a warning when the soft open-file limit is below [`RECOMMENDED_OPEN_FILES`]. systemd's
/// default of 1024 equals the client listener's own connection cap.
pub fn warn_if_open_files_low() {
    if let Some(limit) = open_files_soft_limit() {
        if limit < RECOMMENDED_OPEN_FILES {
            tracing::warn!(
                limit,
                recommended = RECOMMENDED_OPEN_FILES,
                "the open-file limit is low for the connection caps; raise `LimitNOFILE` in the unit or `ulimit -n`"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_open_file_limit_is_read_from_the_proc_table() {
        let text = "Limit                     Soft Limit           Hard Limit           Units\n\
                    Max cpu time              unlimited            unlimited            seconds\n\
                    Max open files            1024                 524288               files\n";
        assert_eq!(parse_open_files(text), Some(1024));
        let unlimited =
            "Max open files            unlimited            unlimited            files\n";
        assert_eq!(parse_open_files(unlimited), Some(u64::MAX));
        assert_eq!(parse_open_files("nothing here\n"), None);
        assert_eq!(parse_open_files("Max open files            lots\n"), None);
    }

    #[tokio::test]
    async fn a_stop_request_wakes_every_waiter_and_stays_requested() {
        let flag = std::sync::Arc::new(StopFlag::new());
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let flag = std::sync::Arc::clone(&flag);
                tokio::spawn(async move { flag.wait().await })
            })
            .collect();
        tokio::task::yield_now().await;
        assert!(!flag.requested());
        flag.request();
        for waiter in waiters {
            tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
                .await
                .unwrap()
                .unwrap();
        }
        assert!(flag.requested());
        // Late waiters return at once.
        tokio::time::timeout(std::time::Duration::from_millis(200), flag.wait())
            .await
            .unwrap();
    }
}
