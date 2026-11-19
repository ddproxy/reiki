//! Reiki's service log: stderr and an optional rotating file, with a level that reloads on SIGHUP.

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, PoisonError};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{reload, EnvFilter, Layer, Registry};

use crate::config::{LogConfig, LogFormat};

struct FileState {
    path: PathBuf,
    max_bytes: u64,
    keep: u32,
    file: Option<File>,
    size: u64,
    /// After a failed rotation, the size at which to try again.
    rotate_after: u64,
}

impl FileState {
    fn open(&mut self) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            // A symlink at the log path could aim lines at a file the service user can write.
            options.mode(0o640).custom_flags(libc::O_NOFOLLOW);
            let file = options.open(&self.path).map_err(|e| {
                if e.raw_os_error() == Some(libc::ELOOP) {
                    io::Error::other("the log path is a symbolic link; refusing to follow it")
                } else {
                    e
                }
            })?;
            // `mode` only applies to a file this call creates; a file left by an older run keeps
            // whatever it had. A file the service user does not own keeps its mode.
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o640));
            self.adopt(file)
        }
        #[cfg(not(unix))]
        {
            let file = options.open(&self.path)?;
            self.adopt(file)
        }
    }

    fn adopt(&mut self, file: File) -> io::Result<()> {
        self.size = file.metadata().map_or(0, |m| m.len());
        self.file = Some(file);
        Ok(())
    }

    /// Shifts `<file>.N-1` to `<file>.N`, the live file to `<file>.1`, and starts a new one.
    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        if self.keep == 0 {
            std::fs::remove_file(&self.path)?;
        } else {
            for n in (1..self.keep).rev() {
                let from = numbered(&self.path, n);
                if from.exists() {
                    std::fs::rename(&from, numbered(&self.path, n + 1))?;
                }
            }
            std::fs::rename(&self.path, numbered(&self.path, 1))?;
        }
        self.open()
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if self.file.is_none() {
            self.open()?;
        }
        let due = self.max_bytes > 0
            && self.size > 0
            && self.size >= self.rotate_after
            && self.size + data.len() as u64 > self.max_bytes;
        // A rotation that fails must not stop logging: keep writing to the live file.
        let rotated = if due { self.rotate() } else { Ok(()) };
        if rotated.is_err() && self.file.is_none() {
            self.open()?;
        }
        if let Some(file) = self.file.as_mut() {
            file.write_all(data)?;
            self.size += data.len() as u64;
        }
        if rotated.is_err() {
            self.rotate_after = self.size + self.max_bytes;
        }
        rotated
    }
}

fn numbered(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// The log file, shared by the subscriber and the SIGHUP handler.
pub struct LogFile {
    state: Mutex<FileState>,
    /// Set by the first failed write, which is also reported on stderr.
    warned: AtomicBool,
}

impl LogFile {
    fn open(config: &LogConfig, path: &str) -> io::Result<Arc<Self>> {
        let mut state = FileState {
            path: PathBuf::from(path),
            max_bytes: config.file_max_bytes,
            keep: config.file_keep,
            file: None,
            size: 0,
            rotate_after: 0,
        };
        state.open()?;
        Ok(Arc::new(Self {
            state: Mutex::new(state),
            warned: AtomicBool::new(false),
        }))
    }

    /// Closes and reopens the file, so a log rotation tool can move it away.
    fn reopen(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.file = None;
        state.open()
    }
}

pub struct FileWriter(Arc<LogFile>);

impl Write for FileWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let result = {
            let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.write(data)
        };
        // A log line must never take the service down, so a failed write is dropped.
        // Reported on stderr: a tracing event would come straight back to this writer.
        if let Err(error) = result {
            if !self.0.warned.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "reiki: cannot write to the log file: {error}; later failures are dropped silently"
                );
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct SharedFile(Arc<LogFile>);

impl<'a> MakeWriter<'a> for SharedFile {
    type Writer = FileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        FileWriter(Arc::clone(&self.0))
    }
}

/// Lets the running service change its log level and reopen its file.
pub struct Logging {
    filter: reload::Handle<EnvFilter, Registry>,
    file: Option<Arc<LogFile>>,
    from_env: bool,
    started: LogConfig,
}

fn filter_from(level: &str) -> (EnvFilter, bool) {
    match EnvFilter::try_from_default_env() {
        Ok(filter) => (filter, true),
        Err(_) => (
            EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info")),
            false,
        ),
    }
}

/// Logs panics through `tracing` once a subscriber exists, then calls the previous hook so the
/// default stderr report, unwinding and abort behaviour stay unchanged.
pub fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!("{}", format_panic(info.payload(), info.location()));
            previous(info);
        }));
    });
}

fn format_panic(
    payload: &(dyn std::any::Any + Send),
    location: Option<&std::panic::Location<'_>>,
) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic");
    match location {
        Some(at) => format!(
            "panic at {}:{}:{}: {message}",
            at.file(),
            at.line(),
            at.column()
        ),
        None => format!("panic at unknown location: {message}"),
    }
}

/// Starts logging. Returns an error when the log file cannot be opened.
pub fn init(config: &LogConfig, with_file: bool) -> Result<Logging, String> {
    let (filter, from_env) = filter_from(&config.level);
    let (filter_layer, filter) = reload::Layer::new(filter);

    let file = match (&config.file, with_file) {
        (Some(path), true) => Some(
            LogFile::open(config, path).map_err(|e| format!("cannot open log file {path}: {e}"))?,
        ),
        _ => None,
    };

    let mut layers: Vec<Box<dyn Layer<_> + Send + Sync>> = Vec::new();
    if config.stderr || file.is_none() {
        layers.push(
            tracing_subscriber::fmt::layer()
                .with_writer(io::stderr)
                .with_ansi(io::stderr().is_terminal())
                .boxed(),
        );
    }
    if let Some(file) = &file {
        let writer = SharedFile(Arc::clone(file));
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(false);
        layers.push(match config.format {
            LogFormat::Json => layer.json().boxed(),
            LogFormat::Text => layer.boxed(),
        });
    }
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(layers)
        .try_init()
        .map_err(|e| format!("cannot start logging: {e}"))?;
    Ok(Logging {
        filter,
        file,
        from_env,
        started: config.clone(),
    })
}

impl Logging {
    /// `[log]` settings that changed but need a restart.
    pub fn restart_needed(&self, new: &LogConfig) -> Vec<String> {
        let old = &self.started;
        [
            ("log.file", old.file != new.file),
            ("log.format", old.format != new.format),
            ("log.stderr", old.stderr != new.stderr),
            (
                "log.file_max_bytes",
                old.file_max_bytes != new.file_max_bytes,
            ),
            ("log.file_keep", old.file_keep != new.file_keep),
        ]
        .into_iter()
        .filter(|(_, differs)| *differs)
        .map(|(name, _)| name.to_owned())
        .collect()
    }

    /// Reopens the log file, so a rotation tool can move it away, without touching the level.
    pub fn reopen(&self) {
        if let Some(file) = &self.file {
            if let Err(error) = file.reopen() {
                tracing::warn!(%error, "cannot reopen the log file");
            }
        }
    }

    /// Applies a changed level and reopens the file. A set `RUST_LOG` keeps winning.
    pub fn reload(&self, config: &LogConfig) {
        if !self.from_env {
            match EnvFilter::try_new(&config.level) {
                Ok(new) => {
                    if self.filter.reload(new).is_ok() {
                        tracing::info!(level = %config.level, "log level applied");
                    }
                }
                Err(error) => tracing::warn!(%error, "log level not applied"),
            }
        }
        self.reopen();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_hook_message_carries_the_location_and_payload() {
        let text = format_panic(&"boom", Some(std::panic::Location::caller()));
        assert!(
            text.contains("boom") && text.contains("logging.rs"),
            "{text}"
        );
        assert!(text.contains("panic at"), "{text}");
        let owned = String::from("owned boom");
        assert!(format_panic(&owned, None).contains("owned boom"));
        assert!(format_panic(&7_u8, None).contains("unknown panic"));
    }

    fn dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reiki-log-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state(path: PathBuf, max_bytes: u64, keep: u32) -> FileState {
        FileState {
            path,
            max_bytes,
            keep,
            file: None,
            size: 0,
            rotate_after: 0,
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_failed_rotation_keeps_the_log_writing() {
        let dir = dir("wedge");
        let path = dir.join("reiki.log");
        std::fs::create_dir(dir.join("reiki.log.1")).unwrap();
        let mut file = state(path.clone(), 10, 1);
        assert!(file.write(b"first line\n").is_ok());
        assert!(
            file.write(b"second line\n").is_err(),
            "the failure is reported"
        );
        assert!(file.write(b"third\n").is_ok(), "and later lines still land");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("second line") && text.contains("third"),
            "{text}"
        );
    }

    fn detached(started: LogConfig) -> (Logging, impl tracing::Subscriber + Send + Sync) {
        let (layer, filter) = reload::Layer::new(EnvFilter::new("info"));
        let logging = Logging {
            filter,
            file: None,
            from_env: false,
            started,
        };
        (logging, Registry::default().with(layer))
    }

    #[test]
    fn only_the_settings_a_reload_cannot_change_need_a_restart() {
        let (logging, _subscriber) = detached(LogConfig::default());
        let mut next = LogConfig::default();
        assert!(logging.restart_needed(&next).is_empty());
        next.level = "debug".to_owned();
        assert!(logging.restart_needed(&next).is_empty(), "level is live");
        next.format = LogFormat::Json;
        next.stderr = !next.stderr;
        next.file_max_bytes += 1;
        next.file_keep += 1;
        next.file = Some("/var/log/reiki.log".to_owned());
        assert_eq!(
            logging.restart_needed(&next),
            [
                "log.file",
                "log.format",
                "log.stderr",
                "log.file_max_bytes",
                "log.file_keep"
            ]
        );
    }

    #[test]
    fn a_reload_applies_the_level() {
        let (logging, subscriber) = detached(LogConfig::default());
        let _guard = tracing::subscriber::set_default(subscriber);
        let next = LogConfig {
            level: "debug".to_owned(),
            ..LogConfig::default()
        };
        logging.reload(&next);
        let level = logging.filter.with_current(ToString::to_string).unwrap();
        assert_eq!(level, "debug");
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_log_file_is_tightened_to_0640() {
        use std::os::unix::fs::PermissionsExt;
        let dir = dir("mode");
        let path = dir.join("reiki.log");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let mut file = state(path.clone(), 0, 2);
        file.write(b"new\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\nnew\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_log_path_is_refused_and_its_target_untouched() {
        let dir = dir("symlink");
        let target = dir.join("target");
        std::fs::write(&target, "keep\n").unwrap();
        let path = dir.join("reiki.log");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let error = LogFile::open(&LogConfig::default(), path.to_str().unwrap())
            .err()
            .expect("refused");
        assert!(error.to_string().contains("symbolic link"), "{error}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep\n");

        let missing = dir.join("missing-target");
        let dangling = dir.join("dangling.log");
        std::os::unix::fs::symlink(&missing, &dangling).unwrap();
        assert!(LogFile::open(&LogConfig::default(), dangling.to_str().unwrap()).is_err());
        assert!(!missing.exists(), "no file was created through the link");
    }

    #[test]
    fn failed_file_writes_never_fail_the_caller() {
        let dir = dir("failing");
        let log = Arc::new(LogFile {
            state: Mutex::new(state(dir.join("no-such-dir").join("reiki.log"), 0, 2)),
            warned: AtomicBool::new(false),
        });
        let mut writer = FileWriter(Arc::clone(&log));
        assert_eq!(writer.write(b"one\n").unwrap(), 4);
        assert_eq!(writer.write(b"two\n").unwrap(), 4);
        assert!(log.warned.load(Ordering::Relaxed));
    }

    #[test]
    fn the_file_rotates_by_size_and_keeps_a_fixed_number() {
        let dir = dir("rotate");
        let path = dir.join("reiki.log");
        let mut file = state(path.clone(), 10, 2);
        for line in ["aaaaaa\n", "bbbbbb\n", "cccccc\n", "dddddd\n"] {
            file.write(line.as_bytes()).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "dddddd\n");
        assert_eq!(
            std::fs::read_to_string(numbered(&path, 1)).unwrap(),
            "cccccc\n"
        );
        assert_eq!(
            std::fs::read_to_string(numbered(&path, 2)).unwrap(),
            "bbbbbb\n"
        );
        assert!(!numbered(&path, 3).exists(), "older files are discarded");
    }

    #[test]
    fn zero_max_bytes_never_rotates_and_zero_keep_discards() {
        let dir = dir("norotate");
        let path = dir.join("reiki.log");
        let mut file = state(path.clone(), 0, 2);
        for _ in 0..50 {
            file.write(b"0123456789\n").unwrap();
        }
        assert!(!numbered(&path, 1).exists());
        let path = dir.join("discard.log");
        let mut file = state(path.clone(), 10, 0);
        file.write(b"aaaaaa\n").unwrap();
        file.write(b"bbbbbb\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "bbbbbb\n");
        assert!(!numbered(&path, 1).exists());
    }

    #[test]
    fn reopening_follows_a_file_moved_away() {
        let dir = dir("reopen");
        let path = dir.join("reiki.log");
        let config = LogConfig::default();
        let log = LogFile::open(&config, path.to_str().unwrap()).unwrap();
        FileWriter(Arc::clone(&log)).write_all(b"before\n").unwrap();
        std::fs::rename(&path, dir.join("moved.log")).unwrap();
        log.reopen().unwrap();
        FileWriter(Arc::clone(&log)).write_all(b"after\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("moved.log")).unwrap(),
            "before\n"
        );
    }
}
