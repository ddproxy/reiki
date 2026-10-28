//! A reload applies plugin settings, keys and streams live, keeps plugin state that did not change,
//! reports what still needs a restart, and leaves everything alone when the new config is bad.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::routing::any;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::proxy::{load_keys, router, App};

const STREAM_TOKEN: &str = "stream-token";
const ADMIN_TOKEN: &str = "admin-token";
const SSN_BODY: &str = "{\"q\":\"ssn 123-45-6789\"}";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Rig {
    dir: PathBuf,
    config: PathBuf,
    mock_url: String,
    app: Arc<App>,
    relay: String,
    admin: String,
    client: reqwest::Client,
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

impl Rig {
    fn text(&self, extra: &str) -> String {
        format!(
            "listen = \"127.0.0.1:41097\"\n\
             [upstream.zen]\nbase_url = \"{}/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
             [admin]\nlisten = \"127.0.0.1:41098\"\ntoken_sha256 = \"{}\"\n{extra}",
            self.mock_url,
            self.dir.join("key-a").display(),
            reiki::auth::sha256_hex(STREAM_TOKEN),
            reiki::auth::sha256_hex(ADMIN_TOKEN),
        )
    }

    async fn new(extra: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("reiki-reload-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("key-a"), "upstream-secret\n").unwrap();
        let mock_url = serve(Router::new().fallback(any(|body: String| async move { body }))).await;
        let mut rig = Self {
            config: dir.join("reiki.toml"),
            dir,
            mock_url,
            app: App::with_header_timeout(
                Config::parse("[upstream.x]\nbase_url = \"https://x.test\"\n").unwrap(),
                HashMap::new(),
                Policy::default(),
                Duration::from_secs(5),
            ),
            relay: String::new(),
            admin: String::new(),
            client: reqwest::Client::new(),
        };
        let text = rig.text(extra);
        std::fs::write(&rig.config, &text).unwrap();
        let config = Config::parse(&text).unwrap();
        let keys = load_keys(&config).unwrap();
        let plugins =
            reiki::plugins::PluginChain::build(&config.plugins, &reiki::plugins::builtin())
                .unwrap();
        rig.app = App::with_plugins(
            config,
            keys,
            Policy::default(),
            Duration::from_secs(5),
            plugins,
        );
        rig.app.set_config_path(rig.config.clone());
        rig.relay = serve(router(Arc::clone(&rig.app))).await;
        rig.admin = serve(reiki::admin::router(Arc::clone(&rig.app))).await;
        rig
    }

    fn rewrite(&self, extra: &str) {
        std::fs::write(&self.config, self.text(extra)).unwrap();
    }

    async fn send(&self, body: &str) -> u16 {
        self.client
            .post(format!("{}/zen/chat", self.relay))
            .bearer_auth(STREAM_TOKEN)
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn admin_get(&self, path: &str) -> String {
        self.client
            .get(format!("{}{path}", self.admin))
            .bearer_auth(ADMIN_TOKEN)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn a_changed_plugin_setting_applies_to_the_next_request() {
    let rig = Rig::new("[plugins.phi]\nmode = \"report\"\n").await;
    assert_eq!(rig.send(SSN_BODY).await, 200, "report mode lets it through");

    rig.rewrite("[plugins.phi]\nmode = \"block\"\n");
    let app = Arc::clone(&rig.app);
    let report = tokio::task::spawn_blocking(move || app.reload_now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.generation, 1);
    assert_eq!(report.changed, ["plugins phi"]);
    assert!(report.restart_needed.is_empty());
    assert_eq!(rig.send(SSN_BODY).await, 422, "block mode now rejects it");

    rig.rewrite("[plugins.phi]\nmode = \"report\"\n");
    let app = Arc::clone(&rig.app);
    tokio::task::spawn_blocking(move || app.reload_now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rig.send(SSN_BODY).await, 200);
}

#[tokio::test]
async fn a_plugin_that_did_not_change_keeps_its_counters() {
    let rig = Rig::new("[plugins.metrics]\n[plugins.phi]\nmode = \"report\"\n").await;
    assert_eq!(rig.send("{\"q\":\"hello\"}").await, 200);
    let before = rig.admin_get("/metrics").await;
    assert!(
        before.contains("reiki_requests_total{stream=\"s\",upstream=\"zen\",status=\"success\"} 1"),
        "{before}"
    );

    rig.rewrite("[plugins.metrics]\n[plugins.phi]\nmode = \"block\"\n");
    let app = Arc::clone(&rig.app);
    let report = tokio::task::spawn_blocking(move || app.reload_now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.changed, ["plugins phi"], "metrics was not rebuilt");
    let after = rig.admin_get("/metrics").await;
    assert!(
        after.contains("reiki_requests_total{stream=\"s\",upstream=\"zen\",status=\"success\"} 1"),
        "the counter survived: {after}"
    );
}

#[tokio::test]
async fn a_bad_config_changes_nothing_and_the_status_says_why() {
    let rig = Rig::new("[plugins.phi]\nmode = \"block\"\n").await;
    rig.rewrite("[plugins.phi]\nmode = \"maybe\"\n");
    let app = Arc::clone(&rig.app);
    let error = tokio::task::spawn_blocking(move || app.reload_now())
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.contains("mode"), "{error}");
    assert_eq!(
        rig.send(SSN_BODY).await,
        422,
        "the running block mode is untouched"
    );
    let status = rig.admin_get("/status").await;
    assert!(status.contains("\"generation\":0"), "{status}");
    assert!(status.contains("\"ok\":false"), "{status}");

    std::fs::write(&rig.config, "this is not toml [").unwrap();
    let app = Arc::clone(&rig.app);
    assert!(tokio::task::spawn_blocking(move || app.reload_now())
        .await
        .unwrap()
        .is_err());
}

#[tokio::test]
async fn settings_that_need_a_restart_are_reported_and_not_applied() {
    let rig = Rig::new("").await;
    let moved = rig.text("").replace(
        "listen = \"127.0.0.1:41097\"\n",
        "listen = \"127.0.0.1:41099\"\n",
    );
    std::fs::write(&rig.config, moved).unwrap();
    let app = Arc::clone(&rig.app);
    let report = tokio::task::spawn_blocking(move || app.reload_now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.restart_needed, ["listen"]);
    assert_eq!(rig.send("{}").await, 200, "the relay keeps serving");
}

#[tokio::test]
async fn a_request_log_setting_change_keeps_the_running_log_and_asks_for_a_restart() {
    let log =
        |size: u32| format!("[plugins.requestlog]\nfile = \"LOG\"\nmax_body_bytes = {size}\n");
    let rig = Rig::new("").await;
    let file = rig.dir.join("requests.jsonl");
    let first = log(1000).replace("LOG", &file.display().to_string());
    rig.rewrite(&first);
    let app = Arc::clone(&rig.app);
    let added = tokio::task::spawn_blocking(move || app.reload_now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        added.changed,
        ["plugins requestlog"],
        "a new plugin starts live"
    );

    rig.rewrite(&log(2000).replace("LOG", &file.display().to_string()));
    let app = Arc::clone(&rig.app);
    let changed = tokio::task::spawn_blocking(move || app.reload_now())
        .await
        .unwrap()
        .unwrap();
    assert!(changed.changed.is_empty(), "{changed:?}");
    assert_eq!(changed.restart_needed, ["plugins.requestlog"]);
}

#[tokio::test]
async fn post_reload_needs_the_admin_token_and_reports_the_generation() {
    let rig = Rig::new("[plugins.phi]\nmode = \"report\"\n").await;
    let url = format!("{}/reload", rig.admin);
    let anonymous = rig.client.post(&url).send().await.unwrap();
    assert_eq!(anonymous.status(), 401);
    let wrong = rig
        .client
        .post(&url)
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    rig.rewrite("[plugins.phi]\nmode = \"block\"\n");
    let ok = rig
        .client
        .post(&url)
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let body = ok.text().await.unwrap();
    assert!(
        body.contains("\"ok\":true") && body.contains("\"generation\":1"),
        "{body}"
    );
    assert!(body.contains("\"changed\":[\"plugins phi\"]"), "{body}");
    assert!(rig.admin_get("/status").await.contains("\"generation\":1"));
    assert_eq!(rig.send(SSN_BODY).await, 422);

    std::fs::write(&rig.config, "broken [").unwrap();
    let bad = rig
        .client
        .post(&url)
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 422);
    assert!(bad.text().await.unwrap().contains("\"ok\":false"));
}

#[tokio::test]
async fn a_changed_terms_file_applies_on_reload_without_a_config_change() {
    let rig = Rig::new("").await;
    let terms = rig.dir.join("terms.txt");
    std::fs::write(&terms, "Falcon\n").unwrap();
    let placeholders = format!(
        "[plugins.placeholders]\nterms_file = \"{}\"\nbuiltin = false\nrehydrate = false\n",
        terms.display()
    );
    rig.rewrite(&placeholders);
    let app = Arc::clone(&rig.app);
    let added = tokio::task::spawn_blocking(move || app.reload_now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(added.changed, ["plugins placeholders"]);

    let echo = |text: &str| {
        let url = format!("{}/zen/chat", rig.relay);
        let body = text.to_owned();
        let client = rig.client.clone();
        async move {
            client
                .post(url)
                .bearer_auth(STREAM_TOKEN)
                .body(body)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };
    assert!(
        !echo("Falcon").await.contains("Falcon"),
        "a listed term is masked"
    );
    assert_eq!(echo("Osprey").await, "Osprey", "an unlisted one passes");

    // The config text is untouched; only the terms file grows.
    std::fs::write(&terms, "Falcon\nOsprey\n").unwrap();
    let app = Arc::clone(&rig.app);
    let report = tokio::task::spawn_blocking(move || app.reload_now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.changed, ["plugins placeholders"]);
    assert!(!echo("Osprey").await.contains("Osprey"), "now masked too");

    let app = Arc::clone(&rig.app);
    let quiet = tokio::task::spawn_blocking(move || app.reload_now())
        .await
        .unwrap()
        .unwrap();
    assert!(
        quiet.changed.is_empty(),
        "no change on disk, no rebuild: {quiet:?}"
    );
}
