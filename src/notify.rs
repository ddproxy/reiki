//! systemd readiness and watchdog messages over `$NOTIFY_SOCKET` (the `sd_notify` protocol), with
//! the standard library only. Every call does nothing, and says so, when Reiki is not started by
//! a manager that offers the socket.

use std::time::Duration;

/// Sends `state` (for example `READY=1`) to the socket named by `NOTIFY_SOCKET`. Returns whether
/// a message went out.
pub fn notify(state: &str) -> bool {
    std::env::var_os("NOTIFY_SOCKET").is_some_and(|target| send(&target.to_string_lossy(), state))
}

/// Tells the manager the service is up and serving.
pub fn ready(status: &str) -> bool {
    notify(&format!("READY=1\nSTATUS={}", one_line(status)))
}

/// Tells the manager a clean stop has begun, so it does not read the drain as a hang.
pub fn stopping() -> bool {
    notify("STOPPING=1\nSTATUS=draining")
}

/// A status text is one line; anything else would inject further assignments into the message.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(unix)]
pub(crate) fn send(target: &str, state: &str) -> bool {
    use std::os::unix::net::UnixDatagram;
    let Ok(socket) = UnixDatagram::unbound() else {
        return false;
    };
    let sent = if let Some(name) = target.strip_prefix('@') {
        abstract_send(&socket, name, state)
    } else if target.starts_with('/') {
        socket.send_to(state.as_bytes(), target).map(|_| ())
    } else {
        return false;
    };
    sent.is_ok()
}

#[cfg(all(unix, target_os = "linux"))]
fn abstract_send(
    socket: &std::os::unix::net::UnixDatagram,
    name: &str,
    state: &str,
) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
    socket.send_to_addr(state.as_bytes(), &addr).map(|_| ())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn abstract_send(
    _socket: &std::os::unix::net::UnixDatagram,
    _name: &str,
    _state: &str,
) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(not(unix))]
pub(crate) fn send(_target: &str, _state: &str) -> bool {
    false
}

/// The watchdog period the manager asks for, from `WATCHDOG_USEC` (and `WATCHDOG_PID`, when set,
/// has to be this process).
pub fn watchdog_interval() -> Option<Duration> {
    parse_watchdog(
        std::env::var("WATCHDOG_USEC").ok().as_deref(),
        std::env::var("WATCHDOG_PID").ok().as_deref(),
        std::process::id(),
    )
}

pub(crate) fn parse_watchdog(usec: Option<&str>, pid: Option<&str>, own: u32) -> Option<Duration> {
    let usec: u64 = usec?.trim().parse().ok()?;
    if let Some(pid) = pid {
        if pid.trim().parse::<u32>().ok()? != own {
            return None;
        }
    }
    (usec > 0).then(|| Duration::from_micros(usec))
}

/// Pings the watchdog while the async runtime still runs tasks. A task stamps a counter on the
/// runtime and a plain thread sends the ping only while the stamp is fresh, so a wedged runtime
/// stops the pings and the manager restarts the service.
pub fn spawn_watchdog() {
    let Some(interval) = watchdog_interval() else {
        return;
    };
    let beat = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let started = std::time::Instant::now();
    let stamp = std::sync::Arc::clone(&beat);
    let tick = (interval / 4).max(Duration::from_millis(10));
    tokio::spawn(async move {
        loop {
            let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            stamp.store(ms.max(1), std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(tick).await;
        }
    });
    let spawned = std::thread::Builder::new()
        .name("reiki-watchdog".to_owned())
        .spawn(move || loop {
            std::thread::sleep(interval / 2);
            let last = beat.load(std::sync::atomic::Ordering::Relaxed);
            let now = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let fresh = last != 0
                && now.saturating_sub(last)
                    < u64::try_from(interval.as_millis() / 2).unwrap_or(u64::MAX);
            if fresh {
                notify("WATCHDOG=1");
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "cannot start the watchdog thread");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    fn listener(label: &str) -> (UnixDatagram, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("reiki-notify-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notify.sock");
        let socket = UnixDatagram::bind(&path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        (socket, path)
    }

    fn receive(socket: &UnixDatagram) -> String {
        let mut buf = [0u8; 256];
        let n = socket.recv(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    #[test]
    fn a_message_reaches_a_path_socket() {
        let (socket, path) = listener("path");
        assert!(send(path.to_str().unwrap(), "READY=1"));
        assert_eq!(receive(&socket), "READY=1");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_message_reaches_an_abstract_socket() {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("reiki-notify-abstract-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let socket = UnixDatagram::bind_addr(&addr).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert!(send(&format!("@{name}"), "STOPPING=1"));
        assert_eq!(receive(&socket), "STOPPING=1");
    }

    #[test]
    fn a_target_that_is_neither_a_path_nor_abstract_sends_nothing() {
        assert!(!send("relative/path", "READY=1"));
        assert!(!send("", "READY=1"));
        assert!(!send("/no/such/socket/anywhere", "READY=1"));
    }

    #[test]
    fn a_status_text_cannot_add_assignments() {
        let (socket, path) = listener("status");
        // Same path as `ready`, minus the environment lookup.
        let message = format!(
            "READY=1\nSTATUS={}",
            one_line("ok\nWATCHDOG=1\r\nMAINPID=1")
        );
        assert!(send(path.to_str().unwrap(), &message));
        let text = receive(&socket);
        assert_eq!(text.lines().count(), 2, "{text:?}");
    }

    #[test]
    fn the_watchdog_period_comes_from_the_environment_values() {
        assert_eq!(
            parse_watchdog(Some("30000000"), None, 7),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_watchdog(Some("30000000"), Some("7"), 7),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_watchdog(Some("30000000"), Some("8"), 7),
            None,
            "another process's"
        );
        assert_eq!(parse_watchdog(Some("0"), None, 7), None);
        assert_eq!(parse_watchdog(Some("soon"), None, 7), None);
        assert_eq!(parse_watchdog(None, None, 7), None);
    }
}
