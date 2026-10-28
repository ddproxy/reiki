//! Hands a request to a forwarder as plain HTTP/1.1, over a Unix socket or the stdio of a command.
//!
//! The forwarder can be anything that speaks HTTP: a reverse proxy, a queue producer, another
//! harness. Its answer is relayed to the client, and a failure of the link itself is never
//! treated as a verdict on the key.

use std::fmt;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{header, HeaderName, HeaderValue, Method, Request, StatusCode, Version};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::config::Forward;
use crate::proxy::{passthrough_headers, upstream_headers};
use crate::upstream::{strip_hop_by_hop, Call, SendFuture, Transport, UpstreamResponse};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_HEADERS: usize = 128;
const MAX_HEADER_VALUE: usize = 64 * 1024;
const MAX_HEAD_BYTES: usize = 256 * 1024;
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(1);

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
            match (&spec.socket, &spec.command) {
                (Some(path), _) => exchange_socket(path, spec.expect_uid, request).await,
                (None, Some(_)) => {
                    let permit = self.permit(spec)?;
                    exchange_command(spec, request, permit).await
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
    exchange(stream, request).await
}

#[cfg(not(unix))]
async fn exchange_socket(
    _path: &std::path::Path,
    _expect_uid: Option<u32>,
    _request: Request<Full<Bytes>>,
) -> Result<UpstreamResponse, std::io::Error> {
    Err(fail("socket forwarding needs a Unix platform"))
}

async fn exchange_command(
    spec: &Forward,
    request: Request<Full<Bytes>>,
    permit: tokio::sync::OwnedSemaphorePermit,
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
    let result = exchange(tokio::io::join(stdout, stdin), request).await;
    // Reap off the request path: the answer is already complete, and a child that
    // lingers is stopped after a grace period instead of being left running.
    let failed = result.is_err();
    tokio::spawn(async move {
        let _permit = permit;
        if failed
            || tokio::time::timeout(CHILD_EXIT_GRACE, child.wait())
                .await
                .is_err()
        {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    });
    result
}

/// One request and its fully buffered response over `io`; every failure is a [`ForwarderError`].
async fn exchange<T>(
    io: T,
    request: Request<Full<Bytes>>,
) -> Result<UpstreamResponse, std::io::Error>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let is_head = request.method() == Method::HEAD;
    let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
        .max_headers(MAX_RESPONSE_HEADERS)
        .max_buf_size(MAX_HEAD_BYTES)
        .handshake(TokioIo::new(io))
        .await
        .map_err(|e| fail(format!("handshake failed: {e}")))?;
    tokio::pin!(connection);

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
        let body = Limited::new(body, MAX_RESPONSE_BYTES)
            .collect()
            .await
            .map_err(|e| fail(format!("response body failed: {e}")))?
            .to_bytes();
        let mut headers = parts.headers;
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
        Ok(UpstreamResponse::from_bytes(
            status,
            headers,
            Version::HTTP_11,
            body,
        ))
    };
    tokio::pin!(work);

    let mut connection_done = false;
    loop {
        tokio::select! {
            biased;
            result = &mut work => return result,
            ended = &mut connection, if !connection_done => {
                connection_done = true;
                if let Err(e) = ended {
                    // A reset that arrives with an already complete answer must not discard it.
                    return tokio::select! {
                        biased;
                        result = &mut work => result,
                        () = std::future::ready(()) => Err(fail(format!("connection failed: {e}"))),
                    };
                }
            }
        }
    }
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
    use std::os::unix::fs as unixfs;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    use crate::config::Config;

    use super::socket_warnings;

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
    fn socket_in_0777_warns_and_expect_uid_suppresses() {
        let dir = temp_dir();
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        unixfs::PermissionsExt::set_mode(&mut perms, 0o777);
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
    fn a_missing_socket_in_a_writable_directory_still_warns_about_the_directory() {
        let dir = temp_dir();
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        unixfs::PermissionsExt::set_mode(&mut perms, 0o777);
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
}
