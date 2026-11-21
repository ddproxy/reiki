//! The running binary keeps the `[tunnel]` table in its config snapshot, so an unchanged reload
//! reports no restart and the interception CA files are watched.

mod common;
use common::write_key;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use reiki::intercept::init_ca;

const ADMIN_TOKEN: &str = "admin-token-for-startup-test";

struct Child {
    child: std::process::Child,
    dir: std::path::PathBuf,
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http(port: u16, request: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    let mut out = String::new();
    stream.read_to_string(&mut out).ok()?;
    Some(out)
}

fn start(label: &str) -> Option<(Child, u16)> {
    let dir = std::env::temp_dir().join(format!("reiki-startup-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_key(dir.join("key"), "upstream-secret\n");
    init_ca(&dir.join("ca"), &["api.test".to_owned()], 30, false).unwrap();
    let (client, admin, tunnel) = (free_port(), free_port(), free_port());
    let text = format!(
        r#"
        listen = "127.0.0.1:{client}"
        [upstream.api]
        base_url = "http://127.0.0.1:1/v1"
        [keys.k1]
        upstream = "api"
        file = "{dir}/key"
        [streams.s]
        token_sha256 = "{stream}"
        chain = ["k1"]
        [admin]
        listen = "127.0.0.1:{admin}"
        token_sha256 = "{hash}"
        [tunnel]
        listen = "127.0.0.1:{tunnel}"
        allow = ["api.test"]
        [tunnel.intercept]
        ca_cert = "{dir}/ca/ca.pem"
        ca_key = "{dir}/ca/ca.key"
        hosts = [{{ host = "api.test", upstream = "api", stream = "s" }}]
        "#,
        dir = dir.display(),
        stream = reiki::auth::sha256_hex("stream-token-for-startup"),
        hash = reiki::auth::sha256_hex(ADMIN_TOKEN),
    );
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
    let mut running = Child { child, dir };
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if running.child.try_wait().ok().flatten().is_some() {
            return None;
        }
        if [client, admin, tunnel]
            .iter()
            .all(|port| TcpStream::connect(("127.0.0.1", *port)).is_ok())
        {
            return Some((running, admin));
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn reload(admin: u16, query: &str) -> String {
    http(
        admin,
        &format!(
            "POST /v1/reload{query} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {ADMIN_TOKEN}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        ),
    )
    .expect("reload answers")
}

fn running(label: &str) -> (Child, u16) {
    for attempt in 0..3 {
        if let Some(started) = start(&format!("{label}-{attempt}")) {
            return started;
        }
    }
    panic!("reiki never started");
}

#[test]
fn an_unchanged_reload_of_a_tunnel_config_needs_no_restart() {
    let (_child, admin) = running("reload");
    let reply = reload(admin, "");
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(
        !reply.contains("tunnel.listen"),
        "an unchanged reload reports nothing: {reply}"
    );
}

#[test]
fn a_reload_reports_log_settings_that_need_a_restart() {
    let (child, admin) = running("log");
    let config = child.dir.join("reiki.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[log]\nformat = \"json\"\nfile_keep = 9\nfile_max_bytes = 7\n");
    std::fs::write(&config, text).unwrap();
    let reply = reload(admin, "");
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    for name in ["log.format", "log.file_keep", "log.file_max_bytes"] {
        assert!(reply.contains(&format!("\"{name}\"")), "{name}: {reply}");
    }
    assert!(!reply.contains("log.file\""), "{reply}");
    let preview = reload(admin, "?dry_run=true");
    assert!(preview.contains("\"log.format\""), "{preview}");
}

#[test]
fn a_toml_error_in_a_reload_never_echoes_the_source() {
    let (child, admin) = running("toml");
    let config = child.dir.join("reiki.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[log]\nlevel = SENTINEL-s3cr3t\n");
    std::fs::write(&config, text).unwrap();
    let reply = reload(admin, "");
    assert!(reply.starts_with("HTTP/1.1 422"), "{reply}");
    assert!(reply.contains("line "), "{reply}");
    assert!(!reply.contains("SENTINEL"), "{reply}");
    let status = http(
        admin,
        &format!(
            "GET /v1/status HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {ADMIN_TOKEN}\r\nConnection: close\r\n\r\n"
        ),
    )
    .unwrap();
    assert!(status.contains("\"ok\":false"), "{status}");
    assert!(!status.contains("SENTINEL"), "{status}");
}

#[test]
fn an_unknown_plugin_name_fails_startup() {
    let dir = std::env::temp_dir().join(format!("reiki-startup-plugin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_key(dir.join("key"), "upstream-secret\n");
    let config = dir.join("reiki.toml");
    std::fs::write(
        &config,
        format!(
            "[upstream.api]\nbase_url = \"http://127.0.0.1:1/v1\"\n\
             [keys.k1]\nupstream = \"api\"\nfile = \"{}/key\"\n\
             [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k1\"]\n\
             [plugins.unknown]\nmode = \"report\"\n",
            dir.display(),
            reiki::auth::sha256_hex("stream-token-for-startup"),
        ),
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_reiki"))
        .arg("--config")
        .arg(&config)
        .arg("check")
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("unknown") && stderr.contains("not compiled in"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn sighup_reloads_the_running_instance() {
    let (child, admin) = running("hup");
    let config = child.dir.join("reiki.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[log]\nlevel = \"debug\"\n");
    std::fs::write(&config, text).unwrap();
    let status = std::process::Command::new("kill")
        .args(["-HUP", &child.child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let reply = http(
            admin,
            &format!(
                "GET /v1/status HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {ADMIN_TOKEN}\r\nConnection: close\r\n\r\n"
            ),
        )
        .unwrap();
        if reply.contains("\"generation\":1") {
            return;
        }
        assert!(Instant::now() < deadline, "no reload happened: {reply}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn check_prints_the_connection_caps_and_names_unserved_hub_tables() {
    let dir = std::env::temp_dir().join(format!("reiki-check-caps-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_key(dir.join("key"), "upstream-secret\n");
    let config = dir.join("reiki.toml");
    let text = format!(
        "[server]\nmax_connections = 32\nmax_connections_per_addr = 8\n\
         [upstream.api]\nbase_url = \"https://example.net/v1\"\n\
         [keys.k1]\nupstream = \"api\"\nfile = \"{}/key\"\n",
        dir.display()
    );
    #[cfg(feature = "hub")]
    let text = format!("{text}[accounts.acct-a]\nupstream = \"api\"\nkeys = [\"k1\"]\n");
    std::fs::write(&config, text).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_reiki"))
        .arg("--config")
        .arg(&config)
        .arg("check")
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(
        stdout.contains("caps: client 32 (8 per address), admin 64 (16 per address)"),
        "{stdout}"
    );
    #[cfg(feature = "hub")]
    assert!(
        stdout.contains("hub: configured but not served in this version"),
        "{stdout}"
    );
}
