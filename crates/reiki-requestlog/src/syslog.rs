//! Sends each record to a syslog collector as an RFC 5424 message.
//!
//! The writer thread owns the connection. A record that cannot be sent is reported to the
//! caller, which counts it as dropped. A lost connection is retried with a doubling delay.

use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
#[cfg(unix)]
use std::os::unix::net::UnixDatagram;
#[cfg(unix)]
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crate::config::SyslogTarget;
use crate::timefmt::rfc3339_millis;

/// RFC 5424 facility names and codes.
const FACILITIES: &[(&str, u8)] = &[
    ("kern", 0),
    ("user", 1),
    ("mail", 2),
    ("daemon", 3),
    ("auth", 4),
    ("syslog", 5),
    ("lpr", 6),
    ("news", 7),
    ("uucp", 8),
    ("cron", 9),
    ("authpriv", 10),
    ("ftp", 11),
    ("ntp", 12),
    ("audit", 13),
    ("alert", 14),
    ("clock", 15),
    ("local0", 16),
    ("local1", 17),
    ("local2", 18),
    ("local3", 19),
    ("local4", 20),
    ("local5", 21),
    ("local6", 22),
    ("local7", 23),
];

pub const SEVERITY_WARNING: u8 = 4;
pub const SEVERITY_INFO: u8 = 6;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Bounds a stalled collector, so a full socket buffer holds the writer for at most this long.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const RETRY_INITIAL: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(30);

pub fn facility_code(name: &str) -> Option<u8> {
    FACILITIES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, code)| *code)
}

/// What the header needs from a record: its MSGID, severity and time.
pub struct Meta {
    pub msgid: &'static str,
    pub severity: u8,
    pub at: SystemTime,
}

/// `<PRI>1 TIMESTAMP HOSTNAME APP-NAME PROCID MSGID - MSG`, with no structured data.
pub fn message(
    facility: u8,
    meta: &Meta,
    hostname: &str,
    app_name: &str,
    procid: u32,
    body: &str,
) -> String {
    let pri = u32::from(facility) * 8 + u32::from(meta.severity);
    format!(
        "<{pri}>1 {} {hostname} {app_name} {procid} {} - {body}",
        rfc3339_millis(meta.at),
        meta.msgid
    )
}

/// The machine's host name, or `-` when it is unknown or not a valid RFC 5424 HOSTNAME.
pub fn hostname() -> String {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .unwrap_or_default();
    let name = raw.trim();
    if name.is_empty() || name.len() > 255 || !name.bytes().all(|b| b.is_ascii_graphic()) {
        "-".to_owned()
    } else {
        name.to_owned()
    }
}

enum Link {
    /// Unconnected, so a later send does not report an earlier unreachable peer.
    Udp {
        socket: UdpSocket,
        addr: SocketAddr,
    },
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixDatagram),
}

pub struct Syslog {
    target: SyslogTarget,
    facility: u8,
    app_name: String,
    hostname: String,
    procid: u32,
    link: Option<Link>,
    /// Keeps the first failure and the recovery from being logged for every record.
    failing: bool,
    retry_at: Option<Instant>,
    delay: Duration,
    retry_initial: Duration,
    retry_max: Duration,
}

impl Syslog {
    pub fn new(target: SyslogTarget, facility: u8, app_name: String) -> Self {
        Self {
            target,
            facility,
            app_name,
            hostname: hostname(),
            procid: std::process::id(),
            link: None,
            failing: false,
            retry_at: None,
            delay: RETRY_INITIAL,
            retry_initial: RETRY_INITIAL,
            retry_max: RETRY_MAX,
        }
    }

    #[cfg(test)]
    fn with_retry(mut self, initial: Duration, max: Duration) -> Self {
        self.delay = initial;
        self.retry_initial = initial;
        self.retry_max = max;
        self
    }

    /// Sends one record; `false` means it was not sent and should be counted as dropped.
    pub fn send(&mut self, meta: &Meta, body: &str) -> bool {
        let now = Instant::now();
        if self.link.is_none() && !self.try_connect(now) {
            return false;
        }
        let msg = message(
            self.facility,
            meta,
            &self.hostname,
            &self.app_name,
            self.procid,
            body,
        );
        let Some(link) = self.link.as_mut() else {
            return false;
        };
        match write_frame(link, &msg) {
            Ok(()) => true,
            Err(e) => {
                // A datagram that failed alone, such as one too large, leaves the socket usable.
                if !matches!(link, Link::Udp { .. }) {
                    self.fail(now, &e);
                }
                false
            }
        }
    }

    fn try_connect(&mut self, now: Instant) -> bool {
        if self.retry_at.is_some_and(|at| now < at) {
            return false;
        }
        match connect(&self.target) {
            Ok(link) => {
                self.link = Some(link);
                self.retry_at = None;
                self.delay = self.retry_initial;
                if self.failing {
                    self.failing = false;
                    tracing::info!("requestlog: syslog collector is reachable again");
                }
                true
            }
            Err(e) => {
                self.fail(now, &e);
                false
            }
        }
    }

    fn fail(&mut self, now: Instant, error: &io::Error) {
        self.link = None;
        self.retry_at = Some(now + self.delay);
        self.delay = (self.delay * 2).min(self.retry_max);
        if !self.failing {
            self.failing = true;
            tracing::warn!(error = %error, "requestlog: cannot send to the syslog collector");
        }
    }
}

/// Name lookups have no timeout of their own, so one runs on a helper thread and a hung
/// resolver costs the log writer at most `CONNECT_TIMEOUT` instead of blocking it.
fn resolve(addr: &str) -> io::Result<SocketAddr> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let name = addr.to_owned();
    std::thread::spawn(move || {
        let _ = sender.send(
            name.to_socket_addrs()
                .map(|found| found.collect::<Vec<_>>()),
        );
    });
    let found = receiver.recv_timeout(CONNECT_TIMEOUT).map_err(|_| {
        io::Error::new(io::ErrorKind::TimedOut, "resolving the collector timed out")
    })??;
    found
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the host has no address"))
}

fn connect(target: &SyslogTarget) -> io::Result<Link> {
    match target {
        SyslogTarget::Udp(addr) => {
            let addr = resolve(addr)?;
            let bind = if addr.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            };
            Ok(Link::Udp {
                socket: UdpSocket::bind(bind)?,
                addr,
            })
        }
        SyslogTarget::Tcp(addr) => {
            let stream = TcpStream::connect_timeout(&resolve(addr)?, CONNECT_TIMEOUT)?;
            stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
            Ok(Link::Tcp(stream))
        }
        #[cfg(unix)]
        SyslogTarget::Unix(path) => Ok(Link::Unix(connect_unix(path)?)),
        #[cfg(not(unix))]
        SyslogTarget::Unix(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unix sockets need a Unix platform",
        )),
    }
}

#[cfg(unix)]
fn connect_unix(path: &Path) -> io::Result<UnixDatagram> {
    let socket = UnixDatagram::unbound()?;
    socket.connect(path)?;
    socket.set_write_timeout(Some(WRITE_TIMEOUT))?;
    Ok(socket)
}

fn write_frame(link: &mut Link, msg: &str) -> io::Result<()> {
    match link {
        Link::Udp { socket, addr } => socket.send_to(msg.as_bytes(), *addr).map(|_| ()),
        // Octet-counting framing (RFC 6587): `LEN SP MSG`.
        Link::Tcp(stream) => stream.write_all(format!("{} {msg}", msg.len()).as_bytes()),
        #[cfg(unix)]
        Link::Unix(socket) => socket.send(msg.as_bytes()).map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, UdpSocket as StdUdp};
    use std::time::UNIX_EPOCH;

    fn meta(msgid: &'static str, severity: u8) -> Meta {
        Meta {
            msgid,
            severity,
            at: UNIX_EPOCH + Duration::from_millis(951_782_400_123),
        }
    }

    fn sender(target: SyslogTarget) -> Syslog {
        Syslog::new(target, 16, "reiki".to_owned())
    }

    /// Reads one octet-counted frame and returns its message.
    fn read_frame(reader: &mut impl BufRead) -> String {
        let mut len = Vec::new();
        reader.read_until(b' ', &mut len).unwrap();
        let len: usize = std::str::from_utf8(&len[..len.len() - 1])
            .unwrap()
            .parse()
            .unwrap();
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        String::from_utf8(body).unwrap()
    }

    #[test]
    fn facility_names_map_to_rfc_5424_codes() {
        assert_eq!(facility_code("kern"), Some(0));
        assert_eq!(facility_code("local0"), Some(16));
        assert_eq!(facility_code("local7"), Some(23));
        assert_eq!(facility_code("Local0"), None);
        assert_eq!(facility_code("nope"), None);
    }

    #[test]
    fn pri_is_facility_times_eight_plus_severity() {
        let m = |severity| message(16, &meta("request", severity), "h", "a", 1, "");
        assert!(m(SEVERITY_INFO).starts_with("<134>1 "));
        assert!(m(SEVERITY_WARNING).starts_with("<132>1 "));
        assert!(message(1, &meta("request", SEVERITY_INFO), "h", "a", 1, "").starts_with("<14>1 "));
    }

    #[test]
    fn the_header_has_every_rfc_5424_field_in_order() {
        let line = message(
            16,
            &meta("response", SEVERITY_WARNING),
            "gw-1",
            "reiki",
            4242,
            "{\"event\":\"response\"}",
        );
        assert_eq!(
            line,
            "<132>1 2000-02-29T00:00:00.123Z gw-1 reiki 4242 response - {\"event\":\"response\"}"
        );
    }

    #[test]
    fn the_host_name_is_a_valid_hostname_field() {
        let name = hostname();
        assert!(!name.is_empty() && !name.contains(' '), "{name:?}");
    }

    #[test]
    fn udp_sends_one_unframed_message_per_datagram() {
        let peer = StdUdp::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let addr = peer.local_addr().unwrap();
        let mut syslog = sender(SyslogTarget::Udp(addr.to_string()));
        assert!(syslog.send(&meta("request", SEVERITY_INFO), "{\"n\":1}"));

        let mut buf = [0u8; 2048];
        let (n, _) = peer.recv_from(&mut buf).unwrap();
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(
            text.starts_with("<134>1 2000-02-29T00:00:00.123Z "),
            "{text}"
        );
        assert!(text.ends_with(" request - {\"n\":1}"), "{text}");
        assert!(text.contains(&format!(" reiki {} request - ", std::process::id())));
    }

    #[test]
    fn tcp_frames_each_message_by_its_octet_count() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let reader = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut frames = Vec::new();
            for _ in 0..2 {
                frames.push(read_frame(&mut reader));
            }
            frames
        });

        let mut syslog = sender(SyslogTarget::Tcp(addr.to_string()));
        assert!(syslog.send(&meta("request", SEVERITY_INFO), "{\"a\":1}"));
        assert!(syslog.send(&meta("finding", SEVERITY_WARNING), "{\"b\":2}"));
        let frames = reader.join().unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].starts_with("<134>1 ") && frames[0].ends_with("{\"a\":1}"));
        assert!(frames[1].starts_with("<132>1 ") && frames[1].ends_with(" finding - {\"b\":2}"));
    }

    #[cfg(unix)]
    #[test]
    fn a_unix_datagram_socket_receives_one_message_per_send() {
        use std::os::unix::net::UnixDatagram;

        let dir = std::env::temp_dir().join(format!("reiki-syslog-unix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.sock");
        let peer = UnixDatagram::bind(&path).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();

        let mut syslog = sender(SyslogTarget::Unix(path.clone()));
        assert!(syslog.send(&meta("request", SEVERITY_INFO), "{\"u\":1}"));
        let mut buf = [0u8; 2048];
        let n = peer.recv(&mut buf).unwrap();
        assert!(std::str::from_utf8(&buf[..n])
            .unwrap()
            .ends_with(" request - {\"u\":1}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_closed_port_fails_fast_and_is_not_retried_for_each_record() {
        let closed = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let mut syslog = sender(SyslogTarget::Tcp(closed.to_string()))
            .with_retry(Duration::from_secs(60), Duration::from_secs(60));
        let started = Instant::now();
        let sent = (0..200)
            .filter(|_| syslog.send(&meta("request", SEVERITY_INFO), "{}"))
            .count();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(sent, 0);
    }

    #[test]
    fn a_tcp_collector_that_restarts_is_reconnected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut syslog = sender(SyslogTarget::Tcp(addr.to_string()))
            .with_retry(Duration::from_millis(10), Duration::from_millis(50));
        assert!(syslog.send(&meta("request", SEVERITY_INFO), "{\"before\":1}"));
        let (first, _) = listener.accept().unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let line = read_frame(&mut BufReader::new(first));
        assert!(line.ends_with("{\"before\":1}"), "{line}");

        drop(listener);
        let rebound = TcpListener::bind(addr).unwrap();
        rebound.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = None;
        while received.is_none() && Instant::now() < deadline {
            syslog.send(&meta("request", SEVERITY_INFO), "{\"after\":1}");
            std::thread::sleep(Duration::from_millis(20));
            if let Ok((stream, _)) = rebound.accept() {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                received = Some(read_frame(&mut BufReader::new(stream)));
            }
        }
        let line = received.expect("the restarted collector got no record");
        assert!(line.ends_with("{\"after\":1}"), "{line}");
    }
}
