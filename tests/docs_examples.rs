//! Checks the documentation against the binary and the config validator.
//!
//! The config snippets in the README and quickstart are rendered with values generated at this
//! test's runtime, then passed to `reiki check`. The quickstart proof is replayed against a
//! throwaway instance on free loopback ports. Help output and doc text are compared so a removed
//! flag or config key cannot stay documented. No secret-shaped value is written to this file.

use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

const CHECKED_FILES: &[&str] = &[
    "README.md",
    "AGENTS.md",
    "SECURITY.md",
    "llms.txt",
    "docs/README.md",
    "docs/install.md",
    "docs/operations.md",
    "docs/dynamic-streams.md",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_reiki"))
}

fn text(path: &str) -> String {
    fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

fn digest(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("reiki-docs-{tag}-{}-{seq}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("keys")).unwrap();
        // A fake key long enough for the validator, built at runtime.
        let key = digest(format!("docs-check-key-{seq}").as_bytes());
        for name in ["example-a", "zen-a"] {
            let path = dir.join("keys").join(name);
            fs::write(&path, format!("{key}-{name}\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        Self { dir }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Replaces doc placeholders with runtime values and doc paths with throwaway paths.
fn render(source: &str, scratch: &Scratch, client_port: u16, admin_port: u16) -> String {
    let source = source
        .replace(
            "/etc/reiki/keys/",
            &format!("{}/keys/", scratch.dir.display()),
        )
        .replace("/etc/reiki/", &format!("{}/", scratch.dir.display()))
        .replace("127.0.0.1:4097", &format!("127.0.0.1:{client_port}"))
        .replace("127.0.0.1:4098", &format!("127.0.0.1:{admin_port}"));
    let mut out = String::with_capacity(source.len());
    let mut rest = source.as_str();
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let Some(end) = tail.find('>') else {
            out.push('<');
            rest = tail;
            continue;
        };
        let placeholder = &tail[..end];
        let value = if placeholder.contains("admin") {
            digest(b"docs-check-admin-identity")
        } else if placeholder.contains("client") {
            digest(b"docs-check-client-identity")
        } else {
            digest(b"docs-check-other-identity")
        };
        out.push_str(&value);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

fn toml_blocks(source: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in source.lines() {
        if line.trim() == "```toml" {
            current = Some(String::new());
        } else if line.trim() == "```" {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
        } else if let Some(block) = current.as_mut() {
            block.push_str(line);
            block.push('\n');
        }
    }
    blocks
}

fn help(args: &[&str]) -> String {
    let out = Command::new(binary())
        .args(args)
        .arg("--help")
        .output()
        .unwrap_or_else(|e| panic!("cannot run reiki {args:?} --help: {e}"));
    assert!(
        out.status.success(),
        "reiki {args:?} --help: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_all(haystack: &str, needles: &[&str], label: &str) {
    for needle in needles {
        assert!(haystack.contains(needle), "{label} misses {needle:?}");
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[test]
fn documented_config_examples_pass_the_real_validator() {
    for doc in ["README.md", "docs/quickstart.md"] {
        let blocks = toml_blocks(&text(doc));
        assert!(!blocks.is_empty(), "{doc} has no TOML example");
        for (index, block) in blocks.iter().enumerate() {
            let scratch = Scratch::new("config");
            let path = scratch.dir.join("reiki.toml");
            fs::write(&path, render(block, &scratch, 4097, 4098)).unwrap();
            let out = Command::new(binary())
                .arg("--config")
                .arg(&path)
                .arg("check")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "{doc} TOML block {index} failed: {}{}",
                stdout,
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                stdout.contains("ok: "),
                "{doc} TOML block {index} printed no ok line: {stdout}"
            );
        }
    }
}

#[test]
fn documented_commands_and_flags_exist_in_the_binary() {
    let top = help(&[]);
    assert_all(
        &top,
        &[
            "hash-token",
            "check",
            "scan",
            "reload",
            "keys",
            "failures",
            "streams",
            "peers",
            "ca-init",
        ],
        "reiki --help",
    );
    assert_all(
        &help(&["--config", "example.toml", "streams"]),
        &[
            "list", "show", "create", "update", "revoke", "token", "audit",
        ],
        "streams help",
    );
    for (args, flags) in [
        (
            vec!["streams"],
            vec!["--reason", "--idempotency-key", "--url", "--token-file"],
        ),
        (vec!["streams", "create"], vec!["--ttl"]),
        (vec!["streams", "update"], vec!["--key", "--rpm", "--tpm"]),
        (vec!["streams", "token", "add"], vec!["--ttl"]),
        (vec!["streams", "audit", "head"], vec!["--n"]),
        (
            vec!["keys"],
            vec!["--check", "--enable", "--json", "--url", "--token-file"],
        ),
        (
            vec!["failures"],
            vec![
                "--limit",
                "--key",
                "--stream",
                "--json",
                "--url",
                "--token-file",
            ],
        ),
        (vec!["reload"], vec!["--dry-run", "--url", "--token-file"]),
        (vec!["scan"], vec!["--plugin", "--kinds", "--all", "--json"]),
        (
            vec!["ca-init"],
            vec!["--dir", "--host", "--days", "--replace"],
        ),
    ] {
        let mut args = args;
        if args[0] == "streams" && args.len() == 1 {
            args.insert(0, "--config");
            args.insert(1, "example.toml");
        }
        assert_all(&help(&args), &flags, &format!("reiki {args:?} --help"));
    }
    let streams_help = help(&["streams", "audit"]);
    assert_all(&streams_help, &["verify", "head"], "streams audit help");
    if cfg!(feature = "top") {
        assert_all(
            &help(&["top"]),
            &["--once", "--full", "--provider", "--interval"],
            "top help",
        );
    }
    if cfg!(feature = "plugin-cost") {
        assert_all(&help(&["cost"]), &["export"], "cost help");
        assert_all(
            &help(&["cost", "export"]),
            &["--period", "--format"],
            "cost export help",
        );
    }
    if cfg!(feature = "plugin-wasm") {
        assert_all(&help(&["plugin"]), &["inspect"], "plugin help");
        assert_all(
            &help(&["plugin", "inspect"]),
            &["--json"],
            "plugin inspect help",
        );
    }
}

#[test]
fn docs_keep_adding_config_keys_and_stream_flags() {
    let mut docs = String::new();
    for path in CHECKED_FILES {
        docs.push_str(&text(path));
    }
    for path in [
        "docs/configuration.md",
        "docs/monitoring.md",
        "docs/troubleshooting.md",
        "docs/cli.md",
        "docs/top.md",
    ] {
        docs.push_str(&text(path));
    }
    assert_all(
        &docs,
        &[
            "failure_ring_size",
            "reiki_failure_ring_entries",
            "reiki_failure_ring_dropped_total",
            "--stream",
            "pass-through",
            "cooldown",
            "rotate",
            "on_store_error",
            "usage_rpm",
            "allowed_upstreams",
            "allowed_models",
            "period_zone",
            "weight",
            "audit_log",
            "state_file",
            "max_streams",
            "allowed_keys",
            "ceilings",
            "max_tokens_per_stream",
            "default_ttl",
            "max_ttl",
            "allowed_listen",
            "require_two_party",
            "confirm_env",
            "token_env",
            "lockout_threshold",
            "lockout_secs",
            "max_body_bytes",
            "--reason",
            "--idempotency-key",
            "--token-file",
            "--ttl",
            "--rpm",
            "--tpm",
            "audit verify",
            "audit head",
            "read_token_sha256",
            "REIKI_ADMIN_READ_TOKEN",
            "X-Reiki-Actor",
            "read_only_token",
        ],
        "docs",
    );
    assert!(
        !docs.contains("rate_limit_per_minute"),
        "docs still describe the removed stream key"
    );
    assert!(
        !docs.contains("default_limits"),
        "docs still describe the removed stream admin key"
    );
}

fn slug(heading: &str) -> String {
    let heading = heading.trim().trim_start_matches('#').trim();
    heading
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                Some(c.to_ascii_lowercase())
            } else if c.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn headings(source: &str) -> Vec<String> {
    source
        .lines()
        .filter(|line| line.starts_with('#'))
        .map(slug)
        .collect()
}

fn links(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("](") {
        let tail = &rest[start + 2..];
        let Some(end) = tail.find(')') else { break };
        let target = &tail[..end];
        if !target.starts_with("http://")
            && !target.starts_with("https://")
            && !target.starts_with("mailto:")
        {
            found.push(target.to_owned());
        }
        rest = &tail[end + 1..];
    }
    found
}

#[test]
fn documentation_links_resolve() {
    for file in CHECKED_FILES {
        let source = text(file);
        let own = headings(&source);
        let base = root().join(file);
        let base_dir = base.parent().unwrap();
        for target in links(&source) {
            let (path, anchor) = match target.split_once('#') {
                Some((path, anchor)) => (path, Some(anchor)),
                None => (target.as_str(), None),
            };
            if path.is_empty() {
                assert!(
                    anchor.is_some_and(|a| own.iter().any(|h| h == a)),
                    "{file}: unresolved local link #{target}"
                );
                continue;
            }
            let resolved = base_dir.join(path);
            assert!(
                resolved.is_file(),
                "{file}: link {target} has no file at {}",
                resolved.display()
            );
            if let Some(anchor) = anchor {
                let target_headings = headings(&fs::read_to_string(&resolved).unwrap());
                assert!(
                    target_headings.iter().any(|h| h == anchor),
                    "{file}: link {target} has no heading #{anchor}"
                );
            }
        }
    }
}

#[tokio::test]
async fn readme_proof_runs_against_a_throwaway_instance() {
    let scratch = Scratch::new("smoke");
    let client_port = free_port();
    let admin_port = free_port();
    let config = scratch.dir.join("reiki.toml");
    fs::write(
        &config,
        render(&text("README.md"), &scratch, client_port, admin_port)
            .lines()
            .skip_while(|line| line.trim() != "```toml")
            .skip(1)
            .take_while(|line| line.trim() != "```")
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();

    let checked = Command::new(binary())
        .arg("--config")
        .arg(&config)
        .arg("check")
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "check failed: {}",
        String::from_utf8_lossy(&checked.stderr)
    );

    let mut child = Command::new(binary())
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut ready = false;
    for _ in 0..100 {
        let url = format!("http://127.0.0.1:{client_port}/__reiki/healthz");
        if let Ok(response) = reqwest::get(&url).await
            && response.status().is_success()
            && response.text().await.unwrap_or_default().contains("ok")
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "the throwaway instance never became healthy");

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{client_port}/example/chat/completions"
        ))
        .header("Authorization", "Bearer not-a-valid-token")
        .header("Content-Type", "application/json")
        .body(r#"{"model":"example-model","messages":[{"role":"user","content":"ping"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"error":{"code":"unauthorized","message":"unknown token"},"ok":false}"#
    );

    // The config placeholder is the digest of this raw token.
    let admin_token = "docs-check-admin-identity";
    for (args, expected) in [
        (vec!["keys"], "example-a"),
        (vec!["failures", "--limit", "5"], "no failures"),
    ] {
        let out = Command::new(binary())
            .arg("--config")
            .arg(&config)
            .args(&args)
            .env("REIKI_ADMIN_TOKEN", admin_token)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "reiki {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(expected), "reiki {args:?}: {stdout}");
    }
    assert!(
        text("README.md").contains("PASS: wrong token rejected, live state answered"),
        "the README proof line is missing"
    );

    let _ = child.kill();
    let _ = child.wait();
}
