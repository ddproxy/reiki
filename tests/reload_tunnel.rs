//! A reload changes the tunnel allowlist and the intercept host table in place, refuses a different
//! CA, and reports interception that appears or disappears as needing a restart.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use reiki::chain::Policy;
use reiki::config::{Config, TunnelConfig};
use reiki::intercept::{init_ca, Interceptor};
use reiki::plugins::PluginChain;
use reiki::proxy::{load_keys, App};
use reiki::tunnel::Tunnel;

const HOSTS: [&str; 2] = ["api.test", "other.test"];
static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Rig {
    dir: PathBuf,
    tunnel: Tunnel,
    interceptor: Arc<Interceptor>,
}

impl Rig {
    fn config(&self, hosts: &[&str], extra_tunnel: &str, ca: &str) -> TunnelConfig {
        let allow = hosts
            .iter()
            .map(|h| format!("\"{h}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let intercept = hosts
            .iter()
            .map(|h| format!("{{ host = \"{h}\", upstream = \"api\", stream = \"s\" }}"))
            .collect::<Vec<_>>()
            .join(", ");
        config_text(&self.dir, &allow, &intercept, extra_tunnel, ca)
            .tunnel
            .unwrap()
    }
}

fn config_text(
    dir: &std::path::Path,
    allow: &str,
    intercept_hosts: &str,
    extra_tunnel: &str,
    ca: &str,
) -> Config {
    Config::parse(&format!(
        r#"
        [upstream.api]
        base_url = "http://127.0.0.1:1/v1"
        [keys.k1]
        upstream = "api"
        file = "{key}"
        [streams.s]
        token_sha256 = "{hash}"
        chain = ["k1"]
        [tunnel]
        listen = "127.0.0.1:41099"
        allow = [{allow}]
        {extra_tunnel}
        [tunnel.intercept]
        ca_cert = "{ca}/ca.pem"
        ca_key = "{ca}/ca.key"
        hosts = [{intercept_hosts}]
        "#,
        key = dir.join("key").display(),
        hash = reiki::auth::sha256_hex("tok"),
    ))
    .unwrap()
}

fn rig() -> Rig {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("reiki-reload-tunnel-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("key"), "upstream-secret\n").unwrap();
    init_ca(&dir.join("ca"), &HOSTS.map(str::to_owned), 30, false).unwrap();
    let ca = dir.join("ca").display().to_string();
    let mut config = config_text(
        &dir,
        "\"api.test\"",
        "{ host = \"api.test\", upstream = \"api\", stream = \"s\" }",
        "",
        &ca,
    );
    let keys = load_keys(&config).unwrap();
    let tunnel_config = config.tunnel.take().unwrap();
    let app = App::new(config, keys, Policy::default(), PluginChain::default());
    let interceptor = Arc::new(
        Interceptor::new(Arc::clone(&app), tunnel_config.intercept.as_ref().unwrap()).unwrap(),
    );
    let tunnel = Tunnel::new(&tunnel_config).with_interceptor(Arc::clone(&interceptor));
    Rig {
        dir,
        tunnel,
        interceptor,
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn ca_of(rig: &Rig) -> String {
    rig.dir.join("ca").display().to_string()
}

#[test]
fn an_identical_config_plans_no_change() {
    let rig = rig();
    let plan = rig
        .tunnel
        .plan_reload(&rig.config(&["api.test"], "", &ca_of(&rig)))
        .unwrap();
    assert!(plan.changed.is_empty(), "{:?}", plan.changed);
    assert!(plan.restart_needed.is_empty());
}

#[test]
fn a_new_intercepted_host_and_allow_entry_apply_in_place() {
    let rig = rig();
    assert!(!rig.interceptor.handles("other.test"));
    let plan = rig
        .tunnel
        .plan_reload(&rig.config(&["api.test", "other.test"], "", &ca_of(&rig)))
        .unwrap();
    assert_eq!(
        plan.changed,
        ["tunnel allow +1/-0", "intercept hosts +1/-0, 0 edited"]
    );
    assert!(
        !rig.interceptor.handles("other.test"),
        "not before it is applied"
    );
    rig.tunnel.apply_reload(plan);
    assert!(rig.interceptor.handles("other.test"));
    assert!(rig.interceptor.handles("api.test"));
}

#[test]
fn require_auth_and_the_private_range_policy_change_live() {
    let rig = rig();
    assert!(
        !rig.interceptor.authorize("api.test", None),
        "a token is required"
    );
    let mut wanted = rig.config(&["api.test"], "deny_private = true", &ca_of(&rig));
    wanted.intercept.as_mut().unwrap().require_auth = false;
    let plan = rig.tunnel.plan_reload(&wanted).unwrap();
    assert!(
        plan.changed
            .contains(&"tunnel deny_private true".to_owned()),
        "{:?}",
        plan.changed
    );
    assert!(
        plan.changed
            .contains(&"intercept require_auth false".to_owned()),
        "{:?}",
        plan.changed
    );
    rig.tunnel.apply_reload(plan);
    assert!(
        rig.interceptor.authorize("api.test", None),
        "no token needed any more"
    );
}

#[test]
fn a_different_ca_refuses_the_reload() {
    let rig = rig();
    let other = rig.dir.join("other-ca");
    init_ca(&other, &HOSTS.map(str::to_owned), 30, false).unwrap();
    let error = rig
        .tunnel
        .plan_reload(&rig.config(&["api.test"], "", &other.display().to_string()))
        .err()
        .expect("a CA path change is refused");
    assert!(error.contains("restart"), "{error}");
}

#[test]
fn interception_that_appears_or_disappears_needs_a_restart() {
    let rig = rig();
    let mut without = rig.config(&["api.test"], "", &ca_of(&rig));
    without.intercept = None;
    let plan = rig.tunnel.plan_reload(&without).unwrap();
    assert_eq!(plan.restart_needed, ["tunnel.intercept removed"]);

    let plain = Tunnel::new(&without);
    let plan = plain
        .plan_reload(&rig.config(&["api.test"], "", &ca_of(&rig)))
        .unwrap();
    assert_eq!(plan.restart_needed, ["tunnel.intercept added"]);
}
