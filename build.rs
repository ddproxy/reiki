//! Generates the compiled-in plugin registry.
//!
//! The catalog of plugins lives in `Cargo.toml` under `[[package.metadata.reiki.plugin]]`, in
//! default chain order. `REIKI_PLUGINS=a,b` at build time picks which plugins are registered
//! and in what order; each one still needs its Cargo feature enabled.

use std::env;
use std::fs;
use std::path::PathBuf;

struct Plugin {
    name: String,
    feature: String,
    krate: String,
}

fn fail(message: &str) -> ! {
    println!("cargo::error={message}");
    std::process::exit(1);
}

fn catalog(manifest: &str) -> Vec<Plugin> {
    let text = fs::read_to_string(manifest)
        .unwrap_or_else(|e| fail(&format!("cannot read {manifest}: {e}")));
    let parsed: toml::Table = text
        .parse()
        .unwrap_or_else(|e| fail(&format!("cannot parse {manifest}: {e}")));
    let entries = parsed
        .get("package")
        .and_then(|v| v.get("metadata"))
        .and_then(|v| v.get("reiki"))
        .and_then(|v| v.get("plugin"))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    entries
        .iter()
        .map(|entry| {
            let field = |key: &str| {
                entry
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .unwrap_or_else(|| {
                        fail(&format!(
                            "[[package.metadata.reiki.plugin]] entry is missing `{key}`"
                        ))
                    })
                    .to_owned()
            };
            Plugin {
                name: field("name"),
                feature: field("feature"),
                krate: field("crate"),
            }
        })
        .collect()
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
