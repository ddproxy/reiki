use std::collections::BTreeSet;
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::config::{Config, Severity};

pub struct Finding {
    pub rule: &'static str,
    pub severity: Severity,
}

fn finding(rule: &'static str, severity: Severity) -> Finding {
    Finding { rule, severity }
}

pub fn check_call(
    name: &str,
    args: &str,
    declared: &BTreeSet<String>,
    provenance: &str,
    config: &Config,
) -> Vec<Finding> {
    let mut out = Vec::new();
    if config.check_declared && !declared.iter().any(|tool| tool.eq_ignore_ascii_case(name)) {
        out.push(finding("undeclared_tool", Severity::High));
    }
    if config.check_hosts && looks_network(name, args) {
        for host in extract_hosts(args) {
            if !host_allowed(&host, &config.allow_hosts, provenance) {
                out.push(finding("unapproved_host", Severity::High));
                break;
            }
        }
    }
    if config.check_tripwires {
        let plain = args.to_ascii_lowercase();
        let unquoted = unquote_shell(&plain);
        if proc_environ(&plain) || proc_environ(&unquoted) {
            out.push(finding("proc_environ", Severity::High));
        }
        let has = |patterns: &[&str]| {
            patterns
                .iter()
                .any(|p| plain.contains(p) || unquoted.contains(p))
        };
        if has(&["unshare", "nsenter", "chroot", "mount --bind"]) {
            out.push(finding("container_escape", Severity::High));
        }
        if has(&[
            "--noproxy",
            "no_proxy",
            "https_proxy",
            "http_proxy",
            "all_proxy",
            "_base_url",
            "unset ",
        ]) {
            out.push(finding("proxy_bypass", Severity::Medium));
        }
        if has(&[
            "/.ssh",
            ".git-credentials",
            ".npmrc",
            ".aws/credentials",
            ".password-store",
            "/etc/reiki",
        ]) {
            out.push(finding("credential_file", Severity::High));
        }
        if self_protection(&unquoted) {
            out.push(finding("self_protection", Severity::High));
        }
    }
    out
}

fn proc_environ(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("/proc/") else {
        return text.contains("/proc/");
    };
    if rest.starts_with("self/environ") {
        return true;
    }
    let Some((pid, tail)) = rest.split_once('/') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit() || b == b'*')
        && tail.starts_with("environ")
}

pub fn unquote_shell(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        out.push(bytes[i + 1]);
                        i += 2;
                    } else if bytes[i] == quote {
                        i += 1;
                        break;
                    } else {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'\\' if i + 1 < bytes.len() => {
                out.push(bytes[i + 1]);
                i += 2;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn self_protection(text: &str) -> bool {
    let words: Vec<&str> = text
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .filter(|w| !w.is_empty())
        .collect();
    // One legacy helper-tool name stays in the matcher for behaviour compatibility.
    let target = words
        .iter()
        .any(|w| w.eq_ignore_ascii_case("reiki") || w.eq_ignore_ascii_case("toolgate"));
    let action = words.iter().any(|w| {
        [
            "stop",
            "kill",
            "pkill",
            "killall",
            "restart",
            "reload",
            "reconfigure",
            "disable",
            "config",
            "locked",
            "chmod",
            "truncate",
        ]
        .iter()
        .any(|k| w.eq_ignore_ascii_case(k))
    });
    target && action
}

pub fn looks_network(name: &str, args: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if ["fetch", "browse", "web"].iter().any(|p| name.contains(p)) {
        return true;
    }
    let plain = args.to_ascii_lowercase();
    let unquoted = unquote_shell(&plain);
    if plain.contains("http") || plain.contains("://") {
        return true;
    }
    let commands = ["curl", "wget", "git", "scp", "ssh", "nc", "socat", "python"];
    [&plain, &unquoted].iter().any(|text| {
        text.split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
            .any(|word| commands.contains(&word))
    })
}

/// Hosts from URLs, bare IP addresses and host-shaped tokens after a network command.
pub fn extract_hosts(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let lower = unquote_shell(&text.to_ascii_lowercase());
    for (_, authority) in lower
        .match_indices("://")
        .map(|(i, _)| (i, &lower[i + 3..]))
    {
        let end = authority
            .find(|c: char| {
                c.is_whitespace()
                    || matches!(
                        c,
                        '/' | '?' | '#' | '"' | '\'' | '`' | '<' | '>' | ',' | ';' | ')'
                    )
            })
            .unwrap_or(authority.len());
        let authority = authority[..end]
            .rsplit('@')
            .next()
            .unwrap_or("")
            .trim_end_matches([']', '.']);
        let host = if let Some(rest) = authority.strip_prefix('[') {
            rest.split(']').next().unwrap_or("")
        } else {
            authority
                .rsplit_once(':')
                .map_or(authority, |(host, port)| {
                    if port.bytes().all(|b| b.is_ascii_digit()) {
                        host
                    } else {
                        authority
                    }
                })
        };
        add_host(&mut out, host);
    }
    for token in lower.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | '`' | '=' | ',' | ';' | '(' | ')' | '{' | '}' | '<' | '>'
            )
    }) {
        let host = token
            .split('/')
            .next()
            .unwrap_or("")
            .trim_start_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':' && c != '[')
            .trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != ']' && c != '.');
        add_host(&mut out, host);
    }
    out
}

fn add_host(out: &mut BTreeSet<String>, raw: &str) {
    let mut host = raw
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split('%')
        .next()
        .unwrap_or("")
        .trim_end_matches('.');
    if let Some(bracketed) = host.strip_prefix('[') {
        host = bracketed.split(']').next().unwrap_or("");
    }
    // Bare values such as `host: 2001:db8::1` become `host:2001:db8::1` after tokenising.
    if let Some((key, rest)) = host.split_once(':')
        && !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'-')
    {
        host = rest;
    }
    if host.parse::<Ipv6Addr>().is_ok() {
        out.insert(host.to_owned());
        return;
    }
    if let Some((address, port)) = host.rsplit_once(':')
        && port.bytes().all(|b| b.is_ascii_digit())
    {
        host = address;
    }
    if let Some(bracketed) = host.strip_prefix('[') {
        host = bracketed.split(']').next().unwrap_or("");
    }
    if host.parse::<Ipv4Addr>().is_ok() || host.parse::<Ipv6Addr>().is_ok() {
        out.insert(host.to_owned());
        return;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let host_like = host == "localhost"
        || (labels.len() >= 2
            && labels.iter().all(|l| {
                !l.is_empty()
                    && l.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            && labels
                .last()
                .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_alphabetic())));
    if host_like {
        out.insert(host.to_owned());
    }
}

pub fn host_allowed(host: &str, allow: &[String], provenance: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if allow.iter().any(|raw| {
        let suffix = raw.trim_end_matches('.').to_ascii_lowercase();
        host == suffix || host.ends_with(&format!(".{suffix}"))
    }) {
        return true;
    }
    let provenance = provenance.to_ascii_lowercase();
    provenance.contains(&host) || extract_hosts(&provenance).contains(&host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        crate::config::parse(&toml::Table::new()).unwrap()
    }

    fn rules(name: &str, args: &str) -> Vec<&'static str> {
        let declared = BTreeSet::from([name.to_owned()]);
        check_call(name, args, &declared, "", &config())
            .into_iter()
            .map(|f| f.rule)
            .collect()
    }

    #[test]
    fn undeclared_and_declared_tools() {
        let found = check_call("get_weather", "{}", &BTreeSet::new(), "", &config());
        assert_eq!(
            found.into_iter().map(|f| f.rule).collect::<Vec<_>>(),
            ["undeclared_tool"]
        );
        let declared = BTreeSet::from(["get_weather".to_owned()]);
        let found = check_call("get_weather", "{}", &declared, "", &config());
        assert!(found.is_empty());
    }

    #[test]
    fn hosts_in_urls_bare_ip_and_ipv6() {
        for args in [
            "{\"url\":\"https://evil.example.org/a\"}",
            "{\"url\":\"http://198.51.100.7:8080/x\"}",
            "{\"host\":\"2001:db8::1\"}",
            "{\"host\":\"[2001:db8::1]:443\"}",
        ] {
            let hosts = extract_hosts(args);
            assert!(!hosts.is_empty(), "{args}");
            assert!(
                rules("web_fetch", args).contains(&"unapproved_host"),
                "{args}"
            );
        }
    }

    #[test]
    fn json_escapes_are_canonicalized_before_a_check() {
        let escaped_host = r#"{"url":"https:\u002f\u002fevil.example.org\u002fx"}"#;
        let canonical = crate::parse::canonical_arguments(escaped_host).unwrap();
        assert_eq!(canonical, r#"{"url":"https://evil.example.org/x"}"#);
        assert!(rules("web_fetch", &canonical).contains(&"unapproved_host"));

        let escaped_path = r#"{"cmd":"curl \u002fproc\u002fself\u002fenviron"}"#;
        let canonical = crate::parse::canonical_arguments(escaped_path).unwrap();
        assert_eq!(canonical, r#"{"cmd":"curl /proc/self/environ"}"#);
        assert!(rules("run", &canonical).contains(&"proc_environ"));

        let mixed = r#"{ "cmd" : "curl \/proc\/self\/environ" }"#;
        let canonical = crate::parse::canonical_arguments(mixed).unwrap();
        assert!(rules("run", &canonical).contains(&"proc_environ"));
    }

    #[test]
    fn scheme_less_hosts_and_allow_list_suffixes() {
        assert!(rules("fetch", "curl evil.example.org/page").contains(&"unapproved_host"));
        let mut config = config();
        config.allow_hosts = vec!["example.org".to_owned()];
        let found = check_call(
            "fetch",
            "curl https://api.example.org/a",
            &BTreeSet::new(),
            "",
            &config,
        );
        assert!(!found.iter().any(|f| f.rule == "unapproved_host"));
        let found = check_call(
            "fetch",
            "curl evil-example.org",
            &BTreeSet::new(),
            "",
            &config,
        );
        assert!(found.iter().any(|f| f.rule == "unapproved_host"));
    }

    #[test]
    fn user_and_system_provenance_is_accepted() {
        let declared = BTreeSet::from(["web_fetch".to_owned()]);
        let found = check_call(
            "web_fetch",
            "https://docs.example.org/page",
            &declared,
            "See https://docs.example.org for details",
            &config(),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn tripwires_match_plain_and_shell_unquoted_text() {
        for args in [
            "cat /proc/self/environ",
            "cat /proc/1234/environ",
            "unshare -rn sh",
            "mount --bind /a /b",
            "curl --noproxy '*' https://x.example.org",
            "export HTTPS_PROXY=",
            "export OPENAI_BASE_URL=http://evil.example.org",
            "cat ~/.ssh/id_rsa",
            "cat .git-credentials",
            "cat /etc/reiki/config.toml",
            "systemctl stop reiki",
            // The legacy matcher target is intentional here; it is not an example tool name.
            "pkill -f toolgate",
        ] {
            let found = rules("run", args);
            assert!(!found.is_empty(), "{args}");
        }
        assert!(rules("run", "echo hello").is_empty());
    }

    #[test]
    fn obfuscated_quoting_still_matches() {
        for args in [
            "c''url http://x.example.org",
            "\"cu\"\"rl\" http://x.example.org",
            "uns\\hare",
        ] {
            assert!(!rules("run", args).is_empty(), "{args}");
        }
        assert_eq!(unquote_shell("c''url \"cu\"\"rl\""), "curl curl");
    }

    #[test]
    fn non_network_calls_do_not_run_the_host_check() {
        let found = check_call(
            "calculate",
            "{\"expr\":\"1+1\"}",
            &BTreeSet::new(),
            "",
            &config(),
        );
        assert!(!found.iter().any(|f| f.rule == "unapproved_host"));
    }
}
