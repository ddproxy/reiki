#![cfg(all(feature = "plugin-placeholders", feature = "plugin-toolpolicy", unix))]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::response::Response;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{PluginChain, builtin};
use reiki::proxy::{App, Secret, router};
use std::time::Duration;

mod common;
use common::serve;

const CLIENT_TOKEN: &str = "client-token";
const UPSTREAM_KEY: &str = "upstream-key-do-not-log";

#[derive(Default)]
struct Seen(Mutex<Vec<String>>);

async fn upstream(State(seen): State<Arc<Seen>>, req: Request) -> Response {
    let body = String::from_utf8(
        to_bytes(req.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    seen.0.lock().unwrap().push(body.clone());
    let placeholder = body
        .split("[[REIKI_TERM_")
        .nth(1)
        .map(|rest| format!("[[REIKI_TERM_{}]]", rest.split("]]").next().unwrap()))
        .expect("the term must be masked before it reaches the upstream");
    let arguments = format!("{{\"cmd\":\"curl {placeholder}\"}}");
    let answer = serde_json::json!({
        "choices": [{"message": {"tool_calls": [{
            "function": {"name": "run", "arguments": arguments}
        }]}}]
    });
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(answer.to_string()))
        .unwrap()
}

#[tokio::test]
async fn toolpolicy_checks_the_restored_body_after_placeholders_run() {
    let seen = Arc::new(Seen::default());
    let mock = serve(
        Router::new()
            .fallback(upstream)
            .with_state(Arc::clone(&seen)),
    )
    .await;
    let text = format!(
        "[upstream.zen]\nbase_url = \"{mock}/v1\"\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
         [streams.client-a]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
         [plugins.placeholders]\nterms = [\"evil.example.org\"]\n\
         [plugins.toolpolicy]\nmode = \"block\"\n",
        reiki::auth::sha256_hex(CLIENT_TOKEN)
    );
    let config = Config::parse(&text).unwrap();
    let chain = PluginChain::build(&config.plugins, &builtin()).unwrap();
    assert_eq!(
        chain.names().collect::<Vec<_>>(),
        ["placeholders", "toolpolicy"],
        "the final hook must run after restoration"
    );
    let keys = [("a".to_owned(), Secret::new(UPSTREAM_KEY))]
        .into_iter()
        .collect();
    let app = App::with_plugins(
        config,
        keys,
        Policy::default(),
        Duration::from_secs(120),
        chain,
    );
    let relay = serve(router(app)).await;

    let request = serde_json::json!({
        "tools": [{"type": "function", "function": {"name": "run"}}],
        "messages": [{"role": "user", "content": "use evil.example.org"}]
    })
    .to_string();
    let reply = reqwest::Client::new()
        .post(format!("{relay}/zen/chat/completions"))
        .bearer_auth(CLIENT_TOKEN)
        .body(request)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let upstream_body = seen.0.lock().unwrap()[0].clone();
    assert!(
        !upstream_body.contains("evil.example.org"),
        "the term must stay masked upstream"
    );
    assert!(
        reply.contains("[toolpolicy] blocked tool call: run"),
        "the restored host must be blocked: {reply}"
    );
    assert!(
        !reply.contains("evil.example.org"),
        "the forbidden host reached the client: {reply}"
    );
}
