use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Method};
use reiki_plugin::{Plugin, PluginError, RequestCtx};
use reiki_wasm::{
    Capability, HostCeilings, ReloadDecision, WasmEngine, WasmPlugin, inspect_component_bytes,
    parse_wasm_plugin_config, reload_narrowing,
};
use sha2::{Digest, Sha256};

const NO_GRANT: &[u8] = include_bytes!("fixtures/no-grant.wasm");
const BODY_EDIT: &[u8] = include_bytes!("fixtures/body-edit.wasm");
const FS_PROBE: &[u8] = include_bytes!("fixtures/fs-probe.wasm");
const WASI_IMPORTS: &[u8] = include_bytes!("fixtures/wasi-imports.wasm");
const FIXTURES: &[(&str, &[u8])] = &[
    ("no-grant", NO_GRANT),
    ("body-edit", BODY_EDIT),
    ("fs-probe", FS_PROBE),
    ("wasi-imports", WASI_IMPORTS),
];

fn sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn base_table(name: &str, bytes: &[u8], mode: &str) -> toml::Table {
    let mut table: toml::Table = toml::from_str(&format!(
        "wasm = \"/tmp/{name}.wasm\"\nsha256 = \"{}\"\n",
        sha256(bytes)
    ))
    .unwrap();
    if !mode.is_empty() {
        let mut config = toml::Table::new();
        config.insert("mode".to_owned(), toml::Value::String(mode.to_owned()));
        table.insert("config".to_owned(), toml::Value::Table(config));
    }
    table
}

fn set_grants(table: &mut toml::Table, grants: &[&str]) {
    table.insert(
        "grants".to_owned(),
        toml::Value::Array(
            grants
                .iter()
                .map(|g| toml::Value::String((*g).to_owned()))
                .collect(),
        ),
    );
}

fn set_limits(table: &mut toml::Table, limits: &[(&str, i64)]) {
    let mut t = toml::Table::new();
    for (key, value) in limits {
        t.insert((*key).to_owned(), toml::Value::Integer(*value));
    }
    table.insert("limits".to_owned(), toml::Value::Table(t));
}

fn set_string(table: &mut toml::Table, key: &str, value: &str) {
    table.insert(key.to_owned(), toml::Value::String(value.to_owned()));
}

fn set_tool_allowlist(table: &mut toml::Table, tools: &[&str]) {
    table.insert(
        "tool_allowlist".to_owned(),
        toml::Value::Array(
            tools
                .iter()
                .map(|tool| toml::Value::String((*tool).to_owned()))
                .collect(),
        ),
    );
}

fn engine() -> Arc<WasmEngine> {
    Arc::new(WasmEngine::with_ceilings(HostCeilings::default()).unwrap())
}

fn load(engine: &Arc<WasmEngine>, bytes: &[u8], table: &toml::Table) -> Result<WasmPlugin, String> {
    let name = table
        .get("wasm")
        .and_then(toml::Value::as_str)
        .and_then(|p| std::path::Path::new(p).file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("plugin");
    let config = parse_wasm_plugin_config(name, table)?;
    let loaded = engine.load_component_bytes(&config, bytes)?;
    WasmPlugin::from_loaded(engine.clone(), loaded)
}

struct Outcome {
    result: Result<(), PluginError>,
    body: Bytes,
    findings: Vec<reiki_plugin::Reported>,
}

fn invoke(plugin: &WasmPlugin, path: &str, body: &[u8]) -> Outcome {
    let method = Method::POST;
    let mut headers = HeaderMap::new();
    let mut body = Bytes::copy_from_slice(body);
    let mut state = ();
    let mut ctx = RequestCtx::new(
        "ct",
        "up",
        &method,
        path,
        &mut headers,
        &mut body,
        &mut state,
    );
    let result = plugin.on_request(&mut ctx);
    let findings = std::mem::take(&mut ctx.reported);
    drop(ctx);
    Outcome {
        result,
        body,
        findings,
    }
}

fn read_leb(bytes: &[u8], pos: usize) -> (u32, usize) {
    let mut value = 0u32;
    let mut shift = 0;
    for (i, b) in bytes[pos..].iter().take(5).enumerate() {
        value |= u32::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return (value, i + 1);
        }
        shift += 7;
    }
    panic!("invalid leb128");
}

fn write_leb(mut value: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return out;
        }
    }
}

fn custom_section(bytes: &[u8], wanted: &str) -> Option<String> {
    let mut pos = 8;
    while pos < bytes.len() {
        let id = bytes[pos];
        pos += 1;
        let (len, n) = read_leb(bytes, pos);
        pos += n;
        let end = pos + len as usize;
        if id == 0 {
            let (name_len, n) = read_leb(bytes, pos);
            let name_start = pos + n;
            let name_end = name_start + name_len as usize;
            let name = std::str::from_utf8(&bytes[name_start..name_end]).ok()?;
            if name == wanted {
                return String::from_utf8(bytes[name_end..end].to_vec()).ok();
            }
        }
        pos = end;
    }
    None
}

fn replace_custom_section(bytes: &[u8], name: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = bytes[..8].to_vec();
    let mut pos = 8;
    while pos < bytes.len() {
        let start = pos;
        let id = bytes[pos];
        pos += 1;
        let (len, n) = read_leb(bytes, pos);
        pos += n;
        let end = pos + len as usize;
        let mut is_wanted = false;
        if id == 0 {
            let (name_len, n) = read_leb(bytes, pos);
            let name_start = pos + n;
            let name_end = name_start + name_len as usize;
            is_wanted = &bytes[name_start..name_end] == name.as_bytes();
        }
        if is_wanted {
            out.push(0);
            let mut name_len = write_leb(name.len() as u32);
            let section_len = name_len.len() + name.len() + payload.len();
            out.extend(write_leb(section_len as u32));
            out.append(&mut name_len);
            out.extend(name.as_bytes());
            out.extend(payload);
        } else {
            out.extend_from_slice(&bytes[start..end]);
        }
        pos = end;
    }
    out
}

fn edit_manifest(bytes: &[u8], edit: impl Fn(&str) -> String) -> Vec<u8> {
    let old = custom_section(bytes, "reiki-manifest").expect("manifest section");
    let new = edit(&old);
    replace_custom_section(bytes, "reiki-manifest", new.as_bytes())
}

#[test]
fn committed_fixtures_are_small_and_hashed() {
    let sums = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/SHA256SUMS"
    ))
    .unwrap();
    for (name, bytes) in FIXTURES {
        assert!(bytes.len() < 128 * 1024, "{name} fixture is not small");
        assert!(
            sums.contains(&format!("  {name}.wasm")),
            "SHA256SUMS is missing {name}.wasm"
        );
        let expected = sums
            .lines()
            .find(|line| line.ends_with(&format!("  {name}.wasm")))
            .and_then(|line| line.split_whitespace().next())
            .unwrap();
        assert_eq!(expected, sha256(bytes), "{name} fixture changed");
    }
}

#[test]
fn rebuilt_fixtures_match_committed_bytes() {
    // The bytes depend on the exact rustc and wasm-tools versions, so the comparison is opt-in:
    // set REIKI_VERIFY_FIXTURE_REBUILD=1 with the toolchain recorded in the example README.
    if std::env::var_os("REIKI_VERIFY_FIXTURE_REBUILD").is_none() {
        eprintln!("skipping rebuild comparison: set REIKI_VERIFY_FIXTURE_REBUILD=1 to run it");
        return;
    }
    if !toolchain_available() {
        eprintln!(
            "skipping rebuild comparison: wasm32-wasip1 target, wasm-tools and perl are required"
        );
        return;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let work = std::env::temp_dir().join(format!("reiki-fixture-rebuild-{stamp}"));
    std::fs::create_dir_all(&work).unwrap();
    for (name, expected) in FIXTURES {
        let example = root.join(format!("examples/wasm-{name}"));
        let out = work.join(format!("{name}.wasm"));
        let target = work.join(format!("{name}-target"));
        let status = std::process::Command::new("bash")
            .arg(example.join("rebuild.sh"))
            .arg(&out)
            .env("CARGO_TARGET_DIR", &target)
            .current_dir(&example)
            .status()
            .unwrap();
        assert!(status.success(), "rebuild.sh failed for {name}");
        assert_eq!(
            sha256(expected),
            sha256(&std::fs::read(&out).unwrap()),
            "committed {name}.wasm differs from a rebuild"
        );
    }
    std::fs::remove_dir_all(&work).unwrap();
}

fn toolchain_available() -> bool {
    let rustc = std::process::Command::new("rustc")
        .args(["--print", "target-libdir", "--target", "wasm32-wasip1"])
        .output();
    let Ok(rustc) = rustc else {
        return false;
    };
    if !rustc.status.success()
        || !std::path::Path::new(String::from_utf8_lossy(&rustc.stdout).trim()).is_dir()
    {
        return false;
    }
    let wasm_tools = std::process::Command::new("wasm-tools")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let perl = std::process::Command::new("perl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    wasm_tools && perl
}

#[test]
fn no_grant_component_loads_and_runs_default_hooks() {
    let engine = engine();
    let plugin = load(&engine, NO_GRANT, &base_table("no-grant", NO_GRANT, "")).unwrap();
    let out = invoke(&plugin, "/v1/messages", br#"{"model":"x"}"#);
    assert!(out.result.is_ok(), "{:?}", out.result);
    assert_eq!(out.body, br#"{"model":"x"}"#.as_slice());

    let mut table = base_table("no-grant", NO_GRANT, "findings");
    set_string(&mut table, "on_error", "reject");
    let plugin = load(&engine, NO_GRANT, &table).unwrap();
    let out = invoke(&plugin, "/v1/messages", b"{}");
    assert!(out.result.is_ok());
    assert_eq!(out.findings.len(), 1);
    assert_eq!(out.findings[0].kind, "wasm.rule");
    assert_eq!(out.findings[0].count, 2);
    assert_eq!(out.findings[0].detail, None);
}

#[test]
fn skip_and_reject_use_manifest_templates_and_sanitised_text() {
    let engine = engine();
    let table = base_table("no-grant", NO_GRANT, "skip-control");
    let plugin = load(&engine, NO_GRANT, &table).unwrap();
    match invoke(&plugin, "/x", b"{}").result {
        Err(PluginError::Skip(reason)) => {
            assert_eq!(reason, "linebreakreason");
        }
        other => panic!("expected sanitized skip, got {other:?}"),
    }

    let table = base_table("no-grant", NO_GRANT, "reject");
    let plugin = load(&engine, NO_GRANT, &table).unwrap();
    match invoke(&plugin, "/x", b"{}").result {
        Err(PluginError::Reject(status, message)) => {
            assert_eq!(status.as_u16(), 403);
            assert_eq!(message, "blocked by wasm plugin");
        }
        other => panic!("expected template reject, got {other:?}"),
    }
}

#[test]
fn pin_and_manifest_mismatches_fail_to_load() {
    let engine = engine();
    let mut table = base_table("no-grant", NO_GRANT, "");
    set_string(&mut table, "sha256", &"0".repeat(64));
    let err = load(&engine, NO_GRANT, &table)
        .err()
        .expect("expected load error");
    assert!(err.contains("sha256 mismatch"), "{err}");

    let table = base_table("other-name", NO_GRANT, "");
    let err = load(&engine, NO_GRANT, &table)
        .err()
        .expect("expected load error");
    assert!(
        err.contains("does not match configured plugin name"),
        "{err}"
    );

    let body_bytes = edit_manifest(BODY_EDIT, |toml| {
        toml.replace("request-body-read = true", "request-body-read = false")
    });
    let mut table = base_table("body-edit", &body_bytes, "");
    set_grants(
        &mut table,
        &[
            "request-body:read",
            "request-body:edit",
            "finding:detail-vocab",
        ],
    );
    let err = load(&engine, &body_bytes, &table)
        .err()
        .expect("expected load error");
    assert!(err.contains("request-body:read"), "{err}");
}

#[test]
fn body_read_edit_and_vocab_findings_work_end_to_end() {
    let engine = engine();
    let mut table = base_table("body-edit", BODY_EDIT, "");
    set_grants(
        &mut table,
        &[
            "request-body:read",
            "request-body:edit",
            "finding:detail-vocab",
        ],
    );
    let plugin = load(&engine, BODY_EDIT, &table).unwrap();
    let out = invoke(
        &plugin,
        "/v1/chat/completions",
        br#"{"model":"original","messages":[]}"#,
    );
    assert!(out.result.is_ok(), "{:?}", out.result);
    let edited: serde_json::Value = serde_json::from_slice(&out.body).unwrap();
    assert_eq!(edited["model"], "redacted");
    assert_eq!(edited["messages"], serde_json::json!([]));
    assert_eq!(out.findings.len(), 1);
    assert_eq!(out.findings[0].kind, "wasm.edit");
    assert_eq!(
        out.findings[0].detail.as_deref(),
        Some("rule.model-redacted")
    );

    // A gated import with no matching grant fails at load; capabilities come from imports.
    let ungranted = base_table("body-edit", BODY_EDIT, "");
    let err = load(&engine, BODY_EDIT, &ungranted)
        .err()
        .expect("expected load error");
    assert!(err.contains("request-body:read"), "{err}");
}

#[test]
fn non_vocab_detail_is_dropped() {
    let engine = engine();
    let mut table = base_table("body-edit", BODY_EDIT, "vocab-bad");
    set_grants(
        &mut table,
        &[
            "request-body:read",
            "request-body:edit",
            "finding:detail-vocab",
        ],
    );
    let plugin = load(&engine, BODY_EDIT, &table).unwrap();
    let out = invoke(&plugin, "/x", br#"{"model":"m"}"#);
    assert!(out.result.is_ok());
    assert_eq!(out.findings.len(), 1);
    assert_eq!(out.findings[0].detail, None);
    assert!(out.findings[0].blocked);
}

#[test]
fn invalid_edit_ops_are_rejected() {
    let engine = engine();
    for mode in ["bad-literal", "bad-pointer"] {
        let mut table = base_table("body-edit", BODY_EDIT, mode);
        set_grants(
            &mut table,
            &[
                "request-body:read",
                "request-body:edit",
                "finding:detail-vocab",
            ],
        );
        set_string(&mut table, "on_error", "reject");
        let plugin = load(&engine, BODY_EDIT, &table).unwrap();
        let out = invoke(&plugin, "/x", br#"{"model":"m"}"#);
        assert!(
            matches!(out.result, Err(PluginError::Reject(status, _)) if status.as_u16() == 503),
            "{mode}: {:?}",
            out.result
        );
    }
}

#[test]
fn fuel_exhaustion_fails_closed() {
    let engine = engine();
    let mut table = base_table("no-grant", NO_GRANT, "fuel");
    set_string(&mut table, "role", "security");
    set_string(&mut table, "on_error", "reject");
    set_limits(
        &mut table,
        &[("fuel-per-call", 20_000), ("fuel-per-kib", 1_000)],
    );
    let plugin = load(&engine, NO_GRANT, &table).unwrap();
    let out = invoke(&plugin, "/x", b"{}");
    match out.result {
        Err(PluginError::Reject(status, message)) => {
            assert_eq!(status.as_u16(), 503);
            assert_eq!(message, "wasm plugin exceeded fuel limit");
        }
        other => panic!("expected fuel reject, got {other:?}"),
    }
}

#[test]
fn memory_limit_traps_and_fails_closed() {
    let engine = engine();
    let mut table = base_table("no-grant", NO_GRANT, "memory");
    set_string(&mut table, "role", "security");
    set_string(&mut table, "on_error", "reject");
    set_limits(&mut table, &[("memory-mib", 2)]);
    let plugin = load(&engine, NO_GRANT, &table).unwrap();
    let out = invoke(&plugin, "/x", b"{}");
    match out.result {
        Err(PluginError::Reject(status, message)) => {
            assert_eq!(status.as_u16(), 503);
            assert_eq!(message, "wasm plugin exceeded memory limit");
        }
        other => panic!("expected memory reject, got {other:?}"),
    }
}

#[test]
fn deadline_reached_inside_host_call_is_enforced() {
    let engine = engine();
    let mut table = base_table("no-grant", NO_GRANT, "deadline");
    set_string(&mut table, "role", "security");
    set_string(&mut table, "on_error", "reject");
    // Load/init uses the wall budget, so a short request deadline cannot make
    // loading flaky. The fixture's host calls then cross the request deadline.
    set_limits(&mut table, &[("deadline-ms", 1)]);
    let plugin = load(&engine, NO_GRANT, &table).unwrap();
    let out = invoke(&plugin, "/x", b"{}");
    match out.result {
        Err(PluginError::Reject(status, message)) => {
            assert_eq!(status.as_u16(), 503);
            assert_eq!(message, "wasm plugin exceeded deadline");
        }
        other => panic!("expected deadline reject, got {other:?}"),
    }
}

#[test]
fn init_is_bounded_by_the_wall_budget() {
    // Keep fuel above what a 1 ms init can burn so the epoch deadline fires first.
    let bytes = edit_manifest(NO_GRANT, |toml| {
        toml.replace("fuel_per_call = 10000000", "fuel_per_call = 2000000000")
    });
    let engine = engine();
    let mut table = base_table("no-grant", &bytes, "init-fuel");
    set_string(&mut table, "on_error", "reject");
    set_limits(&mut table, &[("wall-budget-ms", 1)]);
    let err = load(&engine, &bytes, &table)
        .err()
        .expect("a spinning init must be bounded");
    // The 1 ms budget is enforced either by the epoch interrupt or, when a
    // host call polls first, by the explicit deadline check.
    assert!(
        err.contains("init no-grant: deadline exceeded")
            || err.contains("instantiate no-grant: wasm trap: interrupt"),
        "{err}"
    );
}

#[test]
fn trap_matrix_covers_init_and_request_by_role() {
    let engine = engine();
    let mut init = base_table("no-grant", NO_GRANT, "init-trap");
    set_string(&mut init, "on_error", "reject");
    let err = load(&engine, NO_GRANT, &init)
        .err()
        .expect("expected load error");
    assert!(err.contains("init refused by fixture"), "{err}");

    let mut reject_on_trap = base_table("no-grant", NO_GRANT, "trap");
    set_string(&mut reject_on_trap, "role", "security");
    set_string(&mut reject_on_trap, "on_error", "reject");
    let plugin = load(&engine, NO_GRANT, &reject_on_trap).unwrap();
    match invoke(&plugin, "/x", b"{}").result {
        Err(PluginError::Reject(status, message)) => {
            assert_eq!(status.as_u16(), 503);
            assert_eq!(message, "wasm plugin trapped");
        }
        other => panic!("expected fail-closed trap, got {other:?}"),
    }

    let skip_on_trap = base_table("no-grant", NO_GRANT, "trap");
    let plugin = load(&engine, NO_GRANT, &skip_on_trap).unwrap();
    match invoke(&plugin, "/x", b"{}").result {
        Err(PluginError::Skip(message)) => assert_eq!(message, "wasm plugin trapped"),
        other => panic!("expected skip on trap, got {other:?}"),
    }
}

#[test]
fn tool_allowlist_is_enforced_before_the_wasm_hook() {
    let engine = engine();
    let mut table = base_table("no-grant", NO_GRANT, "");
    set_string(&mut table, "on_error", "reject");
    set_tool_allowlist(&mut table, &["search"]);
    let plugin = load(&engine, NO_GRANT, &table).unwrap();

    let declared = br#"{"messages":[],"tools":[{"name":"lookup"}]}"#;
    match invoke(&plugin, "/v1/chat/completions", declared).result {
        Err(PluginError::Reject(status, message)) => {
            assert_eq!(status.as_u16(), 403);
            assert!(message.contains("lookup"), "{message}");
        }
        other => panic!("expected tool allowlist reject, got {other:?}"),
    }
    assert!(
        invoke(&plugin, "/v1/chat/completions", declared)
            .result
            .is_err(),
        "allowlist must run on every request"
    );
    assert!(
        invoke(&plugin, "/other", declared).result.is_ok(),
        "unrelated routes are not inspected"
    );
}

#[test]
fn unparseable_tool_route_rejects_even_in_pass_through_mode() {
    let engine = engine();
    let mut table = base_table("no-grant", NO_GRANT, "");
    set_tool_allowlist(&mut table, &["search"]);
    let plugin = load(&engine, NO_GRANT, &table).unwrap();
    match invoke(&plugin, "/v1/chat/completions", b"not json").result {
        Err(PluginError::Reject(status, _)) => assert_eq!(status.as_u16(), 403),
        other => panic!("expected reject on unparseable tool route, got {other:?}"),
    }
}

#[test]
fn pure_wasm_loop_is_bounded_by_the_deadline() {
    let bytes = edit_manifest(NO_GRANT, |toml| {
        toml.replace("fuel_per_call = 10000000", "fuel_per_call = 2000000000")
    });
    let engine = engine();
    let mut table = base_table("no-grant", &bytes, "fuel");
    set_string(&mut table, "role", "security");
    set_string(&mut table, "on_error", "reject");
    set_limits(&mut table, &[("deadline-ms", 20)]);
    let plugin = load(&engine, &bytes, &table).unwrap();
    match invoke(&plugin, "/x", b"{}").result {
        Err(PluginError::Reject(status, message)) => {
            assert_eq!(status.as_u16(), 503);
            assert_eq!(message, "wasm plugin exceeded deadline");
        }
        other => panic!("expected deadline reject for a compute loop, got {other:?}"),
    }
}

#[test]
fn reload_narrowing_uses_real_component_configs() {
    let plain = base_table("no-grant", NO_GRANT, "");
    let mut granted = base_table("no-grant", NO_GRANT, "");
    set_grants(&mut granted, &["request-body:read"]);
    let old = parse_wasm_plugin_config("no-grant", &plain).unwrap();
    let new = parse_wasm_plugin_config("no-grant", &granted).unwrap();
    assert!(matches!(
        reload_narrowing(&old, &new, false),
        ReloadDecision::RestartNeeded(_)
    ));
    assert!(matches!(
        reload_narrowing(&new, &old, false),
        ReloadDecision::Live
    ));

    let mut tooled = base_table("no-grant", NO_GRANT, "");
    set_tool_allowlist(&mut tooled, &["search", "lookup"]);
    let old = parse_wasm_plugin_config("no-grant", &tooled).unwrap();
    let mut narrowed = base_table("no-grant", NO_GRANT, "");
    set_tool_allowlist(&mut narrowed, &["search"]);
    let new = parse_wasm_plugin_config("no-grant", &narrowed).unwrap();
    assert!(matches!(
        reload_narrowing(&old, &new, false),
        ReloadDecision::Live
    ));
    assert!(matches!(
        reload_narrowing(&new, &old, false),
        ReloadDecision::RestartNeeded(_)
    ));
}

#[test]
fn inspect_reports_import_derived_capabilities() {
    let plain = inspect_component_bytes(NO_GRANT, "no-grant.wasm".to_owned()).unwrap();
    assert_eq!(plain.sha256, sha256(NO_GRANT));
    assert_eq!(plain.name, "no-grant");
    assert!(plain.requested_gated_capabilities.is_empty());
    assert!(
        plain
            .derived_capabilities
            .contains(&Capability::RequestMeta.as_str().to_owned())
    );

    let edit = inspect_component_bytes(BODY_EDIT, "body-edit.wasm".to_owned()).unwrap();
    assert_eq!(edit.name, "body-edit");
    assert_eq!(
        edit.requested_gated_capabilities,
        vec![
            Capability::FindingDetailVocab.as_str(),
            Capability::RequestBodyEdit.as_str(),
            Capability::RequestBodyRead.as_str(),
        ]
    );
    assert!(edit.derived_capabilities.iter().all(|c| c != "types"));
}

#[test]
fn reference_plugin_cannot_open_truncate_rename_or_hard_link() {
    let engine = engine();
    let table = base_table("fs-probe", FS_PROBE, "");
    let plugin = load(&engine, FS_PROBE, &table).unwrap();
    let out = invoke(&plugin, "/x", br#"{"model":"x"}"#);
    // The guest returns Pass only when all four probes failed; any successful
    // filesystem operation turns into a template reject instead.
    assert!(out.result.is_ok(), "{:?}", out.result);
    assert_eq!(out.body, br#"{"model":"x"}"#.as_slice());
}

#[test]
fn wasi_filesystem_and_sockets_imports_are_refused_at_load() {
    let engine = engine();
    let table = base_table("wasi-imports", WASI_IMPORTS, "");
    let config = parse_wasm_plugin_config("wasi-imports", &table).unwrap();
    let err = match engine.load_component_bytes(&config, WASI_IMPORTS) {
        Ok(_) => panic!("a plugin importing WASI filesystem or sockets must not load"),
        Err(err) => err,
    };
    assert!(err.contains("disallowed interface"), "{err}");
    assert!(err.contains("wasi:filesystem"), "{err}");
    assert!(err.contains("wasi:sockets"), "{err}");
}
