//! Plain styled lines for a terminal UI: turns a [`Fleet`] into lines, with no terminal library.
//! Pure: the caller supplies `now`; no clock reads, no I/O.

use std::time::{Duration, Instant};

use serde_json::Value;

use unicode_width::UnicodeWidthChar;

use crate::matrix::{
    order_peers, resolve, Cell, History, Matrix, Metric, Pressure, Row, MINUTE_SLOTS,
};
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
/// Control characters, and the invisible format characters that reorder or hide text (bidi
/// overrides and isolates, zero-width marks, the byte-order mark): a peer must not be able to
/// spoof what another peer's name or a status line says.
fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
        )
}

fn span(text: impl Into<String>, style: Style) -> Span {
    let text: String = text
        .into()
        .chars()
        .map(|c| if unsafe_char(c) { '?' } else { c })
        .collect();
    Span { text, style }
}

/// Truncates every line to `width` terminal columns, never inside a multi-byte char.
fn line(spans: Vec<Span>, width: usize) -> Line {
    let mut out = Vec::new();
    let mut left = width;
    for s in spans {
        if left == 0 {
            break;
        }
        let w = text_width(&s.text);
        if w <= left {
            left -= w;
            out.push(s);
        } else {
            out.push(span(clip(&s.text, left), s.style));
            break;
        }
    }
    Line { spans: out }
}

/// Terminal columns a string takes; wide characters count two, combining marks none.
fn text_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// One character's columns. A character `span` will replace with `?` takes one, so widths
/// measured before and after sanitising agree.
fn char_width(c: char) -> usize {
    if unsafe_char(c) {
        1
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
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

/// One sparkline character for `value` against `scale`, or `None` for zero. A busy bucket never
/// rounds down to nothing.
fn level(value: u64, scale: u64) -> Option<char> {
    if value == 0 {
        return None;
    }
    if scale == 0 {
        return Some(SPARK[SPARK.len() - 1]);
    }
    let n = (value.saturating_mul(8).saturating_add(scale / 2) / scale).clamp(1, 8) as usize;
    Some(SPARK[n - 1])
}

/// The gauge's metric and window: what the matrix's gauge cells count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GaugeMode {
    pub metric: GaugeMetric,
    pub window: GaugeWindow,
}

/// Requests attempted, or tokens the provider reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GaugeMetric {
    #[default]
    Requests,
    Tokens,
}

/// The last hour in six ten-minute cells, or the last five minutes in one cell per minute.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GaugeWindow {
    #[default]
    Hour,
    Minutes,
}

impl GaugeMode {
    /// The fleet line's tag, such as `[req 1h]`.
    pub fn tag(self) -> String {
        let metric = match self.metric {
            GaugeMetric::Requests => "req",
            GaugeMetric::Tokens => "tok",
        };
        let window = match self.window {
            GaugeWindow::Hour => "1h",
            GaugeWindow::Minutes => "5m",
        };
        format!("[{metric} {window}]")
    }

    /// Cells the window draws; the gauge is padded to [`GAUGE_CELLS`] either way.
    fn cells(self) -> usize {
        match self.window {
            GaugeWindow::Hour => GAUGE_CELLS,
            GaugeWindow::Minutes => MINUTE_CELLS,
        }
    }
}

/// Six paired five-minute buckets, oldest first.
fn bucket_pairs(buckets: &[u64; 12]) -> [u64; GAUGE_CELLS] {
    let mut out = [0u64; GAUGE_CELLS];
    for (slot, pair) in out.iter_mut().zip(buckets.as_chunks::<2>().0) {
        *slot = pair[0].saturating_add(pair[1]);
    }
    out
}

/// The newest [`MINUTE_CELLS`] of a row's ten published minutes, oldest first.
fn newest_minutes(minutes: &[u32; MINUTE_SLOTS]) -> Vec<u64> {
    minutes[MINUTE_SLOTS - MINUTE_CELLS..]
        .iter()
        .map(|&n| u64::from(n))
        .collect()
}

/// The values the gauge draws for one upstream, oldest first, or `None` when the publisher sent
/// no figures for this metric and window.
fn gauge_values(pressure: &Pressure, mode: GaugeMode) -> Option<Vec<u64>> {
    match (mode.metric, mode.window) {
        (GaugeMetric::Requests, GaugeWindow::Hour) => {
            Some(bucket_pairs(&pressure.buckets.map(u64::from)).to_vec())
        }
        (GaugeMetric::Tokens, GaugeWindow::Hour) => pressure
            .token_buckets
            .as_ref()
            .map(|buckets| bucket_pairs(buckets).to_vec()),
        (GaugeMetric::Requests, GaugeWindow::Minutes) => {
            pressure.minutes.as_ref().map(newest_minutes)
        }
        (GaugeMetric::Tokens, GaugeWindow::Minutes) => {
            pressure.token_minutes.as_ref().map(newest_minutes)
        }
    }
}

/// The busiest cell in one column across the fleet: the gauge scale.
fn column_peak(matrix: &Matrix, column: usize, mode: GaugeMode) -> u64 {
    matrix
        .rows
        .iter()
        .filter_map(|row| row.pressure.get(column).and_then(Option::as_ref))
        .filter_map(|pressure| gauge_values(pressure, mode))
        .flatten()
        .max()
        .unwrap_or(0)
}

/// `" ▁▂▃▄▅▆"` after the state glyph: `·` for a zero cell and a dim run when the publisher
/// sent no figures. The minute window has five cells and a trailing space, so both windows fill
/// the same width.
fn gauge(pressure: Option<&Pressure>, scale: u64, mode: GaugeMode) -> (String, Style) {
    let cells = mode.cells();
    let mut text = String::from(" ");
    let style = match pressure.and_then(|p| gauge_values(p, mode)) {
        None => {
            text.push_str(&"·".repeat(cells));
            Style::Dim
        }
        Some(values) => {
            for value in values {
                text.push(level(value, scale).unwrap_or('·'));
            }
            if pressure.is_some_and(|p| p.limited_1h > 0) {
                Style::Warn
            } else {
                Style::Plain
            }
        }
    };
    text.push_str(&" ".repeat(GAUGE_CELLS - cells));
    (text, style)
}

/// Twelve five-minute cells for the detail pane, scaled to this upstream alone.
fn trend(buckets: &[u32; 12]) -> String {
    let peak = buckets.iter().copied().max().map(u64::from).unwrap_or(0);
    buckets
        .iter()
        .map(|&n| level(u64::from(n), peak).unwrap_or('·'))
        .collect()
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
        .map(|r| text_width(&r.upstream))
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
        peer_card(fleet, name, now, width, &mut out);
    }

    out
}

/// One peer's card: its header, any feed error, and each upstream's counts and key rows.
pub fn peer_detail(fleet: &Fleet, name: &str, now: Instant, width: usize) -> Vec<Line> {
    let mut out = Vec::new();
    peer_card(fleet, name, now, width, &mut out);
    out
}

fn peer_card(fleet: &Fleet, name: &str, now: Instant, width: usize, out: &mut Vec<Line>) {
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
        return;
    };
    let mut header = vec![
        span(format!("{name}  "), Style::Title),
        span(format!("instance {}", peer.health.instance), Style::Plain),
    ];
    if let Some(version) = &peer.health.reiki_version {
        header.push(span(format!("  v{version}"), Style::Plain));
    }
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
        let mut spans = vec![
            span(format!("  {}: ", u.name), Style::Plain),
            span(format!("{} healthy", u.healthy_keys), Style::Good),
            span(format!("  {} cooling", u.cooling_keys), Style::Warn),
            span(format!("  {} disabled", u.disabled_keys), Style::Bad),
        ];
        if let Some(class) = crate::matrix::cooldown_class(u) {
            spans.push(span(format!("  class {class}"), Style::Warn));
        }
        if u.cooling_combinations > 0 {
            spans.push(span(
                format!("  {} model cooling", u.cooling_combinations),
                Style::Warn,
            ));
        }
        if let Some(class) = u.last_status_class.as_deref().filter(|c| *c != "2xx") {
            spans.push(span(format!("  last {class}"), Style::Warn));
        }
        out.push(line(spans, width));
        if let Some(requests) = u.requests_1h {
            let limited = u.limited_1h.unwrap_or(0);
            let style = if limited > 0 {
                Style::Warn
            } else {
                Style::Plain
            };
            let mut spans = vec![span(
                format!("    last hour: {requests} requests, {limited} limited"),
                style,
            )];
            if let Some(buckets) = u.request_buckets {
                spans.push(span(format!("  {}", trend(&buckets)), style));
            }
            out.push(line(spans, width));
        }
        if let Some(keys) = &u.keys {
            for k in keys {
                out.push(key_line(k, width));
            }
        }
    }

    if !peer.health.failures.is_empty() {
        let mut spans = vec![span("  failures:", Style::Plain)];
        for (class, count) in &peer.health.failures {
            spans.push(span(format!("  {class} {count}"), Style::Warn));
        }
        out.push(line(spans, width));
    }
    // Only the local instance's admin listener returns rows; a remote peer publishes counts.
    if let Some(failures) = fleet.local_failures(&peer.health.instance) {
        for failure in failures {
            out.push(failure_line(failure, width));
        }
    }
}

fn failure_line(failure: &crate::Failure, width: usize) -> Line {
    let time = failure.time.rem_euclid(86_400);
    let mut spans = vec![
        span(
            format!(
                "    {:02}:{:02}:{:02}  ",
                time / 3_600,
                time % 3_600 / 60,
                time % 60
            ),
            Style::Dim,
        ),
        span(format!("{}  ", failure.stream), Style::Plain),
        span(format!("{}  ", failure.key), Style::Plain),
        span(
            failure
                .status
                .map_or_else(|| "-".to_owned(), |status| status.to_string()),
            Style::Plain,
        ),
        span(format!("  {}", failure.class), Style::Warn),
    ];
    if failure.cooled {
        let left = failure
            .cooling_seconds
            .map_or_else(|| "cooled".to_owned(), |secs| format!("cooled {secs}s"));
        spans.push(span(format!("  {left}"), Style::Warn));
    }
    if !failure.note.is_empty() {
        spans.push(span(format!("  {}", failure.note), Style::Dim));
    }
    line(spans, width)
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

/// Sparkline levels, lowest to highest.
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Cells in the last-hour gauge, one per pair of five-minute buckets.
const GAUGE_CELLS: usize = 6;
/// Cells in the last-five-minutes gauge, one per minute.
const MINUTE_CELLS: usize = 5;
/// Cooling panel lines before the `+N more` count.
const COOLING_LINES: usize = 6;
/// Cells in a cooling panel drain bar.
const COOLING_BAR: usize = 20;
/// Narrowest provider column that can hold the glyph, a space and the gauge.
const GAUGE_WIDTH: usize = 1 + 1 + GAUGE_CELLS;
/// Longest peer name drawn; longer names are clipped.
const NAME_MAX: usize = 16;
/// Longest provider header drawn at full width.
const LABEL_MAX: usize = 24;
/// Samples in a sparkline.
const SPARK_LEN: usize = 8;
/// Below this width the matrix draws only the fleet line and peer names.
const NAMES_ONLY_BELOW: usize = 20;
/// Spaces between columns.
const GAP: usize = 2;
const GAP_TEXT: &str = "  ";
/// Header labels for the host columns, in the order `host_figures` returns them.
const HOST_LABELS: [&str; 3] = ["load", "disk", "sess"];

/// Widest version text in the matrix; the detail card shows the whole of it.
const VER_MAX: usize = 12;
/// Header of the version column.
const VER_HEADER: &str = "Ver";

/// Layouts tried in order: the version column goes first, then the gauge, then provider names
/// are shortened, and the sparkline goes last.
const FITS: [(bool, usize, bool, bool); 6] = [
    (true, LABEL_MAX, true, true),
    (false, LABEL_MAX, true, true),
    (false, LABEL_MAX, true, false),
    (false, 6, true, false),
    (false, 3, true, false),
    (false, 3, false, false),
];

/// The peers-by-providers matrix: a fleet line with one tally per provider, a header, then one
/// row per peer in [`order_peers`] order. `selected` is a display position, wrapped to the row
/// count; `None` draws no marker. Sparklines come from `history`, so an empty history draws
/// none. `filter` names the active provider filter shown in the fleet line. Too narrow for the
/// columns, it falls back to one chip line per peer.
pub fn render_matrix(
    matrix: &Matrix,
    history: &History,
    selected: Option<usize>,
    width: usize,
    filter: Option<&str>,
) -> Vec<Line> {
    render_matrix_mode(
        matrix,
        history,
        selected,
        width,
        filter,
        GaugeMode::default(),
    )
}

/// [`render_matrix`] with the gauge's metric and window chosen by the caller. The fleet line ends
/// with the mode's tag.
pub fn render_matrix_mode(
    matrix: &Matrix,
    history: &History,
    selected: Option<usize>,
    width: usize,
    filter: Option<&str>,
    mode: GaugeMode,
) -> Vec<Line> {
    let order = order_peers(matrix);
    let selected = selected.and_then(|s| resolve(Some(s), order.len()));
    let fresh = matrix.rows.iter().filter(|r| !r.stale).count();
    let mut title = vec![span(
        format!("Fleet {fresh}/{} fresh", matrix.rows.len()),
        Style::Title,
    )];
    if let Some(name) = filter {
        title.push(span(format!("   filter: {name}"), Style::Title));
    }
    for col in &matrix.columns {
        let style = if col.healthy_peers == col.peers_with {
            Style::Good
        } else if col.healthy_peers == 0 {
            Style::Bad
        } else {
            Style::Warn
        };
        title.push(span(
            format!(
                "   {} {}",
                clip(&col.upstream, LABEL_MAX),
                col.total_label()
            ),
            style,
        ));
    }

    if matrix.hidden_columns > 0 {
        title.push(span(
            format!("   +{} more", matrix.hidden_columns),
            Style::Dim,
        ));
    }
    if filter.is_none() {
        let mut alerts = Vec::new();
        for (c, col) in matrix.columns.iter().enumerate() {
            if col.peers_with == 0 {
                continue;
            }
            let mut all_down = true;
            let mut soonest: Option<Duration> = None;
            let mut has_peer = false;
            for row in &matrix.rows {
                let Some(cell) = row.cells.get(c) else {
                    continue;
                };
                if *cell == Cell::Absent {
                    continue;
                }
                has_peer = true;
                match cell {
                    Cell::Cooling { soonest: s, .. } => {
                        if let Some(s) = s {
                            soonest = Some(match soonest {
                                Some(cur) => cur.min(*s),
                                None => *s,
                            });
                        }
                    }
                    Cell::Disabled => {}
                    _ => {
                        all_down = false;
                        break;
                    }
                }
            }
            if has_peer && all_down {
                let mut text = format!("ALERT {} all cooling", col.upstream);
                if let Some(d) = soonest {
                    text.push_str(&format!(", soonest {}", fmt_duration(d)));
                }
                alerts.push(text);
            }
        }
        for text in alerts.iter().take(3) {
            title.push(span(format!("   {text}"), Style::Bad));
        }
        if alerts.len() > 3 {
            title.push(span(format!("   +{} more", alerts.len() - 3), Style::Bad));
        }
    }
    title.push(span(format!("   {}", mode.tag()), Style::Title));

    if order.is_empty() {
        return vec![line(title, width)];
    }
    if width < NAMES_ONLY_BELOW {
        let mut out = vec![line(title, width)];
        for (pos, &i) in order.iter().enumerate() {
            let name = clip(&matrix.rows[i].peer, width);
            out.push(line(
                vec![
                    marker(selected == Some(pos)),
                    span(name, name_style(selected == Some(pos))),
                ],
                width,
            ));
        }
        return out;
    }

    for (ver, label_max, spark, gauge) in FITS {
        let grid = Grid::new(
            matrix,
            &order,
            history,
            label_max,
            spark,
            gauge.then_some(mode),
            ver,
        );
        if grid.width() <= width {
            return grid.lines(title, selected, width);
        }
    }
    chip_lines(matrix, &order, history, title, selected, width)
}

/// Keeps the pinned lines (fleet line, header) and the window of peer rows that holds the
/// selected one, so a fleet taller than the screen scrolls with the selection instead of
/// hiding it. `rows` is the peer count and `selected` a display position.
pub fn window_matrix(
    mut lines: Vec<Line>,
    rows: usize,
    selected: Option<usize>,
    height: usize,
) -> Vec<Line> {
    let pinned = lines.len().saturating_sub(rows);
    if height <= pinned {
        lines.truncate(height);
        return lines;
    }
    let visible = height - pinned;
    if rows <= visible {
        return lines;
    }
    let pos = selected.unwrap_or(0).min(rows - 1);
    let start = (pos + 1).saturating_sub(visible);
    let mut out: Vec<Line> = lines.drain(..pinned).collect();
    out.extend(lines.into_iter().skip(start).take(visible));
    out
}

/// One (peer, provider) pair with cooling keys, as the cooling panel lists it.
struct Cooling {
    peer: String,
    provider: String,
    /// `None` when the publisher sent no cooldown time for the cooling keys.
    remaining: Option<Duration>,
    class: Option<String>,
    last: Option<String>,
}

fn cooling_of(peer: &str, upstream: &crate::Upstream) -> Option<Cooling> {
    let keyed = || {
        upstream
            .keys
            .iter()
            .flatten()
            .filter(|k| k.state == "cooling")
    };
    if upstream.cooling_keys == 0 && keyed().next().is_none() {
        return None;
    }
    let secs = keyed()
        .filter_map(|k| k.cooldown_seconds)
        .min()
        .or(upstream.soonest_cooldown_seconds);
    Some(Cooling {
        peer: peer.to_owned(),
        provider: upstream.name.clone(),
        remaining: secs.map(Duration::from_secs),
        class: crate::matrix::cooldown_class(upstream),
        last: upstream.last_status_class.clone(),
    })
}

/// Cooling lines under the matrix, one per fresh (peer, provider) with cooling keys, soonest first.
pub fn cooling_panel(
    fleet: &Fleet,
    names: &[String],
    provider: Option<&str>,
    now: Instant,
    width: usize,
) -> Vec<Line> {
    let mut entries: Vec<Cooling> = Vec::new();
    for name in names {
        let Some(peer) = fleet.peer(name, now).filter(|p| !p.stale) else {
            continue;
        };
        for upstream in &peer.health.upstreams {
            if provider.is_some_and(|p| p != upstream.name) {
                continue;
            }
            entries.extend(cooling_of(name, upstream));
        }
    }
    if entries.is_empty() {
        return Vec::new();
    }
    entries.sort_by(|a, b| {
        (a.remaining.is_none(), a.remaining, &a.peer, &a.provider).cmp(&(
            b.remaining.is_none(),
            b.remaining,
            &b.peer,
            &b.provider,
        ))
    });

    let shown = entries.len().min(COOLING_LINES);
    let name_w = entries[..shown]
        .iter()
        .map(|e| text_width(&clip(&e.peer, NAME_MAX)))
        .max()
        .unwrap_or(0);
    let provider_w = entries[..shown]
        .iter()
        .map(|e| text_width(&clip(&e.provider, LABEL_MAX)))
        .max()
        .unwrap_or(0);

    let mut out = vec![line(vec![span("Cooling", Style::Title)], width)];
    for e in &entries[..shown] {
        let (filled, left) = match e.remaining {
            Some(remaining) => {
                let scale = cooldown_scale(e.class.as_deref(), remaining);
                (
                    bar_cells(remaining, scale, COOLING_BAR),
                    fmt_duration(remaining),
                )
            }
            None => (COOLING_BAR, "?".to_owned()),
        };
        out.push(line(
            vec![
                span(
                    format!("  {}  ", pad(&clip(&e.peer, NAME_MAX), name_w)),
                    Style::Plain,
                ),
                span(
                    format!("{}  ", pad(&clip(&e.provider, LABEL_MAX), provider_w)),
                    Style::Plain,
                ),
                span("◐ cooling", Style::Warn),
                span(format!("  [{}]", bar(filled, COOLING_BAR)), Style::Warn),
                span(format!("  {left}"), Style::Plain),
                span(
                    format!("  last {}", e.last.as_deref().unwrap_or("-")),
                    Style::Plain,
                ),
            ],
            width,
        ));
    }
    if entries.len() > shown {
        out.push(line(
            vec![span(
                format!("  +{} more", entries.len() - shown),
                Style::Dim,
            )],
            width,
        ));
    }
    out
}

/// One peer's chip line per row: its providers with their cell, then load and disk.
fn chip_lines(
    matrix: &Matrix,
    order: &[usize],
    history: &History,
    title: Vec<Span>,
    selected: Option<usize>,
    width: usize,
) -> Vec<Line> {
    let mut out = vec![line(title, width)];
    for (pos, &i) in order.iter().enumerate() {
        let row = &matrix.rows[i];
        let is_selected = selected == Some(pos);
        let mut spans = vec![
            marker(is_selected),
            span(clip(&row.peer, NAME_MAX), name_style(is_selected)),
        ];
        match host_area(row, history, false) {
            HostArea::Stale(text, style) => {
                spans.push(span(format!("  {text}"), style));
            }
            HostArea::Figures(figures) => {
                for (c, (col, cell)) in matrix.columns.iter().zip(&row.cells).enumerate() {
                    if *cell == Cell::Absent {
                        continue;
                    }
                    let (text, style) = cell_text(cell, row.combos.get(c).copied().unwrap_or(0));
                    spans.push(span(
                        format!("  {} {text}", clip(&col.upstream, LABEL_MAX)),
                        style,
                    ));
                }
                let (load, _) = &figures[0];
                if load != "-" {
                    spans.push(span(format!("  load {load}"), Style::Plain));
                }
                if let Some(pct) = row.host.disk_percent {
                    spans.push(span(format!("  disk {}%", disk_percent(pct)), Style::Plain));
                }
            }
        }
        out.push(line(trim_end(spans), width));
    }
    out
}

/// The column layout for one header width and sparkline setting.
struct Grid<'a> {
    matrix: &'a Matrix,
    order: &'a [usize],
    /// Header per column, clipped to the layout's label width.
    labels: Vec<String>,
    /// Width of each column, the wider of its header and cells.
    widths: Vec<usize>,
    /// Peer names clipped to `NAME_MAX`, padded to the widest.
    name_w: usize,
    /// Spans of each row's cells, padded per column later.
    cells: Vec<Vec<Vec<(String, Style)>>>,
    /// Width of the version column, when the layout has one.
    ver_w: Option<usize>,
    /// Each row's version text, in row order.
    versions: Vec<(String, Style)>,
    host: Vec<HostArea>,
    host_w: [usize; 3],
}

impl<'a> Grid<'a> {
    fn new(
        matrix: &'a Matrix,
        order: &'a [usize],
        history: &History,
        label_max: usize,
        spark: bool,
        gauge: Option<GaugeMode>,
        ver: bool,
    ) -> Self {
        let labels: Vec<String> = matrix
            .columns
            .iter()
            .map(|c| clip(&c.upstream, label_max))
            .collect();
        let mode = gauge.unwrap_or_default();
        let peaks: Vec<u64> = (0..labels.len())
            .map(|c| column_peak(matrix, c, mode))
            .collect();
        let cells: Vec<Vec<Vec<(String, Style)>>> = order
            .iter()
            .map(|&i| {
                let row = &matrix.rows[i];
                row.cells
                    .iter()
                    .zip(&row.combos)
                    .enumerate()
                    .map(|(c, (cell, &n))| {
                        cell_spans(
                            cell,
                            n,
                            row.pressure.get(c).and_then(Option::as_ref),
                            peaks.get(c).copied().unwrap_or(0),
                            gauge,
                        )
                    })
                    .collect()
            })
            .collect();
        let widths = (0..labels.len())
            .map(|c| {
                let content = cells
                    .iter()
                    .filter_map(|row| row.get(c))
                    .map(|spans| spans.iter().map(|(text, _)| text_width(text)).sum())
                    .fold(0, usize::max);
                let width = content.max(text_width(&labels[c]));
                if gauge.is_some() {
                    width.max(GAUGE_WIDTH)
                } else {
                    width
                }
            })
            .collect();
        let name_w = order
            .iter()
            .map(|&i| text_width(&clip(&matrix.rows[i].peer, NAME_MAX)))
            .max()
            .unwrap_or(0);
        let versions: Vec<(String, Style)> = order
            .iter()
            .map(|&i| version_text(&matrix.rows[i]))
            .collect();
        let ver_w = ver.then(|| {
            versions
                .iter()
                .map(|(text, _)| text_width(text))
                .fold(text_width(VER_HEADER), usize::max)
        });
        let host: Vec<HostArea> = order
            .iter()
            .map(|&i| host_area(&matrix.rows[i], history, spark))
            .collect();
        let mut host_w = HOST_LABELS.map(str::len);
        for area in &host {
            if let HostArea::Figures(figures) = area {
                for (w, (text, _)) in host_w.iter_mut().zip(figures) {
                    *w = (*w).max(text_width(text));
                }
            }
        }
        Self {
            matrix,
            order,
            labels,
            widths,
            name_w,
            cells,
            ver_w,
            versions,
            host,
            host_w,
        }
    }

    fn width(&self) -> usize {
        let columns: usize = self.widths.iter().map(|w| GAP + w).sum();
        let ver = self.ver_w.map_or(0, |w| GAP + w);
        let host: usize = self.host_w.iter().map(|w| GAP + w).sum();
        2 + self.name_w + columns + ver + host
    }

    fn lines(&self, title: Vec<Span>, selected: Option<usize>, width: usize) -> Vec<Line> {
        let mut out = vec![line(title, width)];

        let mut head = vec![span(" ".repeat(2 + self.name_w), Style::Plain)];
        for (label, w) in self.labels.iter().zip(&self.widths) {
            head.push(span(GAP_TEXT, Style::Plain));
            head.push(span(pad(label, *w), Style::Title));
        }
        if let Some(w) = self.ver_w {
            head.push(span(GAP_TEXT, Style::Plain));
            head.push(span(pad(VER_HEADER, w), Style::Title));
        }
        for (label, w) in HOST_LABELS.iter().zip(self.host_w) {
            head.push(span(GAP_TEXT, Style::Plain));
            head.push(span(pad(label, w), Style::Title));
        }
        out.push(line(trim_end(head), width));

        for (pos, (&i, (cells, host))) in self
            .order
            .iter()
            .zip(self.cells.iter().zip(&self.host))
            .enumerate()
        {
            let is_selected = selected == Some(pos);
            let row = &self.matrix.rows[i];
            let mut spans = vec![
                marker(is_selected),
                span(
                    pad(&clip(&row.peer, NAME_MAX), self.name_w),
                    name_style(is_selected),
                ),
            ];
            for (parts, w) in cells.iter().zip(&self.widths) {
                spans.push(span(GAP_TEXT, Style::Plain));
                let mut used = 0;
                for (text, style) in parts {
                    if text.is_empty() {
                        continue;
                    }
                    used += text_width(text);
                    spans.push(span(text.clone(), *style));
                }
                if used < *w {
                    spans.push(span(" ".repeat(*w - used), Style::Plain));
                }
            }
            if let Some(w) = self.ver_w {
                let (text, style) = &self.versions[pos];
                spans.push(span(GAP_TEXT, Style::Plain));
                spans.push(span(pad(text, w), *style));
            }
            match host {
                HostArea::Stale(text, style) => {
                    spans.push(span(GAP_TEXT, Style::Plain));
                    spans.push(span(text.clone(), *style));
                }
                HostArea::Figures(figures) => {
                    for ((text, style), w) in figures.iter().zip(self.host_w) {
                        spans.push(span(GAP_TEXT, Style::Plain));
                        spans.push(span(pad(text, w), *style));
                    }
                }
            }
            out.push(line(trim_end(spans), width));
        }
        out
    }
}

/// A row's published version, clipped; `?` when the peer does not publish one.
fn version_text(row: &Row) -> (String, Style) {
    match &row.version {
        Some(version) => (clip(version, VER_MAX), Style::Plain),
        None => ("?".to_owned(), Style::Dim),
    }
}

/// What a row's host area shows: three figures, or the age of a stale or never-seen peer.
enum HostArea {
    Figures([(String, Style); 3]),
    Stale(String, Style),
}

fn host_area(row: &Row, history: &History, spark: bool) -> HostArea {
    if row.stale {
        let text = match row.age {
            Some(age) => format!("stale {}", fmt_duration(age)),
            None => "no data".to_owned(),
        };
        return HostArea::Stale(text, Style::Dim);
    }
    let load = match row.host.load1 {
        None => dash(),
        Some(load) => {
            let mut text = fmt_load(load);
            if spark {
                let levels = history.series(&row.peer, Metric::Load1);
                let tail = &levels[levels.len().saturating_sub(SPARK_LEN)..];
                if !tail.is_empty() {
                    text.push(' ');
                    text.extend(tail.iter().map(|&l| SPARK[l.min(7) as usize]));
                }
            }
            (text, Style::Plain)
        }
    };
    let disk = match row.host.disk_percent {
        None => dash(),
        Some(pct) => {
            let p = disk_percent(pct);
            let style = if p < 75 {
                Style::Good
            } else if p < 90 {
                Style::Warn
            } else {
                Style::Bad
            };
            let filled = (p as usize * 8 + 50) / 100;
            (format!("{p:>3}% {}", bar(filled, 8)), style)
        }
    };
    let sess = match row.host.sessions {
        None => dash(),
        Some(n) if n > 9999 => ("9999+".to_owned(), Style::Plain),
        Some(n) => (n.to_string(), Style::Plain),
    };
    HostArea::Figures([load, disk, sess])
}

fn dash() -> (String, Style) {
    ("-".to_owned(), Style::Dim)
}

/// Whole percent, clamped; NaN becomes 0 through the cast.
fn disk_percent(pct: f64) -> u32 {
    pct.round().clamp(0.0, 100.0) as u32
}

/// One load figure in at most four characters: a non-finite value shows as a dash.
fn fmt_load(load: f64) -> String {
    if !load.is_finite() {
        "-".to_owned()
    } else if load >= 1000.0 {
        "999+".to_owned()
    } else {
        format!("{load:.1}")
    }
}

/// Glyph, soonest cooldown and cooling combinations for one cell, and its colour.
fn cell_text(cell: &Cell, combos: u32) -> (String, Style) {
    let (glyph, soonest, style) = match cell {
        Cell::Absent => return ("·".to_owned(), Style::Dim),
        Cell::Stale => return ("?".to_owned(), Style::Dim),
        // A healthy key with a model cooling is rate limited for that model: not green.
        Cell::Healthy if combos > 0 => ("◐", None, Style::Warn),
        Cell::Healthy => ("■", None, Style::Good),
        Cell::Partial { soonest, .. } => ("◐", *soonest, Style::Warn),
        Cell::Cooling { soonest, .. } => ("■", *soonest, Style::Bad),
        Cell::Disabled => ("⊘", None, Style::Bad),
    };
    let mut text = glyph.to_owned();
    if let Some(d) = soonest {
        text.push(' ');
        text.push_str(&fmt_duration(d));
    }
    if combos > 0 {
        text.push_str(&format!("+{combos}"));
    }
    (text, style)
}

/// One cell split so the gauge can carry its own style after the state glyph. Absent and stale
/// cells stay a single mark: there is no traffic to draw.
fn cell_spans(
    cell: &Cell,
    combos: u32,
    pressure: Option<&Pressure>,
    scale: u64,
    mode: Option<GaugeMode>,
) -> Vec<(String, Style)> {
    let (text, style) = cell_text(cell, combos);
    let Some(mode) = mode else {
        return vec![(text, style)];
    };
    if matches!(cell, Cell::Absent | Cell::Stale) {
        return vec![(text, style)];
    }
    let mut chars = text.chars();
    let glyph = chars.next().map(String::from).unwrap_or_default();
    let rest: String = chars.collect();
    let (gauge, gauge_style) = gauge(pressure, scale, mode);
    let mut spans = vec![(glyph, style), (gauge, gauge_style)];
    if !rest.is_empty() {
        spans.push((rest, style));
    }
    spans
}

/// Drops the padding after the last column, so a line ends with its text.
fn trim_end(mut spans: Vec<Span>) -> Vec<Span> {
    while let Some(last) = spans.last_mut() {
        let kept = last.text.trim_end().len();
        last.text.truncate(kept);
        if !last.text.is_empty() {
            break;
        }
        spans.pop();
    }
    spans
}

fn marker(selected: bool) -> Span {
    span(if selected { "> " } else { "  " }, name_style(selected))
}

fn name_style(selected: bool) -> Style {
    if selected {
        Style::Title
    } else {
        Style::Plain
    }
}

/// The first `n` characters, never splitting one.
fn clip(text: &str, n: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        let w = char_width(c);
        if used + w > n {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

/// Pads with spaces to `width` characters; a longer text is returned as it is.
fn pad(text: &str, width: usize) -> String {
    let len = text_width(text);
    let mut out = text.to_owned();
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(len)));
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn control_characters_from_a_peer_never_reach_the_screen() {
        let spans = [span("evil\u{1b}[2J\u{7}name", Style::Plain)];
        assert_eq!(spans[0].text, "evil?[2J?name");
    }

    use super::*;
    use crate::matrix::{Cell, Column, History, HostSummary, Matrix, Row, Sample};
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

    /// Three peers with mismatched providers: peer-a zen only; peer-b openrouter (partial) and
    /// gemini; peer-c zen cooling and gemini with two cooling combinations.
    fn three_peers(t0: Instant) -> (Fleet, Vec<String>) {
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
             "soonest_cooldown_seconds":null,"last_status_class":"2xx"}],
            "host":{"load_average":[0.4,0.5,0.6],"disks":[{"used_percent":30.0}]},
            "sessions":{"active_sessions":1}}"#,
            t0,
        );
        add(
            &mut f,
            "peer-b",
            r#"{"instance":"peer-b","observed_at":"t","upstreams":[
            {"name":"openrouter","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,
             "soonest_cooldown_seconds":240,"last_status_class":"4xx"},
            {"name":"gemini","healthy_keys":2,"cooling_keys":0,"disabled_keys":0,
             "soonest_cooldown_seconds":null,"last_status_class":"2xx"}]}"#,
            t0,
        );
        add(
            &mut f,
            "peer-c",
            r#"{"instance":"peer-c","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,
             "soonest_cooldown_seconds":45,"last_status_class":"4xx"},
            {"name":"gemini","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
             "soonest_cooldown_seconds":null,"last_status_class":"2xx",
             "cooling_combinations":2}]}"#,
            t0,
        );
        (f, names(&["peer-a", "peer-b", "peer-c"]))
    }

    fn names(peers: &[&str]) -> Vec<String> {
        peers.iter().map(|p| (*p).to_owned()).collect()
    }

    fn matrix_lines(f: &Fleet, peers: &[String], t0: Instant, width: usize) -> Vec<Line> {
        let m = Matrix::build(f, peers, t0);
        render_matrix(&m, &History::new(0), None, width, None)
    }

    /// Char index of each occurrence of `needle` in `text`.
    fn positions(text: &str, needle: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(at) = text[from..].find(needle) {
            let byte = from + at;
            out.push(text[..byte].chars().count());
            from = byte + needle.len();
        }
        out
    }

    fn first(text: &str, needle: &str) -> usize {
        positions(text, needle)[0]
    }

    /// Two peers: one publishes its version, one is older and does not.
    fn versioned_fleet(t0: Instant) -> (Fleet, Vec<String>) {
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"peer-a","reiki_version":"0.10.3","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
             "soonest_cooldown_seconds":null,"last_status_class":"2xx"},
            {"name":"openrouter","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
             "soonest_cooldown_seconds":null,"last_status_class":"2xx"}]}"#,
            t0,
        );
        add(&mut f, "peer-b", BARE_DOC, t0);
        (f, names(&["peer-a", "peer-b"]))
    }

    #[test]
    fn the_card_header_shows_the_version_after_the_instance() {
        let t0 = Instant::now();
        let (f, _) = versioned_fleet(t0);
        let card = peer_detail(&f, "peer-a", t0, 100);
        assert_eq!(
            card[0].plain_text(),
            "peer-a  instance peer-a  v0.10.3",
            "{card:?}"
        );
        let bare = peer_detail(&f, "peer-b", t0, 100);
        assert!(!bare[0].plain_text().contains(" v"), "{bare:?}");
    }

    #[test]
    fn the_matrix_has_a_ver_column_between_providers_and_host_figures() {
        let t0 = Instant::now();
        let (f, peers) = versioned_fleet(t0);
        let lines = matrix_lines(&f, &peers, t0, 100);
        let header = lines[1].plain_text();
        let ver = first(&header, "Ver");
        assert!(first(&header, "openrouter") < ver, "{header:?}");
        assert!(ver < first(&header, "load"), "{header:?}");
        let rows: Vec<String> = lines[2..].iter().map(Line::plain_text).collect();
        let a = rows.iter().find(|r| r.contains("peer-a")).unwrap();
        let b = rows.iter().find(|r| r.contains("peer-b")).unwrap();
        assert_eq!(positions(a, "0.10.3"), vec![ver], "{a:?}");
        assert_eq!(b.chars().nth(ver), Some('?'), "an older peer: {b:?}");
    }

    #[test]
    fn the_ver_column_is_the_first_thing_dropped_when_narrow() {
        let t0 = Instant::now();
        let (f, peers) = versioned_fleet(t0);
        let narrowest = (20..=120)
            .find(|&w| {
                matrix_lines(&f, &peers, t0, w)[1]
                    .plain_text()
                    .contains("Ver")
            })
            .expect("the column fits somewhere");
        let next = matrix_lines(&f, &peers, t0, narrowest - 1);
        let header = next[1].plain_text();
        assert!(
            !header.contains("Ver"),
            "dropped one column narrower: {header:?}"
        );
        assert!(
            header.contains("openrouter"),
            "full provider names stay: {header:?}"
        );
        let gauge = next[2].plain_text();
        assert!(gauge.contains("······"), "the gauge stays: {gauge:?}");
    }

    #[test]
    fn matrix_title_tallies_count_only_peers_with_the_provider() {
        let t0 = Instant::now();
        let (f, peers) = three_peers(t0);
        let lines = matrix_lines(&f, &peers, t0, 100);
        assert_eq!(
            lines[0].plain_text(),
            "Fleet 3/3 fresh   gemini 2/2   zen 1/2   openrouter 0/1   [req 1h]"
        );
    }

    #[test]
    fn matrix_rows_are_problem_first_and_absent_cells_are_dots() {
        let t0 = Instant::now();
        let (f, peers) = three_peers(t0);
        let lines = matrix_lines(&f, &peers, t0, 100);
        let rows: Vec<String> = lines[2..].iter().map(Line::plain_text).collect();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with("  peer-b"), "{rows:?}");
        assert!(rows[1].starts_with("  peer-c"), "{rows:?}");
        assert!(rows[2].starts_with("  peer-a"), "{rows:?}");

        let header = lines[1].plain_text();
        let gemini = first(&header, "gemini");
        let zen = first(&header, "zen");
        let openrouter = first(&header, "openrouter");
        // Columns are left aligned, so every glyph sits under its provider's header.
        assert_eq!(positions(&rows[0], "■"), vec![gemini]);
        assert_eq!(positions(&rows[0], "◐"), vec![openrouter]);
        assert_eq!(positions(&rows[1], "■"), vec![zen]);
        assert_eq!(
            positions(&rows[1], "······+2"),
            vec![gemini + 2],
            "combinations suffix after the gauge"
        );
        assert_eq!(positions(&rows[1], "······ 45s"), vec![zen + 2]);
        let absent = |row: &str, at: usize| {
            row.chars().nth(at) == Some('·') && row.chars().nth(at + 1) != Some('·')
        };
        assert!(absent(&rows[0], zen), "absent zen: {:?}", rows[0]);
        for at in [gemini, openrouter] {
            assert!(absent(&rows[2], at), "absent at {at}: {:?}", rows[2]);
        }
        assert!(rows[0].contains("◐ ······ 4m00s"), "{:?}", rows[0]);
    }

    fn usage_doc(peer: &str, buckets: &str) -> String {
        let total: u64 = buckets
            .trim_matches(['[', ']'])
            .split(',')
            .map(|n| n.trim().parse().unwrap_or(0))
            .sum();
        format!(
            r#"{{"instance":"{peer}","observed_at":"t","upstreams":[
            {{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
             "soonest_cooldown_seconds":null,"last_status_class":"2xx",
             "requests_1h":{total},"limited_1h":0,"request_buckets":{buckets}}}]}}"#
        )
    }

    fn row_text(lines: &[Line], peer: &str) -> String {
        find(lines, &format!("  {peer} ")).plain_text()
    }

    #[test]
    fn gauges_are_six_cells_scaled_across_the_whole_fleet() {
        let t0 = Instant::now();
        let mut alone = fleet();
        add(
            &mut alone,
            "peer-q",
            &usage_doc("peer-q", "[30,30,30,30,30,30,30,30,30,30,30,30]"),
            t0,
        );
        let lines = matrix_lines(&alone, &names(&["peer-q"]), t0, 100);
        assert!(
            row_text(&lines, "peer-q").contains("■ ██████"),
            "alone, the busiest peer fills the gauge: {lines:?}"
        );

        let mut busy = fleet();
        add(
            &mut busy,
            "peer-q",
            &usage_doc("peer-q", "[30,30,30,30,30,30,30,30,30,30,30,30]"),
            t0,
        );
        add(
            &mut busy,
            "peer-b",
            &usage_doc("peer-b", "[60,60,60,60,60,60,60,60,60,60,60,60]"),
            t0,
        );
        let lines = matrix_lines(&busy, &names(&["peer-q", "peer-b"]), t0, 100);
        assert!(
            row_text(&lines, "peer-q").contains("■ ▄▄▄▄▄▄"),
            "the same count reads smaller beside a busier peer: {lines:?}"
        );
        assert!(
            row_text(&lines, "peer-b").contains("■ ██████"),
            "the busiest peer fills the gauge: {lines:?}"
        );
    }

    #[test]
    fn zero_buckets_and_missing_usage_draw_dots() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &usage_doc("peer-a", "[0,0,0,0,0,0,0,0,0,0,0,6]"),
            t0,
        );
        add(
            &mut f,
            "peer-b",
            &usage_doc("peer-b", "[0,0,0,0,0,0,0,0,0,0,0,0]"),
            t0,
        );
        add(&mut f, "peer-c", BARE_DOC, t0);
        let lines = matrix_lines(&f, &names(&["peer-a", "peer-b", "peer-c"]), t0, 100);
        assert!(row_text(&lines, "peer-a").contains("■ ·····█"), "{lines:?}");
        assert!(
            row_text(&lines, "peer-b").contains("■ ······"),
            "zero traffic stays blank: {lines:?}"
        );
        assert!(
            row_text(&lines, "peer-c").contains("■ ······"),
            "an older publisher reads as no figures, not an error: {lines:?}"
        );
    }

    #[test]
    fn a_limited_gauge_is_drawn_in_the_warning_style() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                 "requests_1h":8,"limited_1h":2,
                 "request_buckets":[1,1,1,1,1,1,1,1,1,1,1,1]}]}"#,
            t0,
        );
        let lines = matrix_lines(&f, &names(&["peer-a"]), t0, 100);
        let line = find(&lines, "  peer-a ");
        assert!(
            line.spans
                .iter()
                .any(|s| s.style == Style::Warn && s.text.trim() == "██████"),
            "limited traffic warns: {line:?}"
        );
    }

    #[test]
    fn the_detail_card_lists_the_last_hour_with_a_twelve_cell_trend() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                 "requests_1h":120,"limited_1h":3,
                 "request_buckets":[0,0,0,0,0,0,0,0,0,0,0,3]}]}"#,
            t0,
        );
        let card = peer_detail(&f, "peer-a", t0, 100);
        let line = find(&card, "last hour:");
        let text = line.plain_text();
        assert!(
            text.contains("last hour: 120 requests, 3 limited"),
            "{text}"
        );
        assert!(text.contains("···········█"), "twelve cells: {text}");
    }

    #[test]
    fn the_gauge_is_dropped_before_provider_headers_are_shortened() {
        let t0 = Instant::now();
        let (f, peers) = three_peers(t0);
        let wide = matrix_lines(&f, &peers, t0, 100);
        assert!(wide[1].plain_text().contains("openrouter"));
        assert!(
            wide[2..].iter().any(|l| l.plain_text().contains("······")),
            "a wide matrix draws gauges: {wide:?}"
        );

        let medium = matrix_lines(&f, &peers, t0, 64);
        assert!(
            medium[1].plain_text().contains("openrouter"),
            "headers keep their full names when the gauge goes: {medium:?}"
        );
        assert!(
            !medium[2..]
                .iter()
                .any(|l| l.plain_text().contains("······")),
            "the gauge is gone: {medium:?}"
        );
    }

    #[test]
    fn every_cell_state_has_its_glyph_text_and_colour() {
        let cells = [
            (Cell::Healthy, 0, "■", Style::Good),
            (Cell::Healthy, 2, "◐+2", Style::Warn),
            (
                Cell::Partial {
                    cooling: 1,
                    total: 3,
                    soonest: Some(Duration::from_secs(240)),
                },
                2,
                "◐ 4m00s+2",
                Style::Warn,
            ),
            (
                Cell::Cooling {
                    soonest: Some(Duration::from_secs(45)),
                    class: None,
                },
                0,
                "■ 45s",
                Style::Bad,
            ),
            (
                Cell::Cooling {
                    soonest: None,
                    class: Some("rate_limited".to_owned()),
                },
                0,
                "■",
                Style::Bad,
            ),
            (Cell::Disabled, 0, "⊘", Style::Bad),
            (Cell::Absent, 3, "·", Style::Dim),
            (Cell::Stale, 4, "?", Style::Dim),
        ];
        for (cell, combos, text, style) in cells {
            assert_eq!(
                cell_text(&cell, combos),
                (text.to_owned(), style),
                "{cell:?}"
            );
        }
    }

    #[test]
    fn a_stale_row_shows_its_age_and_a_never_seen_peer_says_no_data() {
        let row = |peer: &str, age: Option<Duration>| Row {
            peer: peer.to_owned(),
            cells: vec![Cell::Stale],
            combos: vec![0],
            pressure: vec![None],
            host: HostSummary::default(),
            version: None,
            error: Some("poll: cannot connect".to_owned()),
            stale: true,
            age,
        };
        let m = Matrix {
            hidden_columns: 0,
            columns: vec![Column {
                upstream: "zen".to_owned(),
                peers_with: 1,
                healthy_peers: 0,
            }],
            rows: vec![
                row("host-1", Some(Duration::from_secs(120))),
                row("edge-1", None),
            ],
        };
        let lines = render_matrix(&m, &History::new(0), None, 100, None);
        let host_1 = lines[3].plain_text();
        assert!(host_1.contains("stale 2m00s"), "{host_1}");
        assert!(!host_1.contains("poll"), "errors stay in the detail pane");
        assert!(
            lines[2].plain_text().contains("no data"),
            "edge-1 sorts first"
        );
        let stale = lines[3]
            .spans
            .iter()
            .find(|s| s.text.starts_with("stale"))
            .unwrap();
        assert_eq!(stale.style, Style::Dim);
    }

    #[test]
    fn narrow_widths_shorten_headers_then_fall_back_to_chips() {
        let t0 = Instant::now();
        let (f, peers) = three_peers(t0);
        let at_60 = matrix_lines(&f, &peers, t0, 60);
        assert!(at_60[1].plain_text().contains("openro"), "{:?}", at_60[1]);
        assert!(!at_60[1].plain_text().contains("openrouter"));

        let at_30 = matrix_lines(&f, &peers, t0, 30);
        assert_eq!(at_30.len(), 4, "fleet line plus one chip line per peer");
        assert!(at_30[1].plain_text().starts_with("  peer-b  gemini ■"));
        assert!(at_30[3]
            .plain_text()
            .starts_with("  peer-a  zen ■  load 0.4"));
        for line in &at_30 {
            assert!(line.plain_text().chars().count() <= 30);
        }
    }

    #[test]
    fn tiny_widths_never_overflow_or_panic() {
        let t0 = Instant::now();
        let (f, peers) = three_peers(t0);
        for width in [0, 1, 19] {
            let lines = matrix_lines(&f, &peers, t0, width);
            for line in &lines {
                assert!(line.plain_text().chars().count() <= width, "width {width}");
            }
        }
        let lines = matrix_lines(&f, &peers, t0, 19);
        assert_eq!(lines.len(), 4, "fleet line and names only");
        assert_eq!(lines[1].plain_text(), "  peer-b");
        assert!(matrix_lines(&f, &peers, t0, 0)
            .iter()
            .all(|l| l.plain_text().is_empty()));
    }

    #[test]
    fn a_sparkline_follows_the_history_and_the_disk_bar_shows_the_percent() {
        let t0 = Instant::now();
        let (f, peers) = three_peers(t0);
        let m = Matrix::build(&f, &peers, t0);
        let mut history = History::new(60);
        for load in [0.0_f64, 0.5, 1.0] {
            history.push(
                "peer-a",
                Sample {
                    load1: Some(load),
                    ..Sample::default()
                },
            );
        }
        let lines = render_matrix(&m, &history, None, 100, None);
        let a = lines[4].plain_text();
        assert!(a.contains("0.4 ▁▅█"), "{a}");
        assert!(a.contains(" 30% ██░░░░░░"), "{a}");
        assert!(lines[2].plain_text().contains("-"), "b has no load");
    }

    #[test]
    fn disk_bars_are_coloured_by_threshold() {
        let disk = |pct: f64| Row {
            peer: "peer-a".to_owned(),
            cells: vec![],
            combos: vec![],
            pressure: vec![],
            host: HostSummary {
                load1: None,
                disk_percent: Some(pct),
                sessions: None,
            },
            version: None,
            error: None,
            stale: false,
            age: Some(Duration::ZERO),
        };
        let m = Matrix {
            hidden_columns: 0,
            columns: vec![],
            rows: vec![disk(53.0), disk(80.0), disk(95.0), disk(100.0)],
        };
        let lines = render_matrix(&m, &History::new(0), None, 100, None);
        for (line, (text, style)) in lines[2..].iter().zip([
            ("53% ████░░░░", Style::Good),
            ("80% ██████░░", Style::Warn),
            ("95% ████████", Style::Bad),
            ("100% ████████", Style::Bad),
        ]) {
            let span = line
                .spans
                .iter()
                .find(|s| {
                    s.text
                        .trim_start()
                        .starts_with(text.split(' ').next().unwrap())
                })
                .unwrap_or_else(|| panic!("{text} in {line:?}"));
            assert_eq!(span.text.trim_start(), text, "{line:?}");
            assert_eq!(span.style, style, "{text}");
        }
    }

    #[test]
    fn hostile_names_are_sanitised_clipped_and_bounded() {
        let t0 = Instant::now();
        let mut f = fleet();
        let long = "n".repeat(80);
        add(
            &mut f,
            &format!("p\u{1b}[2J{long}"),
            r#"{"instance":"x","observed_at":"t","upstreams":[
            {"name":"zé\u0007","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
             "soonest_cooldown_seconds":null,"last_status_class":null},
            {"name":"名前名前名前名前名前名前名前名前名前名前名前名前名前名前名前名前名前名前","healthy_keys":1,
             "cooling_keys":0,"disabled_keys":0,"soonest_cooldown_seconds":null,
             "last_status_class":null}]}"#,
            t0,
        );
        let peers = vec![format!("p\u{1b}[2J{long}")];
        for width in [0, 1, 5, 19, 20, 30, 60, 100, 200] {
            for line in matrix_lines(&f, &peers, t0, width) {
                let text = line.plain_text();
                assert!(!text.contains('\u{1b}'), "{text:?}");
                assert!(text.chars().count() <= width, "width {width}: {text:?}");
            }
        }
        let wide = matrix_lines(&f, &peers, t0, 100);
        let text = wide[2].plain_text();
        assert!(text.contains("p?[2J"), "{text:?}");
        // The escape's five characters count toward the sixteen-character name limit.
        assert!(
            text.contains(&format!("p?[2J{}", "n".repeat(11))),
            "{text:?}"
        );
        assert!(
            !text.contains(&"n".repeat(12)),
            "name clipped to 16: {text:?}"
        );
        assert!(wide[1].plain_text().contains("zé?"), "{:?}", wide[1]);
    }

    #[test]
    fn huge_and_nan_host_numbers_stay_short() {
        let row = |load: f64, sessions: u64| Row {
            peer: "peer-a".to_owned(),
            cells: vec![],
            combos: vec![],
            pressure: vec![],
            host: HostSummary {
                load1: Some(load),
                disk_percent: Some(100.0),
                sessions: Some(sessions),
            },
            version: None,
            error: None,
            stale: false,
            age: Some(Duration::ZERO),
        };
        let m = Matrix {
            hidden_columns: 0,
            columns: vec![],
            rows: vec![row(1e308, u64::MAX), row(f64::NAN, 3)],
        };
        let lines = render_matrix(&m, &History::new(0), None, 100, None);
        let first_row = lines[2].plain_text();
        assert!(first_row.contains("999+"), "{first_row}");
        assert!(first_row.contains("9999+"), "{first_row}");
        let second = lines[3].plain_text();
        assert!(second.contains("  -  "), "NaN load shows a dash: {second}");
        assert!(second.trim_end().ends_with('3'), "{second}");
    }

    #[test]
    fn selection_marks_one_row_and_wraps_past_the_end() {
        let t0 = Instant::now();
        let (f, peers) = three_peers(t0);
        let m = Matrix::build(&f, &peers, t0);
        let lines = render_matrix(&m, &History::new(0), Some(4), 100, None);
        let marked: Vec<&Line> = lines
            .iter()
            .filter(|l| l.plain_text().starts_with("> "))
            .collect();
        assert_eq!(marked.len(), 1, "exactly one marked row");
        assert!(
            marked[0].plain_text().starts_with("> peer-c"),
            "4 wraps to 1"
        );
    }

    #[test]
    fn the_detail_card_names_the_soonest_cooling_class() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &keyed_doc(Some(16260), Some("quota_exhausted")),
            t0,
        );
        let card = peer_detail(&f, "peer-a", t0, 100);
        assert!(
            texts(&card)
                .iter()
                .any(|l| l
                    .starts_with("  zen: 0 healthy  1 cooling  0 disabled  class quota_exhausted")),
            "{:?}",
            texts(&card)
        );
    }

    #[test]
    fn matrix_lines_end_with_their_text_not_padding() {
        let t0 = Instant::now();
        let (f, peers) = three_peers(t0);
        for line in matrix_lines(&f, &peers, t0, 100).iter().skip(1) {
            let text = line.plain_text();
            assert_eq!(text, text.trim_end(), "{text:?}");
        }
    }

    #[test]
    fn a_row_with_fewer_combos_than_cells_draws_zero_for_the_rest() {
        let row = Row {
            peer: "peer-a".to_owned(),
            cells: vec![Cell::Healthy, Cell::Healthy],
            combos: vec![3],
            pressure: vec![None],
            host: HostSummary::default(),
            version: None,
            error: None,
            stale: false,
            age: Some(Duration::ZERO),
        };
        let m = Matrix {
            hidden_columns: 0,
            columns: vec![
                Column {
                    upstream: "zen".to_owned(),
                    peers_with: 1,
                    healthy_peers: 1,
                },
                Column {
                    upstream: "gem".to_owned(),
                    peers_with: 1,
                    healthy_peers: 1,
                },
            ],
            rows: vec![row],
        };
        let lines = render_matrix(&m, &History::new(0), None, 100, None);
        assert!(lines[2].plain_text().contains("◐ ······+3"));
        let narrow = render_matrix(&m, &History::new(0), None, 25, None);
        assert!(narrow[1].plain_text().contains("zen ◐+3"), "{narrow:?}");
    }

    #[test]
    fn no_peers_draw_only_the_fleet_line() {
        let m = Matrix {
            hidden_columns: 0,
            columns: vec![],
            rows: vec![],
        };
        let lines = render_matrix(&m, &History::new(0), Some(5), 100, None);
        assert_eq!(texts(&lines), ["Fleet 0/0 fresh   [req 1h]"]);
    }

    #[test]
    fn a_filtered_matrix_says_filter_and_hides_absent_peers() {
        let t0 = Instant::now();
        let (f, peers) = three_peers(t0);
        let m = Matrix::build(&f, &peers, t0)
            .filter_provider("zen")
            .unwrap();
        let lines = render_matrix(&m, &History::new(0), None, 100, Some("zen"));
        assert!(
            lines[0].plain_text().contains("filter: zen"),
            "{:?}",
            lines[0]
        );
        let rows: Vec<String> = lines[2..].iter().map(Line::plain_text).collect();
        assert_eq!(rows.len(), 2, "peer-b has no zen: {rows:?}");
        assert!(rows.iter().any(|r| r.contains("peer-a")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("peer-c")), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("peer-b")), "{rows:?}");
    }

    #[test]
    fn fleet_line_alerts_when_every_peer_with_a_provider_is_cooling_or_disabled() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,
             "soonest_cooldown_seconds":4800,"last_status_class":"4xx"}]}"#,
            t0,
        );
        add(
            &mut f,
            "peer-b",
            r#"{"instance":"peer-b","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":0,"cooling_keys":0,"disabled_keys":1,
             "soonest_cooldown_seconds":null,"last_status_class":"4xx"}]}"#,
            t0,
        );
        let peers = names(&["peer-a", "peer-b"]);
        let m = Matrix::build(&f, &peers, t0);
        let lines = render_matrix(&m, &History::new(0), None, 100, None);
        let title = lines[0].plain_text();
        assert!(
            title.contains("ALERT zen all cooling, soonest 1h20m"),
            "{title}"
        );
        let alert = lines[0]
            .spans
            .iter()
            .find(|s| s.text.contains("ALERT zen"))
            .expect("alert span");
        assert_eq!(alert.style, Style::Bad);
    }

    #[test]
    fn fleet_line_has_no_alert_for_mixed_or_stale_providers() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
            {"name":"mix","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,
             "soonest_cooldown_seconds":60,"last_status_class":"4xx"},
            {"name":"stale","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,
             "soonest_cooldown_seconds":60,"last_status_class":"4xx"}]}"#,
            t0,
        );
        add(
            &mut f,
            "peer-b",
            r#"{"instance":"peer-b","observed_at":"t","upstreams":[
            {"name":"stale","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,
             "soonest_cooldown_seconds":30,"last_status_class":"4xx"}]}"#,
            t0 - Duration::from_secs(120),
        );
        let peers = names(&["peer-a", "peer-b"]);
        let m = Matrix::build(&f, &peers, t0);
        let lines = render_matrix(&m, &History::new(0), None, 100, None);
        let title = lines[0].plain_text();
        assert!(!title.contains("ALERT mix"), "{title}");
        assert!(
            !title.contains("ALERT stale"),
            "stale peer counts as unknown: {title}"
        );
    }

    #[test]
    fn fleet_line_caps_alerts_at_three_and_hides_them_when_filtered() {
        let t0 = Instant::now();
        let mut f = fleet();
        for (i, name) in ["a", "b", "c", "d"].iter().enumerate() {
            add(
                &mut f,
                &format!("peer-{name}"),
                &format!(
                    r#"{{"instance":"peer-{name}","observed_at":"t","upstreams":[
                    {{"name":"p0","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,"soonest_cooldown_seconds":60,"last_status_class":"4xx"}},
                    {{"name":"p1","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,"soonest_cooldown_seconds":60,"last_status_class":"4xx"}},
                    {{"name":"p2","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,"soonest_cooldown_seconds":60,"last_status_class":"4xx"}},
                    {{"name":"p3","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,"soonest_cooldown_seconds":60,"last_status_class":"4xx"}}]}}"#,
                ),
                t0,
            );
            let _ = i;
        }
        let peers = names(&["peer-a", "peer-b", "peer-c", "peer-d"]);
        let m = Matrix::build(&f, &peers, t0);
        let lines = render_matrix(&m, &History::new(0), None, 200, None);
        let title = lines[0].plain_text();
        assert_eq!(title.matches("ALERT ").count(), 3, "{title}");
        assert!(title.contains("+1 more"), "{title}");
        let fm = m.filter_provider("p0").unwrap();
        let filtered = render_matrix(&fm, &History::new(0), None, 200, Some("p0"));
        assert!(
            !filtered[0].plain_text().contains("ALERT"),
            "hidden by filter: {:?}",
            filtered[0]
        );
    }

    #[test]
    fn fleet_line_alerts_survive_narrow_widths() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,
             "soonest_cooldown_seconds":4800,"last_status_class":"4xx"}]}"#,
            t0,
        );
        let peers = names(&["peer-a"]);
        let m = Matrix::build(&f, &peers, t0);
        for width in [0, 1, 19, 30, 60, 100] {
            let lines = render_matrix(&m, &History::new(0), None, width, None);
            for l in &lines {
                assert!(text_width(&l.plain_text()) <= width, "width {width}: {l:?}");
            }
        }
    }

    #[test]
    fn the_detail_card_is_the_full_views_card_for_that_peer() {
        let t0 = Instant::now();
        let (f, _) = three_peers(t0);
        let all = render(&f, &names(&["peer-b"]), t0, 100);
        let card = peer_detail(&f, "peer-b", t0, 100);
        assert!(
            all.ends_with(&card),
            "the card is the tail of the full view"
        );
        assert!(card[0].plain_text().starts_with("peer-b  instance"));
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
            assert!(text_width(&l.plain_text()) <= 5);
        }
        // The crab is two columns wide and only one is left after `pr-é`, so it is dropped,
        // never split.
        assert_eq!(find(&lines, "é").plain_text(), "pr-é");
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

    #[test]
    fn bidi_and_zero_width_characters_in_names_are_replaced() {
        let mut f = fleet();
        let t0 = Instant::now();
        let doc = r#"{"instance":"x","observed_at":"t","upstreams":[{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0}]}"#;
        add(&mut f, "peer\u{202E}evil\u{200B}\u{2066}x\u{FEFF}", doc, t0);
        let text = texts(&matrix_lines(
            &f,
            &names(&["peer\u{202E}evil\u{200B}\u{2066}x\u{FEFF}"]),
            t0,
            100,
        ))
        .join("\n");
        for bad in ['\u{202E}', '\u{200B}', '\u{2066}', '\u{FEFF}'] {
            assert!(!text.contains(bad), "{bad:?} leaked: {text:?}");
        }
        assert!(text.contains("peer?evil"));
    }

    #[test]
    fn wide_names_clip_and_align_by_display_width() {
        let mut f = fleet();
        let t0 = Instant::now();
        let doc = r#"{"instance":"x","observed_at":"t","upstreams":[{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0}]}"#;
        let wide = "\u{5BB6}".repeat(12);
        add(&mut f, &wide, doc, t0);
        add(&mut f, "peer-b", doc, t0);
        for width in [20, 40, 100] {
            let lines = matrix_lines(&f, &names(&[&wide, "peer-b"]), t0, width);
            for l in &lines {
                assert!(
                    text_width(&l.plain_text()) <= width,
                    "width {width}: {:?}",
                    l.plain_text()
                );
            }
        }
        let lines = texts(&matrix_lines(&f, &names(&[&wide, "peer-b"]), t0, 100));
        let cell_col: Vec<usize> = lines
            .iter()
            .skip(2)
            .filter_map(|l| l.find('\u{25A0}').map(|b| text_width(&l[..b])))
            .collect();
        assert_eq!(cell_col.len(), 2);
        assert_eq!(cell_col[0], cell_col[1], "cells line up: {lines:?}");
    }

    #[test]
    fn the_window_keeps_the_header_and_the_selected_row_in_view() {
        let rows = 20;
        let mk = |n: usize| -> Vec<Line> {
            (0..n)
                .map(|i| line(vec![span(format!("L{i}"), Style::Plain)], 20))
                .collect()
        };
        let plain =
            |lines: &[Line]| -> Vec<String> { lines.iter().map(Line::plain_text).collect() };
        // two pinned lines then 20 rows: L0 L1 pinned, rows are L2..L21
        let win = plain(&window_matrix(mk(rows + 2), rows, Some(15), 10));
        assert_eq!(win.len(), 10);
        assert_eq!(&win[..2], ["L0", "L1"], "header stays");
        assert!(
            win.contains(&"L17".to_owned()),
            "row 15 is line 17: {win:?}"
        );
        let top = plain(&window_matrix(mk(rows + 2), rows, Some(0), 10));
        assert_eq!(top[2], "L2");
        let all = window_matrix(mk(5), 3, Some(2), 40);
        assert_eq!(all.len(), 5, "everything fits");
        assert_eq!(window_matrix(mk(rows + 2), rows, Some(3), 2).len(), 2);
        assert!(window_matrix(mk(rows + 2), rows, Some(3), 0).is_empty());
        let past = plain(&window_matrix(mk(rows + 2), rows, Some(999), 10));
        assert_eq!(
            past.last().unwrap(),
            "L21",
            "a stale selection clamps to the last row"
        );
    }

    #[test]
    fn a_healthy_key_with_a_model_cooling_is_a_warning_not_green() {
        let (text, style) = cell_text(&Cell::Healthy, 2);
        assert_eq!(text, "◐+2");
        assert_eq!(style, Style::Warn);
        let (text, style) = cell_text(&Cell::Healthy, 0);
        assert_eq!((text.as_str(), style), ("■", Style::Good));
    }

    #[test]
    fn the_card_says_when_models_are_cooling_and_the_last_status_was_not_ok() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
            {"name":"openrouter","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
             "cooling_combinations":1,"last_status_class":"4xx"}]}"#,
            t0,
        );
        let card = texts(&peer_detail(&f, "peer-a", t0, 100));
        assert!(
            card.iter().any(|l| l
                == "  openrouter: 1 healthy  0 cooling  0 disabled  1 model cooling  last 4xx"),
            "{card:?}"
        );
    }

    /// One peer with every figure, so each metric and window draws its own gauge.
    fn gauge_doc() -> String {
        r#"{"instance":"peer-a","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
             "requests_1h":6,"limited_1h":0,
             "request_buckets":[0,0,0,0,0,0,0,0,0,0,0,6],
             "request_minutes":[9,9,9,9,9,0,1,2,4,8],
             "tokens_1h":10,"token_buckets":[10,0,0,0,0,0,0,0,0,0,0,0],
             "token_minutes":[4,4,4,4,4,4,4,4,4,4]}]}"#
            .to_owned()
    }

    fn gauge_row(mode: GaugeMode) -> (String, String) {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &gauge_doc(), t0);
        let m = Matrix::build(&f, &names(&["peer-a"]), t0);
        let lines = render_matrix_mode(&m, &History::new(0), None, 100, None, mode);
        (lines[0].plain_text(), row_text(&lines, "peer-a"))
    }

    #[test]
    fn the_gauge_metric_and_window_choose_which_figures_are_drawn() {
        let (title, row) = gauge_row(GaugeMode::default());
        assert!(row.contains("■ ·····█"), "requests over the hour: {row:?}");
        assert!(title.ends_with("[req 1h]"), "{title:?}");

        let tokens = GaugeMode {
            metric: GaugeMetric::Tokens,
            window: GaugeWindow::Hour,
        };
        let (title, row) = gauge_row(tokens);
        assert!(row.contains("■ █·····"), "tokens over the hour: {row:?}");
        assert!(title.ends_with("[tok 1h]"), "{title:?}");

        let minutes = GaugeMode {
            metric: GaugeMetric::Requests,
            window: GaugeWindow::Minutes,
        };
        let (title, row) = gauge_row(minutes);
        assert!(
            row.contains("■ ·▁▂▄█"),
            "the newest five minutes, one cell each: {row:?}"
        );
        assert!(title.ends_with("[req 5m]"), "{title:?}");

        let tokens_minutes = GaugeMode {
            metric: GaugeMetric::Tokens,
            window: GaugeWindow::Minutes,
        };
        let (title, row) = gauge_row(tokens_minutes);
        assert!(row.contains("■ █████"), "{row:?}");
        assert!(title.ends_with("[tok 5m]"), "{title:?}");
    }

    #[test]
    fn the_minute_gauge_is_padded_to_the_hour_gauge_width() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &gauge_doc(), t0);
        add(
            &mut f,
            "peer-b",
            &usage_doc("peer-b", "[1,1,1,1,1,1,1,1,1,1,1,1]"),
            t0,
        );
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b"]), t0);
        let layout = |mode: GaugeMode| {
            let lines = render_matrix_mode(&m, &History::new(0), None, 100, None, mode);
            let header = lines[1].plain_text();
            let row = row_text(&lines, "peer-a");
            (first(&header, "load"), row.chars().count())
        };
        let hour = layout(GaugeMode::default());
        let minutes = layout(GaugeMode {
            metric: GaugeMetric::Requests,
            window: GaugeWindow::Minutes,
        });
        assert_eq!(
            hour, minutes,
            "the host columns sit where the hour gauge put them"
        );
    }

    #[test]
    fn a_peer_without_the_needed_array_draws_dots_not_an_error() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &usage_doc("peer-a", "[0,0,0,0,0,0,0,0,0,0,0,6]"),
            t0,
        );
        let minutes = GaugeMode {
            metric: GaugeMetric::Requests,
            window: GaugeWindow::Minutes,
        };
        let m = Matrix::build(&f, &names(&["peer-a"]), t0);
        let lines = render_matrix_mode(&m, &History::new(0), None, 100, None, minutes);
        assert!(row_text(&lines, "peer-a").contains("■ ·····"), "{lines:?}");
        let tokens = GaugeMode {
            metric: GaugeMetric::Tokens,
            window: GaugeWindow::Hour,
        };
        let lines = render_matrix_mode(&m, &History::new(0), None, 100, None, tokens);
        assert!(row_text(&lines, "peer-a").contains("■ ······"), "{lines:?}");
    }

    #[test]
    fn the_fleet_scale_follows_the_chosen_window() {
        let t0 = Instant::now();
        let minute_doc = |minutes: &str| {
            format!(
                r#"{{"instance":"x","observed_at":"t","upstreams":[
                {{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                 "requests_1h":1,"limited_1h":0,"request_minutes":{minutes}}}]}}"#
            )
        };
        let mut f = fleet();
        add(&mut f, "peer-q", &minute_doc("[0,0,0,0,0,3,3,3,3,3]"), t0);
        add(&mut f, "peer-b", &minute_doc("[0,0,0,0,0,6,6,6,6,6]"), t0);
        let minutes = GaugeMode {
            metric: GaugeMetric::Requests,
            window: GaugeWindow::Minutes,
        };
        let m = Matrix::build(&f, &names(&["peer-q", "peer-b"]), t0);
        let lines = render_matrix_mode(&m, &History::new(0), None, 100, None, minutes);
        assert!(row_text(&lines, "peer-q").contains("■ ▄▄▄▄▄"), "{lines:?}");
        assert!(row_text(&lines, "peer-b").contains("■ █████"), "{lines:?}");
    }

    /// One peer with one provider and one cooling key.
    fn cooling_doc(
        peer: &str,
        provider: &str,
        cooldown: Option<u64>,
        class: Option<&str>,
        last: &str,
    ) -> String {
        let class = class
            .map(|c| format!(r#","cooldown_class":"{c}""#))
            .unwrap_or_default();
        let secs = cooldown.map_or("null".to_owned(), |s| s.to_string());
        format!(
            r#"{{"instance":"{peer}","observed_at":"t","upstreams":[
            {{"name":"{provider}","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,
             "soonest_cooldown_seconds":{secs},"last_status_class":"{last}",
             "keys":[{{"name":"k","state":"cooling","cooldown_seconds":{secs}{class}}}]}}]}}"#
        )
    }

    #[test]
    fn no_cooling_keys_draw_no_cooling_panel() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", BARE_DOC, t0);
        assert!(cooling_panel(&f, &names(&["peer-a"]), None, t0, 100).is_empty());
    }

    #[test]
    fn a_stale_peer_with_cooling_keys_draws_no_cooling_panel() {
        let t0 = Instant::now();
        let mut f = fleet();
        let doc = cooling_doc("peer-a", "zen", Some(60), Some("rate_limited"), "4xx");
        add(&mut f, "peer-a", &doc, t0);
        let later = t0 + Duration::from_secs(200);
        assert!(cooling_panel(&f, &names(&["peer-a"]), None, later, 100).is_empty());
    }

    #[test]
    fn the_cooling_panel_lists_soonest_first_with_drain_time_and_last_class() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &cooling_doc("peer-a", "zen", Some(3600), Some("quota_exhausted"), "4xx"),
            t0,
        );
        add(
            &mut f,
            "peer-b",
            &cooling_doc("peer-b", "gem", Some(45), Some("rate_limited"), "5xx"),
            t0,
        );
        let lines = texts(&cooling_panel(
            &f,
            &names(&["peer-a", "peer-b"]),
            None,
            t0,
            120,
        ));
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], "Cooling");
        assert!(
            lines[1].contains("peer-b")
                && lines[1].contains("gem")
                && lines[1].contains("◐ cooling")
                && lines[1].contains(&format!("[{}{}]", "█".repeat(15), "░".repeat(5)))
                && lines[1].contains("45s")
                && lines[1].contains("last 5xx"),
            "rate limited 45s of 60s: {lines:?}"
        );
        assert!(
            lines[2].contains("peer-a")
                && lines[2].contains("zen")
                && lines[2].contains(&format!("[{}{}]", "█".repeat(4), "░".repeat(16)))
                && lines[2].contains("1h00m")
                && lines[2].contains("last 4xx"),
            "quota 1h of 5h: {lines:?}"
        );
    }

    #[test]
    fn the_cooling_panel_caps_at_six_lines_then_counts_the_rest() {
        let t0 = Instant::now();
        let mut f = fleet();
        let peers: Vec<String> = (0..8).map(|i| format!("peer-{i}")).collect();
        for (i, p) in peers.iter().enumerate() {
            let doc = cooling_doc(p, "zen", Some(10 + i as u64), None, "4xx");
            add(&mut f, p, &doc, t0);
        }
        let lines = texts(&cooling_panel(&f, &peers, None, t0, 100));
        assert_eq!(lines.len(), 1 + 6 + 1, "{lines:?}");
        assert!(lines[1].contains("peer-0"), "{lines:?}");
        assert!(lines[6].contains("peer-5"), "{lines:?}");
        assert_eq!(lines[7].trim(), "+2 more");
    }

    #[test]
    fn the_cooling_panel_follows_the_provider_filter() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &cooling_doc("peer-a", "zen", Some(30), None, "4xx"),
            t0,
        );
        add(
            &mut f,
            "peer-b",
            &cooling_doc("peer-b", "gem", Some(40), None, "4xx"),
            t0,
        );
        let peers = names(&["peer-a", "peer-b"]);
        let lines = texts(&cooling_panel(&f, &peers, Some("gem"), t0, 100));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[1].contains("peer-b"), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("zen")), "{lines:?}");
    }

    #[test]
    fn a_cooling_key_without_a_known_time_draws_a_full_bar_and_a_question_mark() {
        let t0 = Instant::now();
        let mut f = fleet();
        let doc = cooling_doc("peer-a", "zen", None, None, "4xx");
        add(&mut f, "peer-a", &doc, t0);
        let lines = texts(&cooling_panel(&f, &names(&["peer-a"]), None, t0, 100));
        assert!(
            lines[1].contains(&format!("[{}]", "█".repeat(20)))
                && lines[1].contains("  ?  last 4xx"),
            "{lines:?}"
        );
    }

    #[test]
    fn the_cooling_panel_clips_to_narrow_widths_without_panicking() {
        let t0 = Instant::now();
        let mut f = fleet();
        let doc = cooling_doc("peer-a", "zen", Some(90), Some("transient"), "5xx");
        add(&mut f, "peer-a", &doc, t0);
        let peers = names(&["peer-a"]);
        for width in [0, 1, 2, 5, 20, 40] {
            for line in cooling_panel(&f, &peers, None, t0, width) {
                assert!(
                    line.plain_text().chars().count() <= width,
                    "width {width}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn a_card_shows_failure_counts_and_only_the_local_instance_gets_rows() {
        let t0 = Instant::now();
        let mut f = fleet();
        let doc = r#"{"instance":"peer-a","observed_at":"t","upstreams":[],
            "failures":{"rate_limited":2,"transient":1}}"#;
        add(&mut f, "peer-a", doc, t0);
        let remote = texts(&peer_detail(&f, "peer-a", t0, 100));
        assert!(
            remote.iter().any(|l| l.contains("rate_limited 2")),
            "{remote:?}"
        );
        assert!(!remote.iter().any(|l| l.contains("cooled")), "{remote:?}");

        f.set_local_failures(
            "peer-a",
            vec![crate::Failure {
                time: 3_600,
                stream: "s".to_owned(),
                key: "k".to_owned(),
                status: Some(429),
                class: "rate_limited".to_owned(),
                retry_after: Some("120".to_owned()),
                cooled: true,
                cooling_seconds: Some(30),
                note: "body".to_owned(),
            }],
        );
        let local = texts(&peer_detail(&f, "peer-a", t0, 100));
        assert!(
            local
                .iter()
                .any(|l| l.contains("01:00:00") && l.contains("rate_limited")),
            "{local:?}"
        );
        assert!(local.iter().any(|l| l.contains("cooled 30s")), "{local:?}");
        assert!(local.iter().any(|l| l.contains("body")), "{local:?}");
    }

    #[test]
    fn every_gauge_mode_stays_inside_tiny_widths() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &gauge_doc(), t0);
        let m = Matrix::build(&f, &names(&["peer-a"]), t0);
        for metric in [GaugeMetric::Requests, GaugeMetric::Tokens] {
            for window in [GaugeWindow::Hour, GaugeWindow::Minutes] {
                let mode = GaugeMode { metric, window };
                for width in 0..=24 {
                    let lines =
                        render_matrix_mode(&m, &History::new(0), Some(0), width, None, mode);
                    for line in lines {
                        assert!(
                            line.plain_text().chars().count() <= width,
                            "{mode:?} width {width}: {line:?}"
                        );
                    }
                }
            }
        }
    }
}
