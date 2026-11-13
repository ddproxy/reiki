//! With no `[peers]` table Reiki behaves as it did before peers existed: the admin documents and
//! answers stay byte-identical, `/v1/health` is not served, and no extra socket is opened.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reiki::chain::Policy;
use reiki::config::Config;
use reiki::proxy::{router, App, Secret};

const ADMIN_TOKEN: &str = "admin-secret-for-golden";
const STREAM_TOKEN: &str = "stream-token-for-golden";

fn sha(token: &str) -> String {
    reiki::auth::sha256_hex(token)
}

fn config_text(extra: &str) -> String {
    format!(
        "[upstream.zen]\nbase_url = \"http://127.0.0.1:9/v1\"\n\
         [upstream.other]\nbase_url = \"https://example.com/v1\"\n\
         [keys.zen-a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
         [keys.zen-b]\nupstream = \"zen\"\nfile = \"/unused/b\"\n\
         [keys.other-a]\nupstream = \"other\"\nfile = \"/unused/c\"\n\
         [streams.laptop]\ntoken_sha256 = \"{}\"\nchain = [\"zen-a\", \"zen-b\", \"other-a\"]\n\
         [admin]\nlisten = \"127.0.0.1:0\"\ntoken_sha256 = \"{}\"\n\
         [plugins.metrics]\n{extra}",
        sha(STREAM_TOKEN),
        sha(ADMIN_TOKEN),
    )
}

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

async fn admin_for(text: &str) -> (Arc<App>, String) {
    let config = Config::parse(text).unwrap();
    let plugins =
        reiki::plugins::PluginChain::build(&config.plugins, &reiki::plugins::builtin()).unwrap();
    let keys: HashMap<String, Secret> = ["zen-a", "zen-b", "other-a"]
        .iter()
        .map(|name| ((*name).to_owned(), Secret::new(format!("secret-{name}"))))
        .collect();
    let app = App::with_plugins(
        config,
        keys,
        Policy::default(),
        Duration::from_secs(120),
        plugins,
    );
    let admin = serve(reiki::admin::router(Arc::clone(&app))).await;
    (app, admin)
}

async fn admin_get(admin: &str, path: &str) -> (u16, String) {
    let res = reqwest::Client::new()
        .get(format!("{admin}{path}"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    (res.status().as_u16(), res.text().await.unwrap())
}

/// Replaces the values that change from run to run with fixed placeholders.
fn normalize(text: &str) -> String {
    let text = text.replace(env!("CARGO_PKG_VERSION"), "<version>");
    match text.find("\"loaded_at\":\"") {
        Some(start) => {
            let value = start + "\"loaded_at\":\"".len();
            let end = value + text[value..].find('"').unwrap();
            format!("{}<time>{}", &text[..value], &text[end..])
        }
        None => text,
    }
}

/// The stored copy of `name`. `REIKI_BLESS_GOLDEN=1` rewrites it from `actual` instead, for a
/// deliberate change to the documents.
fn golden(name: &str, actual: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("REIKI_BLESS_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{actual}\n")).unwrap();
    }
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
        .trim_end()
        .to_owned()
}

#[tokio::test]
async fn the_openapi_document_and_status_match_the_golden_copies() {
    let (_app, admin) = admin_for(&config_text("")).await;
    let (code, openapi) = admin_get(&admin, "/v1/openapi.json").await;
    assert_eq!(code, 200);
    let openapi = normalize(&openapi);
    assert_eq!(openapi, golden("openapi.json", &openapi));
    let (code, status) = admin_get(&admin, "/v1/status").await;
    assert_eq!(code, 200);
    let status = normalize(&status);
    assert_eq!(status, golden("status.json", &status));
}

#[tokio::test]
async fn health_is_not_served_on_the_admin_listener() {
    let (_app, admin) = admin_for(&config_text("")).await;
    for path in ["/v1/health", "/health", "/v1/health/stream"] {
        let (code, _) = admin_get(&admin, path).await;
        assert_eq!(code, 404, "{path}");
    }
}

#[tokio::test]
async fn health_is_not_served_on_the_client_listener() {
    let (app, _admin) = admin_for(&config_text("")).await;
    let relay = serve(router(app)).await;
    let res = reqwest::Client::new()
        .get(format!("{relay}/v1/health"))
        .bearer_auth(STREAM_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}

/// TCP sockets a process holds, read from `/proc`, as (state, local port).
#[cfg(target_os = "linux")]
fn process_sockets(pid: u32) -> Vec<(String, u16)> {
    let mut inodes = std::collections::HashSet::new();
    // A child that just exited has no fd table; that reads as no sockets.
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd"));
    for entry in fds.into_iter().flatten().flatten() {
        if let Ok(link) = std::fs::read_link(entry.path()) {
            let link = link.to_string_lossy().into_owned();
            if let Some(inode) = link
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
            {
                inodes.insert(inode.to_owned());
            }
        }
    }
    let mut found = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || !inodes.contains(fields[9]) {
                continue;
            }
            let port = fields[1]
                .rsplit_once(':')
                .and_then(|(_, hex)| u16::from_str_radix(hex, 16).ok())
                .unwrap();
            found.push((fields[3].to_owned(), port));
        }
    }
    found
}

/// Client, admin and peer ports for one start attempt, all distinct.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct Ports {
    client: u16,
    admin: u16,
    peer: u16,
}

#[cfg(target_os = "linux")]
impl Ports {
    fn fresh() -> Self {
        // Held together so the system cannot hand out one port twice.
        let held: Vec<std::net::TcpListener> = (0..3)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let port = |i: usize| held[i].local_addr().unwrap().port();
        Self {
            client: port(0),
            admin: port(1),
            peer: port(2),
        }
    }
}

#[cfg(target_os = "linux")]
struct Running {
    child: std::process::Child,
    dir: std::path::PathBuf,
    ports: Ports,
}

#[cfg(target_os = "linux")]
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(target_os = "linux")]
const START_ATTEMPTS: u32 = 3;
#[cfg(target_os = "linux")]
const START_DEADLINE: Duration = Duration::from_secs(8);

/// Starts the binary with `extra(ports)` appended to the config and waits until it listens on the
/// client and admin ports, and the peer port too with `wait_peer`. A port picked by
/// [`Ports::fresh`] is free only until it is released, so another test can take it before the
/// child binds; such an attempt is retried with fresh ports.
#[cfg(target_os = "linux")]
fn start(label: &str, extra: impl Fn(Ports) -> String, wait_peer: bool) -> Running {
    let mut failures = Vec::new();
    for attempt in 1..=START_ATTEMPTS {
        let ports = Ports::fresh();
        match try_start(
            &format!("{label}-{attempt}"),
            ports,
            &extra(ports),
            wait_peer,
        ) {
            Ok(running) => return running,
            Err(reason) => failures.push(format!("attempt {attempt}: {reason}")),
        }
    }
    panic!("reiki never started: {}", failures.join("; "));
}

#[cfg(target_os = "linux")]
fn try_start(label: &str, ports: Ports, extra: &str, wait_peer: bool) -> Result<Running, String> {
    let dir = std::env::temp_dir().join(format!("reiki-peers-off-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["a", "b", "c"] {
        std::fs::write(dir.join(name), format!("secret-{name}\n")).unwrap();
    }
    let text = config_text(extra)
        .replace("/unused/", &format!("{}/", dir.display()))
        .replace(
            "listen = \"127.0.0.1:0\"",
            &format!("listen = \"127.0.0.1:{}\"", ports.admin),
        );
    let text = format!("listen = \"127.0.0.1:{}\"\n{text}", ports.client);
    let config = dir.join("reiki.toml");
    std::fs::write(&config, text).unwrap();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_reiki"))
        .arg("--config")
        .arg(&config)
        .env_remove("REIKI_LISTEN")
        .env("RUST_LOG", "warn")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut running = Running { child, dir, ports };
    let mut wanted = vec![ports.client, ports.admin];
    if wait_peer {
        wanted.push(ports.peer);
    }
    let deadline = std::time::Instant::now() + START_DEADLINE;
    // Read from the child's own socket table, so a port another process took never counts.
    loop {
        if let Ok(Some(status)) = running.child.try_wait() {
            return Err(format!("exited with {status}"));
        }
        let (listening, _) = listening_and_outbound(running.child.id());
        if wanted.iter().all(|port| listening.contains(port)) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("listening on {listening:?}, wanted {wanted:?}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(running)
}

/// Listening ports and the count of outbound connections, which are sockets whose local port is
/// none of the listening ones.
#[cfg(target_os = "linux")]
fn listening_and_outbound(pid: u32) -> (Vec<u16>, usize) {
    const LISTEN: &str = "0A";
    let sockets = process_sockets(pid);
    let mut listening: Vec<u16> = sockets
        .iter()
        .filter(|(state, _)| state == LISTEN)
        .map(|(_, port)| *port)
        .collect();
    listening.sort_unstable();
    listening.dedup();
    let outbound = sockets
        .iter()
        .filter(|(state, port)| state != LISTEN && !listening.contains(port))
        .count();
    (listening, outbound)
}

#[cfg(target_os = "linux")]
fn sorted(mut ports: Vec<u16>) -> Vec<u16> {
    ports.sort_unstable();
    ports
}

#[cfg(target_os = "linux")]
#[test]
fn without_peers_only_the_client_and_admin_sockets_are_open() {
    let running = start("none", |_| String::new(), false);
    let ports = running.ports;
    let (listening, outbound) = listening_and_outbound(running.child.id());
    assert_eq!(listening, sorted(vec![ports.client, ports.admin]));
    assert_eq!(outbound, 0, "no outbound connection");
}

#[cfg(all(target_os = "linux", feature = "peers"))]
#[test]
fn a_disabled_peers_table_opens_nothing_either() {
    let running = start(
        "disabled",
        |ports| {
            format!(
                "[peers]\nenabled = false\npublish = true\nlisten = \"127.0.0.1:{peer}\"\n\
                 peer_token_sha256 = \"{}\"\n\
                 [peers.other]\nurl = \"http://127.0.0.1:{peer}\"\ntoken_file = \"/nonexistent\"\n",
                sha("peer-token-for-tests"),
                peer = ports.peer,
            )
        },
        false,
    );
    let ports = running.ports;
    let (listening, outbound) = listening_and_outbound(running.child.id());
    assert_eq!(listening, sorted(vec![ports.client, ports.admin]));
    assert_eq!(outbound, 0);
}

#[cfg(all(target_os = "linux", feature = "peers"))]
#[test]
fn publishing_adds_exactly_the_peer_listener_and_no_outbound_connection() {
    let running = start(
        "publish",
        |ports| {
            format!(
                "[peers]\nenabled = true\npublish = true\nlisten = \"127.0.0.1:{}\"\n\
                 peer_token_sha256 = \"{}\"\n",
                ports.peer,
                sha("peer-token-for-tests")
            )
        },
        true,
    );
    let ports = running.ports;
    let (listening, outbound) = listening_and_outbound(running.child.id());
    assert_eq!(
        listening,
        sorted(vec![ports.client, ports.admin, ports.peer])
    );
    assert_eq!(outbound, 0);
}

#[cfg(feature = "peers")]
#[test]
fn check_reports_peers_as_disabled_without_a_table() {
    let dir = std::env::temp_dir().join(format!("reiki-peers-check-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["a", "b", "c"] {
        std::fs::write(dir.join(name), "secret\n").unwrap();
    }
    let config = dir.join("reiki.toml");
    let text = config_text("").replace("/unused/", &format!("{}/", dir.display()));
    std::fs::write(&config, &text).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_reiki"))
        .arg("--config")
        .arg(&config)
        .arg("check")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.lines().any(|l| l == "peers: disabled"), "{stdout}");

    std::fs::write(
        &config,
        format!(
            "{text}[peers]\nenabled = true\n[peers.b]\nurl = \"http://127.0.0.1:1\"\ntoken_file = \"/t\"\n"
        ),
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_reiki"))
        .arg("--config")
        .arg(&config)
        .arg("check")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout
            .lines()
            .any(|l| l == "peers: enabled, not publishing, 1 peer(s)[b], poll every 30s"),
        "{stdout}"
    );
}
