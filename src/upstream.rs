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
        let body = Box::pin(ErrMap {
            inner: Box::pin(response.bytes_stream()),
        });
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
    Box::pin(ReaderStream::new(make(BufReader::new(StreamReader::new(
        source,
    )))))
}

type SendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<UpstreamResponse, std::io::Error>> + Send + 'a>>;

/// One request to an upstream. `credential` is the auth header name and value to set,
/// or `None` to send the client's own credentials untouched.
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

    for name in [
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",
    ] {
        headers.remove(name);
    }
    let keep_connection = headers
        .get_all(header::CONNECTION)
        .iter()
        .next()
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"keep-alive"));
    if !keep_connection {
        headers.remove(header::CONNECTION);
    }

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
struct ErrMap<S> {
    inner: Pin<Box<S>>,
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
