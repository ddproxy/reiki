//! Generates the compiled-in plugin registry.
//!
//! The catalog of plugins lives in `Cargo.toml` under `[[package.metadata.reiki.plugin]]`, in
//! default chain order. `REIKI_PLUGINS=a,b` at build time picks which plugins are registered
//! and in what order; each one still needs its Cargo feature enabled.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Plugin {
    name: String,
    feature: String,
    krate: String,
}

/// The plugins whose findings are about the unmasked request, so they must run before `placeholders`.
const DETECTORS: &[&str] = &["phi", "finance", "controlled"];

fn fail(message: &str) -> ! {
    println!("cargo::error={message}");
    std::process::exit(1);
}

fn plain_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_catalog(text: &str) -> Result<Vec<Plugin>, String> {
    let parsed: toml::Table = text
        .parse()
        .map_err(|e| format!("cannot parse the plugin catalog: {e}"))?;
    let Some(entries) = parsed
        .get("package")
        .and_then(|v| v.get("metadata"))
        .and_then(|v| v.get("reiki"))
        .and_then(|v| v.get("plugin"))
    else {
        return Err("[[package.metadata.reiki.plugin]] is missing".to_owned());
    };
    let Some(entries) = entries.as_array() else {
        return Err("[[package.metadata.reiki.plugin]] must be an array of tables".to_owned());
    };
    if entries.is_empty() {
        return Err("[[package.metadata.reiki.plugin]] must not be empty".to_owned());
    }
    let mut names = HashSet::new();
    let mut crates = HashSet::new();
    let mut plugins = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let Some(entry) = entry.as_table() else {
            return Err(format!(
                "[[package.metadata.reiki.plugin]] entry {} must be a table",
                index + 1
            ));
        };
        let field = |key: &str| {
            entry
                .get(key)
                .and_then(toml::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    format!(
                        "[[package.metadata.reiki.plugin]] entry {} is missing `{key}`",
                        index + 1
                    )
                })
                .map(|value| (*value).to_owned())
        };
        let plugin = Plugin {
            name: field("name")?,
            feature: field("feature")?,
            krate: field("crate")?,
        };
        if !plain_identifier(&plugin.krate) {
            return Err(format!(
                "[[package.metadata.reiki.plugin]] `crate` must be a plain identifier, got `{}`",
                plugin.krate
            ));
        }
        if !names.insert(plugin.name.clone()) {
            return Err(format!(
                "[[package.metadata.reiki.plugin]] names `{}` twice",
                plugin.name
            ));
        }
        if !crates.insert(plugin.krate.clone()) {
            return Err(format!(
                "[[package.metadata.reiki.plugin]] uses crate `{}` twice",
                plugin.krate
            ));
        }
        plugins.push(plugin);
    }
    Ok(plugins)
}

fn catalog(manifest: &str) -> Vec<Plugin> {
    let text = fs::read_to_string(manifest)
        .unwrap_or_else(|e| fail(&format!("cannot read {manifest}: {e}")));
    parse_catalog(&text).unwrap_or_else(|e| fail(&format!("{manifest}: {e}")))
}

/// Rejects an order that logs before masking, and warns about orders that only weaken detection
/// or leave `requestlog` without a masking plugin ahead of it.
fn check_order(chosen: &[&Plugin]) -> Result<Vec<String>, String> {
    let position = |name: &str| chosen.iter().position(|p| p.name == name);
    let placeholders = position("placeholders");
    let requestlog = position("requestlog");
    if let (Some(placeholders), Some(requestlog)) = (placeholders, requestlog) {
        if placeholders > requestlog {
            return Err(
                "REIKI_PLUGINS puts `requestlog` before `placeholders`; placeholders must come first so logged bodies are masked"
                    .to_owned(),
            );
        }
    }
    let mut warnings = Vec::new();
    if let Some(requestlog) = requestlog {
        if !chosen[..requestlog]
            .iter()
            .any(|p| p.name == "placeholders")
        {
            warnings.push(
                "REIKI_PLUGINS puts `requestlog` without a preceding `placeholders`; requestlog masks the credential patterns it knows, but anything else can reach the log"
                    .to_owned(),
            );
        }
    }
    if let Some(placeholders) = placeholders {
        let after: Vec<&str> = chosen[placeholders + 1..]
            .iter()
            .map(|p| p.name.as_str())
            .filter(|name| DETECTORS.contains(name))
            .collect();
        if !after.is_empty() {
            warnings.push(format!(
                "REIKI_PLUGINS puts {} after `placeholders`; they scan masked text and can miss findings",
                after.join(", ")
            ));
        }
    }
    Ok(warnings)
}

fn enabled(feature: &str) -> bool {
    let var = format!(
        "CARGO_FEATURE_{}",
        feature.to_ascii_uppercase().replace('-', "_")
    );
    env::var_os(var).is_some()
}

fn main() {
    println!("cargo::rerun-if-env-changed=REIKI_PLUGINS");
    println!("cargo::rerun-if-changed=Cargo.toml");
    println!("cargo::rerun-if-changed=build.rs");

    let manifest = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| fail("no CARGO_MANIFEST_DIR")),
    )
    .join("Cargo.toml");
    let all = catalog(&manifest.to_string_lossy());
    let known = all
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let chosen: Vec<&Plugin> = match env::var("REIKI_PLUGINS") {
        Ok(list) if !list.trim().is_empty() => {
            let mut picked: Vec<&Plugin> = Vec::new();
            for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                let Some(plugin) = all.iter().find(|p| p.name == name) else {
                    fail(&format!("REIKI_PLUGINS names `{name}`, which is not a known plugin (known: {known})"));
                };
                if picked.iter().any(|p| p.name == name) {
                    fail(&format!("REIKI_PLUGINS lists `{name}` twice"));
                }
                if !enabled(&plugin.feature) {
                    fail(&format!(
                        "REIKI_PLUGINS names `{name}` but its feature `{}` is not enabled; build with --features {}",
                        plugin.feature, plugin.feature
                    ));
                }
                picked.push(plugin);
            }
            picked
        }
        _ => all.iter().filter(|p| enabled(&p.feature)).collect(),
    };

    // Placeholders must run before the request log so the log records masked text, and because
    // response hooks run in reverse, restore only after the log has seen the response. This does
    // not replace requestlog's own redaction: a requestlog-only build still logs the credentials
    // its patterns do not recognise. Detectors should also run before placeholders so they see
    // the original text rather than placeholders.
    let warnings = check_order(&chosen).unwrap_or_else(|e| fail(&e));
    for warning in warnings {
        println!("cargo::warning={warning}");
    }

    let mut code = String::from("vec![\n");
    for plugin in chosen {
        code.push_str(&format!(
            "    Registration {{ name: {krate}::NAME, build: {krate}::build }},\n",
            krate = plugin.krate
        ));
    }
    code.push_str("]\n");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap_or_else(|_| fail("no OUT_DIR")))
        .join("builtin_plugins.rs");
    fs::write(&out, code).unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", out.display())));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(entries: &str) -> String {
        format!("[package]\nname = \"reiki\"\n[package.metadata.reiki]\nplugin = [\n{entries}\n]\n")
    }

    fn plugin(name: &str) -> Plugin {
        Plugin {
            name: name.to_owned(),
            feature: format!("plugin-{name}"),
            krate: format!("reiki_{name}"),
        }
    }

    #[test]
    fn parses_a_non_empty_array_of_tables() {
        let text = manifest(
            r#"
    { name = "phi", feature = "plugin-phi", crate = "reiki_phi" },
    { name = "metrics", feature = "plugin-metrics", crate = "reiki_metrics" },
"#,
        );
        let plugins = parse_catalog(&text).unwrap();
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].name, "phi");
        assert_eq!(plugins[1].krate, "reiki_metrics");
    }

    #[test]
    fn rejects_missing_empty_and_non_array_catalogs() {
        let missing = "[package]\nname = \"reiki\"\n";
        assert!(parse_catalog(missing).unwrap_err().contains("missing"));

        let empty = manifest("");
        assert!(parse_catalog(&empty).unwrap_err().contains("empty"));

        let table =
            "[package]\nname = \"reiki\"\n[package.metadata.reiki]\nplugin = { name = \"phi\" }\n";
        assert!(parse_catalog(table)
            .unwrap_err()
            .contains("array of tables"));
    }

    #[test]
    fn rejects_malformed_entries_duplicates_and_unsafe_crate_names() {
        let cases = [
            (r#""phi","#, "must be a table"),
            (
                r#"{ name = "phi", feature = "plugin-phi" },"#,
                "missing `crate`",
            ),
            (
                r#"{ name = "phi", feature = "", crate = "reiki_phi" },"#,
                "missing `feature`",
            ),
            (
                r#"{ name = "phi", feature = "plugin-phi", crate = "reiki-phi" },"#,
                "plain identifier",
            ),
            (
                r#"{ name = "phi", feature = "plugin-phi", crate = "1phi" },"#,
                "plain identifier",
            ),
            (
                r#"{ name = "phi", feature = "plugin-phi", crate = "a::b" },"#,
                "plain identifier",
            ),
            (
                r#"
    { name = "phi", feature = "plugin-phi", crate = "reiki_phi" },
    { name = "phi", feature = "plugin-finance", crate = "reiki_finance" },
"#,
                "names `phi` twice",
            ),
            (
                r#"
    { name = "phi", feature = "plugin-phi", crate = "reiki_phi" },
    { name = "finance", feature = "plugin-finance", crate = "reiki_phi" },
"#,
                "uses crate `reiki_phi` twice",
            ),
        ];
        for (entries, expected) in cases {
            let error = parse_catalog(&manifest(entries)).unwrap_err();
            assert!(error.contains(expected), "{entries}: {error}");
        }
    }

    #[test]
    fn orders_masking_before_the_log_and_detectors_before_masking() {
        let placeholders = plugin("placeholders");
        let requestlog = plugin("requestlog");
        let phi = plugin("phi");

        assert!(check_order(&[&placeholders]).unwrap().is_empty());
        assert!(check_order(&[&placeholders, &requestlog])
            .unwrap()
            .is_empty());
        assert!(check_order(&[&phi, &placeholders]).unwrap().is_empty());

        let reversed = check_order(&[&requestlog, &placeholders]).unwrap_err();
        assert!(reversed.contains("before `placeholders`"), "{reversed}");

        let alone = check_order(&[&requestlog]).unwrap();
        assert_eq!(alone.len(), 1);
        assert!(alone[0].contains("without a preceding"));

        let detectors = check_order(&[&placeholders, &phi]).unwrap();
        assert_eq!(detectors.len(), 1);
        assert!(detectors[0].contains("phi after `placeholders`"));

        let both = check_order(&[&placeholders, &phi, &requestlog]).unwrap();
        assert_eq!(both.len(), 1);
        assert!(both[0].contains("phi after `placeholders`"));
    }
}
