use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Bytes;
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode, Version};
use futures_core::Stream;
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;

pub struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub version: Version,
    body: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
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
        let status = response.status();
        let headers = response.headers().clone();
        let version = response.version();
        let body = Box::pin(ErrMap {
            inner: Box::pin(response.into_body().into_data_stream()),
        });
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
        let response = self
            .client
            .request(request)
            .await
            .map_err(std::io::Error::other)?;
        Ok(UpstreamResponse::from_hyper(response))
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
