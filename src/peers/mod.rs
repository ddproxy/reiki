//! Health sharing between Reiki instances, only built with the `peers` feature and only active
//! with an enabled `[peers]` table.

mod health;
mod openapi;
mod publisher;

pub use openapi::openapi_paths;
pub use publisher::{router, Publisher};
