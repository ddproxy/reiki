use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::task::{Context, Poll};

use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use axum::body::Bytes;
use axum::http::request::Parts;
use axum::http::{
    header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Version,
};
use futures_core::Stream;
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use tokio::io::BufReader;
use tokio_util::io::{ReaderStream, StreamReader};

use crate::config::Upstream;
use crate::proxy::{passthrough_headers, upstream_headers};

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

/// The error a decoded body yields when it passes its configured cap. The proxy turns this into
/// a 502 before any of a buffered response is sent.
const BODY_LIMIT_MESSAGE: &str = "decoded response body exceeds the size limit";

/// True for the error [`Capped`] produces when a decoded body is over its limit.
pub fn is_body_limit_error(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::InvalidData && error.to_string() == BODY_LIMIT_MESSAGE
}

/// Fails the stream once more than `left` bytes have passed through.
struct Capped {
    inner: ByteStream,
    left: u64,
}

impl Stream for Capped {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => match this.left.checked_sub(chunk.len() as u64) {
                Some(left) => {
                    this.left = left;
                    Poll::Ready(Some(Ok(chunk)))
                }
                None => {
                    this.left = 0;
                    this.inner = Box::pin(OnceBody(None));
                    Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        BODY_LIMIT_MESSAGE,
                    ))))
                }
            },
            other => other,
        }
    }
}

fn capped(inner: ByteStream, max: u64) -> ByteStream {
    Box::pin(Capped { inner, left: max })
}

/// Fails a response body when it stops producing chunks for too long, or when it takes
/// longer than the optional total limit. The idle timer starts at the response head and
/// resets after every chunk.
struct DeadlineStream {
    inner: ByteStream,
    idle: Pin<Box<tokio::time::Sleep>>,
    idle_for: std::time::Duration,
    total: Option<Pin<Box<tokio::time::Sleep>>>,
    done: bool,
}

impl Stream for DeadlineStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        if this
            .total
            .as_mut()
            .is_some_and(|total| total.as_mut().poll(cx).is_ready())
        {
            this.done = true;
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the upstream response exceeded its total timeout",
            ))));
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Pending => {
                if this.idle.as_mut().poll(cx).is_ready() {
                    this.done = true;
                    Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "the upstream response idle timeout expired",
                    ))))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(Some(Ok(chunk))) => {
                this.idle
                    .as_mut()
                    .reset(tokio::time::Instant::now() + this.idle_for);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.done = true;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.done = true;
                Poll::Ready(None)
            }
        }
    }
}

pub struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub version: Version,
    body: ByteStream,
    /// True when Reiki decoded a `content-encoding` itself, so the body is no longer
    /// the bytes the upstream sent.
    decoded: bool,
}

impl UpstreamResponse {
    pub fn from_reqwest(response: reqwest::Response, max_decoded_bytes: u64) -> Self {
        let status = response.status();
        let mut headers = response.headers().clone();
        let version = response.version();
        let bodyless = status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED;
        let encoding = if bodyless {
            None
        } else {
            content_encoding(&headers)
        };
        let source: ByteStream = Box::pin(ErrMap {
            inner: Box::pin(response.bytes_stream()),
        });
        let body = match encoding {
            Some(Encoding::Gzip) => decode(source, max_decoded_bytes, GzipDecoder::new),
            Some(Encoding::Deflate) => decode(source, max_decoded_bytes, ZlibDecoder::new),
            Some(Encoding::Brotli) => decode(source, max_decoded_bytes, BrotliDecoder::new),
            Some(Encoding::Zstd) => decode(source, max_decoded_bytes, ZstdDecoder::new),
            None => capped(source, max_decoded_bytes),
        };
        if encoding.is_some() {
            headers.remove(header::CONTENT_ENCODING);
            headers.remove(header::CONTENT_LENGTH);
        }
        Self {
            status,
            headers,
            version,
            body,
            decoded: encoding.is_some(),
        }
    }

    pub fn from_hyper(
        response: hyper::Response<hyper::body::Incoming>,
        max_decoded_bytes: u64,
    ) -> Self {
        Self::from_hyper_inner(response, false, max_decoded_bytes)
    }

    /// A HEAD response carries no body to decode, whatever its headers claim.
    pub fn from_hyper_head(
        response: hyper::Response<hyper::body::Incoming>,
        max_decoded_bytes: u64,
    ) -> Self {
        Self::from_hyper_inner(response, true, max_decoded_bytes)
    }

    fn from_hyper_inner(
        response: hyper::Response<hyper::body::Incoming>,
        is_head: bool,
        max_decoded_bytes: u64,
    ) -> Self {
        let status = response.status();
        let mut headers = response.headers().clone();
        let version = response.version();
        let bodyless =
            is_head || status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED;
        let encoding = if bodyless {
            None
        } else {
            content_encoding(&headers)
        };
        let incoming = response.into_body();
        let source: ByteStream = Box::pin(ErrMap {
            inner: Box::pin(incoming.into_data_stream()),
        });
        let body = match encoding {
            Some(Encoding::Gzip) => decode(source, max_decoded_bytes, GzipDecoder::new),
            Some(Encoding::Deflate) => decode(source, max_decoded_bytes, ZlibDecoder::new),
            Some(Encoding::Brotli) => decode(source, max_decoded_bytes, BrotliDecoder::new),
            Some(Encoding::Zstd) => decode(source, max_decoded_bytes, ZstdDecoder::new),
            None => capped(source, max_decoded_bytes),
        };
        if encoding.is_some() {
            headers.remove(header::CONTENT_ENCODING);
            headers.remove(header::CONTENT_LENGTH);
        }
        Self {
            status,
            headers,
            version,
            body,
            decoded: encoding.is_some(),
        }
    }

    pub async fn chunk(&mut self) -> Option<Result<Bytes, std::io::Error>> {
        poll_fn(|cx| self.body.as_mut().poll_next(cx)).await
    }

    /// True when the body is the decoded form of a compressed upstream response.
    pub fn is_decoded(&self) -> bool {
        self.decoded
    }

    /// Bounds how long the body may take: the idle limit resets on every chunk, and the
    /// optional total limit starts now, once the response head is in hand.
    pub fn with_deadlines(
        mut self,
        idle: std::time::Duration,
        total: Option<std::time::Duration>,
    ) -> Self {
        self.body = Box::pin(DeadlineStream {
            inner: self.body,
            idle: Box::pin(tokio::time::sleep(idle)),
            idle_for: idle,
            total: total.map(|duration| Box::pin(tokio::time::sleep(duration))),
            done: false,
        });
        self
    }

    /// Reads the whole body into memory. The encoding cap still applies, so this cannot
    /// grow without bound; the caller uses it to turn an over-limit response into a clean
    /// 502 before any bytes reach the client.
    pub async fn buffer(mut self) -> Result<Self, std::io::Error> {
        let mut collected = Vec::new();
        while let Some(chunk) = self.chunk().await {
            collected.extend_from_slice(&chunk?);
        }
        Ok(Self::from_bytes(
            self.status,
            self.headers,
            self.version,
            Bytes::from(collected),
        ))
    }

    pub fn into_stream(self) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> {
        self.body
    }

    /// A body relayed as the forwarder produces it.
    pub fn from_stream(
        status: StatusCode,
        headers: HeaderMap,
        version: Version,
        body: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    ) -> Self {
        Self {
            status,
            headers,
            version,
            body: Box::pin(body),
            decoded: false,
        }
    }

    pub fn from_bytes(
        status: StatusCode,
        headers: HeaderMap,
        version: Version,
        body: Bytes,
    ) -> Self {
        Self {
            status,
            headers,
            version,
            body: Box::pin(OnceBody(Some(body))),
            decoded: false,
        }
    }
}

struct OnceBody(Option<Bytes>);

impl Stream for OnceBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.take().map(Ok))
    }
}

#[derive(Clone, Copy)]
enum Encoding {
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

/// Returns `None` for anything we cannot safely decode ourselves, so the client
/// receives exactly the bytes and headers the upstream chose.
fn content_encoding(headers: &HeaderMap) -> Option<Encoding> {
    let mut values = headers.get_all(header::CONTENT_ENCODING).iter();
    let value = values.next()?.to_str().ok()?.trim();
    if values.next().is_some() || value.contains(',') {
        return None;
    }
    match value.to_ascii_lowercase().as_str() {
        "gzip" | "x-gzip" => Some(Encoding::Gzip),
        "deflate" => Some(Encoding::Deflate),
        "br" => Some(Encoding::Brotli),
        "zstd" => Some(Encoding::Zstd),
        _ => None,
    }
}

fn decode<D, F>(source: ByteStream, max: u64, make: F) -> ByteStream
where
    D: tokio::io::AsyncRead + Send + 'static,
    F: FnOnce(BufReader<StreamReader<ByteStream, Bytes>>) -> D,
{
    capped(
        Box::pin(ReaderStream::new(make(BufReader::new(StreamReader::new(
            source,
        ))))),
        max,
    )
}

pub type SendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<UpstreamResponse, std::io::Error>> + Send + 'a>>;

/// One request to an upstream. `credential` is the auth header name and formatted
/// value to set, or `None` to send the client's own credentials untouched.
pub struct Call<'a> {
    pub parts: &'a Parts,
    pub url: &'a str,
    pub credential: Option<(&'a str, &'a str)>,
    pub body: Bytes,
    pub upstream: &'a Upstream,
}

/// Sends a [`Call`] to an upstream and returns its response head and body stream.
pub trait Transport: Send + Sync {
    fn send<'a>(&'a self, call: Call<'a>) -> SendFuture<'a>;
}

/// The reqwest transport: rebuilds the headers from the upstream's settings.
pub struct Standard {
    http1: reqwest::Client,
    http2: reqwest::Client,
    title_case: reqwest::Client,
}

impl Standard {
    pub fn new() -> Self {
        let build = |http1_only: bool, title_case: bool| {
            // Reiki decodes compressed bodies itself, so the configured cap applies to the
            // expanded bytes and error bodies stay readable. Letting reqwest decode would hide
            // `content-encoding` and the size from the relay.
            let builder = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .redirect(reqwest::redirect::Policy::none());
            let builder = if http1_only {
                builder.http1_only()
            } else {
                builder
            };
            let builder = if title_case {
                builder.http1_title_case_headers()
            } else {
                builder
            };
            builder.build().unwrap_or_else(|_| reqwest::Client::new())
        };
        Self {
            http1: build(true, false),
            http2: build(false, false),
            title_case: build(true, true),
        }
    }

    fn client_for(&self, upstream: &Upstream) -> &reqwest::Client {
        if upstream.http2 {
            &self.http2
        } else if upstream.title_case_headers {
            &self.title_case
        } else {
            &self.http1
        }
    }
}

impl Default for Standard {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for Standard {
    fn send<'a>(&'a self, call: Call<'a>) -> SendFuture<'a> {
        Box::pin(async move {
            let Call {
                parts,
                url,
                credential,
                body,
                upstream,
            } = call;
            let headers = match credential {
                Some((name, value)) => upstream_headers(
                    &parts.headers,
                    name,
                    value,
                    upstream.forward_accept_encoding,
                    upstream.send_keep_alive,
                ),
                None => passthrough_headers(
                    &parts.headers,
                    upstream.forward_accept_encoding,
                    upstream.send_keep_alive,
                ),
            };
            self.client_for(upstream)
                .request(parts.method.clone(), url)
                .headers(headers)
                .body(body)
                .send()
                .await
                .map(|response| {
                    let response =
                        UpstreamResponse::from_reqwest(response, upstream.max_decoded_bytes);
                    response.with_deadlines(
                        std::time::Duration::from_secs(upstream.response_idle_timeout_secs),
                        upstream
                            .response_timeout_secs
                            .map(std::time::Duration::from_secs),
                    )
                })
                .map_err(reqwest_error)
        })
    }
}

impl Transport for Faithful {
    fn send<'a>(&'a self, call: Call<'a>) -> SendFuture<'a> {
        Box::pin(async move {
            let request = faithful_request(call.parts, call.url, call.credential, call.body)?;
            Faithful::send(self, request, call.upstream.max_decoded_bytes)
                .await
                .map(|response| {
                    response.with_deadlines(
                        std::time::Duration::from_secs(call.upstream.response_idle_timeout_secs),
                        call.upstream
                            .response_timeout_secs
                            .map(std::time::Duration::from_secs),
                    )
                })
        })
    }
}

/// Marks a failure that happened while connecting, before any request bytes were sent.
/// The proxy may replay a non-idempotent request only for this kind of failure.
fn connect_error<E>(error: E) -> std::io::Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    std::io::Error::new(std::io::ErrorKind::ConnectionRefused, error)
}

fn reqwest_error(error: reqwest::Error) -> std::io::Error {
    if error.is_connect() {
        connect_error(error)
    } else {
        std::io::Error::other(error)
    }
}

fn hyper_error(error: hyper_util::client::legacy::Error) -> std::io::Error {
    if error.is_connect() {
        connect_error(error)
    } else {
        std::io::Error::other(error)
    }
}

pub struct Faithful {
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl Faithful {
    pub fn new() -> Self {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        let mut builder = Client::builder(hyper_util::rt::TokioExecutor::new());
        builder.http1_preserve_header_case(true);
        Self {
            client: builder.build(connector),
        }
    }

    pub async fn send(
        &self,
        request: Request<Full<Bytes>>,
        max_decoded_bytes: u64,
    ) -> Result<UpstreamResponse, std::io::Error> {
        let is_head = request.method() == Method::HEAD;
        let response = self.client.request(request).await.map_err(hyper_error)?;
        Ok(if is_head {
            UpstreamResponse::from_hyper_head(response, max_decoded_bytes)
        } else {
            UpstreamResponse::from_hyper(response, max_decoded_bytes)
        })
    }
}

impl Default for Faithful {
    fn default() -> Self {
        Self::new()
    }
}

/// Removes the fixed hop-by-hop headers and every header the sender's `Connection` names, so a
/// header Reiki sets later cannot be stripped by an intermediary that honours that list.
/// With `keep_alive` the `Connection` header stays where it was, reduced to `keep-alive`.
pub(crate) fn strip_hop_by_hop(headers: &mut HeaderMap, keep_alive: bool) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .filter(|name| name != header::CONNECTION)
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "keep-alive",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "expect",
    ] {
        headers.remove(name);
    }
    if keep_alive {
        headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    } else {
        headers.remove(header::CONNECTION);
    }
}

pub fn faithful_request(
    parts: &Parts,
    url: &str,
    credential: Option<(&str, &str)>,
    body: Bytes,
) -> Result<Request<Full<Bytes>>, std::io::Error> {
    let parsed = reqwest::Url::parse(url).map_err(std::io::Error::other)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| std::io::Error::other("upstream URL has no host"))?;
    let authority = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };

    let mut headers = parts.headers.clone();
    let extensions = parts.extensions.clone();

    let keep_connection = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("keep-alive"));
    strip_hop_by_hop(&mut headers, keep_connection);
    headers.remove(header::CONTENT_LENGTH);

    if let Some((name, value)) = credential {
        for other in ["authorization", "x-api-key", "x-goog-api-key"] {
            if !other.eq_ignore_ascii_case(name) {
                headers.remove(other);
            }
        }
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(std::io::Error::other)?;
        let mut value = HeaderValue::from_str(value).map_err(std::io::Error::other)?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }

    let host = HeaderValue::from_str(&authority).map_err(std::io::Error::other)?;
    headers.insert(header::HOST, host);

    let mut request = Request::new(Full::new(body));
    *request.method_mut() = parts.method.clone();
    *request.uri_mut() = url.parse().map_err(std::io::Error::other)?;
    *request.version_mut() = Version::HTTP_11;
    *request.headers_mut() = headers;
    *request.extensions_mut() = extensions;
    Ok(request)
}

/// Keeps the reqwest body stream usable without pulling in `futures-util`.
pub(crate) struct ErrMap<S> {
    pub(crate) inner: Pin<Box<S>>,
}

impl<S, E> Stream for ErrMap<S>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut()
            .inner
            .as_mut()
            .poll_next(cx)
            .map(|item| item.map(|result| result.map_err(std::io::Error::other)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn from_bytes_yields_the_body_once() {
        let mut response = UpstreamResponse::from_bytes(
            StatusCode::OK,
            HeaderMap::new(),
            Version::HTTP_11,
            Bytes::from_static(b"hi"),
        );
        let chunk = response.chunk().await.unwrap().unwrap();
        assert_eq!(&chunk[..], b"hi");
        assert!(response.chunk().await.is_none());
    }

    #[tokio::test]
    async fn a_body_past_the_decoded_cap_fails_then_ends() {
        let parts: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"defg")),
            Ok(Bytes::from_static(b"more")),
        ];
        let mut capped = Capped {
            inner: Box::pin(futures_util::stream::iter(parts)),
            left: 5,
        };
        let first = poll_fn(|cx| Pin::new(&mut capped).poll_next(cx)).await;
        assert_eq!(&first.unwrap().unwrap()[..], b"abc");
        let second = poll_fn(|cx| Pin::new(&mut capped).poll_next(cx)).await;
        assert!(second.unwrap().is_err());
        let third = poll_fn(|cx| Pin::new(&mut capped).poll_next(cx)).await;
        assert!(third.is_none());
    }
}
