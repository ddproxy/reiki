use reiki_wasm::{
    Capability, HostCeilings, LimitsOverride, Manifest, ManifestLimits, ReloadDecision,
    is_wasm_plugin_table, parse_wasm_plugin_config, reload_narrowing,
};

fn base_table() -> toml::Table {
    toml::from_str(
        r#"
wasm = "/tmp/plugin.wasm"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
"#,
    )
    .unwrap()
}

#[test]
fn unknown_config_key_is_error() {
    let mut t = base_table();
    t.insert("unknown_key".into(), toml::Value::Boolean(true));
    let err = parse_wasm_plugin_config("p", &t).unwrap_err();
    assert!(err.contains("unknown setting"), "{err}");
}

#[test]
fn wasm_table_detection() {
    let t = base_table();
    assert!(is_wasm_plugin_table(&t));
    let t: toml::Table = toml::from_str("enabled = true").unwrap();
    assert!(!is_wasm_plugin_table(&t));
}

#[test]
fn grants_must_not_include_default_allowed() {
    let mut t = base_table();
    t.insert(
        "grants".into(),
        toml::Value::Array(vec![toml::Value::String("request-meta".into())]),
    );
    let err = parse_wasm_plugin_config("p", &t).unwrap_err();
    assert!(err.contains("default-allowed"), "{err}");
}

#[test]
fn grants_support_phase1_gated_caps() {
    let mut t = base_table();
    t.insert(
        "grants".into(),
        toml::Value::Array(vec![
            toml::Value::String("request-body:read".into()),
            toml::Value::String("request-body:edit".into()),
            toml::Value::String("finding:detail-vocab".into()),
        ]),
    );
    let cfg = parse_wasm_plugin_config("p", &t).unwrap();
    assert!(cfg.grants.contains(&Capability::RequestBodyRead));
    assert!(cfg.grants.contains(&Capability::RequestBodyEdit));
    assert!(cfg.grants.contains(&Capability::FindingDetailVocab));
}

#[test]
fn filesystem_and_network_cannot_be_granted() {
    for capability in ["wasi:filesystem", "wasi:sockets", "wasi:http"] {
        let mut t = base_table();
        t.insert(
            "grants".into(),
            toml::Value::Array(vec![toml::Value::String(capability.into())]),
        );
        let err = parse_wasm_plugin_config("p", &t).unwrap_err();
        assert!(err.contains("unknown grant"), "{capability}: {err}");
    }
}

#[test]
fn tool_allowlist_parses_and_rejects_bad_values() {
    let mut t = base_table();
    t.insert(
        "tool_allowlist".into(),
        toml::Value::Array(vec![toml::Value::String("search".into())]),
    );
    let cfg = parse_wasm_plugin_config("p", &t).unwrap();
    assert_eq!(cfg.tool_allowlist, Some(vec!["search".to_owned()]));

    let mut t = base_table();
    t.insert(
        "tool_allowlist".into(),
        toml::Value::Array(vec![toml::Value::String("".into())]),
    );
    assert!(
        parse_wasm_plugin_config("p", &t)
            .unwrap_err()
            .contains("empty")
    );

    let mut t = base_table();
    t.insert(
        "tool_allowlist".into(),
        toml::Value::Array(vec![toml::Value::Integer(1)]),
    );
    assert!(parse_wasm_plugin_config("p", &t).is_err());
}

#[test]
fn reload_narrowing_rejects_widening() {
    let old = parse_wasm_plugin_config("p", &base_table()).unwrap();
    let mut t2 = base_table();
    t2.insert(
        "grants".into(),
        toml::Value::Array(vec![toml::Value::String("request-body:read".into())]),
    );
    let new = parse_wasm_plugin_config("p", &t2).unwrap();
    match reload_narrowing(&old, &new, false) {
        ReloadDecision::RestartNeeded(reasons) => {
            assert!(reasons.iter().any(|r| r.contains("grant")), "{reasons:?}")
        }
        other => panic!("expected restart needed, got {other:?}"),
    }

    // narrowing: remove grant is live
    match reload_narrowing(&new, &old, false) {
        ReloadDecision::Live => {}
        other => panic!("expected live, got {other:?}"),
    }
}

#[test]
fn reload_narrowing_tool_allowlist_widening_needs_restart() {
    let mut t1 = base_table();
    t1.insert(
        "tool_allowlist".into(),
        toml::Value::Array(vec![toml::Value::String("a".into())]),
    );
    let old = parse_wasm_plugin_config("p", &t1).unwrap();
    let mut t2 = base_table();
    t2.insert(
        "tool_allowlist".into(),
        toml::Value::Array(vec![
            toml::Value::String("a".into()),
            toml::Value::String("b".into()),
        ]),
    );
    let new = parse_wasm_plugin_config("p", &t2).unwrap();
    assert!(matches!(
        reload_narrowing(&old, &new, false),
        ReloadDecision::RestartNeeded(_)
    ));
    // tightening is live
    assert!(matches!(
        reload_narrowing(&new, &old, false),
        ReloadDecision::Live
    ));
    // removing allowlist entirely is widening
    let no = parse_wasm_plugin_config("p", &base_table()).unwrap();
    assert!(matches!(
        reload_narrowing(&old, &no, false),
        ReloadDecision::RestartNeeded(_)
    ));
}

#[test]
fn reload_narrowing_checks_all_fields_when_the_pin_changes() {
    let mut t1 = base_table();
    t1.insert(
        "tool_allowlist".into(),
        toml::Value::Array(vec![toml::Value::String("search".into())]),
    );
    t1.insert(
        "streams".into(),
        toml::Value::Array(vec![toml::Value::String("s".into())]),
    );
    let old = parse_wasm_plugin_config("p", &t1).unwrap();

    let mut t2 = base_table();
    t2.insert("sha256".into(), toml::Value::String("1".repeat(64)));
    t2.insert(
        "grants".into(),
        toml::Value::Array(vec![toml::Value::String("request-body:read".into())]),
    );
    let new = parse_wasm_plugin_config("p", &t2).unwrap();

    match reload_narrowing(&old, &new, false) {
        ReloadDecision::RestartNeeded(reasons) => {
            assert!(reasons.iter().any(|r| r == "wasm"), "{reasons:?}");
            assert!(
                reasons.iter().any(|r| r.contains("tool_allowlist")),
                "{reasons:?}"
            );
            assert!(reasons.iter().any(|r| r == "streams:add"), "{reasons:?}");
        }
        other => panic!("expected restart needed, got {other:?}"),
    }
}

#[test]
fn security_config_and_confirm_changes_need_restart() {
    fn config_table(mode: &str, confirm: Option<&str>) -> toml::Table {
        let mut t = base_table();
        let mut config = toml::Table::new();
        config.insert("mode".into(), toml::Value::String(mode.into()));
        t.insert("config".into(), toml::Value::Table(config));
        t.insert("role".into(), toml::Value::String("security".into()));
        t.insert("on_error".into(), toml::Value::String("reject".into()));
        if let Some(value) = confirm {
            t.insert("confirm".into(), toml::Value::String(value.into()));
        }
        t
    }

    let old = parse_wasm_plugin_config("p", &config_table("a", Some("x"))).unwrap();
    let changed = parse_wasm_plugin_config("p", &config_table("b", Some("x"))).unwrap();
    match reload_narrowing(&old, &changed, false) {
        ReloadDecision::RestartNeeded(reasons) => assert!(reasons.iter().any(|r| r == "config")),
        other => panic!("expected restart needed, got {other:?}"),
    }

    let cleared = parse_wasm_plugin_config("p", &config_table("a", None)).unwrap();
    match reload_narrowing(&old, &cleared, false) {
        ReloadDecision::RestartNeeded(reasons) => assert!(reasons.iter().any(|r| r == "confirm")),
        other => panic!("expected restart needed, got {other:?}"),
    }
}

#[test]
fn manifest_parses_and_rejects_free_detail() {
    let err = Manifest::parse(
        r#"
[plugin]
name = "t"
version = "0.1.0"
wit-world = "reiki:plugin/plugin@1.0.0"
hooks = ["on-request"]
[requests]
finding-detail = "free"
"#,
    )
    .unwrap_err();
    assert!(err.contains("not supported"), "{err}");
}

#[test]
fn limits_override_may_only_lower() {
    let manifest = ManifestLimits::default();
    let ceilings = HostCeilings::default();
    let ov = LimitsOverride {
        memory_mib: Some(8),
        ..Default::default()
    };
    let eff = ov.resolve(&manifest, &ceilings).unwrap();
    assert_eq!(eff.memory_mib, 8);

    let ov = LimitsOverride {
        memory_mib: Some(manifest.memory_mib + 1),
        ..Default::default()
    };
    let err = ov.resolve(&manifest, &ceilings).unwrap_err();
    assert!(err.contains("only lower"), "{err}");
}
