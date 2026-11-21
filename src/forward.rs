//! Hands a request to a forwarder as plain HTTP/1.1, over a Unix socket or the stdio of a command.
//!
//! The forwarder can be anything that speaks HTTP: a reverse proxy, a queue producer, another
//! harness. Its answer is relayed to the client, and a failure of the link itself is never
//! treated as a verdict on the key.

use std::fmt;
use std::pin::Pin;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{header, HeaderName, HeaderValue, Method, Request, StatusCode, Version};
use futures_core::Stream;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::config::Forward;
use crate::proxy::{passthrough_headers, upstream_headers};
use crate::upstream::{strip_hop_by_hop, Call, ErrMap, SendFuture, Transport, UpstreamResponse};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_HEADERS: usize = 128;
const MAX_HEADER_VALUE: usize = 64 * 1024;
const MAX_HEAD_BYTES: usize = 256 * 1024;
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(1);

/// A streamed forwarder body has no content length to settle against, so it needs its own cap
/// and total deadline instead of the request header timeout.
const DEFAULT_MAX_STREAM_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_STREAM_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Byte cap and total deadline applied to a streamed forwarder body.
struct StreamLimits {
    max_bytes: u64,
    total: Duration,
}

type ForwardStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

/// The forwarder link failed; carries no key material.
#[derive(Debug)]
pub struct ForwarderError(pub String);

impl fmt::Display for ForwarderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ForwarderError {}

fn fail(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(ForwarderError(message.into()))
}

/// Caps the copies of each command running at once, so unauthenticated or hostile traffic
/// cannot spawn processes without bound.
#[derive(Default)]
pub struct Forwarder {
    limits:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Semaphore>>>,
}

const DEFAULT_MAX_CONCURRENT: u32 = 16;

impl Forwarder {
    fn permit(&self, spec: &Forward) -> Result<tokio::sync::OwnedSemaphorePermit, std::io::Error> {
        let limit = spec.max_concurrent.unwrap_or(DEFAULT_MAX_CONCURRENT);
        let key = format!("{}\0{limit}", spec.command.as_deref().unwrap_or_default());
        let semaphore = {
            let mut limits = self
                .limits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::sync::Arc::clone(limits.entry(key).or_insert_with(|| {
                std::sync::Arc::new(tokio::sync::Semaphore::new(limit as usize))
            }))
        };
        semaphore
            .try_acquire_owned()
            .map_err(|_| fail("too many copies of the forward command are running"))
    }
}

impl Transport for Forwarder {
    fn send<'a>(&'a self, call: Call<'a>) -> SendFuture<'a> {
        Box::pin(async move {
            let spec = call
                .upstream
                .forward
                .as_ref()
                .ok_or_else(|| fail("forward transport used without [upstream.forward]"))?;
            let request = build_request(&call).map_err(fail)?;
            let idle = Duration::from_secs(call.upstream.response_idle_timeout_secs);
            let total = call.upstream.response_timeout_secs.map(Duration::from_secs);
            let stream_limits = StreamLimits {
                max_bytes: spec.max_stream_bytes.unwrap_or(DEFAULT_MAX_STREAM_BYTES),
                total: spec
                    .stream_timeout_secs
                    .map(Duration::from_secs)
                    .unwrap_or(DEFAULT_STREAM_TIMEOUT),
            };
            match (&spec.socket, &spec.command) {
                (Some(path), _) => {
                    exchange_socket(
                        path,
                        spec.expect_uid,
                        request,
                        spec.streaming,
                        idle,
                        total,
                        stream_limits,
                    )
                    .await
                }
                (None, Some(_)) => {
                    let permit = self.permit(spec)?;
                    exchange_command(spec, request, permit, idle, total, stream_limits).await
                }
                (None, None) => Err(fail("[upstream.forward] has neither socket nor command")),
            }
        })
    }
}

/// The request as a reverse proxy would receive it: origin-form target, `Host`, and the
/// credential in its normal header.
fn build_request(call: &Call<'_>) -> Result<Request<Full<Bytes>>, String> {
    let url = reqwest::Url::parse(call.url).map_err(|e| e.to_string())?;
    let host = url.host_str().ok_or("upstream URL has no host")?;
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let mut target = url.path().to_owned();
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }

    let upstream = call.upstream;
    let mut inbound = call.parts.headers.clone();
    strip_hop_by_hop(&mut inbound, false);
    for name in [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-real-ip",
    ] {
        inbound.remove(name);
    }
    let mut headers = match call.credential {
        Some((name, value)) => upstream_headers(
            &inbound,
            name,
            value,
            upstream.forward_accept_encoding,
            upstream.send_keep_alive,
        ),
        None => passthrough_headers(
            &inbound,
            upstream.forward_accept_encoding,
            upstream.send_keep_alive,
        ),
    };
    for name in [header::CONTENT_LENGTH, header::CONNECTION, header::HOST] {
        headers.remove(name);
    }
    headers.insert(
        header::HOST,
        HeaderValue::from_str(&authority).map_err(|e| e.to_string())?,
    );
    headers.insert(
        HeaderName::from_static("x-forwarded-proto"),
        HeaderValue::from_str(url.scheme()).map_err(|e| e.to_string())?,
    );
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));

    let mut request = Request::new(Full::new(call.body.clone()));
    *request.method_mut() = call.parts.method.clone();
    *request.uri_mut() = target.parse().map_err(|e| format!("{e}"))?;
    *request.version_mut() = Version::HTTP_11;
    *request.headers_mut() = headers;
    Ok(request)
}

#[cfg(unix)]
async fn exchange_socket(
    path: &std::path::Path,
    expect_uid: Option<u32>,
    request: Request<Full<Bytes>>,
    streaming: bool,
    idle: Duration,
    total: Option<Duration>,
    stream_limits: StreamLimits,
) -> Result<UpstreamResponse, std::io::Error> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|e| fail(format!("cannot connect to {}: {e}", path.display())))?;
    if let Some(expected) = expect_uid {
        let peer = stream
            .peer_cred()
            .map_err(|e| fail(format!("cannot read the peer of {}: {e}", path.display())))?;
        if peer.uid() != expected {
            return Err(fail(format!(
                "{} is served by uid {}, expected {expected}",
                path.display(),
                peer.uid()
            )));
        }
    }
    exchange(stream, request, streaming, None, idle, total, stream_limits).await
}

#[cfg(not(unix))]
async fn exchange_socket(
    _path: &std::path::Path,
    _expect_uid: Option<u32>,
    _request: Request<Full<Bytes>>,
    _streaming: bool,
    _idle: Duration,
    _total: Option<Duration>,
    _stream_limits: StreamLimits,
) -> Result<UpstreamResponse, std::io::Error> {
    Err(fail("socket forwarding needs a Unix platform"))
}

async fn exchange_command(
    spec: &Forward,
    request: Request<Full<Bytes>>,
    permit: tokio::sync::OwnedSemaphorePermit,
    idle: Duration,
    total: Option<Duration>,
    stream_limits: StreamLimits,
) -> Result<UpstreamResponse, std::io::Error> {
    let command = spec.command.as_deref().unwrap_or_default();
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(&spec.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .env_clear();
    for name in ["PATH", "HOME", "LANG", "LC_ALL"]
        .into_iter()
        .chain(spec.pass_env.iter().map(String::as_str))
    {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| fail(format!("cannot start `{command}`: {e}")))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(fail("forward command stdio was not piped"));
    };
    let guard = ChildGuard {
        child: Some(child),
        permit: Some(permit),
        kill_now: false,
    };
    exchange(
        tokio::io::join(stdout, stdin),
        request,
        spec.streaming,
        Some(guard),
        idle,
        total,
        stream_limits,
    )
    .await
}

/// Owns a forward command and its concurrency slot. Dropping it reaps the child off the request
/// path: one that lingers past a grace period, or whose exchange failed, is killed.
struct ChildGuard {
    child: Option<tokio::process::Child>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    kill_now: bool,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let (Some(mut child), permit, kill_now) =
            (self.child.take(), self.permit.take(), self.kill_now)
        else {
            return;
        };
        // Without a runtime (shutdown) `kill_on_drop` stops the child instead.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let _permit = permit;
            if kill_now
                || tokio::time::timeout(CHILD_EXIT_GRACE, child.wait())
                    .await
                    .is_err()
            {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        });
    }
}

/// A relayed body that keeps its forwarder alive until the body ends or is dropped.
struct Guarded<S> {
    inner: S,
    _guard: Option<ChildGuard>,
}

impl<S> futures_core::Stream for Guarded<S>
where
    S: futures_core::Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// One request and its response over `io`; every failure is a [`ForwarderError`].
///
/// The body is buffered whole unless `streaming` is set, in which case the response is returned
/// as soon as its head is in and the body is relayed as it arrives under its own byte cap and
/// total deadline. `guard` keeps a forward command alive for as long as its body is still being read.
async fn exchange<T>(
    io: T,
    request: Request<Full<Bytes>>,
    streaming: bool,
    mut guard: Option<ChildGuard>,
    idle: Duration,
    total: Option<Duration>,
    stream_limits: StreamLimits,
) -> Result<UpstreamResponse, std::io::Error>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let is_head = request.method() == Method::HEAD;
    let result = exchange_inner(
        io,
        request,
        streaming,
        is_head,
        idle,
        total,
        stream_limits.max_bytes,
    )
    .await;
    match result {
        Ok((Relayed::Buffered(response), _)) => Ok(response),
        Ok((Relayed::Streaming(parts, body), connection)) => {
            // The connection has to keep running for the body to make progress.
            tokio::spawn(async move {
                let _ = connection.await;
            });
            let mut headers = parts.headers;
            strip_hop_by_hop(&mut headers, false);
            let stream = Guarded {
                inner: body,
                _guard: guard.take(),
            };
            Ok(
                UpstreamResponse::from_stream(parts.status, headers, Version::HTTP_11, stream)
                    .with_deadlines(idle, Some(stream_limits.total)),
            )
        }
        Err(error) => {
            if let Some(guard) = guard.as_mut() {
                guard.kill_now = true;
            }
            Err(error)
        }
    }
}

type Connection<T> =
    std::pin::Pin<Box<hyper::client::conn::http1::Connection<TokioIo<T>, Full<Bytes>>>>;

enum Relayed {
    Buffered(UpstreamResponse),
    Streaming(axum::http::response::Parts, ForwardStream),
}

async fn exchange_inner<T>(
    io: T,
    request: Request<Full<Bytes>>,
    streaming: bool,
    is_head: bool,
    idle: Duration,
    total: Option<Duration>,
    max_stream: u64,
) -> Result<(Relayed, Connection<T>), std::io::Error>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
        .max_headers(MAX_RESPONSE_HEADERS)
        .max_buf_size(MAX_HEAD_BYTES)
        .handshake(TokioIo::new(io))
        .await
        .map_err(|e| fail(format!("handshake failed: {e}")))?;
    let mut connection: Connection<T> = Box::pin(connection);

    let work = async {
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| fail(format!("request failed: {e}")))?;
        let (parts, body) = response.into_parts();
        let status = parts.status;
        if !(200..=599).contains(&status.as_u16()) {
            return Err(fail(format!("forwarder answered status {status}")));
        }
        if parts
            .headers
            .values()
            .any(|value| value.len() > MAX_HEADER_VALUE)
        {
            return Err(fail("a response header value is over 64 KiB"));
        }
        if streaming {
            // The body has no content length to settle against yet, so cap it here rather than
            // after the response head has already been handed to the client.
            let source = ErrMap {
                inner: Box::pin(body.into_data_stream()),
            };
            let body = crate::upstream::capped_body(Box::pin(source), max_stream);
            return Ok(Relayed::Streaming(parts, body));
        }
        // The body cap and the response timeouts apply to the forwarder's answer just
        // like to an upstream's, whether it is buffered or streamed.
        let body = ErrMap {
            inner: Box::pin(Limited::new(body, MAX_RESPONSE_BYTES).into_data_stream()),
        };
        let mut response =
            UpstreamResponse::from_stream(status, parts.headers, Version::HTTP_11, body)
                .with_deadlines(idle, total);
        let mut collected = Vec::new();
        while let Some(chunk) = response.chunk().await {
            collected
                .extend_from_slice(&chunk.map_err(|e| fail(format!("response body failed: {e}")))?);
        }
        let body = Bytes::from(collected);
        let mut headers = response.headers;
        let original_length = headers.get(header::CONTENT_LENGTH).cloned();
        strip_hop_by_hop(&mut headers, false);
        headers.remove(header::CONTENT_LENGTH);
        // HEAD keeps the length the forwarder reported, and 204/304 carry none.
        if is_head {
            if let Some(length) = original_length {
                headers.insert(header::CONTENT_LENGTH, length);
            }
        } else if status != StatusCode::NO_CONTENT && status != StatusCode::NOT_MODIFIED {
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        }
        Ok(Relayed::Buffered(UpstreamResponse::from_bytes(
            status,
            headers,
            Version::HTTP_11,
            body,
        )))
    };
    tokio::pin!(work);

    let mut connection_done = false;
    let finished = loop {
        tokio::select! {
            biased;
            result = &mut work => break result?,
            ended = &mut connection, if !connection_done => {
                connection_done = true;
                if let Err(e) = ended {
                    // A reset that arrives with an already complete answer must not discard it.
                    break tokio::select! {
                        biased;
                        result = &mut work => result?,
                        () = std::future::ready(()) => {
                            return Err(fail(format!("connection failed: {e}")));
                        }
                    };
                }
            }
        }
    };
    Ok((finished, connection))
}
#[cfg(unix)]
pub fn socket_warnings(config: &crate::config::Config) -> Vec<String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut upstreams: Vec<_> = config.upstream.iter().collect();
    upstreams.sort_by_key(|(name, _)| *name);
    let mut warnings = Vec::new();
    for (name, upstream) in upstreams {
        let Some(forward) = upstream.forward.as_ref() else {
            continue;
        };
        let Some(path) = forward.socket.as_ref() else {
            continue;
        };
        if forward.expect_uid.is_none() {
            if let Some(meta) = path.parent().and_then(|p| std::fs::metadata(p).ok()) {
                if meta.mode() & 0o022 != 0 {
                    warnings.push(format!(
                        "upstream `{name}`: {} is in a directory others can write to; set `expect_uid` so the key is not sent to another process's socket",
                        path.display()
                    ));
                }
            }
        }
        match std::fs::symlink_metadata(path) {
            Err(_) => warnings.push(format!(
                "upstream `{name}`: socket {} does not exist yet (the forwarder may start later)",
                path.display()
            )),
            Ok(meta) if !meta.file_type().is_socket() => warnings.push(format!(
                "upstream `{name}`: {} exists but is not a socket",
                path.display()
            )),
            Ok(_) => {}
        }
    }
    warnings
}

#[cfg(not(unix))]
pub fn socket_warnings(_config: &crate::config::Config) -> Vec<String> {
    Vec::new()
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::{self as unixfs, PermissionsExt};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::time::Duration;

    use axum::body::Bytes;
    use axum::http::{Method, Request};

    use crate::config::{Config, ConfigError};
    use crate::upstream::{Call, Transport};

    use super::{socket_warnings, Forwarder};

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn minimal_base(extra: &str) -> String {
        format!(
            r#"
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"

[keys.zen-a]
upstream = "zen"
file = "/etc/reiki/keys/zen-a"
{extra}
"#
        )
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "reiki-warn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_bare_forward_command_resolves_against_the_child_path() {
        // `env_clear` is applied before spawn, and Rust resolves a bare program name using the
        // child's PATH, not the one Reiki inherited. The command forwarder therefore cannot run
        // a binary from the inherited PATH that the sanitised child environment does not name.
        let dir = temp_dir();
        let name = format!("reiki-path-probe-{}", std::process::id());
        let script = dir.join(&name);
        fs::write(&script, "#!/bin/sh\nprintf child-path\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let output = tokio::process::Command::new(&name)
            .env_clear()
            .env("PATH", &dir)
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(output.stdout, b"child-path");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_forward_request_always_closes_the_connection() {
        let text = minimal_base(&format!(
            "[upstream.zen.forward]\nsocket = \"/run/x.sock\"\n[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        let config = Config::parse(&text).unwrap();
        let mut upstream = config.upstream["zen"].clone();
        upstream.send_keep_alive = true;
        let upstream = &upstream;
        let (parts, _) = Request::builder()
            .method(Method::POST)
            .uri("/x")
            .header("connection", "keep-alive")
            .body(())
            .unwrap()
            .into_parts();
        let call = Call {
            parts: &parts,
            url: "http://example.test/v1/x",
            credential: Some(("authorization", "Bearer k")),
            body: Bytes::new(),
            upstream,
        };
        let request = super::build_request(&call).unwrap();
        assert_eq!(request.headers()["connection"], "close");
        assert_eq!(request.headers().get_all("connection").iter().count(), 1);
    }

    #[test]
    fn missing_path_warns() {
        let dir = temp_dir();
        let path = dir.join("missing.sock");
        let text = minimal_base(&format!(
            "[upstream.zen.forward]\nsocket = \"{}\"\n[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n",
            path.display()
        ));
        let config = Config::parse(&text).unwrap();
        let warnings = socket_warnings(&config);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("does not exist yet"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn regular_file_warns_not_a_socket() {
        let dir = temp_dir();
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        unixfs::PermissionsExt::set_mode(&mut perms, 0o700);
        fs::set_permissions(&dir, perms).unwrap();
        let path = dir.join("file");
        fs::write(&path, "x").unwrap();
        let text = minimal_base(&format!(
            "[upstream.zen.forward]\nsocket = \"{}\"\n[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n",
            path.display()
        ));
        let config = Config::parse(&text).unwrap();
        let warnings = socket_warnings(&config);
        assert!(warnings
            .iter()
            .any(|w| w.contains("exists but is not a socket")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_socket_in_0700_gives_no_warning() {
        let dir = temp_dir();
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        unixfs::PermissionsExt::set_mode(&mut perms, 0o700);
        fs::set_permissions(&dir, perms).unwrap();
        let path = dir.join("sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let text = minimal_base(&format!(
            "[upstream.zen.forward]\nsocket = \"{}\"\n[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n",
            path.display()
        ));
        let config = Config::parse(&text).unwrap();
        let warnings = socket_warnings(&config);
        assert!(warnings.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_socket_in_a_group_or_other_writable_directory_without_the_sticky_bit_is_refused_at_load() {
        for mode in [0o775, 0o777] {
            let dir = temp_dir();
            let mut perms = fs::metadata(&dir).unwrap().permissions();
            unixfs::PermissionsExt::set_mode(&mut perms, mode);
            fs::set_permissions(&dir, perms).unwrap();
            let path = dir.join("sock");
            let text = minimal_base(&format!(
                "[upstream.zen.forward]\nsocket = \"{}\"\n[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n",
                path.display()
            ));
            let err = Config::parse(&text).unwrap_err();
            assert!(
                matches!(err, ConfigError::BadForward { .. }),
                "mode {mode:04o}: {err}"
            );
            let message = err.to_string();
            assert!(message.contains("expect_uid"), "mode {mode:04o}: {message}");
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_socket_in_a_sticky_or_private_directory_is_allowed_without_expect_uid() {
        for mode in [0o1777, 0o700] {
            let dir = temp_dir();
            let mut perms = fs::metadata(&dir).unwrap().permissions();
            unixfs::PermissionsExt::set_mode(&mut perms, mode);
            fs::set_permissions(&dir, perms).unwrap();
            let path = dir.join("sock");
            let text = minimal_base(&format!(
                "[upstream.zen.forward]\nsocket = \"{}\"\n[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n",
                path.display()
            ));
            Config::parse(&text).unwrap_or_else(|err| panic!("mode {mode:04o}: {err}"));
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn expect_uid_allows_a_socket_in_a_writable_directory() {
        let dir = temp_dir();
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        unixfs::PermissionsExt::set_mode(&mut perms, 0o777);
        fs::set_permissions(&dir, perms).unwrap();
        let path = dir.join("sock");
        let text = minimal_base(&format!(
            "[upstream.zen.forward]\nsocket = \"{}\"\nexpect_uid = 1000\n[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n",
            path.display()
        ));
        Config::parse(&text).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn socket_in_a_sticky_writable_directory_warns_and_expect_uid_suppresses() {
        let dir = temp_dir();
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        // Sticky: allowed at load, so the warning path is what is left to test here.
        unixfs::PermissionsExt::set_mode(&mut perms, 0o1777);
        fs::set_permissions(&dir, perms).unwrap();
        let path = dir.join("sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let text = minimal_base(&format!(
            "[upstream.zen.forward]\nsocket = \"{}\"\n[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n",
            path.display()
        ));
        let config = Config::parse(&text).unwrap();
        let warnings = socket_warnings(&config);
        assert!(warnings.iter().any(|w| w.contains("others can write")));
        let text2 = minimal_base(&format!(
            "[upstream.zen.forward]\nsocket = \"{}\"\nexpect_uid = 1000\n[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n",
            path.display()
        ));
        let config2 = Config::parse(&text2).unwrap();
        let warnings2 = socket_warnings(&config2);
        assert!(!warnings2.iter().any(|w| w.contains("others can write")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_socket_in_a_sticky_writable_directory_still_warns_about_the_directory() {
        let dir = temp_dir();
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        // Sticky: allowed at load, so the warning path is what is left to test here.
        unixfs::PermissionsExt::set_mode(&mut perms, 0o1777);
        fs::set_permissions(&dir, perms).unwrap();
        let path = dir.join("later.sock");
        let text = minimal_base(&format!(
            "[upstream.zen.forward]\nsocket = \"{}\"\n[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n",
            path.display()
        ));
        let warnings = socket_warnings(&Config::parse(&text).unwrap());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("others can write to")));
        assert!(warnings.iter().any(|w| w.contains("does not exist yet")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_timed_out_command_releases_its_concurrency_slot() {
        let script = "printf 'HTTP/1.1 200 OK\\r\\nContent-Length: 10\\r\\nConnection: close\\r\\n\\r\\nabc'; sleep 30";
        let text = format!(
            "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
             response_idle_timeout_secs = 1\n\
             [upstream.zen.forward]\ncommand = \"sh\"\nargs = [\"-c\", {script:?}]\nmax_concurrent = 1\n\
             [keys.zen-a]\nupstream = \"zen\"\nfile = \"/unused\"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        );
        let config = Config::parse(&text).unwrap();
        let upstream = &config.upstream["zen"];
        let parts = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let forwarder = Forwarder::default();
        let call = || Call {
            parts: &parts,
            url: "https://example.test/v1/chat",
            credential: Some(("authorization", "Bearer key")),
            body: Bytes::from_static(b"{}"),
            upstream,
        };
        let first = match tokio::time::timeout(Duration::from_secs(5), forwarder.send(call()))
            .await
            .expect("the command's stalled body times out")
        {
            Err(error) => error,
            Ok(_) => panic!("the stalled body is an error"),
        };
        assert!(first.to_string().contains("timeout"), "{first}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let second = match tokio::time::timeout(Duration::from_secs(5), forwarder.send(call()))
            .await
            .expect("the second call does not hang")
        {
            Err(error) => error,
            Ok(_) => panic!("the second command stalls too"),
        };
        assert!(
            !second.to_string().contains("too many copies"),
            "the timed-out command still holds its slot: {second}"
        );
    }

    fn streaming_config(extra: &str) -> Config {
        Config::parse(&format!(
            "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
             [upstream.zen.forward]\ncommand = \"sh\"\n\
             args = [\"-c\", {SCRIPT:?}]\nstreaming = true\n{extra}\n\
             [keys.zen-a]\nupstream = \"zen\"\nfile = \"/unused\"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ))
        .unwrap()
    }

    const SCRIPT: &str = "printf 'HTTP/1.1 200 OK\\r\\nContent-Type: text/event-stream\\r\\nContent-Length: 100000\\r\\nConnection: close\\r\\n\\r\\n'; yes x | head -c 100000";

    async fn streamed_response(config: &Config) -> crate::upstream::UpstreamResponse {
        let parts = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let call = Call {
            parts: &parts,
            url: "https://example.test/v1/chat",
            credential: Some(("authorization", "Bearer key")),
            body: Bytes::from_static(b"{}"),
            upstream: &config.upstream["zen"],
        };
        tokio::time::timeout(Duration::from_secs(5), Forwarder::default().send(call))
            .await
            .expect("the forwarder answers")
            .expect("the streamed response is returned with its head")
    }

    #[tokio::test]
    async fn a_streamed_forward_body_fails_when_it_passes_its_byte_cap() {
        let config = streaming_config("max_stream_bytes = 1024\n");
        let mut response = streamed_response(&config).await;
        let mut error = None;
        while let Some(chunk) = response.chunk().await {
            if let Err(e) = chunk {
                error = Some(e);
                break;
            }
        }
        let error = error.expect("the body ends with a limit error");
        assert!(
            crate::upstream::is_body_limit_error(&error),
            "unexpected stream error: {error}"
        );
    }

    #[test]
    fn stream_limits_default_to_64_mib_and_30_minutes_and_reject_zero() {
        let config = streaming_config("");
        let forward = config.upstream["zen"].forward.as_ref().unwrap();
        assert_eq!(forward.max_stream_bytes, None);
        assert_eq!(forward.stream_timeout_secs, None);
        assert_eq!(super::DEFAULT_MAX_STREAM_BYTES, 64 * 1024 * 1024);
        assert_eq!(super::DEFAULT_STREAM_TIMEOUT, Duration::from_secs(1800));

        for (field, value) in [("max_stream_bytes", "0"), ("stream_timeout_secs", "0")] {
            let text = format!(
                "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
                 [upstream.zen.forward]\ncommand = \"sh\"\n{field} = {value}\n"
            );
            assert!(
                Config::parse(&text).is_err(),
                "{field} = {value} is refused"
            );
        }
    }
}
