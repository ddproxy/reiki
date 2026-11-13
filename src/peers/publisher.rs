//! The peer listener: `GET /v1/health` for holders of the peer token, 404 for everything else.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;

use crate::admin::{bearer_token, text_response, throttled, unauthorized};
use crate::auth::{constant_time_eq, sha256_hex, LoginThrottle, Peer, ThrottleSettings};
use crate::config::PeersConfig;
use crate::proxy::App;

use super::sessions::Sessions;
use super::stream::{ensure_ticker, Frames, Hub, Refused, HEARTBEAT, STREAM_PATH};

pub(crate) const HEALTH_PATH: &str = "/v1/health";

/// State of the peer listener that outlives a reload.
pub struct Publisher {
    /// Kept apart from the client and admin counts, so peer guessing locks out only peers.
    logins: LoginThrottle,
    sessions: Sessions,
    hub: Arc<Hub>,
}

impl Publisher {
    pub(crate) fn new(throttle: ThrottleSettings) -> Arc<Self> {
        Arc::new(Self {
            logins: LoginThrottle::new(throttle),
            sessions: Sessions::new(),
            hub: Hub::new(),
        })
    }

    pub(crate) fn logins(&self) -> &LoginThrottle {
        &self.logins
    }

    pub(crate) fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// Counts the session id of one client request, once per request so retries add nothing.
    pub(crate) fn observe_session(&self, peers: &PeersConfig, stream: &str, headers: &HeaderMap) {
        if !peers.publishing() || !peers.expose_host_stats {
            return;
        }
        let Some(id) = headers
            .get(peers.session_header.as_str())
            .and_then(|value| value.to_str().ok())
        else {
            return;
        };
        let window = Duration::from_secs(peers.session_window_secs);
        self.sessions.observe(stream, id, window, Instant::now());
    }
}

/// The peer listener's routes, for its own socket and never the client or admin one.
pub fn router(app: Arc<App>) -> Router {
    let _ = app.peer_publisher();
    Router::new().fallback(handle).with_state(app)
}

fn not_found() -> Response {
    text_response(StatusCode::NOT_FOUND, "text/plain", String::new())
}

async fn handle(State(app): State<Arc<App>>, req: Request) -> Response {
    let snapshot = app.snapshot();
    // A reload that turns publishing off closes the door before the restart that drops the socket.
    let Some(peers) = snapshot.config.peers.as_ref().filter(|p| p.publishing()) else {
        return not_found();
    };
    let path = req.uri().path();
    if req.method() != Method::GET || (path != HEALTH_PATH && path != STREAM_PATH) {
        return not_found();
    }
    let peer = req.extensions().get::<Peer>().map(|p| p.0);
    if let Err(refusal) = authorize(&app, peers, peer, req.headers()) {
        return *refusal;
    }
    let mut response = if path == STREAM_PATH {
        subscribe(&app, peers, peer)
    } else {
        let body = super::health::document(&app, peers).to_string();
        text_response(StatusCode::OK, "application/json", body)
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Opens one event stream, or refuses it when the subscriber caps are reached.
fn subscribe(app: &Arc<App>, peers: &PeersConfig, peer: Option<IpAddr>) -> Response {
    let hub = Arc::clone(&app.peer_publisher().hub);
    ensure_ticker(app, &hub);
    let limits = (
        peers.max_subscribers as usize,
        peers.max_subscribers_per_peer as usize,
    );
    let subscribed = hub.subscribe(peer, limits, HEARTBEAT, || {
        super::stream::frame("snapshot", &super::health::document(app, peers))
    });
    match subscribed {
        Ok(inbox) => {
            let mut response = Response::new(axum::body::Body::from_stream(Frames(inbox)));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            response
                .headers_mut()
                .insert("x-accel-buffering", HeaderValue::from_static("no"));
            response
        }
        Err(refused) => {
            tracing::warn!(?peer, ?refused, "peer stream refused");
            let status = match refused {
                Refused::Full => StatusCode::SERVICE_UNAVAILABLE,
                Refused::PeerFull => StatusCode::TOO_MANY_REQUESTS,
            };
            let mut response = text_response(status, "text/plain", String::new());
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
            response
        }
    }
}

/// Checks the bearer token against `peer_token_sha256` alone, so no admin or stream token opens
/// this listener; failures are logged with the peer address, never the token.
fn authorize(
    app: &App,
    peers: &PeersConfig,
    peer: Option<IpAddr>,
    headers: &HeaderMap,
) -> Result<(), Box<Response>> {
    let logins = app.peer_publisher().logins();
    if let Some(ip) = peer {
        if let Some(wait) = logins.blocked(ip, Instant::now()) {
            tracing::warn!(?peer, "too many failed peer logins, refusing");
            return Err(Box::new(throttled(wait)));
        }
    }
    match token_matches(peers, headers) {
        Ok(()) => {
            if let Some(ip) = peer {
                logins.succeed(ip);
            }
            Ok(())
        }
        Err(reason) => {
            let failures = peer.map(|ip| logins.fail(ip, Instant::now()));
            tracing::warn!(?peer, reason, ?failures, "failed peer login");
            Err(Box::new(unauthorized()))
        }
    }
}

fn token_matches(peers: &PeersConfig, headers: &HeaderMap) -> Result<(), &'static str> {
    let Some(expected) = peers.peer_token_sha256.as_deref() else {
        return Err("no peer token configured");
    };
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err("missing authorization header");
    };
    let Ok(value) = value.to_str() else {
        return Err("authorization header is not text");
    };
    let Some(token) = bearer_token(value) else {
        return Err("not a bearer credential");
    };
    if constant_time_eq(&expected.to_ascii_lowercase(), &sha256_hex(token)) {
        Ok(())
    } else {
        Err("wrong token")
    }
}
