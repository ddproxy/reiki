//! Plain styled lines for a terminal UI: turns a [`Fleet`] into lines, with no terminal library.
//! Pure: the caller supplies `now`; no clock reads, no I/O.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::Fleet;

/// How a span should read on the terminal. `Plain` is the caller's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Plain,
    Dim,
    Title,
    Good,
    Warn,
    Bad,
}

/// A piece of text with one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// One line, as a list of spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    /// The line's text with styles stripped: for tests and a no-colour mode.
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// Peer-supplied strings reach the terminal, so control characters (escape sequences
/// included) are shown as `?`.
fn span(text: impl Into<String>, style: Style) -> Span {
    let text: String = text
        .into()
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    Span { text, style }
}

/// Truncates every line to `width` characters, never inside a multi-byte char.
fn line(spans: Vec<Span>, width: usize) -> Line {
    let mut out = Vec::new();
    let mut left = width;
    for s in spans {
        if left == 0 {
            break;
        }
        if s.text.chars().count() <= left {
            left -= s.text.chars().count();
            out.push(s);
        } else {
            let text: String = s.text.chars().take(left).collect();
            out.push(span(text, s.style));
            break;
        }
    }
    Line { spans: out }
}

/// `4h31m`, `12m05s` or `45s`, from whole seconds.
fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;
    if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m{secs:02}s")
    } else {
        format!("{secs}s")
    }
}

/// `filled` cells out of `total`, drawn with `█` and `░`.
fn bar(filled: usize, total: usize) -> String {
    let filled = filled.min(total);
    "█".repeat(filled) + &"░".repeat(total - filled)
}

/// The full scale a cooldown bar is drawn against. `None` for the class means the remaining
/// time itself, so the bar reads as full.
fn cooldown_scale(class: Option<&str>, remaining: Duration) -> Duration {
    match class {
        Some("quota_exhausted") => Duration::from_secs(5 * 3600),
        Some("rate_limited") => Duration::from_secs(60),
        Some("transient") => Duration::from_secs(30),
        _ => remaining,
    }
}

fn bar_cells(remaining: Duration, scale: Duration, cells: usize) -> usize {
    if scale.is_zero() {
        return cells;
    }
    let ratio = remaining.as_secs_f64() / scale.as_secs_f64();
    (ratio * cells as f64).round().clamp(0.0, cells as f64) as usize
}

/// f64 as the shortest obvious string: `50` for 50.0, `0.52` for 0.52.
fn fmt_num(n: f64) -> String {
    format!("{n}")
}

/// Renders the fleet as lines: a title, a providers block, then one card per peer in `peer_order`.
pub fn render(fleet: &Fleet, peer_order: &[String], now: Instant, width: usize) -> Vec<Line> {
    let mut out = Vec::new();

    let mut fresh = 0;
    for name in peer_order {
        if fleet.peer(name, now).is_some_and(|p| !p.stale) {
            fresh += 1;
        }
    }
    let stale = peer_order.len() - fresh;
    let names = if peer_order.len() == 1 {
        "peer"
    } else {
        "peers"
    };
    out.push(line(
        vec![span(
            format!(
                "Fleet: {} {names} ({fresh} fresh, {stale} stale)",
                peer_order.len()
            ),
            Style::Title,
        )],
        width,
    ));

    out.push(line(vec![span("Providers", Style::Title)], width));
    let rows = fleet.provider_summary(now);
    let name_w = rows
        .iter()
        .map(|r| r.upstream.chars().count())
        .max()
        .unwrap_or(0);
    for r in &rows {
        let filled = if r.total_keys == 0 {
            0
        } else {
            (r.healthy_keys as f64 / r.total_keys as f64 * 20.0)
                .round()
                .clamp(0.0, 20.0) as usize
        };
        let style = if r.total_keys > 0 && r.healthy_keys == r.total_keys {
            Style::Good
        } else if r.healthy_keys == 0 {
            Style::Bad
        } else {
            Style::Warn
        };
        let ct_word = if r.peer_count == 1 { "peer" } else { "peers" };
        out.push(line(
            vec![
                span(format!("  {:<name_w$}  ", r.upstream), Style::Plain),
                span(format!("[{}]", bar(filled, 20)), style),
                span(
                    format!(
                        "  {}/{}  {} {ct_word}",
                        r.healthy_keys, r.total_keys, r.peer_count
                    ),
                    Style::Plain,
                ),
            ],
            width,
        ));
    }

    for name in peer_order {
        let Some(peer) = fleet.peer(name, now) else {
            out.push(line(
                vec![
                    span(format!("{name}  "), Style::Title),
                    span("no data", Style::Dim),
                ],
                width,
            ));
            if let Some(error) = fleet.error(name) {
                out.push(error_line(error, width));
            }
            continue;
        };
        let mut header = vec![
            span(format!("{name}  "), Style::Title),
            span(format!("instance {}", peer.health.instance), Style::Plain),
        ];
        for seg in host_segments(peer.health) {
            header.push(span(format!("  {seg}"), Style::Plain));
        }
        if let Some(n) = peer
            .health
            .sessions
            .as_ref()
            .and_then(|s| s.get("active_sessions"))
            .and_then(Value::as_u64)
        {
            header.push(span(format!("  sessions {n}"), Style::Plain));
        }
        if peer.stale {
            header.push(span(
                format!("  stale {}", fmt_duration(peer.age)),
                Style::Dim,
            ));
        }
        out.push(line(header, width));
        if let Some(error) = fleet.error(name) {
            out.push(error_line(error, width));
        }

        for u in &peer.health.upstreams {
            out.push(line(
                vec![
                    span(format!("  {}: ", u.name), Style::Plain),
                    span(format!("{} healthy", u.healthy_keys), Style::Good),
                    span(format!("  {} cooling", u.cooling_keys), Style::Warn),
                    span(format!("  {} disabled", u.disabled_keys), Style::Bad),
                ],
                width,
            ));
            if let Some(keys) = &u.keys {
                for k in keys {
                    out.push(key_line(k, width));
                }
            }
        }
    }

    out
}

/// A peer's last feed failure, dim so it never reads as health.
fn error_line(error: &str, width: usize) -> Line {
    line(vec![span(format!("  error {error}"), Style::Dim)], width)
}

/// `load`, one `disk` per readable disk, from the optional `host` object. A shape we cannot
/// read is skipped, never guessed.
fn host_segments(health: &crate::Health) -> Vec<String> {
    let Some(host) = health.host.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(load) = host.get("load_average").and_then(Value::as_array) {
        let nums: Vec<String> = load.iter().filter_map(Value::as_f64).map(fmt_num).collect();
        if !nums.is_empty() {
            out.push(format!("load {}", nums.join(" ")));
        }
    }
    if let Some(disks) = host.get("disks").and_then(Value::as_array) {
        for disk in disks {
            let Some(pct) = disk.get("used_percent").and_then(Value::as_f64) else {
                continue;
            };
            let roles: Vec<String> = disk
                .get("roles")
                .and_then(Value::as_array)
                .map(|r| {
                    r.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let label = roles.join("+");
            match label.is_empty() {
                true => out.push(format!("disk {}%", fmt_num(pct))),
                false => out.push(format!("disk {label} {}%", fmt_num(pct))),
            }
        }
    }
    out
}

fn key_line(k: &crate::KeyView, width: usize) -> Line {
    let state = match k.state.as_str() {
        "available" => Style::Good,
        "cooling" => Style::Warn,
        _ => Style::Bad,
    };
    let mut spans = vec![
        span(format!("    {}  ", k.name), Style::Plain),
        span(k.state.clone(), state),
    ];
    if k.state == "cooling" {
        if let Some(secs) = k.cooldown_seconds {
            let remaining = Duration::from_secs(secs);
            let scale = cooldown_scale(k.cooldown_class.as_deref(), remaining);
            let filled = bar_cells(remaining, scale, 12);
            spans.push(span(format!("  [{}]", bar(filled, 12)), Style::Warn));
            spans.push(span(format!("  {}", fmt_duration(remaining)), Style::Plain));
        }
    }
    line(spans, width)
}

#[cfg(test)]
mod tests {
    #[test]
    fn control_characters_from_a_peer_never_reach_the_screen() {
        let spans = [span("evil\u{1b}[2J\u{7}name", Style::Plain)];
        assert_eq!(spans[0].text, "evil?[2J?name");
    }

    use super::*;
    use crate::Health;

    fn fleet() -> Fleet {
        Fleet::new(Duration::from_secs(90))
    }

    fn add(fleet: &mut Fleet, name: &str, doc: &str, at: Instant) {
        fleet.apply_snapshot(name, Health::from_json(doc).unwrap(), at);
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain_text).collect()
    }

    fn find<'a>(lines: &'a [Line], needle: &str) -> &'a Line {
        lines
            .iter()
            .find(|l| l.plain_text().contains(needle))
            .unwrap_or_else(|| panic!("a line containing {needle:?} exists"))
    }

    /// Two fresh peers with a spread of providers for the summary bar tests.
    const MIX_DOC: &str = r#"{"instance":"peer-a","observed_at":"t","upstreams":[
        {"name":"zen","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,
         "soonest_cooldown_seconds":300,"last_status_class":"4xx"},
        {"name":"open","healthy_keys":2,"cooling_keys":0,"disabled_keys":0,
         "soonest_cooldown_seconds":null,"last_status_class":"2xx"},
        {"name":"gem","healthy_keys":0,"cooling_keys":0,"disabled_keys":3,
         "soonest_cooldown_seconds":null,"last_status_class":"5xx"}]}"#;

    const FULL_DOC: &str = r#"{
        "instance": "peer-a",
        "observed_at": "2026-11-13T09:30:00Z",
        "upstreams": [{"name": "zen", "healthy_keys": 1, "cooling_keys": 0, "disabled_keys": 0,
                       "soonest_cooldown_seconds": null, "last_status_class": "2xx"}],
        "host": {"load_average": [0.52, 0.58, 0.59], "cores": 8,
                 "disks": [{"roles": ["config", "state"], "total_bytes": 107374182400,
                            "available_bytes": 53687091200, "used_percent": 50.0}]},
        "sessions": {"window_seconds": 300, "active_sessions": 3}
    }"#;

    const BARE_DOC: &str = r#"{"instance":"peer-a","observed_at":"t","upstreams":[
        {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
         "soonest_cooldown_seconds":null,"last_status_class":"2xx"}]}"#;

    fn keyed_doc(cooldown: Option<u64>, class: Option<&str>) -> String {
        let class = class
            .map(|c| format!(r#","cooldown_class":"{c}""#))
            .unwrap_or_default();
        let cooldown = cooldown.map(|s| s.to_string()).unwrap_or("null".into());
        format!(
            r#"{{"instance":"peer-a","observed_at":"t","upstreams":[
            {{"name":"zen","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,
             "soonest_cooldown_seconds":{cooldown},"last_status_class":"4xx",
             "keys":[{{"name":"b","state":"cooling","cooldown_seconds":{cooldown}{class}}}]}}]}}"#
        )
    }

    #[test]
    fn title_counts_fresh_and_stale_cts() {
        let t0 = Instant::now();
        let mut f = Fleet::new(Duration::from_secs(30));
        add(&mut f, "peer-a", BARE_DOC, t0 - Duration::from_secs(40)); // stale
        add(&mut f, "peer-b", BARE_DOC, t0); // fresh
        let lines = render(
            &f,
            &[
                "peer-a".to_owned(),
                "peer-b".to_owned(),
                "peer-c".to_owned(),
            ],
            t0,
            200,
        );
        assert_eq!(lines[0].plain_text(), "Fleet: 3 peers (1 fresh, 2 stale)");
    }

    #[test]
    fn provider_bars_show_all_some_and_no_healthy_keys() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", MIX_DOC, t0);
        add(
            &mut f,
            "peer-b",
            r#"{"instance":"peer-b","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":0,"cooling_keys":2,"disabled_keys":1,
             "soonest_cooldown_seconds":300,"last_status_class":"4xx"},
            {"name":"gem","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,
             "soonest_cooldown_seconds":300,"last_status_class":"4xx"}]}"#,
            t0,
        );
        let lines = render(&f, &["peer-a".to_owned(), "peer-b".to_owned()], t0, 200);
        let row = |name: &str, filled: usize, ht: &str, names: &str| {
            format!("  {name:<4}  [{}]  {ht}  {names}", bar(filled, 20))
        };
        assert_eq!(
            find(&lines, "gem").plain_text(),
            row("gem", 0, "0/4", "2 peers")
        );
        assert_eq!(
            find(&lines, "open").plain_text(),
            row("open", 20, "2/2", "1 peer")
        );
        assert_eq!(
            find(&lines, "zen").plain_text(),
            row("zen", 4, "1/5", "2 peers")
        );
        assert_eq!(find(&lines, "zen").spans[1].style, Style::Warn);
        assert_eq!(find(&lines, "open").spans[1].style, Style::Good);
        assert_eq!(find(&lines, "gem").spans[1].style, Style::Bad);
    }

    #[test]
    fn a_quota_cooling_key_has_a_nearly_full_five_hour_bar() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &keyed_doc(Some(16260), Some("quota_exhausted")),
            t0,
        );
        let lines = render(&f, &["peer-a".to_owned()], t0, 200);
        let key = find(&lines, "b  cooling");
        assert_eq!(
            key.plain_text(),
            format!("    b  cooling  [{}]  4h31m", "█".repeat(11) + "░")
        );
    }

    #[test]
    fn a_rate_limited_key_has_a_fullish_sixty_second_bar() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &keyed_doc(Some(45), Some("rate_limited")),
            t0,
        );
        let lines = render(&f, &["peer-a".to_owned()], t0, 200);
        assert_eq!(
            find(&lines, "b  cooling").plain_text(),
            format!("    b  cooling  [{}]  45s", "█".repeat(9) + &"░".repeat(3)),
        );
    }

    #[test]
    fn a_transient_key_uses_a_thirty_second_scale() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &keyed_doc(Some(10), Some("transient")),
            t0,
        );
        let lines = render(&f, &["peer-a".to_owned()], t0, 200);
        assert_eq!(
            find(&lines, "b  cooling").plain_text(),
            format!("    b  cooling  [{}]  10s", "█".repeat(4) + &"░".repeat(8)),
        );
    }

    #[test]
    fn an_unknown_class_draws_a_full_bar() {
        let t0 = Instant::now();
        for class in [Some("brand_new_class"), None] {
            let mut f = fleet();
            add(&mut f, "peer-a", &keyed_doc(Some(725), class), t0);
            let lines = render(&f, &["peer-a".to_owned()], t0, 200);
            assert_eq!(
                find(&lines, "b  cooling").plain_text(),
                format!("    b  cooling  [{}]  12m05s", "█".repeat(12)),
                "class {class:?}",
            );
        }
    }

    #[test]
    fn an_out_of_rotation_key_is_red_with_no_bar() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":0,"cooling_keys":0,"disabled_keys":1,
             "soonest_cooldown_seconds":null,"last_status_class":"4xx",
             "keys":[{"name":"c","state":"out_of_rotation","cooldown_seconds":null}]}]}"#,
            t0,
        );
        let lines = render(&f, &["peer-a".to_owned()], t0, 200);
        let key = find(&lines, "out_of_rotation");
        assert_eq!(key.plain_text(), "    c  out_of_rotation");
        assert_eq!(key.spans[1].style, Style::Bad);
    }

    #[test]
    fn a_stale_ct_is_marked_with_its_age() {
        let t0 = Instant::now();
        let mut f = Fleet::new(Duration::from_secs(30));
        add(&mut f, "peer-a", BARE_DOC, t0 - Duration::from_secs(40));
        let lines = render(&f, &["peer-a".to_owned()], t0, 200);
        let header = find(&lines, "instance");
        assert_eq!(header.plain_text(), "peer-a  instance peer-a  stale 40s");
        assert_eq!(header.spans.last().unwrap().style, Style::Dim);
    }

    #[test]
    fn a_ct_feed_error_is_a_dim_line() {
        let t0 = Instant::now();
        let mut f = fleet();
        f.set_error("peer-a", "stream: the stream ended; poll: cannot connect");
        let lines = render(&f, &["peer-a".to_owned()], t0, 200);
        let error = find(&lines, " error ");
        assert_eq!(
            error.plain_text(),
            "  error stream: the stream ended; poll: cannot connect"
        );
        assert_eq!(error.spans[0].style, Style::Dim);
    }

    #[test]
    fn a_missing_ct_shows_no_data() {
        let t0 = Instant::now();
        let f = fleet();
        let lines = render(&f, &["peer-d".to_owned()], t0, 200);
        assert_eq!(lines[0].plain_text(), "Fleet: 1 peer (0 fresh, 1 stale)");
        assert_eq!(lines[2].plain_text(), "peer-d  no data");
        assert_eq!(lines[2].spans[1].style, Style::Dim);
    }

    #[test]
    fn host_stats_and_sessions_show_when_present() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", FULL_DOC, t0);
        let lines = render(&f, &["peer-a".to_owned()], t0, 200);
        let header = find(&lines, "instance").plain_text();
        assert!(header.contains("load 0.52 0.58 0.59"), "{header}");
        assert!(header.contains("disk config+state 50%"), "{header}");
        assert!(header.contains("sessions 3"), "{header}");
    }

    #[test]
    fn host_stats_and_sessions_are_absent_otherwise() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", BARE_DOC, t0);
        let lines = render(&f, &["peer-a".to_owned()], t0, 200);
        let header = find(&lines, "instance").plain_text();
        for absent in ["load", "disk", "sessions"] {
            assert!(!header.contains(absent), "{absent} in {header}");
        }
    }

    #[test]
    fn an_unreadable_host_shape_is_ignored() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"peer-a","observed_at":"t",
            "upstreams":[],
            "host": {"load_average": null, "disks": [{"roles": "oops", "used_percent": "high"}]},
            "sessions": {"active_sessions": "three"}}"#,
            t0,
        );
        let lines = render(&f, &["peer-a".to_owned()], t0, 200);
        assert_eq!(
            find(&lines, "instance").plain_text(),
            "peer-a  instance peer-a"
        );
    }

    #[test]
    fn lines_truncate_to_width_without_splitting_multibyte_chars() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "pr-é🦀x", BARE_DOC, t0);
        let names = vec!["pr-é🦀x".to_owned()];
        let lines = render(&f, &names, t0, 5);
        for l in &lines {
            assert!(l.plain_text().chars().count() <= 5);
        }
        // c, t, -, é, 🦀: exactly five chars, 🦀 is not split.
        assert_eq!(find(&lines, "é").plain_text(), "pr-é🦀");
    }

    #[test]
    fn width_zero_and_one_never_panic() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", FULL_DOC, t0);
        for width in [0, 1] {
            let lines = render(&f, &["peer-a".to_owned()], t0, width);
            for l in &lines {
                assert!(l.plain_text().chars().count() <= width);
            }
        }
        let lines = render(&f, &["peer-a".to_owned()], t0, 0);
        assert!(lines.iter().all(|l| l.plain_text().is_empty()));
    }

    #[test]
    fn keys_rows_appear_only_with_a_labelled_list() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", BARE_DOC, t0);
        let unlabelled = texts(&render(&f, &["peer-a".to_owned()], t0, 200));
        assert!(
            !unlabelled.iter().any(|l| l.contains("available")),
            "no key rows without labels"
        );
    }
}
