use std::io::Read;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::proxy::{load_keys, router, App};

#[derive(Parser)]
#[command(version, about = "Key-failover relay for LLM APIs")]
struct Cli {
    /// Path to the configuration file.
    #[arg(long, default_value = "/etc/reiki/reiki.toml", global = true)]
    config: PathBuf,
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
    let config = Config::parse(&text).unwrap_or_else(|e| fail(&format!("config: {e}")));
    let keys = load_keys(&config).unwrap_or_else(|e| fail(&e.to_string()));

    if let Some(Command::Check) = cli.command {
        println!(
            "ok: {} upstream(s), {} key(s), {} stream(s)",
            config.upstream.len(),
            keys.len(),
            config.streams.len()
        );
        return;
    }

    let listen = config.listen.clone();
    let app = App::new(config, keys, Policy::default());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| fail(&format!("cannot bind {listen}: {e}")));
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
