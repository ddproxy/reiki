use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{builtin, with_dry_run, PluginChain};
use reiki::proxy::{load_keys, router, App};

#[derive(Parser)]
#[command(
    version,
    about = "Security proxy that isolates LLM harnesses from keys and secrets"
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
        /// Plugin to run (phi, finance, gov); repeat for several. Default: every compiled-in one.
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
    /// Calls `POST /reload` on the admin listener and prints the JSON answer. Exits 0 when the
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
}

/// The `reiki reload` command: asks the running instance over its admin listener.
async fn run_reload(
    config: &std::path::Path,
    dry_run: bool,
    url: Option<&str>,
    token_file: Option<&std::path::Path>,
) -> i32 {
    let token = match token_file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text.trim().to_owned(),
            Err(e) => {
                eprintln!("reiki: cannot read {}: {e}", path.display());
                return 2;
            }
        },
        None => std::env::var("REIKI_ADMIN_TOKEN").unwrap_or_default(),
    };
    if token.is_empty() {
        eprintln!("reiki: give the admin token with --token-file or REIKI_ADMIN_TOKEN");
        return 2;
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
                    eprintln!("reiki: the config has no [admin] listener; pass --url");
                    return 2;
                }
                Err(e) => {
                    eprintln!("reiki: {e}; pass --url");
                    return 2;
                }
            }
        }
    };
    let query = if dry_run { "?dry_run=true" } else { "" };
    let response = reqwest::Client::new()
        .post(format!("{base}/reload{query}"))
        .bearer_auth(&token)
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
            eprintln!("reiki: cannot reach {base}: {e}");
            2
        }
    }
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

#[cfg(unix)]
fn spawn_sighup_reload(app: Arc<App>, config: PathBuf, logging: Arc<reiki::logging::Logging>) {
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
            app.reload_on_hangup(&config);
            match std::fs::read_to_string(&config)
                .map_err(|e| e.to_string())
                .and_then(|text| Config::parse(&text).map_err(|e| e.to_string()))
            {
                Ok(parsed) => logging.reload(&parsed.log),
                Err(error) => tracing::warn!(%error, "log settings not reloaded"),
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
    if let Some(Command::HashToken) = cli.command {
        let mut token = String::new();
        if std::io::stdin().read_to_string(&mut token).is_err() {
            fail("cannot read stdin");
        }
        println!("{}", reiki::auth::sha256_hex(token.trim()));
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

    let text = std::fs::read_to_string(&cli.config)
        .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", cli.config.display())));
    let mut config = Config::parse(&text).unwrap_or_else(|e| fail(&format!("config: {e}")));
    let checking = matches!(cli.command, Some(Command::Check));
    let logging =
        Arc::new(reiki::logging::init(&config.log, !checking).unwrap_or_else(|e| fail(&e)));
    let keys = load_keys(&config).unwrap_or_else(|e| fail(&e.to_string()));
    let plugin_tables = if checking {
        with_dry_run(&config.plugins)
    } else {
        config.plugins.clone()
    };
    let plugins = PluginChain::build(&plugin_tables, &builtin())
        .unwrap_or_else(|e| fail(&format!("config: {e}")));
    let env_listen = std::env::var("REIKI_LISTEN").ok();
    let listen = effective_listen(cli.listen.as_deref(), env_listen.as_deref(), &config.listen)
        .unwrap_or_else(|e| fail(&e));
    let admin_listen = config.admin.as_ref().map(|admin| admin.listen.clone());
    if admin_listen.as_deref() == Some(listen.as_str()) {
        fail("admin listen must differ from the client listen address");
    }

    if let Some(Command::Check) = cli.command {
        if let Some(intercept) = config.tunnel.as_ref().and_then(|t| t.intercept.as_ref()) {
            match reiki::intercept::check_ca(intercept) {
                Ok(days) => println!("intercept CA valid for {days} more day(s)"),
                Err(e) => fail(&format!("intercept: {e}")),
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
        println!(
            "ok: {} upstream(s), {} key(s), {} stream(s){plugin_part}, listening on {listen}{admin}",
            config.upstream.len(),
            keys.len(),
            config.streams.len(),
        );
        return;
    }

    let tunnel_config = config.tunnel.take();
    for text in reiki::forward::socket_warnings(&config) {
        tracing::warn!("{text}");
    }
    let app = App::new(config, keys, Policy::default(), plugins);
    app.set_state_path(reiki::state::path_for(&cli.config));
    app.set_config_path(cli.config.clone());
    tokio::spawn(reiki::reload::watch(Arc::clone(&app)));
    app.load_persisted_state();
    #[cfg(unix)]
    spawn_sighup_reload(Arc::clone(&app), cli.config.clone(), Arc::clone(&logging));
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| fail(&format!("cannot bind {listen}: {e}")));
    if let Some(addr) = &admin_listen {
        let admin_listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| fail(&format!("cannot bind admin {addr}: {e}")));
        tracing::info!(%addr, "reiki admin listening");
        let admin_router = reiki::admin::router(Arc::clone(&app));
        tokio::spawn(async move {
            reiki::serve::serve(admin_listener, admin_router, 64, std::future::pending()).await;
        });
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
        tokio::spawn(reiki::tunnel::serve(tunnel_listener, built));
    }
    tracing::info!(%listen, "reiki listening");
    reiki::serve::serve(listener, router(app), 1024, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await;
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
