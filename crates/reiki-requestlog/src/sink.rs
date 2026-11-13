use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// Append-only JSONL file that rotates by size and is readable by its owner only.
pub struct FileSink {
    path: PathBuf,
    file: File,
    size: u64,
    max_bytes: u64,
    keep: u32,
    rotation: Rotation,
}

/// What a caller should tell the operator about rotation.
#[derive(Debug, PartialEq, Eq)]
pub enum Notice {
    /// Rotation failed; records still go to the current file and rotation is retried.
    RotateFailed(String),
    RotateRecovered,
}

const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Retry state after a failed rotation.
struct Rotation {
    failing: bool,
    retry_at: Option<Instant>,
    backoff: Duration,
    base: Duration,
    /// The old file was renamed but the new one could not be opened yet.
    reopen: bool,
    notice: Option<Notice>,
}

fn open_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn numbered(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

impl FileSink {
    pub fn open(path: &Path, max_bytes: u64, keep: u32) -> io::Result<Self> {
        let file = open_append(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_owned(),
            file,
            size,
            max_bytes,
            keep,
            rotation: Rotation {
                failing: false,
                retry_at: None,
                backoff: RETRY_BASE,
                base: RETRY_BASE,
                reopen: false,
                notice: None,
            },
        })
    }

    /// The report a failed or recovered rotation left, once.
    pub fn take_notice(&mut self) -> Option<Notice> {
        self.rotation.notice.take()
    }

    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let needed = line.len() as u64 + 1;
        let due = self.rotation.reopen
            || (self.max_bytes > 0 && self.size > 0 && self.size + needed > self.max_bytes);
        if due && self.rotation.retry_at.is_none_or(|at| Instant::now() >= at) {
            self.try_rotate();
        }
        let mut buf = String::with_capacity(line.len() + 1);
        buf.push_str(line);
        buf.push('\n');
        self.file.write_all(buf.as_bytes())?;
        self.file.flush()?;
        self.size += needed;
        Ok(())
    }

    /// A failed rotation leaves the current file in use and is retried after a growing pause.
    fn try_rotate(&mut self) {
        let result = self.rotate();
        let state = &mut self.rotation;
        match result {
            Ok(()) => {
                state.retry_at = None;
                state.backoff = state.base;
                if std::mem::take(&mut state.failing) {
                    state.notice = Some(Notice::RotateRecovered);
                }
            }
            Err(error) => {
                state.retry_at = Some(Instant::now() + state.backoff);
                state.backoff = (state.backoff * 2).min(RETRY_MAX);
                if !state.failing {
                    state.failing = true;
                    state.notice = Some(Notice::RotateFailed(error.to_string()));
                }
            }
        }
    }

    fn rotate(&mut self) -> io::Result<()> {
        if self.rotation.reopen {
            self.file = open_append(&self.path)?;
            self.size = self.file.metadata()?.len();
            self.rotation.reopen = false;
            return Ok(());
        }
        if self.keep == 0 {
            self.file.set_len(0)?;
        } else {
            for n in (1..self.keep).rev() {
                let from = numbered(&self.path, n);
                if from.exists() {
                    std::fs::rename(&from, numbered(&self.path, n + 1))?;
                }
            }
            std::fs::rename(&self.path, numbered(&self.path, 1))?;
            self.rotation.reopen = true;
            self.file = open_append(&self.path)?;
            self.rotation.reopen = false;
        }
        self.size = 0;
        Ok(())
    }
}

#[cfg(unix)]
pub use unix::SocketHub;

#[cfg(unix)]
mod unix {
    use std::io::{self, ErrorKind, Write};
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{sync_channel, SyncSender};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    const MAX_SUBSCRIBERS: usize = 32;
    const SUBSCRIBER_QUEUE: usize = 256;
    const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
    const ACCEPT_POLL: Duration = Duration::from_millis(50);

    type Subscribers = Arc<Mutex<Vec<SyncSender<Arc<str>>>>>;

    /// Live JSONL feed on a Unix socket; each subscriber has its own bounded queue and is dropped when it falls behind.
    pub struct SocketHub {
        path: PathBuf,
        subscribers: Subscribers,
        stop: Arc<AtomicBool>,
        accept: Option<JoinHandle<()>>,
    }

    impl SocketHub {
        pub fn bind(path: &Path) -> io::Result<Self> {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                if std::fs::metadata(parent)?.permissions().mode() & 0o002 != 0 {
                    return Err(io::Error::new(
                        ErrorKind::PermissionDenied,
                        "the socket's directory is writable by everyone",
                    ));
                }
            }
            match std::fs::symlink_metadata(path) {
                Ok(meta) if !meta.file_type().is_socket() => {
                    return Err(io::Error::new(
                        ErrorKind::AlreadyExists,
                        "something that is not a socket already exists at that path",
                    ));
                }
                Ok(_) => {
                    if UnixStream::connect(path).is_ok() {
                        return Err(io::Error::new(
                            ErrorKind::AddrInUse,
                            "another process is already listening on that socket",
                        ));
                    }
                    std::fs::remove_file(path)?;
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let listener = UnixListener::bind(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;

            let subscribers: Subscribers = Arc::default();
            let stop = Arc::new(AtomicBool::new(false));
            let accept = {
                let subscribers = Arc::clone(&subscribers);
                let stop = Arc::clone(&stop);
                thread::Builder::new()
                    .name("requestlog-accept".to_owned())
                    .spawn(move || accept_loop(&listener, &subscribers, &stop))?
            };
            Ok(Self {
                path: path.to_owned(),
                subscribers,
                stop,
                accept: Some(accept),
            })
        }

        pub fn broadcast(&self, line: &Arc<str>) {
            let mut subscribers = self
                .subscribers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            subscribers.retain(|tx| tx.try_send(Arc::clone(line)).is_ok());
        }

        #[cfg(test)]
        pub fn subscriber_count(&self) -> usize {
            self.subscribers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len()
        }
    }

    impl Drop for SocketHub {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.accept.take() {
                let _ = handle.join();
            }
            let still_ours = std::fs::symlink_metadata(&self.path)
                .is_ok_and(|meta| meta.file_type().is_socket());
            if still_ours {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    fn accept_loop(listener: &UnixListener, subscribers: &Subscribers, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let mut subs = subscribers.lock().unwrap_or_else(PoisonError::into_inner);
                    subs.retain(|tx| tx.try_send(Arc::from("")).is_ok());
                    if subs.len() >= MAX_SUBSCRIBERS {
                        continue;
                    }
                    let (tx, rx) = sync_channel::<Arc<str>>(SUBSCRIBER_QUEUE);
                    let spawned = thread::Builder::new()
                        .name("requestlog-subscriber".to_owned())
                        .spawn(move || serve_subscriber(stream, &rx));
                    if spawned.is_ok() {
                        subs.push(tx);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                Err(_) => thread::sleep(ACCEPT_POLL),
            }
        }
    }

    fn serve_subscriber(mut stream: UnixStream, rx: &std::sync::mpsc::Receiver<Arc<str>>) {
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        while let Ok(line) = rx.recv() {
            if line.is_empty() {
                continue;
            }
            let mut buf = String::with_capacity(line.len() + 1);
            buf.push_str(&line);
            buf.push('\n');
            if stream.write_all(buf.as_bytes()).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reiki-requestlog-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lines_are_appended_with_owner_only_permissions() {
        let dir = temp("append");
        let path = dir.join("req.jsonl");
        let mut sink = FileSink::open(&path, 0, 0).unwrap();
        sink.write_line("{\"a\":1}").unwrap();
        sink.write_line("{\"a\":2}").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"a\":1}\n{\"a\":2}\n"
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_existing_loose_file_is_tightened_and_appended_to() {
        let dir = temp("loose");
        let path = dir.join("req.jsonl");
        std::fs::write(&path, "old\n").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut sink = FileSink::open(&path, 0, 0).unwrap();
        sink.write_line("new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\nnew\n");
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_keeps_the_newest_files_and_drops_the_oldest() {
        let dir = temp("rotate");
        let path = dir.join("req.jsonl");
        let mut sink = FileSink::open(&path, 20, 2).unwrap();
        for n in 1..=7 {
            sink.write_line(&format!("line-{n:08}")).unwrap();
        }
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
        assert_eq!(read(&path), "line-00000007\n");
        assert_eq!(read(&numbered(&path, 1)), "line-00000006\n");
        assert_eq!(read(&numbered(&path, 2)), "line-00000005\n");
        assert!(!numbered(&path, 3).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keep_zero_discards_the_old_file() {
        let dir = temp("keep0");
        let path = dir.join("req.jsonl");
        let mut sink = FileSink::open(&path, 20, 0).unwrap();
        sink.write_line("line-00000001").unwrap();
        sink.write_line("line-00000002").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line-00000002\n");
        assert!(!numbered(&path, 1).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn blocked_rotation(tag: &str, keep: u32) -> (PathBuf, PathBuf, FileSink) {
        let dir = temp(tag);
        let path = dir.join("req.jsonl");
        let mut sink = FileSink::open(&path, 20, keep).unwrap();
        sink.rotation.base = Duration::from_secs(3600);
        sink.rotation.backoff = Duration::from_secs(3600);
        // A directory at the target makes the rename of the log file fail.
        let blocker = numbered(&path, 1);
        std::fs::create_dir_all(blocker.join("occupied")).unwrap();
        (dir, blocker, sink)
    }

    #[test]
    fn a_failed_rotation_keeps_logging_to_the_current_file_and_reports_once() {
        let (dir, blocker, mut sink) = blocked_rotation("rotfail", 1);
        let path = dir.join("req.jsonl");
        for n in 1..=5 {
            sink.write_line(&format!("line-{n:08}")).unwrap();
        }
        let all: String = (1..=5).map(|n| format!("line-{n:08}\n")).collect();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), all);
        assert!(matches!(sink.take_notice(), Some(Notice::RotateFailed(_))));
        assert_eq!(sink.take_notice(), None, "reported once, not per record");
        // Fix the cause: still no retry before the backoff has passed.
        std::fs::remove_dir_all(&blocker).unwrap();
        sink.write_line("line-00000006").unwrap();
        assert!(!numbered(&path, 1).exists());
        // Past the backoff the rotation goes through; every record is in exactly one file.
        sink.rotation.retry_at = Some(Instant::now());
        sink.write_line("line-00000007").unwrap();
        assert_eq!(sink.take_notice(), Some(Notice::RotateRecovered));
        let rotated = std::fs::read_to_string(numbered(&path, 1)).unwrap();
        let current = std::fs::read_to_string(&path).unwrap();
        assert_eq!(rotated, format!("{all}line-00000006\n"));
        assert_eq!(current, "line-00000007\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rotation_that_fails_partway_neither_loses_nor_repeats_records() {
        let dir = temp("rotpartway");
        let path = dir.join("req.jsonl");
        let mut sink = FileSink::open(&path, 20, 3).unwrap();
        sink.rotation.base = Duration::ZERO;
        sink.rotation.backoff = Duration::ZERO;
        for n in 1..=3 {
            sink.write_line(&format!("line-{n:08}")).unwrap();
        }
        // .3 is a non-empty directory, so shifting .2 to .3 fails before anything has moved.
        std::fs::create_dir_all(numbered(&path, 3).join("occupied")).unwrap();
        sink.write_line("line-00000004").unwrap();
        sink.write_line("line-00000005").unwrap();
        assert!(matches!(sink.take_notice(), Some(Notice::RotateFailed(_))));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line-00000003\nline-00000004\nline-00000005\n"
        );
        std::fs::remove_dir_all(numbered(&path, 3)).unwrap();
        sink.write_line("line-00000006").unwrap();
        assert_eq!(sink.take_notice(), Some(Notice::RotateRecovered));
        let read = |n: u32| std::fs::read_to_string(numbered(&path, n)).unwrap_or_default();
        let mut seen: Vec<String> = [
            std::fs::read_to_string(&path).unwrap(),
            read(1),
            read(2),
            read(3),
        ]
        .iter()
        .flat_map(|t| t.lines().map(str::to_owned).collect::<Vec<_>>())
        .collect();
        seen.sort();
        let want: Vec<String> = (1..=6).map(|n| format!("line-{n:08}")).collect();
        assert_eq!(seen, want);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keep_zero_rotation_failure_cannot_lose_the_file() {
        let dir = temp("keep0fail");
        let path = dir.join("req.jsonl");
        let mut sink = FileSink::open(&path, 20, 0).unwrap();
        sink.write_line("line-00000001").unwrap();
        sink.write_line("line-00000002").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line-00000002\n");
        assert_eq!(sink.take_notice(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    mod socket {
        use super::*;
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixStream;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        fn wait_for(mut check: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if check() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("condition not reached in time");
        }

        #[test]
        fn subscribers_receive_lines_and_the_socket_is_private() {
            let dir = temp("sock");
            let path = dir.join("req.sock");
            let hub = SocketHub::bind(&path).unwrap();
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );

            let a = UnixStream::connect(&path).unwrap();
            let b = UnixStream::connect(&path).unwrap();
            wait_for(|| hub.subscriber_count() == 2);
            hub.broadcast(&Arc::from("{\"n\":1}"));
            hub.broadcast(&Arc::from("{\"n\":2}"));

            for stream in [a, b] {
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line, "{\"n\":1}\n");
                line.clear();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line, "{\"n\":2}\n");
            }
            drop(hub);
            assert!(!path.exists(), "the socket file should be removed on drop");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_disconnected_subscriber_is_dropped_and_others_continue() {
            let dir = temp("sockdrop");
            let path = dir.join("req.sock");
            let hub = SocketHub::bind(&path).unwrap();
            let gone = UnixStream::connect(&path).unwrap();
            let stay = UnixStream::connect(&path).unwrap();
            wait_for(|| hub.subscriber_count() == 2);
            drop(gone);

            wait_for(|| {
                hub.broadcast(&Arc::from("{\"x\":1}"));
                hub.subscriber_count() == 1
            });
            stay.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut line = String::new();
            BufReader::new(stay).read_line(&mut line).unwrap();
            assert_eq!(line, "{\"x\":1}\n");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_regular_file_at_the_path_is_refused_and_left_alone() {
            let dir = temp("sockfile");
            let path = dir.join("req.sock");
            std::fs::write(&path, "precious").unwrap();
            let err = SocketHub::bind(&path).err().unwrap();
            assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "precious");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_directory_anyone_can_write_to_is_refused() {
            let dir = temp("sockperm");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
            let err = SocketHub::bind(&dir.join("req.sock")).err().unwrap();
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(SocketHub::bind(&dir.join("req.sock")).is_ok());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_second_hub_on_a_live_socket_is_refused_but_a_stale_file_is_replaced() {
            let dir = temp("socksecond");
            let path = dir.join("req.sock");
            let first = SocketHub::bind(&path).unwrap();
            assert!(SocketHub::bind(&path).is_err());
            drop(first);

            // A listener that is dropped leaves its socket file behind: a stale socket.
            drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
            assert!(path.exists() && UnixStream::connect(&path).is_err());
            let again = SocketHub::bind(&path).unwrap();
            assert!(UnixStream::connect(&path).is_ok());
            drop(again);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
