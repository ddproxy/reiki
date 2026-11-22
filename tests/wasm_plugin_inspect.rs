#![cfg(feature = "plugin-wasm")]

use std::path::Path;
use std::process::Command;

const FIXTURES: &[(&str, &str)] = &[
    (
        "no-grant",
        "8c0345cbe4b07e589a508f977dd9cf6b4d4f49e063a66922a874f962b8f279c1",
    ),
    (
        "body-edit",
        "36ab9202adc63ccbaf2f2b2147f81612894af362ba297d9a4f0d933fd16c3db0",
    ),
];

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("crates/reiki-wasm/tests/fixtures")
        .join(format!("{name}.wasm"))
        .display()
        .to_string()
}

fn run(args: &[&str]) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_reiki"))
        .args(args)
        .output()
        .expect("run reiki");
    assert!(
        output.status.success(),
        "reiki {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn plugin_inspect_prints_text_and_json_for_both_fixtures() {
    for (name, sha256) in FIXTURES {
        let file = fixture(name);
        let text = String::from_utf8(run(&["plugin", "inspect", &file]).stdout).unwrap();
        assert!(text.contains(&format!("sha256: {sha256}")), "{text}");
        assert!(text.contains(&format!("name: {name}")), "{text}");
        assert!(
            text.contains("pin: manifest-present=true section=reiki-manifest"),
            "{text}"
        );

        let json = run(&["plugin", "inspect", &file, "--json"]).stdout;
        let report: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(report["name"].as_str(), Some(*name));
        assert_eq!(report["sha256"].as_str(), Some(*sha256));
        assert!(report["derived_capabilities"].is_array());
        assert!(
            report["derived_capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .all(|cap| cap.as_str() != Some("types")),
            "{report}"
        );
        if *name == "body-edit" {
            let gated = report["requested_gated_capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|cap| cap.as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                gated,
                vec![
                    "finding:detail-vocab",
                    "request-body:edit",
                    "request-body:read"
                ]
            );
        } else {
            assert_eq!(
                report["requested_gated_capabilities"],
                serde_json::json!([])
            );
        }
    }
}
