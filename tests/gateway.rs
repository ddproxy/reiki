//! End-to-end coverage for multi-user gateway foundations: self-service usage isolation, fixed
//! refusal bodies, event emission and counters that survive a reload.
//!
//! The recorder plugin is built from a plain function pointer, so it records into statics. All
//! scenarios therefore run in one test to keep them from overwriting each other.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{PluginChain, Registration};
use reiki::proxy::{App, Secret, router};
use reiki_plugin::{AdmissionRefusal, Event, Plugin};

mod common;
use common::serve;

type Recorded = Arc<Mutex<Vec<(String, u64)>>>;
type Refusals = Arc<Mutex<Vec<String>>>;

static EVENTS: Mutex<Option<Recorded>> = Mutex::new(None);
static REFUSALS: Mutex<Option<Refusals>> = Mutex::new(None);

struct Recorder;

impl Plugin for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }
    fn on_event(&self, event: &Event<'_>) {
        match event {
            Event::Usage { stream, tokens, .. } => {
                if let Some(events) = EVENTS.lock().unwrap().as_ref() {
                    events.lock().unwrap().push(((*stream).to_owned(), *tokens));
                }
            }
            Event::GatewayRejected { stream, reason } => {
                if let Some(refusals) = REFUSALS.lock().unwrap().as_ref() {
                    refusals
                        .lock()
                        .unwrap()
                        .push(format!("{stream}:{}", reason.label()));
                }
            }
            _ => {}
        }
    }
}

async fn upstream(_: Request) -> Response {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"usage":{"prompt_tokens":6,"completion_tokens":4}}"#,
        ))
        .unwrap()
}

fn sha(token: &str) -> String {
    reiki::auth::sha256_hex(token)
}

fn config(url: &str, limits: &str) -> Config {
    Config::parse(&format!(
        "[upstream.zen]\nbase_url = \"{url}/v1\"\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
         [streams.one]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
         [streams.two]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
         {limits}\n[plugins.recorder]\n",
        sha("tok-one"),
        sha("tok-two")
    ))
    .unwrap()
}

fn keys() -> std::collections::HashMap<String, Secret> {
    [("a".to_owned(), Secret::new("up-a"))]
        .into_iter()
        .collect()
}

async fn relay(config: Config, events: Recorded, refusals: Refusals) -> (String, Arc<App>) {
    *EVENTS.lock().unwrap() = Some(events);
    *REFUSALS.lock().unwrap() = Some(refusals);
    let registry = vec![Registration {
        name: "recorder",
        build: |_| Ok(Box::new(Recorder) as Box<dyn Plugin>),
    }];
    let plugins = PluginChain::build(&config.plugins, &registry).unwrap();
    let app = App::new(config, keys(), Policy::default(), plugins);
    let url = serve(router(Arc::clone(&app))).await;
    (url, app)
}

async fn post(relay: &str, token: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{relay}/zen/chat/completions"))
        .bearer_auth(token)
        .body("{\"model\":\"secret-model\",\"messages\":[]}")
        .send()
        .await
        .unwrap()
}

async fn usage_of(relay: &str, token: &str) -> serde_json::Value {
    let text = reqwest::Client::new()
        .get(format!("{relay}/v1/usage"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn gateway_foundations_work_end_to_end() {
    let url = serve(Router::new().fallback(upstream)).await;

    // A stream's token sees only its own usage; final usage replaces the request-size estimate.
    let events: Recorded = Arc::default();
    let (client_url, _app) = relay(config(&url, ""), Arc::clone(&events), Arc::default()).await;
    assert_eq!(post(&client_url, "tok-one").await.status(), 200);
    assert_eq!(post(&client_url, "tok-one").await.status(), 200);
    assert_eq!(post(&client_url, "tok-two").await.status(), 200);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        *events.lock().unwrap(),
        [
            ("one".to_owned(), 10),
            ("one".to_owned(), 10),
            ("two".to_owned(), 10)
        ]
    );

    let body = usage_of(&client_url, "tok-one").await;
    let streams = body["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 1, "a tenant sees only its own row");
    assert_eq!(streams[0]["name"], "one");
    assert_eq!(streams[0]["requests"], 2);
    assert_eq!(
        streams[0]["tokens"], 20,
        "final usage replaces the estimate"
    );
    let body = usage_of(&client_url, "tok-two").await;
    assert_eq!(body["streams"].as_array().unwrap()[0]["requests"], 1);
    let anonymous = reqwest::Client::new()
        .get(format!("{client_url}/v1/usage"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);

    // A gateway refusal has a fixed body, a Retry-After and a fixed-name event.
    let refusals: Refusals = Arc::default();
    let (limited, _app) = relay(
        config(&url, "[streams.one.limits]\nrpm = 1\n"),
        Arc::default(),
        Arc::clone(&refusals),
    )
    .await;
    assert_eq!(post(&limited, "tok-one").await.status(), 200);
    let refused = post(&limited, "tok-one").await;
    assert_eq!(refused.status(), 429);
    let retry_after: u64 = refused.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry_after));
    assert!(
        refused.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("application/json")
    );
    let body = refused.text().await.unwrap();
    assert!(body.contains("gateway_rate_limited"), "body was {body}");
    assert!(body.contains("the stream's request rate limit was reached"));
    assert!(!body.contains("secret-model"), "the model never leaks");
    assert!(!body.contains("tok-one"), "the token never leaks");
    assert_eq!(*refusals.lock().unwrap(), ["one:rate"]);

    // Counters survive a reload with the same token, and a rotated token starts fresh.
    let (reloadable, app) = relay(config(&url, ""), Arc::default(), Arc::default()).await;
    assert_eq!(post(&reloadable, "tok-one").await.status(), 200);
    assert_eq!(
        usage_of(&reloadable, "tok-one").await["streams"][0]["requests"],
        1
    );
    app.reload(config(&url, ""), keys());
    assert_eq!(
        usage_of(&reloadable, "tok-one").await["streams"][0]["requests"],
        1
    );
    let mut rotated = config(&url, "");
    rotated.streams.get_mut("one").unwrap().token_sha256 = sha("tok-one-rotated");
    app.reload(rotated, keys());
    assert_eq!(
        usage_of(&reloadable, "tok-one-rotated").await["streams"][0]["requests"],
        0
    );
}

struct BudgetRefuser;

impl Plugin for BudgetRefuser {
    fn name(&self) -> &str {
        "budget"
    }

    fn admission_refusal(&self, stream: &str, _now: SystemTime) -> Option<AdmissionRefusal> {
        (stream == "one").then_some(AdmissionRefusal {
            kind: "budget_monthly",
            message: "monthly cost budget exceeded",
        })
    }
}

#[tokio::test]
async fn plugin_budget_refusal_uses_the_gateway_429_path() {
    let url = serve(Router::new().fallback(upstream)).await;
    let text = format!(
        "[upstream.zen]\nbase_url = \"{url}/v1\"\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
         [streams.one]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
         [streams.two]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
         [plugins.budget]\n",
        sha("tok-one"),
        sha("tok-two")
    );
    let config = Config::parse(&text).unwrap();
    let registry = vec![Registration {
        name: "budget",
        build: |_| Ok(Box::new(BudgetRefuser) as Box<dyn Plugin>),
    }];
    let plugins = PluginChain::build(&config.plugins, &registry).unwrap();
    let app = App::new(config, keys(), Policy::default(), plugins);
    let client_url = serve(router(Arc::clone(&app))).await;

    let refused = post(&client_url, "tok-one").await;
    assert_eq!(refused.status(), 429);
    assert_eq!(refused.headers()["retry-after"], "60");
    let body: serde_json::Value = serde_json::from_str(&refused.text().await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "budget_monthly");
    assert_eq!(body["error"]["message"], "monthly cost budget exceeded");

    // A stream without a budget still goes through the normal path.
    assert_eq!(post(&client_url, "tok-two").await.status(), 200);
    // No request was reserved for the refused stream. An untouched stream has no usage row.
    let usage = usage_of(&client_url, "tok-one").await;
    let streams = usage["streams"].as_array().unwrap();
    assert!(streams.is_empty() || streams[0]["requests"] == 0);
}
