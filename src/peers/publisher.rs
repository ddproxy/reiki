//! The peer listener: `GET /v1/health` for holders of the peer token, 404 for everything else.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;

use crate::admin::{text_response, throttled, unauthorized};
use crate::auth::{
    bearer_token, constant_time_eq, sha256_hex, LoginThrottle, Peer, ThrottleSettings,
};
use crate::config::{PeerToken, PeersConfig};
use crate::proxy::App;

use super::sessions::Sessions;
use super::stream::{ensure_ticker, Frames, Hub, Limits, Refused, HEARTBEAT, STREAM_PATH};

pub(crate) const HEALTH_PATH: &str = "/v1/health";

/// The address a caller is told apart by, or `None` for one arriving from loopback. Tunnels
/// (`ssh -L`, WireGuard forwarding) and a local TLS-terminating proxy all connect from loopback,
/// so on such a connection the source says nothing about which peer is calling.
fn distinct_source(peer: Option<IpAddr>) -> Option<IpAddr> {
    peer.filter(|ip| !ip.is_loopback() && !ipv4_mapped_loopback(*ip))
}

fn ipv4_mapped_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
        IpAddr::V4(_) => false,
    }
}

/// State of the peer listener that outlives a reload.
pub struct Publisher {
    /// Kept apart from the client and admin counts, so peer guessing locks out only peers.
    logins: LoginThrottle,
    sessions: Sessions,
    hub: Arc<Hub>,
    /// Whether the running listener serves HTTPS, set once when it starts.
    listener_tls: OnceLock<bool>,
    /// The certificate the HTTPS listener hands each new connection, and its fingerprint.
    tls: RwLock<Option<(String, Arc<rustls::ServerConfig>)>>,
}

/// What a reload does to the peer listener's TLS.
#[derive(Default)]
pub(crate) struct TlsReload {
    /// Switching between HTTP and HTTPS needs a new listener.
    pub(crate) restart: bool,
    swap: Option<super::tls::Loaded>,
}

impl TlsReload {
    pub(crate) fn swaps(&self) -> bool {
        self.swap.is_some()
    }
}

impl Publisher {
    pub(crate) fn new(throttle: ThrottleSettings) -> Arc<Self> {
        Arc::new(Self {
            logins: LoginThrottle::new(throttle),
            sessions: Sessions::new(),
            hub: Hub::new(),
            listener_tls: OnceLock::new(),
            tls: RwLock::new(None),
        })
    }

    fn current_tls(&self) -> Option<Arc<rustls::ServerConfig>> {
        self.tls
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|(_, config)| Arc::clone(config))
    }

    fn set_tls(&self, loaded: super::tls::Loaded) {
        *self.tls.write().unwrap_or_else(PoisonError::into_inner) =
            Some((loaded.fingerprint, loaded.config));
    }

    /// Loads the TLS files a reload asks for, without applying them; an error fails the reload.
    pub(crate) fn plan_tls(&self, next: Option<&PeersConfig>) -> Result<TlsReload, String> {
        let Some(&running) = self.listener_tls.get() else {
            return Ok(TlsReload::default());
        };
        // Stopping or moving the listener is a `peers.listen` restart already.
        let Some(peers) = next.filter(|p| p.publishing()) else {
            return Ok(TlsReload::default());
        };
        if peers.tls_files().is_some() != running {
            return Ok(TlsReload {
                restart: true,
                swap: None,
            });
        }
        let Some(loaded) = super::tls::load(peers)? else {
            return Ok(TlsReload::default());
        };
        let same = self
            .tls
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|(fingerprint, _)| *fingerprint == loaded.fingerprint);
        Ok(TlsReload {
            restart: false,
            swap: (!same).then_some(loaded),
        })
    }

    /// Hands new connections the certificate a planned reload loaded; open ones keep theirs.
    pub(crate) fn apply_tls(&self, plan: TlsReload) {
        if let Some(loaded) = plan.swap {
            self.set_tls(loaded);
        }
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

/// Serves the peer listener on `listener`: HTTPS when `[peers]` has `tls_cert` and `tls_key`,
/// plain HTTP otherwise. Fails when the certificate or key cannot be used.
pub fn serve(
    listener: tokio::net::TcpListener,
    app: Arc<App>,
    max_connections: usize,
) -> Result<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>, String> {
    serve_with_per_addr(listener, app, max_connections, 4)
}

/// [`serve`] with an explicit cap on open connections per remote address.
pub fn serve_with_per_addr(
    listener: tokio::net::TcpListener,
    app: Arc<App>,
    max_connections: usize,
    max_connections_per_addr: usize,
) -> Result<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>, String> {
    let loaded = match app.snapshot().config.peers.as_ref() {
        Some(peers) => super::tls::load(peers)?,
        None => None,
    };
    let publisher = Arc::clone(app.peer_publisher());
    let _ = publisher.listener_tls.set(loaded.is_some());
    let routes = router(app);
    let shutdown = crate::lifecycle::stopped();
    let Some(loaded) = loaded else {
        return Ok(Box::pin(crate::serve::serve_per_addr(
            listener,
            routes,
            max_connections,
            max_connections_per_addr,
            shutdown,
        )));
    };
    let fallback = Arc::clone(&loaded.config);
    publisher.set_tls(loaded);
    let source: crate::serve::TlsSource = Arc::new(move || {
        publisher
            .current_tls()
            .unwrap_or_else(|| Arc::clone(&fallback))
    });
    Ok(Box::pin(crate::serve::serve_tls_per_addr(
        listener,
        routes,
        max_connections,
        max_connections_per_addr,
        source,
        shutdown,
    )))
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
    let mut response = match authorize(&app, peers, peer, req.headers()).await {
        Ok(token) if path == STREAM_PATH => subscribe(&app, peers, peer, token).await,
        Ok(_) => {
            let body = super::health::document(&app, peers).await.to_string();
            text_response(StatusCode::OK, "application/json", body)
        }
        Err(refusal) => *refusal,
    };
    // Never let a refused credential keep a keep-alive slot alive for the next guess.
    if matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::TOO_MANY_REQUESTS
    ) {
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Opens one event stream, or refuses it when the subscriber caps are reached.
async fn subscribe(
    app: &Arc<App>,
    peers: &PeersConfig,
    peer: Option<IpAddr>,
    token: &str,
) -> Response {
    let hub = Arc::clone(&app.peer_publisher().hub);
    ensure_ticker(app, &hub);
    let limits = Limits {
        total: peers.max_subscribers as usize,
        per_peer: peers.max_subscribers_per_peer as usize,
        per_token: peers.max_subscribers_per_token as usize,
    };
    // Behind a loopback tunnel every stream shares one address; the token cap tells them apart.
    // Joined before the snapshot is built, so a change while it is built still reaches the stream.
    let subscribed = match hub.join(distinct_source(peer), token, limits) {
        Ok(joined) => {
            let snapshot = super::health::document(app, peers).await;
            Ok(joined.start(super::stream::frame("snapshot", &snapshot), HEARTBEAT))
        }
        Err(refused) => Err(refused),
    };
    if subscribed.is_ok() {
        ensure_ticker(app, &hub);
    }
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
            tracing::warn!(?peer, token, ?refused, "peer stream refused");
            let status = match refused {
                Refused::Full => StatusCode::SERVICE_UNAVAILABLE,
                Refused::PeerFull | Refused::TokenFull => StatusCode::TOO_MANY_REQUESTS,
            };
            let mut response = text_response(status, "text/plain", String::new());
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
            response
        }
    }
}

/// Checks the bearer token against the peer token hashes alone, so no admin or stream token
/// opens this listener, and returns the matching token's name. Failures are logged with the peer
/// address, never the token.
///
/// Failures count per source address (IPv6 by /64), loopback included, and lock that address out
/// as on the other listeners. A success never clears a count here, so valid requests between
/// guesses do not reset it. Peers behind one tunnel share the loopback address and therefore its
/// lockout; keep tokens high entropy so a stale one is not guessed.
async fn authorize<'a>(
    app: &App,
    peers: &'a PeersConfig,
    peer: Option<IpAddr>,
    headers: &HeaderMap,
) -> Result<&'a str, Box<Response>> {
    let logins = app.peer_publisher().logins();
    let key = peer.unwrap_or(IpAddr::from([0, 0, 0, 0]));
    if let Some(wait) = logins.blocked(key, Instant::now()) {
        tracing::warn!(?peer, "too many failed peer logins, refusing");
        return Err(Box::new(throttled(wait)));
    }
    match token_matches(peers, headers) {
        Ok(name) => {
            tracing::debug!(?peer, token = name, "peer authenticated");
            Ok(name)
        }
        Err(reason) => {
            let failures = logins.fail(key, Instant::now());
            tracing::warn!(?peer, reason, failures, "failed peer login");
            Err(Box::new(unauthorized()))
        }
    }
}

fn token_matches<'a>(peers: &'a PeersConfig, headers: &HeaderMap) -> Result<&'a str, &'static str> {
    if peers.tokens.is_empty() {
        return Err("no peer token configured");
    }
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err("missing authorization header");
    };
    let Ok(value) = value.to_str() else {
        return Err("authorization header is not text");
    };
    let Some(token) = bearer_token(value) else {
        return Err("not a bearer credential");
    };
    matching_token(&peers.tokens, &sha256_hex(token)).ok_or("wrong token")
}

/// The name of the token whose hash is `presented`. Every hash is compared, whatever matched
/// before, so the time taken does not tell which token, or how many, came first.
fn matching_token<'a>(tokens: &'a [PeerToken], presented: &str) -> Option<&'a str> {
    let mut found = None;
    for token in tokens {
        let hit = constant_time_eq(&token.token_sha256.to_ascii_lowercase(), presented);
        found = found.or(hit.then_some(token.name.as_str()));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_configured_token_matches_and_names_itself() {
        let token = |name: &str, secret: &str| PeerToken {
            name: name.to_owned(),
            token_sha256: sha256_hex(secret).to_ascii_uppercase(),
        };
        let tokens = [token("default", "one"), token("peer-b", "two")];
        assert_eq!(matching_token(&tokens, &sha256_hex("one")), Some("default"));
        assert_eq!(matching_token(&tokens, &sha256_hex("two")), Some("peer-b"));
        assert_eq!(matching_token(&tokens, &sha256_hex("three")), None);
        assert_eq!(matching_token(&[], &sha256_hex("one")), None);
    }

    const GOOD: &str = "good-peer-token-for-unit-tests";
    const OTHER: &str = "other-peer-token-for-unit-tests";

    fn app() -> Arc<App> {
        let text = format!(
            "[upstream.zen]\nbase_url = \"https://example.com/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"/unused\"\n\
             [peers]\nenabled = true\npublish = true\nlisten = \"127.0.0.1:4099\"\n\
             peer_token_sha256 = \"{}\"\n\
             [[peers.tokens]]\nname = \"peer-b\"\ntoken_sha256 = \"{}\"\n",
            sha256_hex(GOOD),
            sha256_hex(OTHER)
        );
        App::new(
            crate::config::Config::parse(&text).unwrap(),
            [("a".to_owned(), crate::proxy::Secret::new("k"))].into(),
            crate::chain::Policy::default(),
            crate::plugins::PluginChain::default(),
        )
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    /// The status `token` gets from `ip`, or 200 when it is let in.
    async fn status(app: &App, ip: &str, token: &str) -> u16 {
        let snapshot = app.snapshot();
        let peers = snapshot.config.peers.as_ref().unwrap();
        let peer = Some(ip.parse().unwrap());
        match authorize(app, peers, peer, &bearer(token)).await {
            Ok(_) => 200,
            Err(response) => response.status().as_u16(),
        }
    }

    #[tokio::test]
    async fn a_distinct_address_is_locked_out_per_address_and_by_ipv6_prefix() {
        let app = app();
        for _ in 0..10 {
            assert_eq!(status(&app, "192.0.2.1", "wrong").await, 401);
        }
        assert_eq!(status(&app, "192.0.2.1", GOOD).await, 429);
        assert_eq!(status(&app, "192.0.2.2", GOOD).await, 200);
        for _ in 0..10 {
            assert_eq!(status(&app, "2001:db8::1", "wrong").await, 401);
        }
        assert_eq!(status(&app, "2001:db8::ffff", GOOD).await, 429, "same /64");
        assert_eq!(status(&app, "2001:db8:0:1::1", GOOD).await, 200);
    }

    #[tokio::test]
    async fn a_valid_token_does_not_reset_the_failure_count() {
        let app = app();
        for _ in 0..9 {
            assert_eq!(status(&app, "192.0.2.7", "wrong").await, 401);
        }
        assert_eq!(status(&app, "192.0.2.7", GOOD).await, 200);
        assert_eq!(status(&app, "192.0.2.7", "wrong").await, 401);
        assert_eq!(
            status(&app, "192.0.2.7", GOOD).await,
            429,
            "the tenth failure counts despite the success between"
        );
    }

    async fn refusal(app: &App, ip: &str, token: &str) -> Response {
        let snapshot = app.snapshot();
        let peers = snapshot.config.peers.as_ref().unwrap();
        match authorize(app, peers, Some(ip.parse().unwrap()), &bearer(token)).await {
            Ok(_) => panic!("expected a refusal"),
            Err(response) => *response,
        }
    }

    #[tokio::test]
    async fn loopback_failures_lock_out_the_address_even_for_a_valid_token() {
        let app = app();
        for _ in 0..9 {
            assert_eq!(status(&app, "127.0.0.1", "stale-token").await, 401);
        }
        assert_eq!(
            status(&app, "127.0.0.1", GOOD).await,
            200,
            "below the threshold a valid token still gets in"
        );
        assert_eq!(status(&app, "127.0.0.1", "stale-token").await, 401);
        let response = refusal(&app, "127.0.0.1", GOOD).await;
        assert_eq!(response.status(), 429);
        assert!(
            response.headers().contains_key(header::RETRY_AFTER),
            "the lockout says when to retry"
        );
        assert_eq!(
            status(&app, "192.0.2.9", GOOD).await,
            200,
            "a distinct address has its own count"
        );
    }
}
