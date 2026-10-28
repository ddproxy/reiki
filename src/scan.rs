//! Runs the compiled-in detection plugins over text and reports where they find identifiers.
//!
//! A finding carries the plugin, the kind and the position. It never carries the matched text, so
//! the output is safe to paste into a ticket while tuning a plugin's `kinds` or its mode.

// Without any detection plugin compiled in there is nothing to run, and the helpers go unused.
#![cfg_attr(
    not(any(
        feature = "plugin-phi",
        feature = "plugin-finance",
        feature = "plugin-gov"
    )),
    allow(dead_code, unused_variables)
)]

/// One identifier a detector found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub plugin: &'static str,
    pub kind: &'static str,
    pub start: usize,
    pub end: usize,
}

/// Which kinds to look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kinds {
    /// Each plugin's default set, as its config would use.
    Default,
    /// Every kind, including the ones that are off by default.
    All,
    /// These labels, for whichever plugins know them.
    Named(Vec<String>),
}

/// Names of the detection plugins compiled into this binary.
pub fn plugins() -> Vec<&'static str> {
    [
        ("phi", cfg!(feature = "plugin-phi")),
        ("finance", cfg!(feature = "plugin-finance")),
        ("gov", cfg!(feature = "plugin-gov")),
    ]
    .into_iter()
    .filter_map(|(name, compiled)| compiled.then_some(name))
    .collect()
}

/// The kinds of `selection` that one plugin knows; `None` when it knows none of the named ones.
fn pick<K: Copy>(
    selection: &Kinds,
    default: &[K],
    all: &[K],
    parse: fn(&str) -> Option<K>,
) -> Option<Vec<K>> {
    match selection {
        Kinds::Default => Some(default.to_vec()),
        Kinds::All => Some(all.to_vec()),
        Kinds::Named(labels) => {
            let kinds: Vec<K> = labels.iter().filter_map(|l| parse(l)).collect();
            (!kinds.is_empty()).then_some(kinds)
        }
    }
}

/// Every kind label any compiled-in plugin knows.
pub fn known_labels() -> Vec<&'static str> {
    let mut labels = Vec::new();
    #[cfg(feature = "plugin-phi")]
    labels.extend(reiki_phi::Kind::ALL.iter().map(|k| k.label()));
    #[cfg(feature = "plugin-finance")]
    labels.extend(reiki_finance::Kind::ALL.iter().map(|k| k.label()));
    #[cfg(feature = "plugin-gov")]
    labels.extend(reiki_gov::Kind::ALL.iter().map(|k| k.label()));
    labels.sort_unstable();
    labels.dedup();
    labels
}

/// Scans `text` with the named plugins (all compiled-in ones when `only` is empty).
pub fn scan(text: &str, only: &[String], selection: &Kinds) -> Result<Vec<Finding>, String> {
    let available = plugins();
    if let Some(unknown) = only.iter().find(|p| !available.contains(&p.as_str())) {
        return Err(format!(
            "unknown plugin `{unknown}`; this build has: {}",
            available.join(", ")
        ));
    }
    if let Kinds::Named(labels) = selection {
        let known = known_labels();
        if let Some(bad) = labels.iter().find(|l| !known.contains(&l.as_str())) {
            return Err(format!(
                "unknown kind `{bad}`; known kinds: {}",
                known.join(", ")
            ));
        }
    }
    let wanted = |name: &str| only.is_empty() || only.iter().any(|p| p == name);
    let mut findings: Vec<Finding> = Vec::new();
    #[cfg(feature = "plugin-phi")]
    if wanted("phi") {
        if let Some(kinds) = pick(
            selection,
            &reiki_phi::Kind::DEFAULT,
            &reiki_phi::Kind::ALL,
            reiki_phi::Kind::parse,
        ) {
            findings.extend(reiki_phi::scan(text, &kinds).into_iter().map(|h| Finding {
                plugin: "phi",
                kind: h.kind.label(),
                start: h.start,
                end: h.end,
            }));
        }
    }
    #[cfg(feature = "plugin-finance")]
    if wanted("finance") {
        if let Some(kinds) = pick(
            selection,
            &reiki_finance::Kind::DEFAULT,
            &reiki_finance::Kind::ALL,
            reiki_finance::Kind::parse,
        ) {
            findings.extend(
                reiki_finance::scan(text, &kinds)
                    .into_iter()
                    .map(|h| Finding {
                        plugin: "finance",
                        kind: h.kind.label(),
                        start: h.start,
                        end: h.end,
                    }),
            );
        }
    }
    #[cfg(feature = "plugin-gov")]
    if wanted("gov") {
        if let Some(kinds) = pick(
            selection,
            &reiki_gov::Kind::DEFAULT,
            &reiki_gov::Kind::ALL,
            reiki_gov::Kind::parse,
        ) {
            findings.extend(reiki_gov::scan(text, &kinds).into_iter().map(|h| Finding {
                plugin: "gov",
                kind: h.kind.label(),
                start: h.start,
                end: h.end,
            }));
        }
    }
    #[cfg(not(any(
        feature = "plugin-phi",
        feature = "plugin-finance",
        feature = "plugin-gov"
    )))]
    let _ = (&wanted, selection);
    findings.sort_by_key(|f| (f.start, f.end, f.plugin));
    Ok(findings)
}

/// Line and column (both 1-based, in bytes) of a byte offset.
pub fn position(text: &str, offset: usize) -> (usize, usize) {
    let before = &text.as_bytes()[..offset.min(text.len())];
    let line = before.iter().filter(|b| **b == b'\n').count() + 1;
    let col = before.len()
        - before
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |p| p + 1)
        + 1;
    (line, col)
}

/// Counts per plugin and kind, in a stable order.
pub fn summary(findings: &[Finding]) -> Vec<(&'static str, &'static str, usize)> {
    let mut counts: std::collections::BTreeMap<(&str, &str), usize> =
        std::collections::BTreeMap::new();
    for f in findings {
        *counts.entry((f.plugin, f.kind)).or_default() += 1;
    }
    counts.into_iter().map(|((p, k), n)| (p, k, n)).collect()
}

/// Escapes `text` for a JSON string.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `reiki scan` command. Findings go to stdout, one per line, and a summary goes to stderr.
/// Returns the exit code: 0 for none, 1 for findings, 2 for a usage or read error.
pub fn run(
    plugins: &[String],
    kinds: &[String],
    all: bool,
    json: bool,
    files: &[std::path::PathBuf],
) -> i32 {
    use std::io::{Read, Write};
    let selection = if all {
        Kinds::All
    } else if kinds.is_empty() {
        Kinds::Default
    } else {
        Kinds::Named(kinds.to_vec())
    };
    let mut inputs: Vec<(String, Vec<u8>)> = Vec::new();
    if files.is_empty() || files.iter().any(|f| f.as_os_str() == "-") {
        let mut bytes = Vec::new();
        if let Err(e) = std::io::stdin().read_to_end(&mut bytes) {
            eprintln!("reiki: cannot read stdin: {e}");
            return 2;
        }
        inputs.push(("-".to_owned(), bytes));
    }
    for file in files.iter().filter(|f| f.as_os_str() != "-") {
        match std::fs::read(file) {
            Ok(bytes) => inputs.push((file.display().to_string(), bytes)),
            Err(e) => {
                eprintln!("reiki: cannot read {}: {e}", file.display());
                return 2;
            }
        }
    }
    let mut out = std::io::stdout().lock();
    let mut all_findings = Vec::new();
    for (name, bytes) in &inputs {
        let text = String::from_utf8_lossy(bytes);
        let findings = match scan(&text, plugins, &selection) {
            Ok(findings) => findings,
            Err(e) => {
                eprintln!("reiki: {e}");
                return 2;
            }
        };
        for f in &findings {
            let (line, col) = position(&text, f.start);
            let written = if json {
                writeln!(
                    out,
                    "{{\"file\":{},\"line\":{line},\"col\":{col},\"plugin\":\"{}\",\"kind\":\"{}\"}}",
                    json_string(name),
                    f.plugin,
                    f.kind
                )
            } else {
                writeln!(out, "{name}:{line}:{col} {} {}", f.plugin, f.kind)
            };
            if written.is_err() {
                return 2;
            }
        }
        all_findings.extend(findings);
    }
    drop(out);
    let counts = summary(&all_findings)
        .iter()
        .map(|(p, k, n)| format!("{p} {k} x{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    if all_findings.is_empty() {
        eprintln!("scanned {} input(s): no findings", inputs.len());
        0
    } else {
        eprintln!(
            "scanned {} input(s): {} finding(s): {counts}",
            inputs.len(),
            all_findings.len()
        );
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, only: &[&str], kinds: &Kinds) -> Vec<(String, String)> {
        let only: Vec<String> = only.iter().map(|s| (*s).to_owned()).collect();
        scan(text, &only, kinds)
            .unwrap()
            .into_iter()
            .map(|f| (f.plugin.to_owned(), f.kind.to_owned()))
            .collect()
    }

    #[test]
    fn findings_name_the_plugin_and_kind_and_never_the_text() {
        let text = "ssn 123-45-6789 and cusip 037833100 and TOP SECRET//SI//NOFORN";
        let found = run(text, &[], &Kinds::Default);
        assert!(found.contains(&("phi".into(), "ssn".into())), "{found:?}");
        assert!(
            found.contains(&("finance".into(), "cusip".into())),
            "{found:?}"
        );
        assert!(
            found.contains(&("gov".into(), "classification".into())),
            "{found:?}"
        );
    }

    #[test]
    fn plugin_and_kind_filters_narrow_the_scan() {
        let text = "cusip 037833100 and, well past any keyword window, ssn 123-45-6789";
        assert_eq!(
            run(text, &["phi"], &Kinds::Default),
            [("phi".to_owned(), "ssn".to_owned())]
        );
        let named = run(text, &[], &Kinds::Named(vec!["cusip".into()]));
        assert_eq!(named, [("finance".to_owned(), "cusip".to_owned())]);
    }

    #[test]
    fn all_includes_the_kinds_that_are_off_by_default() {
        let text = "mail jane@example.com";
        assert!(run(text, &[], &Kinds::Default).is_empty());
        assert_eq!(
            run(text, &["phi"], &Kinds::All),
            [("phi".to_owned(), "email".to_owned())]
        );
    }

    #[test]
    fn unknown_plugins_and_kinds_are_errors() {
        assert!(scan("x", &["nope".into()], &Kinds::Default)
            .unwrap_err()
            .contains("unknown plugin"));
        assert!(scan("x", &[], &Kinds::Named(vec!["bogus".into()]))
            .unwrap_err()
            .contains("unknown kind"));
    }

    #[test]
    fn positions_are_one_based_lines_and_columns() {
        let text = "a\nbc\n\ndef";
        assert_eq!(position(text, 0), (1, 1));
        assert_eq!(position(text, 3), (2, 2));
        assert_eq!(position(text, 8), (4, 3));
        assert_eq!(
            position(text, 999),
            (4, 4),
            "past the end clamps to the end"
        );
    }

    #[test]
    fn the_summary_counts_by_plugin_and_kind() {
        let found = scan("123-45-6789 234-56-7890", &[], &Kinds::Default).unwrap();
        assert_eq!(summary(&found), [("phi", "ssn", 2)]);
    }
}
