//! Helpers shared by the integration tests.

use std::path::Path;

/// Writes a key file with owner-only permissions, as the proxy requires by default.
#[allow(dead_code)]
pub fn write_key(path: impl AsRef<Path>, text: impl AsRef<str>) {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .unwrap()
        .write_all(text.as_ref().as_bytes())
        .unwrap();
}

/// Serves `router` on an ephemeral loopback port and returns its base URL.
#[allow(dead_code)]
pub async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}
