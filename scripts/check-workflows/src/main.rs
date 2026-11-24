//! Checks workflow YAML, action pinning and job permissions.
//! Run from the repository root:
//!   cargo run --quiet --manifest-path scripts/check-workflows/Cargo.toml

use serde_norway::{Mapping, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn text(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        _ => None,
    }
}

fn add(need: &mut BTreeMap<String, String>, scope: &str, level: &str) {
    let entry = need.entry(scope.to_string()).or_default();
    if level == "write" || entry.is_empty() {
        *entry = level.to_string();
    }
}

fn required_permissions(
    job: &Mapping,
    errors: &mut Vec<String>,
    job_name: &str,
) -> BTreeMap<String, String> {
    let mut need = BTreeMap::new();
    let steps = match job.get("steps") {
        Some(Value::Sequence(steps)) => steps,
        _ => {
            errors.push(format!("{job_name}: no steps list"));
            return need;
        }
    };

    for step in steps {
        let Some(step) = step.as_mapping() else {
            errors.push(format!("{job_name}: step is not a mapping"));
            continue;
        };
        let action = step
            .get("uses")
            .and_then(text)
            .map(|uses| uses.split('@').next().unwrap_or("").to_ascii_lowercase());
        let run = step
            .get("run")
            .and_then(text)
            .unwrap_or("")
            .to_ascii_lowercase();

        match action.as_deref() {
            Some("actions/checkout")
            | Some("github/codeql-action/init")
            | Some("actions/dependency-review-action")
            | Some("embarkstudios/cargo-deny-action")
            | Some("actions/configure-pages")
            | Some("actions/jekyll-build-pages")
            | Some("actions/upload-pages-artifact") => add(&mut need, "contents", "read"),
            Some("github/codeql-action/analyze") | Some("github/codeql-action/upload-sarif") => {
                add(&mut need, "security-events", "write")
            }
            Some("ossf/scorecard-action") => {
                add(&mut need, "security-events", "write");
                let publishes = step
                    .get("with")
                    .and_then(Value::as_mapping)
                    .and_then(|with| with.get("publish_results"))
                    .is_some_and(|value| match value {
                        Value::Bool(publish) => *publish,
                        Value::String(publish) => publish == "true",
                        _ => false,
                    });
                if publishes {
                    add(&mut need, "id-token", "write");
                }
            }
            Some("actions/attest-build-provenance") => {
                add(&mut need, "attestations", "write");
                add(&mut need, "id-token", "write");
                add(&mut need, "contents", "write");
            }
            Some("actions/deploy-pages") => {
                add(&mut need, "pages", "write");
                add(&mut need, "id-token", "write");
            }
            Some("actions/upload-artifact")
            | Some("actions/download-artifact")
            | Some("dtolnay/rust-toolchain")
            | Some("swatinem/rust-cache") => {}
            Some(other) => {
                errors.push(format!("{job_name}: no permission rule for {other}"));
            }
            None => {
                if run.contains("gh release ") || run.contains("git push ") {
                    add(&mut need, "contents", "write");
                }
                if run.contains("gh workflow run ") {
                    add(&mut need, "actions", "write");
                }
            }
        }
    }
    need
}

fn check_pin(line: &str, file: &str, number: usize, errors: &mut Vec<String>) -> bool {
    let Some(index) = line.find("uses:") else {
        return false;
    };
    let mut parts = line[index + 5..].split_whitespace();
    let Some(reference) = parts.next() else {
        return true;
    };
    let Some((_, sha)) = reference.rsplit_once('@') else {
        errors.push(format!("{file}:{number}: {reference} is not pinned by SHA"));
        return true;
    };
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        errors.push(format!("{file}:{number}: {reference} is not pinned by SHA"));
    }
    let comment: Vec<&str> = parts.collect();
    if !comment.first().is_some_and(|c| *c == "#")
        || !comment.get(1).is_some_and(|v| {
            v.starts_with('v') && v[1..].bytes().all(|b| b.is_ascii_digit() || b == b'.')
        })
    {
        errors.push(format!(
            "{file}:{number}: missing version comment after {reference}"
        ));
    }
    true
}

fn check_workflow(path: &Path, errors: &mut Vec<String>) -> usize {
    let file = path.file_name().unwrap_or_default().to_string_lossy();
    let source = fs::read_to_string(path).expect("read workflow");
    let workflow: Value = match serde_norway::from_str(&source) {
        Ok(value) => value,
        Err(err) => {
            errors.push(format!("{file}: invalid YAML: {err}"));
            return 0;
        }
    };
    let Some(root) = workflow.as_mapping() else {
        errors.push(format!("{file}: top level is not a mapping"));
        return 0;
    };

    match root.get("permissions") {
        Some(Value::Mapping(perms)) if perms.is_empty() => {}
        _ => errors.push(format!("{file}: top-level permissions must be {{}}")),
    }

    let mut pins = 0;
    for (number, line) in source.lines().enumerate() {
        if check_pin(line, &file, number + 1, errors) {
            pins += 1;
        }
    }

    let Some(Value::Mapping(jobs)) = root.get("jobs") else {
        errors.push(format!("{file}: no jobs"));
        return pins;
    };
    for (name, job) in jobs {
        let job_name = format!("{file}/{name}", name = text(name).unwrap_or("?"));
        let Some(job) = job.as_mapping() else {
            errors.push(format!("{job_name}: job is not a mapping"));
            continue;
        };
        let need = required_permissions(job, errors, &job_name);
        let Some(Value::Mapping(granted)) = job.get("permissions") else {
            errors.push(format!("{job_name}: missing job permissions"));
            continue;
        };
        let mut actual = BTreeMap::new();
        for (scope, level) in granted {
            let (Some(scope), Some(level)) = (text(scope), text(level)) else {
                errors.push(format!(
                    "{job_name}: permissions must be scope: level pairs"
                ));
                continue;
            };
            if !matches!(level, "read" | "write") {
                errors.push(format!("{job_name}: invalid {scope} permission {level}"));
            }
            actual.insert(scope.to_string(), level.to_string());
        }
        if actual != need {
            errors.push(format!(
                "{job_name}: permissions {actual:?}, needed {need:?}"
            ));
        }
    }
    pins
}

fn check_support_yaml(root: &Path, errors: &mut Vec<String>) {
    let path = root.join(".github/dependabot.yml");
    if let Err(err) =
        serde_norway::from_str::<Value>(&fs::read_to_string(&path).expect("read dependabot.yml"))
    {
        errors.push(format!("dependabot.yml: invalid YAML: {err}"));
    }
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join(".github/workflows");
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read workflows directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "yml"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no workflows found");

    let mut errors = Vec::new();
    let mut pins = 0;
    for path in &paths {
        pins += check_workflow(path, &mut errors);
    }
    check_support_yaml(&root, &mut errors);
    if errors.is_empty() {
        println!("OK: {} workflows, {pins} pinned uses", paths.len());
    } else {
        for error in &errors {
            eprintln!("{error}");
        }
        eprintln!("{} error(s)", errors.len());
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow(name: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.github/workflows")
            .join(name);
        serde_norway::from_str(&fs::read_to_string(path).expect("read workflow")).expect("yaml")
    }

    fn job_needs(workflow: &Value, job: &str) -> BTreeMap<String, String> {
        let job = workflow["jobs"][job].as_mapping().expect("job");
        required_permissions(job, &mut Vec::new(), job_name_for_tests())
    }

    fn job_name_for_tests() -> &'static str {
        "test"
    }

    #[test]
    fn workflow_dispatch_needs_actions_write() {
        let dev = workflow("dev-release.yml");
        let need = job_needs(&dev, "tag");
        assert_eq!(need.get("actions").map(String::as_str), Some("write"));
        assert_eq!(need.get("contents").map(String::as_str), Some("write"));
    }

    #[test]
    fn release_is_dispatched_on_tag_refs_not_called() {
        let release = workflow("release.yml");
        let on = release["on"].as_mapping().expect("on");
        assert!(on.get("workflow_dispatch").is_some());
        assert!(on.get("workflow_call").is_none());
        let tags: Vec<&str> = on["push"]["tags"]
            .as_sequence()
            .expect("tags")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(tags, ["v*", "!v*-dev.*"]);
        let dev = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/dev-release.yml"),
        )
        .expect("read");
        assert!(dev.contains("gh workflow run release.yml"));
        assert!(!dev.contains("uses: ./"));
    }
}
