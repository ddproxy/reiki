use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;

use crate::auth::{constant_time_eq, extract_token, sha256_hex, strip_query_key};
use crate::chain::{Chain, KeyState, Policy};
use crate::classify::{classify, FailureClass, Outcome};
use crate::config::Config;

const MAX_REQUEST_BODY: usize = 8 * 1024 * 1024;
const MAX_ERROR_BODY: usize = 64 * 1024;
const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(120);
const HEALTH_PATH: &str = "/__reiki/healthz";

pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug)]
pub enum LoadError {
    Read { key: String, source: std::io::Error },
    Empty(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { key, source } => write!(f, "cannot read key file for `{key}`: {source}"),
            Self::Empty(key) => write!(f, "key file for `{key}` is empty"),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Empty(_) => None,
        }
    }
}

pub fn load_keys(config: &Config) -> Result<HashMap<String, Secret>, LoadError> {
    let mut keys = HashMap::new();
    for (name, key) in &config.keys {
        let raw = std::fs::read_to_string(&key.file).map_err(|source| LoadError::Read {
            key: name.clone(),
            source,
        })?;
        let value = raw.trim();
        if value.is_empty() {
            return Err(LoadError::Empty(name.clone()));
        }
        keys.insert(name.clone(), Secret::new(value));
    }
    Ok(keys)
}

pub struct App {
    config: Config,
    keys: HashMap<String, Secret>,
    health: Mutex<KeyState>,
    chains: HashMap<String, Mutex<Chain>>,
    policy: Policy,
    client: reqwest::Client,
    header_timeout: Duration,
}

impl App {
    pub fn new(config: Config, keys: HashMap<String, Secret>, policy: Policy) -> Arc<Self> {
        Self::with_header_timeout(config, keys, policy, RESPONSE_HEADER_TIMEOUT)
    }

    pub fn with_header_timeout(
        config: Config,
        keys: HashMap<String, Secret>,
        policy: Policy,
        header_timeout: Duration,
    ) -> Arc<Self> {
        let chains = config
            .streams
            .iter()
            .map(|(name, stream)| {
                (
                    name.clone(),
                    Mutex::new(Chain::new(stream.chain.clone(), stream.strategy.into())),
                )
            })
            .collect();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Arc::new(Self {
            config,
            keys,
            health: Mutex::new(KeyState::default()),
            chains,
            policy,
            client,
            header_timeout,
        })
    }

    fn authenticate(&self, token: &str) -> Option<&str> {
        let hash = sha256_hex(token);
        let mut found = None;
        // Check every stream so timing does not reveal which one matched.
        for (name, stream) in &self.config.streams {
            if constant_time_eq(&stream.token_sha256.to_ascii_lowercase(), &hash) {
                found = Some(name.as_str());
            }
        }
        found
    }

    fn pick(&self, stream: &str, upstream: &str, skip: &[String]) -> Option<String> {
        let health = self.health.lock().ok()?;
        let mut chain = self.chains.get(stream)?.lock().ok()?;
        let keys = &self.config.keys;
        chain
            .pick_where(&health, Instant::now(), |k| {
                !skip.iter().any(|s| s == k) && keys.get(k).is_some_and(|c| c.upstream == upstream)
            })
            .map(str::to_owned)
    }

    fn soonest_recovery(&self, stream: &str) -> Option<Duration> {
        let health = self.health.lock().ok()?;
        let chain = self.chains.get(stream)?.lock().ok()?;
        health.soonest_recovery(chain.keys(), Instant::now())
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}

fn json_error(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = format!(
        "{{\"error\":{{\"type\":\"{kind}\",\"message\":\"{}\"}}}}",
        message.replace('"', "'")
    );
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn skip_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
            | "authorization"
            | "x-api-key"
            | "x-goog-api-key"
            | "accept-encoding"
    )
}

fn upstream_headers(inbound: &HeaderMap, auth_header: &str, auth_value: &str) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in inbound {
        if !skip_header(name) {
            out.append(name.clone(), value.clone());
        }
    }
    // Compressed error bodies cannot be classified, so ask for plain responses.
    out.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    if let (Ok(name), Ok(mut value)) = (
        HeaderName::from_bytes(auth_header.as_bytes()),
        HeaderValue::from_str(auth_value),
    ) {
        value.set_sensitive(true);
        out.insert(name, value);
    }
    out
}

fn split_upstream(path: &str) -> Option<(&str, &str)> {
    let trimmed = path.strip_prefix('/')?;
    let (name, rest) = match trimmed.find('/') {
        Some(i) => (&trimmed[..i], &trimmed[i..]),
        None => (trimmed, ""),
    };
    if name.is_empty() {
        None
    } else {
        Some((name, rest))
    }
}

async fn handle(State(app): State<Arc<App>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    if parts.uri.path() == HEALTH_PATH && parts.method == Method::GET {
        return Response::new(Body::from("ok"));
    }

    let Some(token) = extract_token(&parts.headers, parts.uri.query()) else {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized", "missing token");
    };
    let Some(stream) = app.authenticate(&token) else {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized", "unknown token");
    };
    let stream = stream.to_owned();

    let Some((upstream_name, rest)) = split_upstream(parts.uri.path()) else {
        return json_error(StatusCode::NOT_FOUND, "not_found", "no upstream in path");
    };
    let Some(upstream) = app.config.upstream.get(upstream_name) else {
        return json_error(StatusCode::NOT_FOUND, "not_found", "unknown upstream");
    };

    let Ok(body) = to_bytes(body, MAX_REQUEST_BODY).await else {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "request body too large",
        );
    };

    let mut url = format!("{}{}", upstream.base_url.trim_end_matches('/'), rest);
    if let Some(query) = parts.uri.query().and_then(strip_query_key) {
        url.push('?');
        url.push_str(&query);
    }

    // Enough attempts to walk the chain once, plus one retry of a transient failure.
    let max_attempts = app
        .chains
        .get(&stream)
        .map_or(1, |_| app.config.streams[&stream].chain.len() + 1);

    let mut skip: Vec<String> = Vec::new();
    let mut last_refusal: Option<Response> = None;

    for _ in 0..max_attempts {
        let Some(key_name) = app.pick(&stream, upstream_name, &skip) else {
            break;
        };
        let Some(secret) = app.keys.get(&key_name) else {
            break;
        };
        let auth_value = upstream.auth_format.replace("{key}", &secret.0);
        let headers = upstream_headers(&parts.headers, &upstream.auth_header, &auth_value);

        let sent = tokio::time::timeout(
            app.header_timeout,
            app.client
                .request(parts.method.clone(), &url)
                .headers(headers)
                .body(body.clone())
                .send(),
        )
        .await;

        let response = match sent {
            Ok(Ok(response)) => response,
            Ok(Err(_)) | Err(_) => {
                record(
                    &app,
                    &stream,
                    &key_name,
                    crate::classify::FailureClass::Transient,
                    None,
                );
                continue;
            }
        };

        let status = response.status();
        tracing::info!(
            stream = %stream,
            key = %key_name,
            method = %parts.method,
            path = %parts.uri.path(),
            status = status.as_u16(),
            "forwarded"
        );
        if status.is_success() || status.is_redirection() {
            if let Ok(mut health) = app.health.lock() {
                health.record_success(&key_name);
            }
            return stream_response(response);
        }

        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let response_headers = response.headers().clone();
        let snippet = read_limited(response).await;
        let text = String::from_utf8_lossy(&snippet);

        match classify(status.as_u16(), retry_after.as_deref(), &text) {
            Outcome::Success | Outcome::PassThrough => {
                return buffered_response(status, &response_headers, snippet);
            }
            Outcome::KeyFailure(FailureClass::AccessDisabled) => {
                // "Model access is disabled" can depend on the endpoint or model of this one
                // request (a client may be probing /responses before falling back to
                // /chat/completions), so it must not put the whole key on cooldown. Try the
                // next key; if none helps, hand the upstream's own answer back to the caller.
                tracing::debug!(status = status.as_u16(), body = %text.chars().take(160).collect::<String>(), "upstream refused this request");
                tracing::warn!(stream = %stream, key = %key_name, status = status.as_u16(), "key refused this request, trying the next key");
                skip.push(key_name);
                last_refusal = Some(buffered_response(status, &response_headers, snippet));
            }
            Outcome::KeyFailure(class) => {
                tracing::debug!(status = status.as_u16(), body = %text.chars().take(160).collect::<String>(), "upstream error");
                record(&app, &stream, &key_name, class, Some(status.as_u16()));
            }
        }
    }

    if let Some(refusal) = last_refusal {
        return refusal;
    }
    exhausted(&app, &stream)
}

fn record(
    app: &App,
    stream: &str,
    key: &str,
    class: crate::classify::FailureClass,
    status: Option<u16>,
) {
    if let Ok(mut health) = app.health.lock() {
        let cooled = health.record_failure(key, class, &app.policy, Instant::now());
        tracing::warn!(stream, key, ?class, ?status, cooled, "upstream key failed");
    }
}

fn exhausted(app: &App, stream: &str) -> Response {
    let mut response = json_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "reiki_exhausted",
        "every key in this stream's chain is unavailable",
    );
    if let Some(wait) = app.soonest_recovery(stream) {
        let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        if let Ok(value) = HeaderValue::from_str(&secs.max(1).to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

async fn read_limited(mut response: reqwest::Response) -> Bytes {
    let mut collected = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = MAX_ERROR_BODY.saturating_sub(collected.len());
        collected.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if collected.len() >= MAX_ERROR_BODY {
            break;
        }
    }
    Bytes::from(collected)
}

fn copy_response_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from {
        if !skip_header(name) {
            to.append(name.clone(), value.clone());
        }
    }
}

fn stream_response(response: reqwest::Response) -> Response {
    let status = response.status();
    let mut headers = HeaderMap::new();
    copy_response_headers(response.headers(), &mut headers);
    // Pass the length through when the upstream gave one, so non-chunked bodies stay non-chunked.
    if let Some(length) = response.headers().get(header::CONTENT_LENGTH) {
        headers.insert(header::CONTENT_LENGTH, length.clone());
    }
    let mut out = Response::new(Body::from_stream(response.bytes_stream()));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

fn buffered_response(status: StatusCode, headers: &HeaderMap, body: Bytes) -> Response {
    let mut out = Response::new(Body::from(body));
    *out.status_mut() = status;
    copy_response_headers(headers, out.headers_mut());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_upstream_name_off_the_path() {
        assert_eq!(
            split_upstream("/zen/chat/completions"),
            Some(("zen", "/chat/completions"))
        );
        assert_eq!(split_upstream("/zen"), Some(("zen", "")));
        assert_eq!(split_upstream("/"), None);
        assert_eq!(split_upstream("//x"), None);
    }

    #[test]
    fn secrets_never_print() {
        assert_eq!(format!("{:?}", Secret::new("sk-123")), "Secret(<redacted>)");
    }

    #[test]
    fn caller_credentials_are_not_forwarded_and_the_upstream_key_is_set() {
        let mut inbound = HeaderMap::new();
        inbound.insert(
            "authorization",
            HeaderValue::from_static("Bearer caller-token"),
        );
        inbound.insert("x-api-key", HeaderValue::from_static("caller-token"));
        inbound.insert("x-goog-api-key", HeaderValue::from_static("caller-token"));
        inbound.insert("user-agent", HeaderValue::from_static("opencode/1.0"));
        inbound.insert("x-custom", HeaderValue::from_static("keep"));
        inbound.insert("connection", HeaderValue::from_static("close"));
        inbound.insert("host", HeaderValue::from_static("127.0.0.1:4097"));
        let out = upstream_headers(&inbound, "authorization", "Bearer upstream-key");
        assert_eq!(out["authorization"], "Bearer upstream-key");
        assert_eq!(out["user-agent"], "opencode/1.0");
        assert_eq!(out["x-custom"], "keep");
        assert!(out.get("x-api-key").is_none());
        assert!(out.get("x-goog-api-key").is_none());
        assert!(out.get("connection").is_none());
        assert!(out.get("host").is_none());
        assert!(!out
            .values()
            .any(|v| v.to_str().is_ok_and(|s| s.contains("caller-token"))));
    }
}
