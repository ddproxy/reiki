//! Reiki's service log: stderr and an optional rotating file, with a level that reloads on SIGHUP.

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

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
}

impl FileState {
    fn open(&mut self) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o640);
        let file = options.open(&self.path)?;
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
        if self.max_bytes > 0 && self.size > 0 && self.size + data.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        if let Some(file) = self.file.as_mut() {
            file.write_all(data)?;
            self.size += data.len() as u64;
        }
        Ok(())
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
}

impl LogFile {
    fn open(config: &LogConfig, path: &str) -> io::Result<Arc<Self>> {
        let mut state = FileState {
            path: PathBuf::from(path),
            max_bytes: config.file_max_bytes,
            keep: config.file_keep,
            file: None,
            size: 0,
        };
        state.open()?;
        Ok(Arc::new(Self {
            state: Mutex::new(state),
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
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        // A log line must never take the service down, so a failed write is dropped.
        let _ = state.write(data);
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
        .init();
    Ok(Logging {
        filter,
        file,
        from_env,
    })
}

impl Logging {
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
        if let Some(file) = &self.file {
            if let Err(error) = file.reopen() {
                tracing::warn!(%error, "cannot reopen the log file");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
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
