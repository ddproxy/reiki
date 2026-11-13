//! Health sharing between Reiki instances, only built with the `peers` feature and only active
//! with an enabled `[peers]` table.

pub mod client;
mod health;
mod host;
mod openapi;
mod publisher;
mod sessions;
mod stream;
pub mod tls;

pub use openapi::openapi_paths;
pub(crate) use publisher::TlsReload;
pub use publisher::{router, serve, Publisher};
