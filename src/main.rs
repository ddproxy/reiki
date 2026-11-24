use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{PluginChain, builtin, with_dry_run};
use reiki::proxy::{App, load_keys, router};
use reiki_plugin::PluginHost;

#[derive(Parser)]
#[command(
    version,
    about = "Security proxy for agent and LLM traffic: isolates keys, filters what leaves, keeps access reliable"
)]
struct Cli {
    /// Path to the configuration file.
    #[arg(long, default_value = "/etc/reiki/reiki.toml", global = true)]
    config: PathBuf,
    /// Address to listen on; overrides `listen` in the config.
    #[arg(long, global = true)]
    listen: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Read a token from stdin and print the SHA-256 hex for `token_sha256`.
    HashToken,
    /// Check the configuration and key files, then exit.
    Check,
    /// Scan files or stdin with the detection plugins and print where they find identifiers.
    ///
    /// Prints `file:line:col plugin kind` for each finding and never the matched text. Exits 1 when
    /// there are findings, 2 on a usage or read error. Use it to tune `kinds` before switching a
    /// plugin from `report` to `block`.
    Scan {
        /// Plugin to run (phi, finance, controlled); repeat for several. Default: every compiled-in one.
        #[arg(long = "plugin")]
        plugins: Vec<String>,
        /// Kinds to look for, comma separated. Default: each plugin's default set.
        #[arg(long, value_delimiter = ',', conflicts_with = "all")]
        kinds: Vec<String>,
        /// Look for every kind, including the ones that are off by default.
        #[arg(long)]
        all: bool,
        /// Print one JSON object per finding.
        #[arg(long)]
        json: bool,
        /// Files to scan; stdin when none are given.
        files: Vec<PathBuf>,
    },
    /// Ask the running Reiki to reload its config, or preview what a reload would change.
    ///
    /// Calls `POST /v1/reload` on the admin listener and prints the JSON answer. Exits 0 when the
    /// reload (or preview) succeeded and 1 when it was refused or failed.
    Reload {
        /// Only report what would change; apply nothing.
        #[arg(long)]
        dry_run: bool,
        /// Admin address, such as `http://127.0.0.1:4098`. Default: the `[admin] listen` in the config.
        #[arg(long)]
        url: Option<String>,
        /// File holding the admin token. Default: the `REIKI_ADMIN_TOKEN` environment variable.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Show each key's health from the running instance, or check key files offline.
    ///
    /// Calls `GET /v1/status` on the admin listener and prints a table. Exits 0 when the request
    /// succeeded, 1 when the instance refused it and 2 when it could not be reached.
    Keys {
        /// Check the configured key files without contacting a running instance.
        #[arg(long, conflicts_with_all = ["json", "url", "token_file", "enable"])]
        check: bool,
        /// Re-enable a disabled key by name.
        #[arg(long, conflicts_with_all = ["check", "json"])]
        enable: Option<String>,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Admin address, such as `http://127.0.0.1:4098`. Default: the `[admin] listen` in the config.
        #[arg(long)]
        url: Option<String>,
        /// File holding the admin token. Default: the `REIKI_ADMIN_TOKEN` environment variable.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Show recent key failures from the running instance's in-memory ring.
    ///
    /// Calls `GET /v1/failures` on the admin listener and prints a table, or the same JSON array
    /// with `--json`. Exits 0 when the request succeeded, 1 when the instance refused it and 2
    /// when it could not be reached.
    Failures {
        /// Records to show, newest first; 1 to 64.
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u8).range(1..=64))]
        limit: u8,
        /// Only failures for this key name.
        #[arg(long)]
        key: Option<String>,
        /// Print the JSON array instead of a table.
        #[arg(long)]
        json: bool,
        /// Admin address, such as `http://127.0.0.1:4098`. Default: the `[admin] listen` in the config.
        #[arg(long)]
        url: Option<String>,
        /// File holding the admin token. Default: the `REIKI_ADMIN_TOKEN` environment variable.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Manage opt-in dynamic streams on the admin listener.
    ///
    /// The token is read from --token-file or the config-named environment variable, never
    /// from a CLI argument. Mutating commands require --reason and send an idempotency key.
    Streams {
        /// Admin address, such as `http://127.0.0.1:4098`. Default: `[admin] listen` in the config.
        #[arg(long)]
        url: Option<String>,
        /// File holding the stream-admin token. Default: the configured token environment variable.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Why the change is made; required for mutations and written to the audit log.
        #[arg(long)]
        reason: Option<String>,
        /// Idempotency key for a mutation. Generated when omitted.
        #[arg(long)]
        idempotency_key: Option<String>,
        #[command(subcommand)]
        command: StreamsCommand,
    },
    /// Read each `[peers.<name>]` instance's `/v1/health` and print what it reports.
    ///
    /// A peer that cannot be reached, refuses the token or answers with data older than three
    /// poll intervals shows as `unknown`. Exits 0 when a peer gave fresh data, 1 when none did and
    /// 2 on a config problem.
    #[cfg(feature = "peers")]
    Peers {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Follow each peer's `/v1/health/stream` and print on every change, polling while a
        /// stream is down.
        #[arg(long)]
        watch: bool,
    },
    /// Show a live terminal view of every `[peers.<name>]` instance's key health.
    ///
    /// Follows each peer's `/v1/health/stream`, polling while a stream is down, and draws a
    /// peers-by-providers matrix with the selected peer's detail below it. `--once` polls once
    /// and prints plain text. `--full` shows the per-peer text view instead. Needs the `top`
    /// build feature and an enabled `[peers]` table with at least one peer.
    #[cfg(feature = "top")]
    Top {
        /// Poll every peer once, print the fleet as plain text and exit.
        #[arg(long)]
        once: bool,
        /// Seconds between redraws; the default is 1.
        #[arg(long)]
        interval: Option<u64>,
        /// Show the per-peer text view instead of the matrix.
        #[arg(long)]
        full: bool,
        /// Filter the matrix to one provider.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Export estimated cost from the running instance's cost plugin.
    ///
    /// Calls `GET /v1/cost` on the admin listener and prints the answer unchanged.
    /// Figures are estimates from the operator price table, not provider invoices.
    #[cfg(feature = "plugin-cost")]
    Cost {
        #[command(subcommand)]
        command: CostCommand,
    },
    /// Create a CA limited to the named hosts, for `[tunnel.intercept]`.
    CaInit {
        /// Directory that receives `ca.pem` and `ca.key`.
        #[arg(long)]
        dir: PathBuf,
        /// A host the CA may sign for; repeat for several.
        #[arg(long = "host", required = true)]
        hosts: Vec<String>,
        /// How long the CA is valid.
        #[arg(long, default_value_t = 365)]
        days: u64,
        /// Overwrite an existing CA; clients then need the new `ca.pem`.
        #[arg(long)]
        replace: bool,
    },
    /// Work with Wasm plugins.
    #[cfg(feature = "plugin-wasm")]
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
}

#[cfg(feature = "plugin-cost")]
#[derive(Subcommand)]
enum CostCommand {
    /// Export one UTC period as CSV or JSON.
    Export {
        /// UTC period: `YYYY-MM`, `YYYY-MM-DD` or `YYYY-MM-DDTHH`.
        #[arg(long, default_value_t = reiki_cost::current_period())]
        period: String,
        /// Output format.
        #[arg(long, value_enum, default_value_t = CostFormat::Json)]
        format: CostFormat,
        /// Admin address, such as `http://127.0.0.1:4098`. Default: `[admin] listen` in the config.
        #[arg(long)]
        url: Option<String>,
        /// File holding the admin token. Default: the `REIKI_ADMIN_TOKEN` environment variable.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
}

#[cfg(feature = "plugin-cost")]
#[derive(clap::ValueEnum, Clone, Copy)]
enum CostFormat {
    Csv,
    Json,
}

#[cfg(feature = "plugin-cost")]
impl CostFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }
}

#[derive(Subcommand)]
enum StreamsCommand {
    /// List dynamic streams.
    List,
    /// Show one dynamic stream.
    Show {
        /// Dynamic stream name.
        name: String,
    },
    /// Create a stream and print its first token once.
    Create {
        /// Stream name; must not collide with a static `[streams.NAME]`.
        name: String,
        /// Configured, assignable key name.
        key: String,
        /// Token lifetime in seconds; default and maximum come from `[stream_admin]`.
        #[arg(long)]
        ttl: Option<u64>,
    },
    /// Change a stream's assignable key or admission limits.
    Update {
        /// Dynamic stream name.
        name: String,
        /// Replacement assignable key.
        #[arg(long)]
        key: Option<String>,
        /// Replacement RPM ceiling for this stream.
        #[arg(long)]
        rpm: Option<u32>,
        /// Replacement TPM ceiling for this stream.
        #[arg(long)]
        tpm: Option<u64>,
    },
    /// Revoke every token on a stream without deleting the stream.
    Revoke {
        /// Dynamic stream name.
        name: String,
    },
    /// Work with tokens on a dynamic stream.
    Token {
        #[command(subcommand)]
        command: StreamTokenCommand,
    },
    /// Verify or inspect a hash-chained audit log without contacting a running instance.
    Audit {
        #[command(subcommand)]
        command: StreamAuditCommand,
    },
}

#[derive(Subcommand)]
enum StreamTokenCommand {
    /// Add a token and print it once.
    Add {
        /// Dynamic stream name.
        name: String,
        /// Token lifetime in seconds.
        #[arg(long)]
        ttl: Option<u64>,
    },
    /// Revoke one token by id.
    Revoke {
        /// Dynamic stream name.
        name: String,
        /// Token id from `streams show`.
        token_id: String,
    },
}

#[derive(Subcommand)]
enum StreamAuditCommand {
    /// Verify every record, including the hash chain, and print the record count.
    Verify {
        /// Path to the JSONL audit log.
        file: PathBuf,
    },
    /// Print the first records in the log.
    Head {
        /// Path to the JSONL audit log.
        file: PathBuf,
        /// Number of records to print; default 10.
        #[arg(long, default_value_t = 10)]
        n: usize,
    },
}

#[cfg(feature = "plugin-wasm")]
#[derive(Subcommand)]
enum PluginCommand {
    /// Inspect a Wasm plugin component and print its manifest, capabilities, and limits.
    Inspect {
        /// Path to the .wasm component file.
        file: PathBuf,
        /// Print JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
}

/// Where the admin listener is and the bearer token to use.
struct AdminTarget {
    base: String,
    token: String,
}

/// Resolves the admin address and token the same way for every command that calls the listener.
fn admin_target(
    config: &std::path::Path,
    url: Option<&str>,
    token_file: Option<&std::path::Path>,
) -> Result<AdminTarget, String> {
    let token = match token_file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?
            .trim()
            .to_owned(),
        None => std::env::var("REIKI_ADMIN_TOKEN").unwrap_or_default(),
    };
    if token.is_empty() {
        return Err("give the admin token with --token-file or REIKI_ADMIN_TOKEN".to_owned());
    }
    let base = match url {
        Some(url) => url.trim_end_matches('/').to_owned(),
        None => {
            let listen = std::fs::read_to_string(config)
                .map_err(|e| format!("cannot read {}: {e}", config.display()))
                .and_then(|text| Config::parse(&text).map_err(|e| e.to_string()))
                .map(|c| c.admin.map(|a| a.listen));
            match listen {
                Ok(Some(listen)) => {
                    format!("http://{}", listen.replace("0.0.0.0", "127.0.0.1"))
                }
                Ok(None) => {
                    return Err("the config has no [admin] listener; pass --url".to_owned());
                }
                Err(e) => return Err(format!("{e}; pass --url")),
            }
        }
    };
    Ok(AdminTarget { base, token })
}

/// The `reiki reload` command: asks the running instance over its admin listener.
async fn run_reload(
    config: &std::path::Path,
    dry_run: bool,
    url: Option<&str>,
    token_file: Option<&std::path::Path>,
) -> i32 {
    let target = match admin_target(config, url, token_file) {
        Ok(target) => target,
        Err(e) => {
            eprintln!("reiki: {e}");
            return 2;
        }
    };
    let query = if dry_run { "?dry_run=true" } else { "" };
    let response = reqwest::Client::new()
        .post(format!("{}/v1/reload{query}", target.base))
        .bearer_auth(&target.token)
        .send()
        .await;
    match response {
        Ok(response) => {
            let ok = response.status().is_success();
            let status = response.status();
            match response.text().await {
                Ok(body) if !body.is_empty() => println!("{body}"),
                Ok(_) => println!("{status}"),
                Err(e) => eprintln!("reiki: cannot read the answer: {e}"),
            }
            i32::from(!ok)
        }
        Err(e) => {
            eprintln!("reiki: cannot reach {}: {e}", target.base);
            2
        }
    }
}

/// The `reiki keys` command: reads health from the running instance's `/v1/status`.
async fn run_keys(
    config: &std::path::Path,
    url: Option<&str>,
    token_file: Option<&std::path::Path>,
    json: bool,
) -> i32 {
    let target = match admin_target(config, url, token_file) {
        Ok(target) => target,
        Err(e) => {
            eprintln!("reiki: {e}");
            return 2;
        }
    };
    let response = reqwest::Client::new()
        .get(format!("{}/v1/status", target.base))
        .bearer_auth(&target.token)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            eprintln!("reiki: cannot reach {}: {e}", target.base);
            return 2;
        }
    };
    let status = response.status();
    let body = match response.text().await {
        Ok(body) => body,
        Err(e) => {
            eprintln!("reiki: cannot read the answer: {e}");
            return 2;
        }
    };
    if !status.is_success() {
        if !body.is_empty() {
            eprintln!("{body}");
        }
        eprintln!("reiki: the admin listener answered {status}");
        return 1;
    }
    let keys = match reiki::keys::parse_status(&body) {
        Ok(keys) => keys,
        Err(e) => {
            eprintln!("reiki: {e}");
            return 1;
        }
    };
    let output = if json {
        reiki::keys::render_json(&keys)
    } else {
        Ok(reiki::keys::render_table(&keys))
    };
    match output {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(e) => {
            eprintln!("reiki: {e}");
            1
        }
    }
}

/// The `reiki failures` command: reads the running instance's `/v1/failures`.
async fn run_failures(
    config: &std::path::Path,
    limit: u8,
    key: Option<&str>,
    url: Option<&str>,
    token_file: Option<&std::path::Path>,
    json: bool,
) -> i32 {
    let target = match admin_target(config, url, token_file) {
        Ok(target) => target,
        Err(e) => {
            eprintln!("reiki: {e}");
            return 2;
        }
    };
    let mut request = format!("{}/v1/failures?limit={limit}", target.base);
    if let Some(key) = key {
        request.push_str("&key=");
        request.push_str(&percent_encode_query(key));
    }
    let response = reqwest::Client::new()
        .get(request)
        .bearer_auth(&target.token)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            eprintln!("reiki: cannot reach {}: {e}", target.base);
            return 2;
        }
    };
    let status = response.status();
    let body = match response.text().await {
        Ok(body) => body,
        Err(e) => {
            eprintln!("reiki: cannot read the answer: {e}");
            return 2;
        }
    };
    if !status.is_success() {
        if !body.is_empty() {
            eprintln!("{body}");
        }
        eprintln!("reiki: the admin listener answered {status}");
        return 1;
    }
    let records = match reiki::failures::parse(&body) {
        Ok(records) => records,
        Err(e) => {
            eprintln!("reiki: {e}");
            return 1;
        }
    };
    let output = if json {
        reiki::failures::render_json(&records)
    } else {
        Ok(reiki::failures::render_table(&records, terminal_width()))
    };
    match output {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(e) => {
            eprintln!("reiki: {e}");
            1
        }
    }
}

/// The `reiki cost export` command: streams the admin plugin's answer to stdout.
#[cfg(feature = "plugin-cost")]
async fn run_cost_export(
    config: &std::path::Path,
    period: &str,
    format: CostFormat,
    url: Option<&str>,
    token_file: Option<&std::path::Path>,
) -> i32 {
    if !reiki_cost::validate_period(period) {
        eprintln!("reiki: period must be YYYY-MM, YYYY-MM-DD or YYYY-MM-DDTHH");
        return 2;
    }
    let target = match admin_target(config, url, token_file) {
        Ok(target) => target,
        Err(e) => {
            eprintln!("reiki: {e}");
            return 2;
        }
    };
    let query = format!(
        "period={}&format={}",
        percent_encode_query(period),
        format.as_str()
    );
    let response = reqwest::Client::new()
        .get(format!("{}/v1/cost?{query}", target.base))
        .bearer_auth(&target.token)
        .send()
        .await;
    match response {
        Ok(response) => {
            let ok = response.status().is_success();
            let status = response.status();
            match response.text().await {
                Ok(body) if ok => println!("{body}"),
                Ok(body) if !body.is_empty() => eprintln!("reiki: {status}: {body}"),
                Ok(_) => eprintln!("reiki: {status}"),
                Err(e) => eprintln!("reiki: cannot read the answer: {e}"),
            }
            i32::from(!ok)
        }
        Err(e) => {
            eprintln!("reiki: cannot reach {}: {e}", target.base);
            2
        }
    }
}

/// Terminal width for clipped tables: `COLUMNS` when set, otherwise a plain 120.
fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|width| *width > 0)
        .unwrap_or(120)
}

/// Percent-encodes one query value, so a key name may contain any character.
fn percent_encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The `reiki keys --enable <name>` command: asks the running instance over its admin
/// listener. Exits 0 when the key was re-enabled, 1 when it was refused, and 2 when the
/// listener could not be reached.
async fn run_enable_key(
    config: &std::path::Path,
    key: &str,
    url: Option<&str>,
    token_file: Option<&std::path::Path>,
) -> i32 {
    let target = match admin_target(config, url, token_file) {
        Ok(target) => target,
        Err(e) => {
            eprintln!("reiki: {e}");
            return 2;
        }
    };
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/keys/enable?key={}",
            target.base,
            percent_encode_query(key)
        ))
        .bearer_auth(&target.token)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            eprintln!("reiki: cannot reach {}: {e}", target.base);
            return 2;
        }
    };
    let status = response.status();
    let body = match response.text().await {
        Ok(body) => body,
        Err(e) => {
            eprintln!("reiki: cannot read the answer: {e}");
            return 2;
        }
    };
    if !status.is_success() {
        if !body.is_empty() {
            eprintln!("{body}");
        }
        eprintln!("reiki: the admin listener answered {status}");
        return 1;
    }
    println!("{body}");
    0
}

struct StreamTarget {
    base: String,
    token: String,
    confirm: Option<String>,
    settings: reiki::config::StreamAdminConfig,
}

fn stream_target(
    config: &std::path::Path,
    url: Option<&str>,
    token_file: Option<&std::path::Path>,
) -> Result<StreamTarget, String> {
    let text = std::fs::read_to_string(config)
        .map_err(|e| format!("cannot read {}: {e}", config.display()))?;
    let parsed = Config::parse(&text).map_err(|e| format!("config: {e}"))?;
    let settings = parsed
        .stream_admin
        .clone()
        .ok_or_else(|| "the config has no [stream_admin] table".to_owned())?;
    let admin = parsed
        .admin
        .as_ref()
        .ok_or_else(|| "the config has no [admin] listener".to_owned())?;
    let token = match token_file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read the token file: {e}"))?
            .trim()
            .to_owned(),
        None => std::env::var(&settings.token_env).unwrap_or_default(),
    };
    if token.is_empty() {
        return Err(format!(
            "give the stream-admin token with --token-file or {}",
            settings.token_env
        ));
    }
    let confirm = std::env::var(&settings.confirm_env)
        .ok()
        .filter(|v| !v.is_empty());
    if !settings.require_two_party.is_empty() && confirm.is_none() {
        return Err(format!(
            "two-party confirmation is configured; set {}",
            settings.confirm_env
        ));
    }
    let base = match url {
        Some(url) => url.trim_end_matches('/').to_owned(),
        None => format!("http://{}", admin.listen.replace("0.0.0.0", "127.0.0.1")),
    };
    Ok(StreamTarget {
        base,
        token,
        confirm,
        settings,
    })
}

fn mutation_meta(
    reason: Option<&String>,
    key: Option<&String>,
) -> Result<(String, String), String> {
    let reason = reason
        .filter(|reason| !reason.is_empty())
        .ok_or_else(|| "mutations need --reason".to_owned())?
        .clone();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let key = key
        .filter(|key| !key.is_empty())
        .cloned()
        .unwrap_or_else(|| format!("cli-{}-{nanos}", std::process::id()));
    Ok((reason, key))
}

async fn stream_call(
    target: &StreamTarget,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    idempotency: Option<&str>,
) -> i32 {
    let mut request = reqwest::Client::new()
        .request(method.clone(), format!("{}{}", target.base, path))
        .bearer_auth(&target.token);
    if method != reqwest::Method::GET && method != reqwest::Method::HEAD {
        if let Some(confirm) = &target.confirm {
            request = request.header("x-confirm-token", confirm);
        }
        if let Some(key) = idempotency {
            request = request.header("idempotency-key", key);
        }
    }
    if let Some(body) = body {
        match serde_json::to_vec(&body) {
            Ok(bytes) => {
                request = request
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(bytes);
            }
            Err(e) => {
                eprintln!("reiki: cannot encode the request: {e}");
                return 2;
            }
        }
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            eprintln!("reiki: cannot reach {}: {e}", target.base);
            return 2;
        }
    };
    let status = response.status();
    let text = match response.text().await {
        Ok(text) => text,
        Err(e) => {
            eprintln!("reiki: cannot read the answer: {e}");
            return 2;
        }
    };
    if !text.is_empty() {
        println!("{text}");
    }
    if status.is_success() {
        0
    } else {
        eprintln!("reiki: the stream admin API answered {status}");
        1
    }
}

fn streams_usage_error(message: &str) -> i32 {
    eprintln!("reiki: {message}");
    2
}

async fn run_streams(
    config: &std::path::Path,
    url: Option<&str>,
    token_file: Option<&std::path::Path>,
    reason: Option<&String>,
    idempotency: Option<&String>,
    command: &StreamsCommand,
) -> i32 {
    if let StreamsCommand::Audit { command } = command {
        return match command {
            StreamAuditCommand::Verify { file } => match reiki::stream_admin::verify_audit(file) {
                Ok(count) => {
                    println!("ok: {count} audit record(s), chain verified");
                    0
                }
                Err(error) => streams_usage_error(&format!("audit verify failed: {error}")),
            },
            StreamAuditCommand::Head { file, n } => {
                match reiki::stream_admin::audit_head(file, *n) {
                    Ok(records) => {
                        for record in records {
                            match serde_json::to_string(&record) {
                                Ok(line) => println!("{line}"),
                                Err(_) => {
                                    return streams_usage_error("cannot render an audit record");
                                }
                            }
                        }
                        0
                    }
                    Err(error) => streams_usage_error(&format!("audit head failed: {error}")),
                }
            }
        };
    }
    let target = match stream_target(config, url, token_file) {
        Ok(target) => target,
        Err(error) => return streams_usage_error(&error),
    };
    let _ = &target.settings;
    match command {
        StreamsCommand::List => {
            stream_call(&target, reqwest::Method::GET, "/v1/streams", None, None).await
        }
        StreamsCommand::Show { name } => {
            stream_call(
                &target,
                reqwest::Method::GET,
                &format!("/v1/streams/{}", percent_encode_query(name)),
                None,
                None,
            )
            .await
        }
        StreamsCommand::Create { name, key, ttl } => {
            let (reason, idempotency) = match mutation_meta(reason, idempotency) {
                Ok(meta) => meta,
                Err(error) => return streams_usage_error(&error),
            };
            let mut body = serde_json::json!({"name": name, "key": key, "reason": reason});
            if let Some(ttl) = ttl {
                body["ttl"] = serde_json::json!(ttl);
            }
            stream_call(
                &target,
                reqwest::Method::POST,
                "/v1/streams/create",
                Some(body),
                Some(&idempotency),
            )
            .await
        }
        StreamsCommand::Update {
            name,
            key,
            rpm,
            tpm,
        } => {
            let (reason, idempotency) = match mutation_meta(reason, idempotency) {
                Ok(meta) => meta,
                Err(error) => return streams_usage_error(&error),
            };
            let mut body = serde_json::json!({"reason": reason});
            if let Some(key) = key {
                body["key"] = serde_json::json!(key);
            }
            let mut limits = serde_json::Map::new();
            if let Some(rpm) = rpm {
                limits.insert("rpm".to_owned(), serde_json::json!(rpm));
            }
            if let Some(tpm) = tpm {
                limits.insert("tpm".to_owned(), serde_json::json!(tpm));
            }
            if !limits.is_empty() {
                body["limits"] = serde_json::Value::Object(limits);
            }
            stream_call(
                &target,
                reqwest::Method::PUT,
                &format!("/v1/streams/{}", percent_encode_query(name)),
                Some(body),
                Some(&idempotency),
            )
            .await
        }
        StreamsCommand::Revoke { name } => {
            let (reason, idempotency) = match mutation_meta(reason, idempotency) {
                Ok(meta) => meta,
                Err(error) => return streams_usage_error(&error),
            };
            stream_call(
                &target,
                reqwest::Method::POST,
                &format!("/v1/streams/{}/revoke", percent_encode_query(name)),
                Some(serde_json::json!({"reason": reason})),
                Some(&idempotency),
            )
            .await
        }
        StreamsCommand::Token { command } => match command {
            StreamTokenCommand::Add { name, ttl } => {
                let (reason, idempotency) = match mutation_meta(reason, idempotency) {
                    Ok(meta) => meta,
                    Err(error) => return streams_usage_error(&error),
                };
                let mut body = serde_json::json!({"reason": reason});
                if let Some(ttl) = ttl {
                    body["ttl"] = serde_json::json!(ttl);
                }
                stream_call(
                    &target,
                    reqwest::Method::POST,
                    &format!("/v1/streams/{}/tokens", percent_encode_query(name)),
                    Some(body),
                    Some(&idempotency),
                )
                .await
            }
            StreamTokenCommand::Revoke { name, token_id } => {
                let (reason, idempotency) = match mutation_meta(reason, idempotency) {
                    Ok(meta) => meta,
                    Err(error) => return streams_usage_error(&error),
                };
                stream_call(
                    &target,
                    reqwest::Method::POST,
                    &format!(
                        "/v1/streams/{}/tokens/{}/revoke",
                        percent_encode_query(name),
                        percent_encode_query(token_id)
                    ),
                    Some(serde_json::json!({"reason": reason})),
                    Some(&idempotency),
                )
                .await
            }
        },
        StreamsCommand::Audit { .. } => unreachable!("handled above"),
    }
}

#[cfg(feature = "plugin-wasm")]
fn print_inspect_report(report: &reiki_wasm::InspectReport) {
    println!("file: {}", report.path);
    println!("sha256: {}", report.sha256);
    println!("name: {}", report.name);
    println!("version: {}", report.version);
    println!("wit-world: {}", report.wit_world);
    println!(
        "hooks: {}",
        if report.hooks.is_empty() {
            "(none)".to_owned()
        } else {
            report.hooks.join(", ")
        }
    );
    println!(
        "derived-capabilities: {}",
        if report.derived_capabilities.is_empty() {
            "(none)".to_owned()
        } else {
            report.derived_capabilities.join(", ")
        }
    );
    println!(
        "requested-gated-capabilities: {}",
        if report.requested_gated_capabilities.is_empty() {
            "(none)".to_owned()
        } else {
            report.requested_gated_capabilities.join(", ")
        }
    );
    println!(
        "default-allowed-capabilities: {}",
        report.default_allowed_capabilities.join(", ")
    );
    println!("limits:");
    println!("  memory-mib: {}", report.manifest_limits.memory_mib);
    println!("  fuel-per-call: {}", report.manifest_limits.fuel_per_call);
    println!("  fuel-per-kib: {}", report.manifest_limits.fuel_per_kib);
    println!("  deadline-ms: {}", report.manifest_limits.deadline_ms);
    println!(
        "  wall-budget-ms: {}",
        report.manifest_limits.wall_budget_ms
    );
    println!("  max-body-kib: {}", report.manifest_limits.max_body_kib);
    println!(
        "pin: manifest-present={} section={}",
        report.pin.manifest_present, report.pin.embedded_section
    );
}

fn fail(message: &str) -> ! {
    eprintln!("reiki: {message}");
    std::process::exit(1);
}

fn effective_listen(flag: Option<&str>, env: Option<&str>, config: &str) -> Result<String, String> {
    let chosen = flag.or(env).unwrap_or(config);
    if let Err(e) = chosen.parse::<std::net::SocketAddr>() {
        // `TcpListener::bind` also accepts `host:port`, so allow that instead
        // of rejecting a hostname outright.
        let host_port = chosen
            .rsplit_once(':')
            .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok());
        if !host_port {
            return Err(format!("invalid listen address `{chosen}`: {e}"));
        }
    }
    Ok(chosen.to_owned())
}

/// Reads and parses the config for a command that only reads `[peers]`; exits 2 when it cannot.
#[cfg(feature = "peers")]
fn load_peers_config(path: &std::path::Path) -> Config {
    std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
        .and_then(|text| Config::parse(&text).map_err(|e| format!("config: {e}")))
        .unwrap_or_else(|e| {
            eprintln!("reiki: {e}");
            std::process::exit(2);
        })
}

#[cfg(unix)]
fn spawn_sighup_reload(app: Arc<App>, config: PathBuf) {
    let _handle = tokio::spawn(async move {
        let mut hangup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        {
            Ok(stream) => stream,
            Err(e) => {
                tracing::warn!(error = %e, "cannot install SIGHUP reload handler");
                return;
            }
        };
        while hangup.recv().await.is_some() {
            // The reload reads files and builds plugins, which must not block the runtime.
            let (worker, path) = (Arc::clone(&app), config.clone());
            if tokio::task::spawn_blocking(move || worker.reload_on_hangup(&path))
                .await
                .is_err()
            {
                tracing::warn!("the SIGHUP reload task failed");
            }
        }
    });
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Some(Command::Scan {
        plugins,
        kinds,
        all,
        json,
        files,
    }) = &cli.command
    {
        std::process::exit(reiki::scan::run(plugins, kinds, *all, *json, files));
    }
    if let Some(Command::Reload {
        dry_run,
        url,
        token_file,
    }) = &cli.command
    {
        std::process::exit(
            run_reload(&cli.config, *dry_run, url.as_deref(), token_file.as_deref()).await,
        );
    }
    if let Some(Command::Keys {
        check,
        enable,
        json,
        url,
        token_file,
    }) = &cli.command
    {
        if *check {
            let text = std::fs::read_to_string(&cli.config).unwrap_or_else(|e| {
                eprintln!("reiki: cannot read {}: {e}", cli.config.display());
                std::process::exit(2);
            });
            let config = Config::parse(&text).unwrap_or_else(|e| {
                eprintln!("reiki: config: {e}");
                std::process::exit(2);
            });
            let checks = reiki::keys::check_files(&config);
            print!("{}", reiki::keys::render_check(&checks));
            std::process::exit(i32::from(reiki::keys::check_has_problems(&checks)));
        }
        if let Some(key) = enable {
            std::process::exit(
                run_enable_key(&cli.config, key, url.as_deref(), token_file.as_deref()).await,
            );
        }
        std::process::exit(
            run_keys(&cli.config, url.as_deref(), token_file.as_deref(), *json).await,
        );
    }
    if let Some(Command::Failures {
        limit,
        key,
        json,
        url,
        token_file,
    }) = &cli.command
    {
        std::process::exit(
            run_failures(
                &cli.config,
                *limit,
                key.as_deref(),
                url.as_deref(),
                token_file.as_deref(),
                *json,
            )
            .await,
        );
    }
    if let Some(Command::Streams {
        url,
        token_file,
        reason,
        idempotency_key,
        command,
    }) = &cli.command
    {
        std::process::exit(
            run_streams(
                &cli.config,
                url.as_deref(),
                token_file.as_deref(),
                reason.as_ref(),
                idempotency_key.as_ref(),
                command,
            )
            .await,
        );
    }
    #[cfg(feature = "peers")]
    if let Some(Command::Peers { json, watch }) = &cli.command {
        let config = load_peers_config(&cli.config);
        std::process::exit(reiki::peers::client::run(config.peers.as_ref(), *json, *watch).await);
    }
    #[cfg(feature = "top")]
    if let Some(Command::Top {
        once,
        interval,
        full,
        provider,
    }) = &cli.command
    {
        let config = load_peers_config(&cli.config);
        let local = reiki::top::LocalFailures::from_config(&config);
        std::process::exit(
            reiki::top::run_with_local(
                config.peers.as_ref(),
                local,
                *once,
                *interval,
                *full,
                provider.as_deref(),
            )
            .await,
        );
    }
    #[cfg(feature = "plugin-cost")]
    if let Some(Command::Cost { command }) = &cli.command {
        match command {
            CostCommand::Export {
                period,
                format,
                url,
                token_file,
            } => {
                std::process::exit(
                    run_cost_export(
                        &cli.config,
                        period,
                        *format,
                        url.as_deref(),
                        token_file.as_deref(),
                    )
                    .await,
                );
            }
        }
    }
    if let Some(Command::HashToken) = cli.command {
        let mut token = String::new();
        if std::io::stdin().read_to_string(&mut token).is_err() {
            fail("cannot read stdin");
        }
        match reiki::auth::hash_token(&token) {
            Ok(hash) => println!("{hash}"),
            Err(error) => fail(&format!("hash-token: {error}")),
        }
        return;
    }

    if let Some(Command::CaInit {
        dir,
        hosts,
        days,
        replace,
    }) = &cli.command
    {
        match reiki::intercept::init_ca(dir, hosts, *days, *replace) {
            Ok(()) => println!(
                "wrote {} and {}; give ca.pem to clients, keep ca.key private",
                dir.join("ca.pem").display(),
                dir.join("ca.key").display()
            ),
            Err(e) => fail(&e),
        }
        return;
    }

    #[cfg(feature = "plugin-wasm")]
    if let Some(Command::Plugin { command }) = &cli.command {
        match command {
            PluginCommand::Inspect { file, json } => match reiki_wasm::inspect_component(file) {
                Ok(report) => {
                    if *json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&report).unwrap_or_else(|e| {
                                eprintln!("reiki: {e}");
                                std::process::exit(1);
                            })
                        );
                    } else {
                        print_inspect_report(&report);
                    }
                }
                Err(e) => {
                    eprintln!("reiki: {e}");
                    std::process::exit(1);
                }
            },
        }
        return;
    }

    let text = std::fs::read_to_string(&cli.config)
        .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", cli.config.display())));
    let config = Config::parse(&text).unwrap_or_else(|e| fail(&format!("config: {e}")));
    let checking = matches!(cli.command, Some(Command::Check));
    let mut deferred_warnings = Vec::new();
    let mut log_with_file = !checking;
    if !checking && let Some(file) = &config.log.file {
        let dir = std::path::Path::new(file)
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        match reiki::state::check_writable_startup(
            dir,
            "log directory",
            // A log file that cannot be written must never keep the gateway from starting: the
            // request log already counts write errors, so fall back to no file logging.
            reiki::config::StoreErrorMode::Open,
        ) {
            Ok(None) => {}
            Ok(Some(warning)) => {
                deferred_warnings.push(warning);
                log_with_file = false;
            }
            Err(e) => fail(&e),
        }
    }
    let logging =
        Arc::new(reiki::logging::init(&config.log, log_with_file).unwrap_or_else(|e| fail(&e)));
    reiki::logging::install_panic_hook();
    for warning in deferred_warnings {
        tracing::warn!("{warning}");
    }
    let keys = load_keys(&config).unwrap_or_else(|e| {
        // Name the key and the kind of failure; the key file's contents never reach stderr.
        fail(&format!("cannot load key `{}`: {}", e.key(), e.kind()))
    });
    let plugin_tables = if checking {
        with_dry_run(&config.plugins)
    } else {
        config.plugins.clone()
    };
    let mut state_path = cli.config.clone();
    match state_path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if !ext.is_empty() => {
            state_path.set_extension(format!("{ext}.state"));
        }
        _ => {
            state_path.set_extension("state");
        }
    }
    let host = PluginHost::with_state_path(state_path);
    let plugins = PluginChain::build_with_host(&plugin_tables, &builtin(), &host)
        .unwrap_or_else(|e| fail(&format!("config: {e}")));
    let env_listen = std::env::var("REIKI_LISTEN").ok();
    let listen = effective_listen(cli.listen.as_deref(), env_listen.as_deref(), &config.listen)
        .unwrap_or_else(|e| fail(&e));
    let admin_listen = config.admin.as_ref().map(|admin| admin.listen.clone());
    if admin_listen.as_deref() == Some(listen.as_str()) {
        fail("admin listen must differ from the client listen address");
    }
    let peer_listen = config
        .peers
        .as_ref()
        .filter(|peers| peers.publishing())
        .and_then(|peers| peers.listen.clone());
    #[cfg(feature = "peers")]
    let config_subscribers = config
        .peers
        .as_ref()
        .map_or(0, |peers| peers.max_subscribers as usize);
    #[cfg(feature = "peers")]
    let config_connections_per_addr = config
        .peers
        .as_ref()
        .map_or(4, |peers| peers.max_connections_per_addr as usize);
    if peer_listen.as_deref() == Some(listen.as_str()) {
        fail("peers listen must differ from the client listen address");
    }

    if let Some(Command::Check) = cli.command {
        if let Some(intercept) = config.tunnel.as_ref().and_then(|t| t.intercept.as_ref()) {
            match reiki::intercept::check_ca(intercept) {
                Ok(days) => println!("intercept CA valid for {days} more day(s)"),
                Err(e) => fail(&format!("intercept: {e}")),
            }
        }
        #[cfg(feature = "peers")]
        if let Some(peers) = config.peers.as_ref().filter(|p| p.publishing()) {
            match reiki::peers::tls::check(peers) {
                Ok(Some(days)) => println!("peers TLS certificate valid for {days} more day(s)"),
                Ok(None) => {}
                Err(e) => fail(&format!("peers: {e}")),
            }
        }
        for text in reiki::forward::socket_warnings(&config) {
            println!("warning: {text}");
        }
        let admin = admin_listen
            .as_ref()
            .map_or(String::new(), |addr| format!(", admin on {addr}"));
        let admin = match &config.tunnel {
            Some(tunnel) => {
                let intercepted = tunnel
                    .intercept
                    .as_ref()
                    .map_or(0, |intercept| intercept.hosts.len());
                let private = if tunnel.deny_private {
                    ", private ranges denied"
                } else {
                    ""
                };
                format!(
                    "{admin}, tunnel on {} ({} allowed, {intercepted} intercepted{private})",
                    tunnel.listen,
                    tunnel.allow.len()
                )
            }
            None => admin,
        };
        let names = plugins.names().collect::<Vec<_>>();
        let plugin_part = if names.is_empty() {
            String::new()
        } else {
            format!(", {} plugin(s)[{}]", names.len(), names.join(", "))
        };
        println!("{}", config.server.check_line());
        println!(
            "{}",
            reiki::config::PeersConfig::check_line(config.peers.as_ref())
        );
        #[cfg(feature = "hub")]
        if let Some(line) = config.hub_check_line() {
            println!("{line}");
        }
        println!(
            "ok: {} upstream(s), {} key(s), {} stream(s){plugin_part}, listening on {listen}{admin}",
            config.upstream.len(),
            keys.len(),
            config.streams.len(),
        );
        return;
    }

    let state_path = reiki::state::path_for(&cli.config);
    let state_dir = state_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    match reiki::state::check_writable_startup(
        state_dir,
        "state directory",
        config.gateway.on_store_error,
    ) {
        Ok(None) => {}
        Ok(Some(warning)) => tracing::warn!("{warning}"),
        Err(e) => fail(&e),
    }

    let tunnel_config = config.tunnel.clone();
    let caps = config.server;
    for text in reiki::forward::socket_warnings(&config) {
        tracing::warn!("{text}");
    }
    reiki::serve::set_drain_timeout(std::time::Duration::from_secs(
        config.service.shutdown_timeout_secs,
    ));
    reiki::lifecycle::spawn_signal_handler();
    reiki::lifecycle::warn_if_open_files_low();
    reiki::notify::spawn_watchdog();
    #[cfg(feature = "peers")]
    let peer_ca = config
        .peers
        .as_ref()
        .map(reiki::peers::tls::check_ca_files)
        .unwrap_or_default();
    let stream_admin_config = config.stream_admin.clone();
    let app = App::new(config, keys, Policy::default(), plugins);
    #[cfg(feature = "peers")]
    app.set_peer_ca(peer_ca);
    app.set_state_path(state_path.clone());
    app.start_state_writer();
    app.set_config_path(cli.config.clone());
    app.set_logging(Arc::clone(&logging));
    if let Some(settings) = &stream_admin_config {
        let manager = reiki::stream_admin::Manager::new(settings, state_dir);
        app.set_stream_admin(Arc::clone(&manager));
        manager.reload_from_disk();
    }
    tokio::spawn(reiki::reload::watch(Arc::clone(&app)));
    app.load_persisted_state();
    #[cfg(unix)]
    spawn_sighup_reload(Arc::clone(&app), cli.config.clone());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| fail(&format!("cannot bind {listen}: {e}")));
    // The other listeners drain with the client one; their tasks are joined before exit.
    let mut listeners: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    if let Some(addr) = &admin_listen {
        // Bind the validated literal, not a string a later resolution could point off loopback.
        let bind_addr = reiki::config::check_admin_listen(addr)
            .unwrap_or_else(|e| fail(&format!("invalid admin listen: {e}")));
        let admin_listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .unwrap_or_else(|e| fail(&format!("cannot bind admin {addr}: {e}")));
        tracing::info!(%addr, "reiki admin listening");
        let admin_router = reiki::admin::router(Arc::clone(&app));
        let limits = reiki::serve::ConnectionLimits {
            max_connections: caps.admin_max_connections as usize,
            max_connections_per_addr: caps.admin_max_connections_per_addr as usize,
            exempt_loopback: true,
        };
        listeners.push(tokio::spawn(async move {
            reiki::serve::serve_with_limits(
                admin_listener,
                admin_router,
                limits,
                reiki::lifecycle::stopped(),
            )
            .await;
        }));
    }
    #[cfg(feature = "peers")]
    if let Some(addr) = &peer_listen {
        let peer_listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| fail(&format!("cannot bind peers {addr}: {e}")));
        tracing::info!(%addr, "reiki peer health listening");
        // Room for every stream subscriber plus plain health polls.
        let max_open = config_subscribers + 16;
        let served = reiki::peers::serve_with_per_addr(
            peer_listener,
            Arc::clone(&app),
            max_open,
            config_connections_per_addr,
        )
        .unwrap_or_else(|e| fail(&format!("peers: {e}")));
        listeners.push(tokio::spawn(served));
    }
    if let Some(tunnel) = &tunnel_config {
        let tunnel_listener = tokio::net::TcpListener::bind(&tunnel.listen)
            .await
            .unwrap_or_else(|e| fail(&format!("cannot bind tunnel {}: {e}", tunnel.listen)));
        tracing::info!(addr = %tunnel.listen, destinations = tunnel.allow.len(), "reiki tunnel listening");
        let mut built = reiki::tunnel::Tunnel::new(tunnel);
        if let Some(intercept) = &tunnel.intercept {
            let interceptor = reiki::intercept::Interceptor::new(Arc::clone(&app), intercept)
                .unwrap_or_else(|e| fail(&format!("intercept: {e}")));
            tracing::info!(hosts = intercept.hosts.len(), "reiki tunnel intercepting");
            built = built.with_interceptor(Arc::new(interceptor));
        }
        let built = Arc::new(built);
        app.set_tunnel(Arc::clone(&built));
        listeners.push(tokio::spawn(reiki::tunnel::serve(tunnel_listener, built)));
    }
    tracing::info!(%listen, "reiki listening");
    reiki::notify::ready(&format!("listening on {listen}"));
    let flush = Arc::clone(&app);
    reiki::serve::serve_with_limits(
        listener,
        router(app),
        reiki::serve::ConnectionLimits {
            max_connections: caps.max_connections as usize,
            max_connections_per_addr: caps.max_connections_per_addr as usize,
            exempt_loopback: true,
        },
        reiki::lifecycle::stopped(),
    )
    .await;
    // The client listener has drained; the others started draining at the same moment.
    let grace = reiki::serve::drain_timeout() + std::time::Duration::from_secs(2);
    for task in listeners {
        let _ = tokio::time::timeout(grace, task).await;
    }
    // Writing out the request log blocks, so it stays off the async threads.
    let plugins = Arc::clone(&flush);
    let _ = tokio::task::spawn_blocking(move || plugins.shutdown_plugins()).await;
    flush.flush_state();
    tracing::info!("stopped");
}

#[cfg(test)]
mod tests {
    use super::effective_listen;

    #[test]
    fn config_address_is_used_when_the_flag_and_variable_are_absent() {
        assert_eq!(
            effective_listen(None, None, "127.0.0.1:4097").unwrap(),
            "127.0.0.1:4097"
        );
    }

    #[test]
    fn the_variable_overrides_the_config_address() {
        assert_eq!(
            effective_listen(None, Some("0.0.0.0:5000"), "127.0.0.1:4097").unwrap(),
            "0.0.0.0:5000"
        );
    }

    #[test]
    fn the_flag_overrides_the_variable_and_the_config_address() {
        assert_eq!(
            effective_listen(
                Some("127.0.0.1:6000"),
                Some("0.0.0.0:5000"),
                "127.0.0.1:4097"
            )
            .unwrap(),
            "127.0.0.1:6000"
        );
    }

    #[test]
    fn a_hostname_is_accepted() {
        assert_eq!(
            effective_listen(Some("localhost:4097"), None, "127.0.0.1:4097").unwrap(),
            "localhost:4097"
        );
    }

    #[test]
    fn an_invalid_address_is_rejected() {
        let err = effective_listen(Some("not-an-address"), None, "127.0.0.1:4097").unwrap_err();
        assert!(err.contains("invalid listen address"), "error was: {err}");
        assert!(effective_listen(None, Some("127.0.0.1:99999"), "").is_err());
    }
}
