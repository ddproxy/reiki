//! Tunnel reload: policy and intercept hosts apply in place, and a changed CA is validated before the swap

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, GeneralSubtree, IsCa, KeyPair, KeyUsagePurpose,
    NameConstraints,
};
use reiki::chain::Policy;
use reiki::config::{Config, TunnelConfig};
use reiki::intercept::{init_ca, Interceptor};
use reiki::plugins::PluginChain;
use reiki::proxy::{load_keys, App};
use reiki::tunnel::Tunnel;
use time::OffsetDateTime;

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

/// Writes a CA with the given validity window and DNS name constraint; no constraint when `dns` is empty.
fn write_ca(dir: &Path, dns: &[&str], not_before: OffsetDateTime, not_after: OffsetDateTime) {
    std::fs::create_dir_all(dir).unwrap();
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    if !dns.is_empty() {
        params.name_constraints = Some(NameConstraints {
            permitted_subtrees: dns
                .iter()
                .map(|d| GeneralSubtree::DnsName((*d).to_owned()))
                .collect(),
            excluded_subtrees: Vec::new(),
        });
    }
    params.not_before = not_before;
    params.not_after = not_after;
    let cert = params.self_signed(&key).unwrap();
    std::fs::write(dir.join("ca.pem"), cert.pem()).unwrap();
    std::fs::write(dir.join("ca.key"), key.serialize_pem()).unwrap();
}

fn write_valid_ca(dir: &Path, dns: &[&str]) {
    let now = OffsetDateTime::now_utc();
    write_ca(
        dir,
        dns,
        now - time::Duration::hours(1),
        now + time::Duration::days(30),
    );
}

#[test]
fn a_different_valid_ca_is_planned_as_a_change() {
    let rig = rig();
    let other = rig.dir.join("other-ca");
    init_ca(&other, &HOSTS.map(str::to_owned), 30, false).unwrap();
    let plan = rig
        .tunnel
        .plan_reload(&rig.config(&["api.test"], "", &other.display().to_string()))
        .unwrap();
    assert!(
        plan.changed.contains(&"intercept ca".to_owned()),
        "{:?}",
        plan.changed
    );
    assert!(plan.restart_needed.is_empty(), "{:?}", plan.restart_needed);
}

#[test]
fn a_ca_that_misses_a_configured_host_refuses_the_reload() {
    let rig = rig();
    let narrow = rig.dir.join("narrow-ca");
    write_valid_ca(&narrow, &["api.test"]);
    let error = rig
        .tunnel
        .plan_reload(&rig.config(
            &["api.test", "other.test"],
            "",
            &narrow.display().to_string(),
        ))
        .err()
        .unwrap_or_else(|| panic!("a CA that does not cover every host is refused"));
    assert!(error.contains("other.test"), "{error}");
}

#[test]
fn a_ca_key_that_does_not_match_its_certificate_refuses_the_reload() {
    let rig = rig();
    let (a, b, mixed) = (
        rig.dir.join("ca-a"),
        rig.dir.join("ca-b"),
        rig.dir.join("mixed"),
    );
    write_valid_ca(&a, &HOSTS);
    write_valid_ca(&b, &HOSTS);
    std::fs::create_dir_all(&mixed).unwrap();
    std::fs::copy(a.join("ca.pem"), mixed.join("ca.pem")).unwrap();
    std::fs::copy(b.join("ca.key"), mixed.join("ca.key")).unwrap();
    let error = rig
        .tunnel
        .plan_reload(&rig.config(&["api.test"], "", &mixed.display().to_string()))
        .err()
        .unwrap_or_else(|| panic!("a mismatched key is refused"));
    assert!(error.contains("does not match"), "{error}");
}

#[test]
fn an_expired_ca_refuses_the_reload() {
    let rig = rig();
    let old = rig.dir.join("expired-ca");
    let now = OffsetDateTime::now_utc();
    write_ca(
        &old,
        &HOSTS,
        now - time::Duration::days(60),
        now - time::Duration::days(1),
    );
    let error = rig
        .tunnel
        .plan_reload(&rig.config(&["api.test"], "", &old.display().to_string()))
        .err()
        .unwrap_or_else(|| panic!("an expired CA is refused"));
    assert!(error.contains("expired"), "{error}");
}

#[test]
fn a_ca_without_a_dns_name_constraint_refuses_the_reload() {
    let rig = rig();
    let open = rig.dir.join("unconstrained-ca");
    write_valid_ca(&open, &[]);
    let error = rig
        .tunnel
        .plan_reload(&rig.config(&["api.test"], "", &open.display().to_string()))
        .err()
        .unwrap_or_else(|| panic!("an unconstrained CA is refused"));
    assert!(error.contains("name constraint"), "{error}");
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
