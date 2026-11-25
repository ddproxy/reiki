//! A key echoed in every encoding in a 429 body must not survive into `/v1/failures`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::any;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{App, Secret, router};

mod common;
use common::serve;

const TOKEN: &str = "client-token";
const ADMIN: &str = "admin-secret";
const KEY: &str = "upstream-secret-key-1234";

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64(data: &[u8], alphabet: &[u8; 64], padded: bool) -> String {
    let mut out = String::new();
    for group in data.chunks(3) {
        let n = (u32::from(group[0]) << 16)
            | (u32::from(*group.get(1).unwrap_or(&0)) << 8)
            | u32::from(*group.get(2).unwrap_or(&0));
        for i in 0..=group.len() {
            out.push(alphabet[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
        if padded {
            for _ in group.len()..3 {
                out.push('=');
            }
        }
    }
    out
}

fn aligned(offset: usize, alphabet: &[u8; 64]) -> String {
    let total = offset + KEY.len();
    let start = offset + 1;
    let end = total / 3 * 4
        + match total % 3 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => unreachable!(),
        };
    let mut data = vec![0; offset];
    data.extend_from_slice(KEY.as_bytes());
    base64(&data, alphabet, false)[start..end].to_string()
}

fn json_minimal(c: char) -> String {
    match c {
        '"' => "\\\"".to_owned(),
        '\\' => "\\\\".to_owned(),
        c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32),
        c => c.to_string(),
    }
}

fn representations() -> Vec<(&'static str, String)> {
    let all_escaped: String = KEY
        .chars()
        .map(|c| format!("\\u{:04x}", c as u32))
        .collect();
    let alnum_escaped: String = KEY
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                format!("\\u{:04x}", c as u32)
            } else {
                json_minimal(c)
            }
        })
        .collect();
    let one_escaped: String = KEY
        .chars()
        .map(|c| {
            if c == 'u' {
                "\\u0075".to_owned()
            } else {
                json_minimal(c)
            }
        })
        .collect();
    let all_percent: String = KEY.bytes().map(|b| format!("%{b:02X}")).collect();
    let unreserved_percent: String = KEY
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                format!("%{b:02X}")
            } else {
                char::from(b).to_string()
            }
        })
        .collect();
    let one_percent = KEY.replacen('u', "%75", 1);
    let all_split: String = KEY
        .chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let control_split = KEY.replacen('s', "s\u{1b}", 1);
    vec![
        ("json all escaped", all_escaped),
        ("json alnum escaped", alnum_escaped),
        ("json one escaped", one_escaped),
        ("percent all", all_percent),
        ("percent unreserved", unreserved_percent),
        ("percent one", one_percent),
        ("base64", base64(KEY.as_bytes(), B64_STD, true)),
        ("base64 unpadded", base64(KEY.as_bytes(), B64_STD, false)),
        ("base64url", base64(KEY.as_bytes(), B64_URL, true)),
        ("base64url unpadded", base64(KEY.as_bytes(), B64_URL, false)),
        ("base64 offset 1", aligned(1, B64_STD)),
        ("base64 offset 2", aligned(2, B64_STD)),
        ("base64url offset 1", aligned(1, B64_URL)),
        ("base64url offset 2", aligned(2, B64_URL)),
        ("split by space", all_split),
        ("split by control", control_split),
    ]
}

async fn failing(State(bodies): State<Arc<Mutex<VecDeque<String>>>>, _: Request) -> Response {
    let body = bodies.lock().unwrap().pop_front().unwrap_or_default();
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn every_encoding_is_scrubbed_from_the_failure_ring() {
    let forms = representations();
    let bodies: VecDeque<String> = forms
        .iter()
        .map(|(_, form)| format!("quota exceeded: {form}"))
        .collect();
    let bodies = Arc::new(Mutex::new(bodies));
    let upstream = serve(
        Router::new()
            .fallback(any(failing))
            .with_state(Arc::clone(&bodies)),
    )
    .await;

    let text = format!(
        "[upstream.zen]\nbase_url = \"{upstream}/v1\"\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
         [admin]\nlisten = \"127.0.0.1:0\"\ntoken_sha256 = \"{}\"\n",
        reiki::auth::sha256_hex(TOKEN),
        reiki::auth::sha256_hex(ADMIN),
    );
    let config = Config::parse(&text).unwrap();
    let keys = [("a".to_owned(), Secret::new(KEY))].into_iter().collect();
    let app = App::with_plugins(
        config,
        keys,
        Policy::default(),
        Duration::from_secs(30),
        PluginChain::default(),
    );
    let relay = serve(router(Arc::clone(&app))).await;
    let admin = serve(reiki::admin::router(Arc::clone(&app))).await;
    let client = reqwest::Client::new();

    for (name, _) in &forms {
        let response = client
            .post(format!("{relay}/zen/chat/completions"))
            .bearer_auth(TOKEN)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_ne!(response.status(), 200, "{name}");
        let _ = response.text().await;
        let enabled = client
            .post(format!("{admin}/v1/keys/enable?key=a"))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap();
        assert!(enabled.status().is_success(), "{name}");
    }

    let ring = client
        .get(format!("{admin}/v1/failures?limit=64"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let rows: serde_json::Value = serde_json::from_str(&ring).unwrap();
    assert_eq!(
        rows["failures"].as_array().unwrap().len(),
        forms.len(),
        "the failure ring has one row per key form"
    );
    assert_eq!(rows["failures_total"], forms.len());
    assert!(!ring.contains(KEY), "the key leaked into the failure ring");
    for (name, form) in forms {
        assert!(
            !ring.contains(&form),
            "the {name} form leaked into the ring"
        );
    }
}
