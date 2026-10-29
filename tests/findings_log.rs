//! A detection plugin's finding reaches the request log, in OCSF when asked, tied to its request
//! by id, and the matched value is nowhere in the log.

use std::sync::Arc;
use std::time::Duration;

use axum::routing::any;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{builtin, PluginChain};
use reiki::proxy::{load_keys, router, App};

const TOKEN: &str = "stream-token";
const SSN: &str = "123-45-6789";

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// Runs one request that holds an SSN through phi and the request log, and returns the log lines.
async fn logged(format: &str, mode: &str, tag: &str) -> (u16, Vec<serde_json::Value>) {
    let dir = std::env::temp_dir().join(format!("reiki-findings-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("key"), "upstream-secret\n").unwrap();
    let log = dir.join("requests.jsonl");
    let upstream = serve(Router::new().fallback(any(|| async { "ok" }))).await;
    let text = format!(
        "listen = \"127.0.0.1:41097\"\n\
         [upstream.zen]\nbase_url = \"{upstream}/v1\"\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"{key}\"\n\
         [streams.s]\ntoken_sha256 = \"{hash}\"\nchain = [\"a\"]\n\
         [plugins.phi]\nmode = \"{mode}\"\n\
         [plugins.requestlog]\nfile = \"{log}\"\nformat = \"{format}\"\nresponse = \"status\"\nbody = \"summary\"\n",
        key = dir.join("key").display(),
        hash = reiki::auth::sha256_hex(TOKEN),
        log = log.display(),
    );
    let config = Config::parse(&text).unwrap();
    let keys = load_keys(&config).unwrap();
    let plugins = PluginChain::build(&config.plugins, &builtin()).unwrap();
    let app = App::with_plugins(
        config,
        keys,
        Policy::default(),
        Duration::from_secs(5),
        plugins,
    );
    let relay = serve(router(Arc::clone(&app))).await;
    let status = reqwest::Client::new()
        .post(format!("{relay}/zen/chat"))
        .bearer_auth(TOKEN)
        .body(format!("{{\"q\":\"ssn {SSN}\"}}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();

    let mut lines = Vec::new();
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        lines = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if lines.len() >= 3 || (mode == "block" && !lines.is_empty()) {
            break;
        }
    }
    let raw = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !raw.contains(SSN),
        "the matched value must not reach the log: {raw}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    (status, lines)
}

#[tokio::test]
async fn an_ocsf_log_pairs_the_finding_with_its_request_by_id() {
    let (status, lines) = logged("ocsf", "report", "ocsf").await;
    assert_eq!(status, 200);
    let finding = lines
        .iter()
        .find(|l| l["class_uid"] == 2004)
        .expect("a detection finding");
    assert_eq!(finding["finding_info"]["types"][0], "ssn");
    assert_eq!(finding["disposition"], "Logged");
    let request = lines
        .iter()
        .find(|l| l["class_uid"] == 4002 && l["http_request"].is_object())
        .expect("the HTTP Activity request");
    assert_eq!(
        request["metadata"]["correlation_uid"], finding["metadata"]["correlation_uid"],
        "the finding and the request share an id"
    );
    let response = lines
        .iter()
        .find(|l| l["class_uid"] == 4002 && l["http_response"].is_object())
        .expect("the HTTP Activity response");
    assert_eq!(
        response["metadata"]["correlation_uid"],
        finding["metadata"]["correlation_uid"]
    );
}

#[tokio::test]
async fn a_blocked_request_is_logged_as_a_blocked_finding_only() {
    let (status, lines) = logged("ocsf", "block", "blocked").await;
    assert_eq!(status, 422);
    let finding = lines
        .iter()
        .find(|l| l["class_uid"] == 2004)
        .expect("a detection finding");
    assert_eq!(finding["disposition"], "Blocked");
    assert!(
        lines.iter().all(|l| l["class_uid"] != 4002),
        "the chain stopped at phi, so the request log never saw the request"
    );
}

#[tokio::test]
async fn the_json_log_records_a_reported_finding_beside_the_request() {
    let (status, lines) = logged("json", "report", "json").await;
    assert_eq!(status, 200, "report mode lets the request through");
    let finding = lines
        .iter()
        .find(|l| l["event"] == "finding")
        .expect("a finding record");
    assert_eq!(
        (finding["plugin"].as_str(), finding["kind"].as_str()),
        (Some("phi"), Some("ssn"))
    );
    assert_eq!(finding["action"], "reported");
    let request = lines
        .iter()
        .find(|l| l["event"] == "request")
        .expect("the request record");
    assert_eq!(request["id"], finding["id"]);
}
