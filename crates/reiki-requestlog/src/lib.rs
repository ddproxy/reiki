//! Reiki plugin that records the requests passing through the relay.
//!
//! Records are JSON lines written to a rotating file and/or streamed live over a
//! Unix socket. The hooks only copy a few small values onto a bounded queue; a
//! writer thread parses, masks and writes them, so logging never delays a request.

mod config;
mod redact;
mod sink;
mod timefmt;
mod writer;

use std::any::Any;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime};

use reiki_plugin::{ChunkCtx, Plugin, PluginError, RequestCtx, ResponseHeadCtx};

pub use config::{BodyMode, Config, DENIED_HEADERS};
use sink::FileSink;
#[cfg(unix)]
use sink::SocketHub;
use writer::{Event, RequestEvent, ResponseEvent, Sinks};

/// The `[plugins.requestlog]` table this plugin reads.
pub const NAME: &str = "requestlog";

/// Builds the plugin from its config table.
pub fn build(table: &toml::Table) -> Result<Box<dyn Plugin>, String> {
    let config = Config::parse(table).map_err(|e| format!("{NAME}: {e}"))?;
    Ok(Box::new(RequestLog::new(config)?))
}

pub struct RequestLog {
    config: Arc<Config>,
    tx: Option<SyncSender<Event>>,
    dropped: Arc<AtomicU64>,
    writer: Option<JoinHandle<()>>,
}

/// Per-request data. Dropping it is the one reliable end-of-response signal: with a
/// known content length the host never makes the final `is_last` call.
struct RequestState {
    id: String,
    started: Instant,
    status: Option<u16>,
    bytes: u64,
    requested: bool,
    log_response: bool,
    tx: SyncSender<Event>,
    dropped: Arc<AtomicU64>,
}

impl Drop for RequestState {
    fn drop(&mut self) {
        if !(self.requested && self.log_response) {
            return;
        }
        let duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let event = Event::Response(ResponseEvent {
            id: std::mem::take(&mut self.id),
            at: SystemTime::now(),
            status: self.status,
            duration_ms,
            bytes: self.bytes,
        });
        if let Err(TrySendError::Full(_)) = self.tx.try_send(event) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    hasher.write_u128(nanos);
    format!("{:016x}", hasher.finish())
}

impl RequestLog {
    pub fn new(config: Config) -> Result<Self, String> {
        let file = match &config.file {
            Some(path) => Some(
                FileSink::open(path, config.file_max_bytes, config.file_keep)
                    .map_err(|e| format!("{NAME}: cannot open {}: {e}", path.display()))?,
            ),
            None => None,
        };
        #[cfg(unix)]
        let socket = match &config.socket {
            Some(path) => Some(
                SocketHub::bind(path)
                    .map_err(|e| format!("{NAME}: cannot listen on {}: {e}", path.display()))?,
            ),
            None => None,
        };
        #[cfg(not(unix))]
        if config.socket.is_some() {
            return Err(format!("{NAME}: `socket` needs a Unix platform"));
        }

        let config = Arc::new(config);
        let (tx, rx) = sync_channel(config.queue);
        let dropped = Arc::new(AtomicU64::new(0));
        let sinks = Sinks {
            file,
            #[cfg(unix)]
            socket,
        };
        let writer = writer::spawn(Arc::clone(&config), sinks, rx, Arc::clone(&dropped))
            .map_err(|e| format!("{NAME}: cannot start the writer thread: {e}"))?;
        Ok(Self {
            config,
            tx: Some(tx),
            dropped,
            writer: Some(writer),
        })
    }

    fn send(&self, event: Event) {
        let Some(tx) = &self.tx else {
            return;
        };
        match tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        let tx = self.tx.take();
        if let Some(handle) = self.writer.take() {
            if let Some(tx) = tx {
                let _ = tx.send(Event::Shutdown);
            }
            let _ = handle.join();
        }
    }
}

impl Plugin for RequestLog {
    fn name(&self) -> &str {
        NAME
    }

    fn new_state(&self) -> Box<dyn Any + Send> {
        let Some(tx) = self.tx.clone() else {
            return Box::new(());
        };
        Box::new(RequestState {
            id: new_id(),
            started: Instant::now(),
            status: None,
            bytes: 0,
            requested: false,
            log_response: self.config.response,
            tx,
            dropped: Arc::clone(&self.dropped),
        })
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        let Some(state) = ctx.state.downcast_mut::<RequestState>() else {
            return Ok(());
        };
        state.requested = true;
        let id = state.id.clone();
        let headers = self
            .config
            .headers
            .iter()
            .filter_map(|name| {
                let value = ctx.headers.get(name.as_str())?;
                Some((
                    name.clone(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                ))
            })
            .collect();
        let body = (self.config.body != BodyMode::None).then(|| ctx.body.clone());
        self.send(Event::Request(RequestEvent {
            id,
            at: SystemTime::now(),
            stream: ctx.stream.to_owned(),
            upstream: ctx.upstream.to_owned(),
            method: ctx.method.as_str().to_owned(),
            headers,
            body_bytes: ctx.body.len(),
            body,
        }));
        Ok(())
    }

    fn on_response_head(&self, ctx: &mut ResponseHeadCtx<'_>) {
        if let Some(state) = ctx.state.downcast_mut::<RequestState>() {
            state.status = Some(ctx.status.as_u16());
        }
    }

    fn on_response_chunk(&self, ctx: &mut ChunkCtx<'_>) {
        if let Some(state) = ctx.state.downcast_mut::<RequestState>() {
            state.bytes += ctx.chunk.len() as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::{HeaderMap, HeaderValue, Method, StatusCode};
    use serde_json::Value;
    use std::path::PathBuf;

    fn dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reiki-requestlog-lib-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plugin(extra: &str, file: &std::path::Path) -> Box<dyn Plugin> {
        let text = format!("file = \"{}\"\n{extra}", file.display());
        build(&text.parse().unwrap()).unwrap()
    }

    /// Drives one request through the three hooks the way the relay does.
    fn drive(plugin: &dyn Plugin, headers: &[(&str, &str)], body: &str, status: u16) {
        let mut state = plugin.new_state();
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        let mut payload = Bytes::from(body.to_owned());
        plugin
            .on_request(&mut RequestCtx {
                stream: "zen",
                upstream: "zen",
                method: &Method::POST,
                headers: &mut map,
                body: &mut payload,
                state: state.as_mut(),
            })
            .unwrap();
        let mut code = StatusCode::from_u16(status).unwrap();
        plugin.on_response_head(&mut ResponseHeadCtx {
            status: &mut code,
            headers: &mut HeaderMap::new(),
            state: state.as_mut(),
        });
        for (chunk, is_last) in [("hel", false), ("lo", false), ("", true)] {
            let mut bytes = Bytes::from(chunk);
            plugin.on_response_chunk(&mut ChunkCtx {
                chunk: &mut bytes,
                is_last,
                state: state.as_mut(),
            });
        }
    }

    fn lines(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn a_request_and_its_response_are_logged_and_correlated() {
        let dir = dir("pair");
        let path = dir.join("req.jsonl");
        let plugin = plugin("headers = [\"user-agent\"]", &path);
        drive(
            plugin.as_ref(),
            &[
                ("user-agent", "opencode/1.18.35"),
                ("authorization", "Bearer client-secret-token"),
            ],
            r#"{"model":"m","messages":[{"content":"hi"}]}"#,
            200,
        );
        drop(plugin);

        let records = lines(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["event"], "request");
        assert_eq!(records[1]["event"], "response");
        assert_eq!(records[0]["id"], records[1]["id"]);
        assert_eq!(records[0]["stream"], "zen");
        assert_eq!(records[0]["body"]["messages"][0]["content"], "hi");
        assert_eq!(records[1]["status"], 200);
        assert_eq!(records[1]["bytes"], 5);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("client-secret-token"),
            "credential leaked: {text}"
        );
        assert!(!text.to_lowercase().contains("authorization"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn response_logging_can_be_turned_off() {
        let dir = dir("noresp");
        let path = dir.join("req.jsonl");
        let plugin = plugin("response = \"none\"", &path);
        drive(plugin.as_ref(), &[], "{}", 200);
        drop(plugin);
        let records = lines(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["event"], "request");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requests_get_distinct_ids() {
        let dir = dir("ids");
        let path = dir.join("req.jsonl");
        let plugin = plugin("response = \"none\"", &path);
        for _ in 0..50 {
            drive(plugin.as_ref(), &[], "{}", 200);
        }
        drop(plugin);
        let ids: std::collections::HashSet<String> = lines(&path)
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids.len(), 50);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_config_or_unwritable_path_fails_the_build() {
        assert!(build(&"body = \"none\"".parse().unwrap()).is_err());
        let err = build(
            &"file = \"/nonexistent-dir-for-test/req.jsonl\""
                .parse()
                .unwrap(),
        )
        .err()
        .unwrap();
        assert!(err.starts_with("requestlog: cannot open"), "{err}");
    }

    #[test]
    fn a_full_queue_drops_records_instead_of_blocking() {
        let config = Config::parse(&"file = \"/unused\"\nqueue = 16".parse().unwrap()).unwrap();
        let (tx, _rx) = sync_channel::<Event>(config.queue);
        let log = RequestLog {
            config: Arc::new(config),
            tx: Some(tx),
            dropped: Arc::new(AtomicU64::new(0)),
            writer: None,
        };
        let started = Instant::now();
        for _ in 0..200 {
            drive(&log, &[], "{}", 200);
        }
        assert!(started.elapsed().as_secs() < 2, "hooks blocked");
        assert!(log.dropped.load(Ordering::Relaxed) > 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_stream_carries_the_same_records() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let dir = dir("stream");
        let sock = dir.join("req.sock");
        let text = format!("socket = \"{}\"\nresponse = \"none\"", sock.display());
        let plugin = build(&text.parse().unwrap()).unwrap();

        let client = UnixStream::connect(&sock).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        drive(plugin.as_ref(), &[], r#"{"model":"streamed"}"#, 200);

        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        let record: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(record["body"]["model"], "streamed");
        drop(plugin);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
