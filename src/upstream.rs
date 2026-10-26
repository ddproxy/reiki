use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, Version};
use futures_core::Stream;

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

    pub async fn chunk(&mut self) -> Option<Result<Bytes, std::io::Error>> {
        poll_fn(|cx| self.body.as_mut().poll_next(cx)).await
    }

    pub fn into_stream(self) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> {
        self.body
    }
}

/// Keeps the reqwest body stream usable without pulling in `futures-util`.
struct ErrMap<S> {
    inner: Pin<Box<S>>,
}

impl<S> Stream for ErrMap<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
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
