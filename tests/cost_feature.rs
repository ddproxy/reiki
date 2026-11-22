//! Build-time and config-time guarantees for the optional cost plugin.

#[cfg(feature = "plugin-cost")]
use reiki::config::Config;
#[cfg(feature = "plugin-cost")]
use reiki::plugins::PluginChain;
use reiki::plugins::builtin;

#[cfg(feature = "plugin-cost")]
mod common;

const MANIFEST: &str = include_str!("../Cargo.toml");

#[test]
fn cost_is_compiled_in_but_off_by_default() {
    assert!(MANIFEST.contains("name = \"cost\""));
    let default = MANIFEST
        .lines()
        .find(|line| line.starts_with("default = ["))
        .expect("default features are listed");
    assert!(
        !default.contains("plugin-cost"),
        "cost must be opt-in: {default}"
    );

    #[cfg(feature = "plugin-cost")]
    assert!(builtin().iter().any(|entry| entry.name == "cost"));
    #[cfg(not(feature = "plugin-cost"))]
    assert!(!builtin().iter().any(|entry| entry.name == "cost"));
}

#[cfg(feature = "plugin-cost")]
#[test]
fn cost_plugin_owns_only_bounded_admin_paths() {
    let config = Config::parse(
        r#"
[plugins.cost]
[plugins.cost.prices.main]
currency = "USD"
[plugins.cost.prices.main.models]
"*" = { input_per_million = "1", output_per_million = "1" }
"#,
    )
    .unwrap();
    let plugins = PluginChain::build(&config.plugins, &builtin()).unwrap();
    let mut paths = plugins.admin_paths();
    paths.sort_unstable();
    assert_eq!(paths, ["/cost", "/cost/metrics"]);
    assert!(
        plugins
            .status_fragments()
            .first()
            .is_some_and(|fragment| fragment.get("cost").is_some())
    );
}

#[cfg(feature = "plugin-cost")]
#[test]
fn unknown_cost_config_keys_are_errors() {
    let config = Config::parse(
        r#"
[plugins.cost]
surprise = true
[plugins.cost.prices.main]
currency = "USD"
[plugins.cost.prices.main.models]
"*" = { input_per_million = "1", output_per_million = "1" }
"#,
    )
    .unwrap();
    let message = PluginChain::build(&config.plugins, &builtin())
        .unwrap_err()
        .to_string();
    assert!(message.contains("surprise"), "{message}");
}

#[cfg(feature = "plugin-cost")]
#[tokio::test]
async fn admin_cost_route_requires_the_admin_token() {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use reiki::chain::Policy;
    use reiki::proxy::{App, Secret};

    let admin_token = "cost-admin-secret";
    let stream_token = "cost-stream-secret";
    let text = format!(
        "[upstream.zen]\nbase_url = \"http://127.0.0.1:9/v1\"\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
         [streams.one]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
         [admin]\nlisten = \"127.0.0.1:0\"\ntoken_sha256 = \"{}\"\n\
         [plugins.cost]\n[plugins.cost.prices.zen]\ncurrency = \"USD\"\n\
         [plugins.cost.prices.zen.models]\n\"*\" = {{ input_per_million = \"1\", output_per_million = \"1\" }}\n",
        reiki::auth::sha256_hex(stream_token),
        reiki::auth::sha256_hex(admin_token),
    );
    let config = Config::parse(&text).unwrap();
    let plugins = PluginChain::build(&config.plugins, &builtin()).unwrap();
    let keys: HashMap<String, Secret> = [("a".to_owned(), Secret::new("up-a"))]
        .into_iter()
        .collect();
    let app = App::with_plugins(
        config,
        keys,
        Policy::default(),
        Duration::from_secs(120),
        plugins,
    );
    let admin = common::serve(reiki::admin::router(Arc::clone(&app))).await;

    let anonymous = reqwest::Client::new()
        .get(format!("{admin}/v1/cost"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);

    let authorized = reqwest::Client::new()
        .get(format!("{admin}/v1/cost"))
        .bearer_auth(admin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(authorized.status(), 200);
    let body: serde_json::Value = serde_json::from_str(&authorized.text().await.unwrap()).unwrap();
    assert_eq!(body["estimated"], true);
    assert_eq!(body["currency"], "USD");
}
