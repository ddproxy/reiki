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

/// Most bytes a decoded response body may reach, so a small compressed body cannot expand without bound.
const MAX_DECODED_BYTES: u64 = 512 * 1024 * 1024;

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
                        "decoded response body exceeds the size limit",
                    ))))
                }
            },
            other => other,
        }
    }
}

fn capped(inner: ByteStream) -> ByteStream {
    Box::pin(Capped {
        inner,
        left: MAX_DECODED_BYTES,
    })
}

pub struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub version: Version,
    body: ByteStream,
}

impl UpstreamResponse {
    pub fn from_reqwest(response: reqwest::Response) -> Self {
        let status = response.status();
        let headers = response.headers().clone();
        let version = response.version();
        let body = capped(Box::pin(ErrMap {
            inner: Box::pin(response.bytes_stream()),
        }));
        Self {
            status,
            headers,
            version,
            body,
        }
    }

    pub fn from_hyper(response: hyper::Response<hyper::body::Incoming>) -> Self {
        Self::from_hyper_inner(response, false)
    }

    /// A HEAD response carries no body to decode, whatever its headers claim.
    pub fn from_hyper_head(response: hyper::Response<hyper::body::Incoming>) -> Self {
        Self::from_hyper_inner(response, true)
    }

    fn from_hyper_inner(response: hyper::Response<hyper::body::Incoming>, is_head: bool) -> Self {
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
        let body = match encoding {
            Some(Encoding::Gzip) => decode(incoming, GzipDecoder::new),
            Some(Encoding::Deflate) => decode(incoming, ZlibDecoder::new),
            Some(Encoding::Brotli) => decode(incoming, BrotliDecoder::new),
            Some(Encoding::Zstd) => decode(incoming, ZstdDecoder::new),
            None => Box::pin(ErrMap {
                inner: Box::pin(incoming.into_data_stream()),
            }),
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
        }
    }

    pub async fn chunk(&mut self) -> Option<Result<Bytes, std::io::Error>> {
        poll_fn(|cx| self.body.as_mut().poll_next(cx)).await
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

fn decode<D, F>(body: hyper::body::Incoming, make: F) -> ByteStream
where
    D: tokio::io::AsyncRead + Send + 'static,
    F: FnOnce(BufReader<StreamReader<ByteStream, Bytes>>) -> D,
{
    let source: ByteStream = Box::pin(ErrMap {
        inner: Box::pin(body.into_data_stream()),
    });
    capped(Box::pin(ReaderStream::new(make(BufReader::new(
        StreamReader::new(source),
    )))))
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
            let builder = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
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
                .map(UpstreamResponse::from_reqwest)
                .map_err(std::io::Error::other)
        })
    }
}

impl Transport for Faithful {
    fn send<'a>(&'a self, call: Call<'a>) -> SendFuture<'a> {
        Box::pin(async move {
            let request = faithful_request(call.parts, call.url, call.credential, call.body)?;
            Faithful::send(self, request).await
        })
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
    ) -> Result<UpstreamResponse, std::io::Error> {
        let is_head = request.method() == Method::HEAD;
        let response = self
            .client
            .request(request)
            .await
            .map_err(std::io::Error::other)?;
        Ok(if is_head {
            UpstreamResponse::from_hyper_head(response)
        } else {
            UpstreamResponse::from_hyper(response)
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
