use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{builtin, with_dry_run, PluginChain};
use reiki::proxy::{load_keys, router, App};

#[derive(Parser)]
#[command(version, about = "Key-failover relay for LLM APIs")]
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
            app.reload_on_hangup(&config);
        }
    });
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();

    if let Some(Command::HashToken) = cli.command {
        let mut token = String::new();
        if std::io::stdin().read_to_string(&mut token).is_err() {
            fail("cannot read stdin");
        }
        println!("{}", reiki::auth::sha256_hex(token.trim()));
        return;
    }

    let text = std::fs::read_to_string(&cli.config)
        .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", cli.config.display())));
    let mut config = Config::parse(&text).unwrap_or_else(|e| fail(&format!("config: {e}")));
    let keys = load_keys(&config).unwrap_or_else(|e| fail(&e.to_string()));
    let checking = matches!(cli.command, Some(Command::Check));
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
        let admin = admin_listen
            .as_ref()
            .map_or(String::new(), |addr| format!(", admin on {addr}"));
        let admin = match &config.tunnel {
            Some(tunnel) => format!("{admin}, tunnel on {}", tunnel.listen),
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
    let app = App::new(config, keys, Policy::default(), plugins);
    app.set_state_path(reiki::state::path_for(&cli.config));
    app.load_persisted_state();
    #[cfg(unix)]
    spawn_sighup_reload(Arc::clone(&app), cli.config.clone());
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
            if let Err(e) = axum::serve(admin_listener, admin_router).await {
                tracing::error!(error = %e, "admin server error");
            }
        });
    }
    if let Some(tunnel) = &tunnel_config {
        let tunnel_listener = tokio::net::TcpListener::bind(&tunnel.listen)
            .await
            .unwrap_or_else(|e| fail(&format!("cannot bind tunnel {}: {e}", tunnel.listen)));
        tracing::info!(addr = %tunnel.listen, destinations = tunnel.allow.len(), "reiki tunnel listening");
        tokio::spawn(reiki::tunnel::serve(
            tunnel_listener,
            Arc::new(reiki::tunnel::Tunnel::new(tunnel)),
        ));
    }
    tracing::info!(%listen, "reiki listening");
    let served = axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    if let Err(e) = served {
        fail(&format!("server error: {e}"));
    }
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
