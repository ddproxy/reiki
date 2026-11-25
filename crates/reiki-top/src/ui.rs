//! Ratatui drawing, state and key mapping for `reiki top`. Compiled only with the `tui`
//! feature; everything here is still pure except [`draw`], which writes into a frame.

pub use crossterm;
pub use ratatui;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fw_tui::sanitize::printable;
use fw_tui::text::{display_width, pad_width, truncate_width};
use fw_tui::theme::Theme;
use fw_tui::widgets::{
    FrequencyBar, LineChartConfig, Panel, Series, ToggleChip, render_footer_bar,
    render_frequency_chart, render_header_bar, render_line_chart, render_overlay, render_tabs,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style as RatStyle};
use ratatui::text::{Line as RatLine, Span as RatSpan};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::matrix::{PeerColumn, PeerSort, UsageMetric};
use crate::streams::{StreamColumn, StreamSort};
use crate::view::{self, GaugeMetric, GaugeMode, GaugeWindow, Span, Style};

/// The key hints on the footer's right side, full view.
const HINTS: &str =
    "q quit  j/k scroll  p pause  Tab next peer  c all  v matrix  t streams  ? keys";
/// The key hints under the matrix header. Kept within 100 columns so the header's status fits.
const HINTS_MATRIX: &str =
    "q quit  j/k  Enter detail  f filter  u metric  w window  m chart  t streams  a all  ? keys";
/// The key hints under the Streams header.
const HINTS_STREAMS: &str =
    "q quit  j/k select  PgUp/PgDn detail  o sort  p pause  t matrix  ? keys";

/// Rows of the matrix header panel: a border, the status line, the hints and a border.
const HEADER_ROWS: u16 = 4;
/// Height from which the chart row appears.
pub const CHARTS_MIN_HEIGHT: u16 = 28;
/// Rows of the chart row, borders included.
pub const CHART_ROWS: u16 = 10;

/// The keys the matrix overlay lists.
const MATRIX_KEYS: &[&str] = &[
    "j/k, arrows, Tab, Shift-Tab   move the peer selection",
    "Enter, e   open or close the peer detail",
    "PgUp/PgDn, g/Home, G/End   scroll the detail",
    "f   cycle the provider filter",
    "c   clear the filter and the selection",
    "u   gauge and chart series: requests, limited, tokens",
    "w   gauge: last hour or last five minutes",
    "m   chart: load, disk, sessions, requests",
    "s   requests chart: overlaid series or stacked total",
    "a   jump back to ALL (the fleet row)",
    "o or O   sort the peers by the next or previous column; past the last, unsorted",
    "r   reverse the sort",
    "p   freeze the screen; again to resume",
    "v or E   full text view",
    "t   read-only Streams view",
    "?   this list; any key closes it, Ctrl-C quits",
    "q or Esc   quit",
];

/// The keys the full view's overlay lists.
const FULL_KEYS: &[&str] = &[
    "j/k, arrows, PgUp/PgDn, g/Home, G/End   scroll",
    "Tab, Shift-Tab   filter to the next or previous peer",
    "c   clear the filter",
    "p   freeze the screen; again to resume",
    "v or E   back to the matrix",
    "t   read-only Streams view",
    "?   this list; any key closes it, Ctrl-C quits",
    "q or Esc   quit",
];

/// The keys the Streams overlay lists.
const STREAMS_KEYS: &[&str] = &[
    "j/k, arrows, Tab, Shift-Tab   move the stream selection",
    "PgUp/PgDn, g/Home, G/End   scroll the detail pane",
    "o or O   sort by the next or previous column; past the last, default usage order",
    "r   reverse the sort",
    "p   freeze the screen; again to resume",
    "t   back to the matrix",
    "?   this list; any key closes it, Ctrl-C quits",
    "q or Esc   quit",
];

/// The filter the caller applies before rendering; this layer only stores it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub peer: Option<String>,
    pub provider: Option<String>,
}

/// What the terminal does with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
    TogglePause,
    NextPeer,
    PrevPeer,
    ClearFilter,
    /// Opens or closes the selected peer's detail pane; the caller knows whether it is a problem.
    ToggleDetail,
    /// Switches between the matrix and the full text view.
    ToggleMode,
    /// Opens the read-only Streams view, or returns to the matrix from it.
    ToggleStreams,
    /// Cycles the matrix provider filter: all, then each column, then all.
    CycleProvider,
    /// Switches the matrix gauge between requests and tokens.
    ToggleMetric,
    /// Switches the matrix gauge between the last hour and the last five minutes.
    ToggleWindow,
    /// Cycles the chart's metric: load, disk, sessions, requests.
    CycleChart,
    /// Switches the requests chart between overlaid lines and a stacked total.
    ToggleStack,
    /// Selects the `ALL` pseudo-peer.
    SelectFleet,
    /// Opens or closes the key list.
    ToggleHelp,
    /// Sorts the current screen by the next column, or unsorted after the last. Needs the screen's
    /// columns, so the caller applies it with [`UiState::cycle_sort`] or
    /// [`UiState::cycle_stream_sort`].
    SortNext,
    /// Sorts the current screen by the previous column, or unsorted before the first.
    SortPrev,
    /// Flips the current screen's sort direction. A no-op while the rows are unsorted.
    SortReverse,
}

/// What the matrix chart plots. Load is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ChartMetric {
    #[default]
    Load,
    Disk,
    Sessions,
    Requests,
}

impl ChartMetric {
    /// The next metric in the `m` cycle.
    pub fn next(self) -> Self {
        match self {
            Self::Load => Self::Disk,
            Self::Disk => Self::Sessions,
            Self::Sessions => Self::Requests,
            Self::Requests => Self::Load,
        }
    }

    /// The lower-case name the footer chip shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Disk => "disk",
            Self::Sessions => "sessions",
            Self::Requests => "requests",
        }
    }

    /// The capitalised name the chart title shows.
    pub fn title(self) -> &'static str {
        match self {
            Self::Load => "Load",
            Self::Disk => "Disk",
            Self::Sessions => "Sessions",
            Self::Requests => "Requests",
        }
    }
}

/// Which screen the live view shows. The matrix is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Matrix,
    /// The per-peer text view, with the Tab peer filter.
    Full,
    /// The read-only local stream console.
    Streams,
}

/// The scrolling and filter state the caller keeps between frames.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiState {
    pub scroll: usize,
    pub filter: Filter,
    /// An index into the caller's peer list, not a line number.
    pub selected: Option<usize>,
    pub paused: bool,
    pub mode: Mode,
    /// `None` opens the detail pane only while the selected peer is a problem; `Some` is an
    /// explicit choice made with Enter.
    pub detail: Option<bool>,
    /// What the matrix gauges draw; requests over the hour by default.
    pub gauge: GaugeMode,
    /// What the matrix chart plots.
    pub chart: ChartMetric,
    /// Requests chart lines are overlaid until `s` stacks them.
    pub stacked: bool,
    /// Which figure the provider-coloured chart sums.
    pub chart_usage: UsageMetric,
    /// The stream under the cursor on the Streams screen, by name. A refresh or a re-sort moves
    /// the display row with the stream. `None` selects the first row.
    pub selected_stream: Option<String>,
    /// Whether the key list is open over the frame.
    pub help: bool,
    /// The matrix sort. `None` keeps the default order.
    pub peer_sort: Option<PeerSort>,
    /// The Streams sort. `None` keeps the default usage order.
    pub stream_sort: Option<StreamSort>,
    /// The peer under the cursor, by name. `selected` is its position in the last order the
    /// caller resolved, so a refresh or re-sort moves the cursor with the peer.
    pub selected_peer: Option<String>,
}

/// The last top line that still fills the viewport, or the last line when the viewport is
/// empty. Every scroll ends up between 0 and this value, so the window never passes the end.
fn max_scroll(total_lines: usize, viewport: usize) -> usize {
    total_lines.saturating_sub(viewport.max(1))
}

impl UiState {
    /// Whether the detail pane is open. Unless Enter chose, it opens for a peer that needs
    /// attention and for any peer the user has moved to.
    pub fn detail_open(&self, problem_selected: bool) -> bool {
        self.detail
            .unwrap_or(problem_selected || self.selected.is_some())
    }

    /// Flips the detail pane from what is showing now, so the first Enter on a problem peer
    /// closes its automatically open pane.
    pub fn toggle_detail(&mut self, problem_selected: bool) {
        self.detail = Some(!self.detail_open(problem_selected));
    }

    /// Switches view. Each view starts at its top.
    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            Mode::Matrix => Mode::Full,
            Mode::Full | Mode::Streams => Mode::Matrix,
        };
        self.scroll = 0;
    }

    /// Opens Streams from either other view, or returns to the matrix from Streams. The table
    /// starts at its first row; the detail pane starts at its top.
    pub fn toggle_streams(&mut self) {
        self.mode = match self.mode {
            Mode::Streams => Mode::Matrix,
            Mode::Matrix | Mode::Full => Mode::Streams,
        };
        self.scroll = 0;
        if self.mode == Mode::Streams {
            self.selected_stream = None;
        }
    }

    /// Cycles the matrix provider filter over `providers` in column order. A selected provider
    /// that is not in the list falls back to all on the next cycle.
    pub fn cycle_provider(&mut self, providers: &[String]) {
        let next = match self.filter.provider.as_deref() {
            None => providers.first().cloned(),
            Some(current) => match providers.iter().position(|p| p == current) {
                Some(i) if i + 1 < providers.len() => Some(providers[i + 1].clone()),
                _ => None,
            },
        };
        self.filter.provider = next;
        self.selected = None;
        self.detail = None;
        self.scroll = 0;
    }

    /// Steps the matrix sort through `columns` and then back to unsorted, ascending on each
    /// column. `forward` is `o`; the reverse step is `O`. A sort on a column that is no longer
    /// listed counts as unsorted, so the next step lands on the first column.
    pub fn cycle_sort(&mut self, columns: &[PeerColumn], forward: bool) {
        if columns.is_empty() {
            return;
        }
        // Slot 0 is unsorted; slot n is `columns[n - 1]`.
        let slots = columns.len() + 1;
        let current = self.peer_sort.as_ref().map_or(0, |sort| {
            columns
                .iter()
                .position(|c| *c == sort.column)
                .map_or(0, |i| i + 1)
        });
        let next = if forward {
            (current + 1) % slots
        } else {
            (current + slots - 1) % slots
        };
        self.peer_sort = (next > 0).then(|| PeerSort {
            column: columns[next - 1].clone(),
            descending: false,
        });
    }

    /// Flips the sort's direction. Returns false while the rows are unsorted.
    pub fn reverse_sort(&mut self) -> bool {
        match self.peer_sort.as_mut() {
            Some(sort) => {
                sort.descending = !sort.descending;
                true
            }
            None => false,
        }
    }

    /// Steps the Streams sort through `columns` and back to the default usage order, like
    /// [`UiState::cycle_sort`] does for the matrix.
    pub fn cycle_stream_sort(&mut self, columns: &[StreamColumn], forward: bool) {
        if columns.is_empty() {
            return;
        }
        let slots = columns.len() + 1;
        let current = self.stream_sort.map_or(0, |sort| {
            columns
                .iter()
                .position(|c| *c == sort.column)
                .map_or(0, |i| i + 1)
        });
        let next = if forward {
            (current + 1) % slots
        } else {
            (current + slots - 1) % slots
        };
        self.stream_sort = (next > 0).then(|| StreamSort {
            column: columns[next - 1],
            descending: false,
        });
    }

    /// Flips the Streams sort's direction. Returns false while the rows are unsorted.
    pub fn reverse_stream_sort(&mut self) -> bool {
        match self.stream_sort.as_mut() {
            Some(sort) => {
                sort.descending = !sort.descending;
                true
            }
            None => false,
        }
    }

    /// Applies one key action. Returns false only for [`Action::Quit`]; a false return leaves
    /// the state untouched. Scroll is clamped to the last line, so any view size is safe.
    pub fn apply(&mut self, action: Action, total_lines: usize, viewport: usize) -> bool {
        let step = viewport.max(1);
        match action {
            Action::Quit => return false,
            Action::ScrollUp => self.scroll = self.scroll.saturating_sub(1),
            Action::ScrollDown => self.scroll = self.scroll.saturating_add(1),
            Action::PageUp => self.scroll = self.scroll.saturating_sub(step),
            Action::PageDown => self.scroll = self.scroll.saturating_add(step),
            Action::Top => self.scroll = 0,
            Action::Bottom => self.scroll = max_scroll(total_lines, viewport),
            Action::TogglePause => self.paused = !self.paused,
            // The peer list lives in the caller, so track an index and let it resolve the name.
            Action::NextPeer => {
                self.selected = Some(self.selected.map_or(0, |i| i.saturating_add(1)));
            }
            Action::PrevPeer => {
                self.selected = Some(self.selected.map_or(0, |i| i.saturating_sub(1)));
            }
            Action::ClearFilter => self.filter = Filter::default(),
            Action::ToggleMode => self.toggle_mode(),
            Action::ToggleStreams => self.toggle_streams(),
            // Needs the selected peer's problem flag, so the caller calls `toggle_detail`.
            Action::ToggleDetail => {}
            // Needs the current provider columns; the caller cycles the filter.
            Action::CycleProvider => {}
            Action::ToggleMetric => {
                self.gauge.metric = match self.gauge.metric {
                    GaugeMetric::Requests => GaugeMetric::Tokens,
                    GaugeMetric::Tokens => GaugeMetric::Requests,
                };
                // One key moves both figures that `u` names: the gauge and the chart series.
                self.chart_usage = self.chart_usage.next();
            }
            Action::ToggleWindow => {
                self.gauge.window = match self.gauge.window {
                    GaugeWindow::Hour => GaugeWindow::Minutes,
                    GaugeWindow::Minutes => GaugeWindow::Hour,
                };
            }
            Action::CycleChart => self.chart = self.chart.next(),
            Action::ToggleStack => self.stacked = !self.stacked,
            Action::SelectFleet => {
                self.selected = Some(0);
                self.detail = None;
                self.scroll = 0;
            }
            Action::ToggleHelp => self.help = !self.help,
            // The screen's columns live in the caller; it calls `cycle_sort`/`cycle_stream_sort`
            // and `reverse_sort`/`reverse_stream_sort`.
            Action::SortNext | Action::SortPrev | Action::SortReverse => {}
        }
        self.scroll = self.scroll.min(max_scroll(total_lines, viewport));
        true
    }
}

/// Maps a key press to an action. Releases and unknown keys are ignored; Ctrl-C quits.
pub fn map_key(event: KeyEvent) -> Option<Action> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    if event.code == KeyCode::Char('c') && event.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(Action::Quit);
    }
    let action = match event.code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Char('j') | KeyCode::Down => Action::ScrollDown,
        KeyCode::Char('k') | KeyCode::Up => Action::ScrollUp,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Char('g') | KeyCode::Home => Action::Top,
        KeyCode::Char('G') | KeyCode::End => Action::Bottom,
        KeyCode::Char('p') => Action::TogglePause,
        KeyCode::Tab => Action::NextPeer,
        KeyCode::BackTab => Action::PrevPeer,
        KeyCode::Char('c') => Action::ClearFilter,
        KeyCode::Enter | KeyCode::Char('e') => Action::ToggleDetail,
        KeyCode::Char('v') | KeyCode::Char('E') => Action::ToggleMode,
        KeyCode::Char('t') => Action::ToggleStreams,
        KeyCode::Char('f') => Action::CycleProvider,
        KeyCode::Char('u') => Action::ToggleMetric,
        KeyCode::Char('w') => Action::ToggleWindow,
        KeyCode::Char('m') => Action::CycleChart,
        KeyCode::Char('s') => Action::ToggleStack,
        KeyCode::Char('a') => Action::SelectFleet,
        KeyCode::Char('o') => Action::SortNext,
        KeyCode::Char('O') => Action::SortPrev,
        KeyCode::Char('r') => Action::SortReverse,
        KeyCode::Char('?') => Action::ToggleHelp,
        _ => return None,
    };
    Some(action)
}

/// While the key list is open, the next key press closes it and does nothing else. Ctrl-C is
/// not swallowed, so it still quits. Returns true when the key was used up here.
pub fn dismiss_help(state: &mut UiState, event: KeyEvent) -> bool {
    if !state.help || event.kind == KeyEventKind::Release {
        return false;
    }
    if event.code == KeyCode::Char('c') && event.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    state.help = false;
    true
}

fn rat_style(style: Style) -> RatStyle {
    match style {
        Style::Plain => RatStyle::new(),
        Style::Dim => RatStyle::new().fg(Color::DarkGray),
        Style::Title => RatStyle::new().add_modifier(Modifier::BOLD),
        Style::Good => RatStyle::new().fg(Color::Green),
        Style::Warn => RatStyle::new().fg(Color::Yellow),
        Style::Bad => RatStyle::new().fg(Color::Red),
    }
}

/// Converts view lines to ratatui lines, keeping every span and its style.
pub fn to_ratatui(lines: &[view::Line]) -> Vec<RatLine<'static>> {
    lines
        .iter()
        .map(|line| {
            let spans: Vec<RatSpan<'static>> = line
                .spans
                .iter()
                .map(|span: &Span| RatSpan::styled(span.text.clone(), rat_style(span.style)))
                .collect();
            RatLine::from(spans)
        })
        .collect()
}

/// Draws the bordered body and a one-line footer. A zero-size or one-line terminal is fine;
/// the footer always takes the last row and the body gets whatever is left.
pub fn draw(frame: &mut Frame, lines: &[view::Line], state: &UiState, status: &str) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let body_height = area.height.saturating_sub(1);
    let footer_y = area.y.saturating_add(body_height);

    if body_height > 0 {
        let body = Rect::new(area.x, area.y, area.width, body_height);
        let block = Block::default().borders(Borders::ALL);
        let inner = block.inner(body);
        frame.render_widget(block, body);
        if inner.width > 0 && inner.height > 0 {
            let viewport = usize::from(inner.height);
            let start = state.scroll.min(max_scroll(lines.len(), viewport));
            let end = start.saturating_add(viewport).min(lines.len());
            let visible = to_ratatui(&lines[start..end]);
            frame.render_widget(Paragraph::new(visible), inner);
        }
    }

    let footer_area = Rect::new(area.x, footer_y, area.width, 1);
    frame.render_widget(
        Paragraph::new(footer_text(state, status, HINTS)),
        footer_area,
    );
    if state.help {
        draw_help(frame, area, FULL_KEYS);
    }
}

fn draw_help(frame: &mut Frame, area: Rect, keys: &[&str]) {
    let lines: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
    render_overlay(area, frame.buffer_mut(), " Keys ", &lines, &Theme::auto());
}

/// The detail pane's total height for a terminal of `screen_height` rows: a third of the body
/// above the footer, or none when that would be too short to hold a border and a line.
pub fn detail_height(screen_height: u16) -> u16 {
    let third = screen_height.saturating_sub(1) / 3;
    if third >= 3 { third } else { 0 }
}

/// Rows of each band of the matrix frame, top to bottom. Zero means the band is not drawn.
/// `charts` and `detail` never both have rows: the open peer card takes the chart row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bands {
    pub header: u16,
    pub tabs: u16,
    pub peers: u16,
    pub cooling: u16,
    pub charts: u16,
    pub detail: u16,
    pub footer: u16,
}

/// Splits a terminal height into the matrix frame's bands. Short terminals lose the chart row,
/// then the cooling panel, then the tab row, and the header folds to one line, so a panel goes
/// before the matrix loses a row. The Peers panel takes what is left, clipped only when even the
/// bare matrix does not fit. `matrix_lines` and `cooling_lines` count content, not borders.
pub fn bands(height: u16, matrix_lines: usize, cooling_lines: usize, detail_open: bool) -> Bands {
    let footer = u16::from(height > 0);
    let body = height - footer;
    let detail = if detail_open {
        detail_height(height)
    } else {
        0
    };
    let charts = if detail == 0 && height >= CHARTS_MIN_HEIGHT {
        CHART_ROWS
    } else {
        0
    };
    let cooling = if cooling_lines == 0 {
        0
    } else {
        u16::try_from(cooling_lines).unwrap_or(u16::MAX - 2) + 2
    };
    let mut b = Bands {
        header: HEADER_ROWS,
        tabs: 1,
        peers: 0,
        cooling,
        charts,
        detail,
        footer,
    };
    let peers_want = u32::try_from(matrix_lines).unwrap_or(u32::MAX - 2) + 2;
    let fits = |b: &Bands| {
        u32::from(b.header)
            + u32::from(b.tabs)
            + u32::from(b.cooling)
            + u32::from(b.charts)
            + u32::from(b.detail)
            + peers_want
            <= u32::from(body)
    };
    if !fits(&b) {
        b.charts = 0;
    }
    if !fits(&b) {
        b.cooling = 0;
    }
    if !fits(&b) {
        b.tabs = 0;
    }
    if !fits(&b) {
        b.header = if body > 0 { 1 } else { 0 };
    }
    let used = u32::from(b.header)
        + u32::from(b.tabs)
        + u32::from(b.cooling)
        + u32::from(b.charts)
        + u32::from(b.detail);
    b.peers = u16::try_from(u32::from(body).saturating_sub(used)).unwrap_or(0);
    b
}

/// One coloured line in the chart. Provider names are in provider-column order.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartSeries {
    pub name: String,
    pub points: Vec<Option<f64>>,
    pub color: Color,
}

impl ChartSeries {
    /// A legend entry: a coloured square, the name and the latest value.
    fn legend_spans(&self) -> Vec<RatSpan<'static>> {
        let latest = self
            .points
            .iter()
            .rev()
            .flatten()
            .copied()
            .next()
            .unwrap_or(0.0);
        let value = latest.round().max(0.0) as u64;
        vec![
            RatSpan::styled("■", RatStyle::new().fg(self.color)),
            RatSpan::raw(format!(" {} {}  ", truncate_width(&self.name, 12), value)),
        ]
    }
}

/// What one matrix frame draws, built by the caller from the fleet and its history.
pub struct MatrixFrame<'a> {
    pub version: &'a str,
    /// The UTC time of this frame, `HH:MM:SSZ`.
    pub clock: &'a str,
    /// `N of M peers fresh`, with the age of a paused snapshot.
    pub status: &'a str,
    /// Every provider column in order, unfiltered: the tabs.
    pub providers: &'a [String],
    /// The matrix lines: the fleet line, the grid header, then one line per peer.
    pub matrix: &'a [view::Line],
    /// How many of `matrix` are peer rows, and the selected row's display position.
    pub rows: usize,
    pub selected: Option<usize>,
    /// The cooling panel's lines, without its title. Empty hides the panel.
    pub cooling: &'a [view::Line],
    /// The selected peer's detail card, when open.
    pub detail: Option<&'a [view::Line]>,
    pub chart_title: &'a str,
    /// One line per provider for the requests chart, or one line for the other charts.
    pub chart_series: &'a [ChartSeries],
    /// Time covered by one chart point, for the x axis labels.
    pub chart_interval: Duration,
    /// Healthy, cooling and disabled keys across the fleet.
    pub keys: [u64; 3],
}

/// Takes `rows` off the top of `rest`, or whatever is left of it.
fn carve(rest: &mut Rect, rows: u16) -> Rect {
    let rows = rows.min(rest.height);
    let top = Rect::new(rest.x, rest.y, rest.width, rows);
    rest.y = rest.y.saturating_add(rows);
    rest.height -= rows;
    top
}

/// A line with `left` at the start and `right` at the end, cut to `width` columns.
fn split_line(width: u16, left: &str, right: &str, theme: &Theme) -> RatLine<'static> {
    let width = usize::from(width);
    let right = truncate_width(right, width);
    let right_w = display_width(&right);
    let left_w = width.saturating_sub(right_w);
    let left = pad_width(&truncate_width(left, left_w), left_w);
    RatLine::from(vec![
        RatSpan::raw(left),
        RatSpan::styled(right, theme.label),
    ])
}

fn paint(frame: &mut Frame, area: Rect, lines: &[view::Line]) {
    frame.render_widget(Paragraph::new(to_ratatui(lines)), area);
}

fn draw_header(
    frame: &mut Frame,
    area: Rect,
    view: &MatrixFrame<'_>,
    state: &UiState,
    theme: &Theme,
) {
    let title = format!("reiki top v{}", view.version);
    let live = if state.paused { "paused" } else { "live" };
    if area.height < HEADER_ROWS {
        let left = format!("{title}  {live} · {}", view.status);
        render_header_bar(
            area,
            frame.buffer_mut(),
            &[left],
            &[view.clock.to_owned()],
            theme,
        );
        return;
    }
    frame.render_widget(
        Panel::new(&title)
            .block(theme)
            .title_top(RatLine::from(view.clock).right_aligned()),
        area,
    );
    let inner = Panel::inner(area);
    let status = format!("{live} · {}", view.status);
    let status_row = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(
        Paragraph::new(split_line(inner.width, &status, "", theme)),
        status_row,
    );
    let hints_row = Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1);
    frame.render_widget(
        Paragraph::new(split_line(inner.width, HINTS_MATRIX, "", theme)),
        hints_row,
    );
}

fn draw_tabs(
    frame: &mut Frame,
    area: Rect,
    view: &MatrixFrame<'_>,
    state: &UiState,
    theme: &Theme,
) {
    let mut labels: Vec<&str> = vec!["All"];
    labels.extend(view.providers.iter().map(String::as_str));
    let selected = state
        .filter
        .provider
        .as_deref()
        .and_then(|p| view.providers.iter().position(|c| c == p))
        .map_or(0, |i| i + 1);
    render_tabs(area, frame.buffer_mut(), &labels, selected, theme);
}

/// Colours in provider-column order. Glyphs carry the same order without colour.
const SERIES_COLORS: [Color; 8] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::Blue,
    Color::LightGreen,
    Color::LightCyan,
    Color::LightYellow,
];
const SERIES_GLYPHS: [char; 8] = ['●', '◆', '▲', '■', '○', '◇', '△', '□'];

fn series_color(index: usize) -> Color {
    SERIES_COLORS[index % SERIES_COLORS.len()]
}

/// The colour of provider `index`, for callers building [`ChartSeries`].
pub fn chart_color(index: usize) -> Color {
    series_color(index)
}

fn series_glyph(index: usize) -> char {
    SERIES_GLYPHS[index % SERIES_GLYPHS.len()]
}

fn put_char(frame: &mut Frame, x: u16, y: u16, ch: char, style: RatStyle) {
    let area = frame.area();
    if x >= area.width || y >= area.height {
        return;
    }
    let Some(cell) = frame.buffer_mut().cell_mut((x, y)) else {
        return;
    };
    cell.set_symbol(&ch.to_string());
    cell.set_style(style);
}

fn put_text(frame: &mut Frame, x: u16, y: u16, text: &str, style: RatStyle) {
    for (i, ch) in text.chars().enumerate() {
        let at = x.saturating_add(u16::try_from(i).unwrap_or(u16::MAX));
        if at >= frame.area().width {
            return;
        }
        put_char(frame, at, y, ch, style);
    }
}

/// The smallest 1/2/5 x 10^n step at or above `value`; zero for an all-zero chart.
fn nice_ceil(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 0.0;
    }
    let power = 10f64.powf(value.log10().floor());
    let unit = value / power;
    let step = if unit <= 1.0 {
        1.0
    } else if unit <= 2.0 {
        2.0
    } else if unit <= 5.0 {
        5.0
    } else {
        10.0
    };
    step * power
}

fn axis_label(value: f64) -> String {
    if value <= 0.0 {
        "0".to_owned()
    } else if value >= 1000.0 && (value / 1000.0).fract() == 0.0 {
        format!("{}k", (value / 1000.0) as u64)
    } else if value.fract() == 0.0 {
        format!("{}", value as u64)
    } else {
        let text = format!("{value:.2}");
        let trimmed = text.trim_end_matches('0').trim_end_matches('.');
        if trimmed.is_empty() {
            "0".to_owned()
        } else {
            trimmed.to_owned()
        }
    }
}

fn x_label(interval: Duration) -> String {
    let secs = interval.as_secs();
    if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("-{}h", secs / 3600)
    } else if secs >= 60 {
        format!("-{}m", secs / 60)
    } else {
        format!("-{}s", secs.max(1))
    }
}

fn point_at(series: &ChartSeries, index: usize) -> f64 {
    series
        .points
        .get(index)
        .copied()
        .flatten()
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
        .max(0.0)
}

/// The requests chart: a coloured line per provider, a clipped legend, and labels derived from
/// the sample interval rather than the point indices. `s` switches to a stacked total.
fn draw_usage_chart(frame: &mut Frame, area: Rect, view: &MatrixFrame<'_>, state: &UiState) {
    if area.width < 8 || area.height < 4 || view.chart_series.is_empty() {
        return;
    }
    let points = view
        .chart_series
        .iter()
        .map(|s| s.points.len())
        .max()
        .unwrap_or(0);
    if points == 0 {
        return;
    }
    let peak = if state.stacked {
        (0..points)
            .map(|i| {
                view.chart_series
                    .iter()
                    .map(|s| point_at(s, i))
                    .sum::<f64>()
            })
            .fold(0.0_f64, f64::max)
    } else {
        view.chart_series
            .iter()
            .flat_map(|s| s.points.iter().flatten().copied())
            .filter(|v| v.is_finite())
            .fold(0.0_f64, f64::max)
    };
    let top = nice_ceil(peak);
    let max_label = axis_label(top);
    let gutter = u16::try_from(display_width(&max_label) + 1)
        .unwrap_or(u16::MAX)
        .min(area.width / 2);
    let plot = Rect::new(
        area.x.saturating_add(gutter),
        area.y.saturating_add(1),
        area.width.saturating_sub(gutter),
        area.height.saturating_sub(2),
    );
    if plot.width == 0 || plot.height == 0 {
        return;
    }
    let baseline = plot.y.saturating_add(plot.height - 1);
    let dim = RatStyle::new().fg(Color::DarkGray);
    let mut spans = Vec::new();
    for series in view.chart_series {
        spans.extend(series.legend_spans());
    }
    frame.render_widget(
        Paragraph::new(RatLine::from(spans)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    put_text(frame, area.x, plot.y, &max_label, dim);
    put_text(frame, area.x, baseline, "0", dim);
    put_text(
        frame,
        area.x,
        area.y.saturating_add(area.height - 1),
        &x_label(
            view.chart_interval
                .saturating_mul(u32::try_from(points).unwrap_or(u32::MAX)),
        ),
        dim,
    );
    let now = "now";
    let now_x = area
        .x
        .saturating_add(area.width.saturating_sub(display_width(now) as u16));
    put_text(
        frame,
        now_x,
        area.y.saturating_add(area.height - 1),
        now,
        dim,
    );

    let x_of = |i: usize| -> u16 {
        if points <= 1 {
            plot.x.saturating_add(plot.width - 1)
        } else {
            let span = u32::from(plot.width.saturating_sub(1));
            let step = u32::try_from(i).unwrap_or(u32::MAX) * span
                / u32::try_from(points - 1).unwrap_or(u32::MAX);
            plot.x
                .saturating_add(u16::try_from(step).unwrap_or(u16::MAX).min(plot.width - 1))
        }
    };
    let y_of = |value: f64| -> u16 {
        if top <= 0.0 || plot.height <= 1 {
            baseline
        } else {
            let height = f64::from(plot.height - 1);
            let scaled = (value / top).clamp(0.0, 1.0) * height;
            baseline.saturating_sub(scaled.round() as u16)
        }
    };

    if top <= 0.0 {
        for x in plot.x..plot.x.saturating_add(plot.width) {
            put_char(frame, x, baseline, '─', dim);
        }
    }
    if state.stacked && top > 0.0 {
        for i in 0..points {
            let x = x_of(i);
            let mut sum = 0.0;
            for (n, series) in view.chart_series.iter().enumerate() {
                let value = point_at(series, i);
                if value <= 0.0 {
                    continue;
                }
                let upper = y_of(sum + value);
                let lower = y_of(sum);
                for y in upper..=lower {
                    put_char(frame, x, y, '█', RatStyle::new().fg(series_color(n)));
                }
                sum += value;
            }
        }
    }
    if !state.stacked {
        for (n, series) in view.chart_series.iter().enumerate() {
            let style = RatStyle::new().fg(series_color(n));
            let glyph = series_glyph(n);
            for i in 0..points {
                let x = x_of(i);
                let y = y_of(point_at(series, i));
                if i > 0 {
                    let from = x_of(i - 1);
                    let mid = y.saturating_add(y_of(point_at(series, i - 1))) / 2;
                    for at in from.saturating_add(1)..x {
                        put_char(frame, at, mid, '─', style);
                    }
                }
                put_char(frame, x, y, glyph, style);
            }
        }
    }
}

#[allow(clippy::default_constructed_unit_structs)] // the real fw-tui may add fields later
fn draw_charts(
    frame: &mut Frame,
    area: Rect,
    view: &MatrixFrame<'_>,
    state: &UiState,
    theme: &Theme,
) {
    let left_w = u16::try_from(u32::from(area.width) * 2 / 3).unwrap_or(area.width);
    let left = Rect::new(area.x, area.y, left_w, area.height);
    let right = Rect::new(
        area.x.saturating_add(left_w),
        area.y,
        area.width - left_w,
        area.height,
    );
    let title = printable(view.chart_title);
    frame.render_widget(Panel::new(&title).block(theme), left);
    let inner = Panel::inner(left);
    if view.chart_title.starts_with("Requests") {
        draw_usage_chart(frame, inner, view, state);
    } else {
        let series: Vec<Series> = view
            .chart_series
            .iter()
            .map(|s| Series::new(&s.name, s.points.clone()))
            .collect();
        render_line_chart(
            inner,
            frame.buffer_mut(),
            &series,
            &LineChartConfig::default(),
            theme,
        );
    }

    frame.render_widget(Panel::new("Keys").block(theme), right);
    let [healthy, cooling, disabled] = view.keys;
    let bars = [
        FrequencyBar {
            label: "healthy".to_owned(),
            count: healthy,
        },
        FrequencyBar {
            label: "cooling".to_owned(),
            count: cooling,
        },
        FrequencyBar {
            label: "disabled".to_owned(),
            count: disabled,
        },
    ];
    render_frequency_chart(Panel::inner(right), frame.buffer_mut(), &bars, theme);
}

/// The data one read-only Streams frame is drawn from. The caller owns the lines so the drawing
/// layer stays free of parsing and policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamsFrame<'a> {
    /// Table header and rows, already in display order.
    pub table: &'a [view::Line],
    /// Detail lines for the selected stream, scrolled by `UiState::scroll`.
    pub detail: &'a [view::Line],
    /// Disabled, empty and degraded-state explanations.
    pub status: &'a [view::Line],
    /// Selected table row, used to keep it in view.
    pub selected: Option<usize>,
}

/// Draws the Streams screen: a status line, the stream table, the detail pane and the footer.
/// It is read-only; no key here changes a stream. Small terminals keep the table and drop the
/// detail pane when there is no room for two columns.
pub fn draw_streams(frame: &mut Frame, view: &StreamsFrame<'_>, state: &UiState) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = Theme::auto();
    let mut rest = area;
    let header = carve(&mut rest, 1);
    let footer = carve(&mut rest, 1);
    let body = rest;

    if header.height > 0 {
        let status = if view.status.is_empty() {
            "streams: read-only local view".to_owned()
        } else {
            view.status
                .iter()
                .map(view::Line::plain_text)
                .collect::<Vec<_>>()
                .join(" · ")
        };
        frame.render_widget(Paragraph::new(RatLine::raw(status)), header);
    }

    if body.height > 0 && body.width > 0 {
        let (table_area, detail_area) = if body.width >= 60 {
            let table_width = body.width * 3 / 5;
            let table = Rect::new(body.x, body.y, table_width, body.height);
            let detail = Rect::new(
                body.x.saturating_add(table_width),
                body.y,
                body.width.saturating_sub(table_width),
                body.height,
            );
            (table, detail)
        } else {
            (body, Rect::new(body.x, body.y, 0, 0))
        };

        frame.render_widget(Panel::new("Streams").block(&theme), table_area);
        let inner = Panel::inner(table_area);
        if inner.width > 0 && inner.height > 0 {
            let viewport = usize::from(inner.height);
            let start = match view.selected {
                Some(row) if row + 2 > viewport => row + 2 - viewport,
                _ => 0,
            };
            let start = start.min(max_scroll(view.table.len(), viewport));
            let end = start.saturating_add(viewport).min(view.table.len());
            paint(frame, inner, &view.table[start..end]);
        }

        if detail_area.width > 0 && detail_area.height > 0 {
            frame.render_widget(Panel::new("Detail").block(&theme), detail_area);
            let inner = Panel::inner(detail_area);
            if inner.width > 0 && inner.height > 0 {
                let viewport = usize::from(inner.height);
                let start = state.scroll.min(max_scroll(view.detail.len(), viewport));
                let end = start.saturating_add(viewport).min(view.detail.len());
                paint(frame, inner, &view.detail[start..end]);
            }
        }
    }

    if footer.height > 0 {
        frame.render_widget(
            Paragraph::new(footer_text(state, "", HINTS_STREAMS)),
            footer,
        );
    }
    if state.help {
        draw_help(frame, area, STREAMS_KEYS);
    }
}

/// Draws the matrix frame: a header panel, the provider tabs, the Peers panel, the cooling panel,
/// then either the selected peer's detail card or the chart row, and the footer chips. The bands
/// come from [`bands`]; nothing here is clipped except what a short terminal cannot fit.
pub fn draw_matrix(frame: &mut Frame, view: &MatrixFrame<'_>, state: &UiState) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = Theme::auto();
    let b = bands(
        area.height,
        view.matrix.len(),
        view.cooling.len(),
        view.detail.is_some(),
    );

    let mut rest = area;
    let header = carve(&mut rest, b.header);
    let tabs = carve(&mut rest, b.tabs);
    let peers = carve(&mut rest, b.peers);
    let cooling = carve(&mut rest, b.cooling);
    let bottom = carve(&mut rest, b.charts.max(b.detail));
    let footer = carve(&mut rest, b.footer);

    if header.height > 0 {
        draw_header(frame, header, view, state, &theme);
    }
    if tabs.height > 0 {
        draw_tabs(frame, tabs, view, state, &theme);
    }
    if peers.height > 0 {
        frame.render_widget(Panel::new("Peers").block(&theme), peers);
        let inner = Panel::inner(peers);
        let visible = view::window_matrix(
            view.matrix.to_vec(),
            view.rows,
            view.selected,
            usize::from(inner.height),
        );
        paint(frame, inner, &visible);
    }
    if cooling.height > 0 {
        frame.render_widget(Panel::new("Cooling").block(&theme), cooling);
        paint(frame, Panel::inner(cooling), view.cooling);
    }
    if b.detail > 0 {
        if let Some(card) = view.detail {
            frame.render_widget(Panel::new("Detail").block(&theme), bottom);
            let inner = Panel::inner(bottom);
            let viewport = usize::from(inner.height);
            let start = state.scroll.min(max_scroll(card.len(), viewport));
            let end = start.saturating_add(viewport).min(card.len());
            paint(frame, inner, &card[start..end]);
        }
    } else if b.charts > 0 {
        draw_charts(frame, bottom, view, state, &theme);
    }

    if footer.height > 0 {
        let gauge = state
            .gauge
            .tag()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let mut chips = vec![
            ToggleChip::toggle("detail", view.detail.is_some()),
            ToggleChip {
                label: gauge,
                on: None,
                value: None,
            },
            ToggleChip::value("chart", state.chart.label()),
        ];
        if state.chart == ChartMetric::Requests {
            chips.push(ToggleChip::toggle("stack", state.stacked));
            chips.push(ToggleChip::value("series", state.chart_usage.label()));
        }
        if let Some(provider) = &state.filter.provider {
            chips.push(ToggleChip::value("filter", provider));
        }
        if state.paused {
            chips.push(ToggleChip {
                label: "paused".to_owned(),
                on: None,
                value: None,
            });
        }
        render_footer_bar(footer, frame.buffer_mut(), "", &chips, &theme);
    }

    if state.help {
        draw_help(frame, area, MATRIX_KEYS);
    }
}

/// `HH:MM:SSZ` for a Unix time in seconds, in UTC.
pub fn utc_clock(unix_secs: u64) -> String {
    let day_secs = unix_secs % 86_400;
    format!(
        "{:02}:{:02}:{:02}Z",
        day_secs / 3600,
        (day_secs / 60) % 60,
        day_secs % 60
    )
}

fn footer_text(state: &UiState, status: &str, hints: &str) -> String {
    let mut footer = String::new();
    if state.paused {
        footer.push_str("(paused)  ");
    }
    footer.push_str(status);
    if !status.is_empty() {
        footer.push_str("  ");
    }
    footer.push_str(hints);
    footer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::stream_columns;
    use crossterm::event::KeyEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn line(text: &str) -> view::Line {
        view::Line {
            spans: vec![Span {
                text: text.to_owned(),
                style: Style::Plain,
            }],
        }
    }

    fn numbered(n: usize) -> Vec<view::Line> {
        (0..n).map(|i| line(&format!("line {i}"))).collect()
    }

    fn rendered(
        lines: &[view::Line],
        state: &UiState,
        status: &str,
        width: u16,
        height: u16,
    ) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("a test terminal");
        terminal
            .draw(|frame| draw(frame, lines, state, status))
            .expect("drawing succeeds");
        terminal.backend().buffer().clone()
    }

    fn row_text(buffer: &Buffer, row: u16) -> String {
        if buffer.area.width == 0 {
            return String::new();
        }
        buffer
            .content()
            .chunks(usize::from(buffer.area.width))
            .nth(usize::from(row))
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn to_ratatui_keeps_spans_and_maps_every_style() {
        let source = vec![view::Line {
            spans: vec![
                Span {
                    text: "plain".to_owned(),
                    style: Style::Plain,
                },
                Span {
                    text: "dim".to_owned(),
                    style: Style::Dim,
                },
                Span {
                    text: "title".to_owned(),
                    style: Style::Title,
                },
                Span {
                    text: "good".to_owned(),
                    style: Style::Good,
                },
                Span {
                    text: "warn".to_owned(),
                    style: Style::Warn,
                },
                Span {
                    text: "bad".to_owned(),
                    style: Style::Bad,
                },
            ],
        }];
        let converted = to_ratatui(&source);
        assert_eq!(converted.len(), 1);
        let spans = &converted[0].spans;
        assert_eq!(spans.len(), 6, "every span survives");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "plaindimtitlegoodwarnbad");
        assert_eq!(spans[0].style, RatStyle::new());
        assert_eq!(spans[1].style.fg, Some(Color::DarkGray));
        assert!(spans[2].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[3].style.fg, Some(Color::Green));
        assert_eq!(spans[4].style.fg, Some(Color::Yellow));
        assert_eq!(spans[5].style.fg, Some(Color::Red));
    }

    #[test]
    fn scrolling_clamps_at_both_ends() {
        let mut state = UiState::default();
        for _ in 0..20 {
            assert!(state.apply(Action::ScrollDown, 10, 3));
        }
        assert_eq!(state.scroll, 7, "the last page starts at 10 - 3");
        for _ in 0..20 {
            state.apply(Action::ScrollUp, 10, 3);
        }
        assert_eq!(state.scroll, 0);
        state.apply(Action::Bottom, 10, 3);
        assert_eq!(state.scroll, 7);
        state.apply(Action::Top, 10, 3);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn paging_uses_the_viewport_and_clamps() {
        let mut state = UiState::default();
        state.apply(Action::PageDown, 100, 10);
        assert_eq!(state.scroll, 10);
        state.apply(Action::PageDown, 100, 10);
        assert_eq!(state.scroll, 20);
        state.apply(Action::PageUp, 100, 10);
        assert_eq!(state.scroll, 10);
        for _ in 0..20 {
            state.apply(Action::PageDown, 100, 10);
        }
        assert_eq!(state.scroll, 90);
    }

    #[test]
    fn an_empty_view_never_panics_and_stays_at_zero() {
        for action in [
            Action::ScrollUp,
            Action::ScrollDown,
            Action::PageUp,
            Action::PageDown,
            Action::Top,
            Action::Bottom,
        ] {
            let mut state = UiState::default();
            assert!(state.apply(action, 0, 0), "{action:?} is not quit");
            assert_eq!(state.scroll, 0, "{action:?} on an empty view");
        }
    }

    #[test]
    fn a_zero_viewport_still_clamps_to_the_last_line() {
        let mut state = UiState::default();
        for _ in 0..20 {
            state.apply(Action::ScrollDown, 5, 0);
        }
        assert_eq!(state.scroll, 4);
        state.apply(Action::Bottom, 5, 0);
        assert_eq!(state.scroll, 4);
    }

    #[test]
    fn a_viewport_taller_than_the_view_stays_at_the_top() {
        let mut state = UiState::default();
        for _ in 0..5 {
            state.apply(Action::ScrollDown, 2, 10);
        }
        assert_eq!(state.scroll, 0);
        state.apply(Action::Bottom, 2, 10);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn quit_is_false_and_leaves_state_alone() {
        let mut state = UiState {
            scroll: 3,
            paused: true,
            selected: Some(2),
            filter: Filter {
                peer: Some("peer-a".to_owned()),
                provider: Some("zen".to_owned()),
            },
            mode: Mode::Full,
            detail: Some(true),
            gauge: GaugeMode::default(),
            chart: ChartMetric::Disk,
            stacked: true,
            chart_usage: UsageMetric::Tokens,
            selected_stream: Some("busy".to_owned()),
            help: true,
            peer_sort: Some(PeerSort {
                column: PeerColumn::Load,
                descending: true,
            }),
            stream_sort: Some(StreamSort {
                column: StreamColumn::Rate,
                descending: true,
            }),
            selected_peer: Some("peer-b".to_owned()),
        };
        let before = state.clone();
        assert!(!state.apply(Action::Quit, 10, 3));
        assert_eq!(state, before);
    }

    #[test]
    fn pause_toggles_and_other_actions_report_true() {
        let mut state = UiState::default();
        assert!(state.apply(Action::TogglePause, 10, 3));
        assert!(state.paused);
        state.apply(Action::TogglePause, 10, 3);
        assert!(!state.paused);
        assert!(state.apply(Action::ScrollDown, 10, 3));
    }

    #[test]
    fn ct_navigation_tracks_an_index_without_underflow() {
        let mut state = UiState::default();
        state.apply(Action::NextPeer, 10, 3);
        assert_eq!(state.selected, Some(0));
        state.apply(Action::NextPeer, 10, 3);
        assert_eq!(state.selected, Some(1));
        state.apply(Action::PrevPeer, 10, 3);
        assert_eq!(state.selected, Some(0));
        state.apply(Action::PrevPeer, 10, 3);
        assert_eq!(state.selected, Some(0), "never underflows");
    }

    #[test]
    fn clearing_the_filter_clears_ct_and_provider() {
        let mut state = UiState {
            filter: Filter {
                peer: Some("peer-a".to_owned()),
                provider: Some("zen".to_owned()),
            },
            ..UiState::default()
        };
        state.apply(Action::ClearFilter, 10, 3);
        assert_eq!(state.filter, Filter::default());
    }

    #[test]
    fn cycling_the_provider_filter_wraps_and_resets_selection() {
        let providers = vec![
            "gemini".to_owned(),
            "zen".to_owned(),
            "openrouter".to_owned(),
        ];
        let mut state = UiState {
            selected: Some(2),
            detail: Some(true),
            scroll: 4,
            ..UiState::default()
        };
        state.cycle_provider(&providers);
        assert_eq!(state.filter.provider.as_deref(), Some("gemini"));
        assert_eq!(state.selected, None);
        assert_eq!(state.detail, None);
        assert_eq!(state.scroll, 0);
        state.cycle_provider(&providers);
        assert_eq!(state.filter.provider.as_deref(), Some("zen"));
        state.cycle_provider(&providers);
        assert_eq!(state.filter.provider.as_deref(), Some("openrouter"));
        state.cycle_provider(&providers);
        assert_eq!(state.filter.provider, None, "wraps back to all");
        state.cycle_provider(&providers);
        assert_eq!(state.filter.provider.as_deref(), Some("gemini"));
    }

    #[test]
    fn cycling_a_missing_provider_falls_back_to_all() {
        let providers = vec!["gemini".to_owned(), "zen".to_owned()];
        let mut state = UiState {
            filter: Filter {
                peer: None,
                provider: Some("gone".to_owned()),
            },
            ..UiState::default()
        };
        state.cycle_provider(&providers);
        assert_eq!(state.filter.provider, None);
    }

    #[test]
    fn the_key_map_covers_every_binding() {
        let cases = [
            (KeyCode::Char('q'), KeyModifiers::NONE, Some(Action::Quit)),
            (KeyCode::Esc, KeyModifiers::NONE, Some(Action::Quit)),
            (
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                Some(Action::Quit),
            ),
            (
                KeyCode::Char('j'),
                KeyModifiers::NONE,
                Some(Action::ScrollDown),
            ),
            (KeyCode::Down, KeyModifiers::NONE, Some(Action::ScrollDown)),
            (
                KeyCode::Char('k'),
                KeyModifiers::NONE,
                Some(Action::ScrollUp),
            ),
            (KeyCode::Up, KeyModifiers::NONE, Some(Action::ScrollUp)),
            (KeyCode::PageUp, KeyModifiers::NONE, Some(Action::PageUp)),
            (
                KeyCode::PageDown,
                KeyModifiers::NONE,
                Some(Action::PageDown),
            ),
            (KeyCode::Char('g'), KeyModifiers::NONE, Some(Action::Top)),
            (KeyCode::Home, KeyModifiers::NONE, Some(Action::Top)),
            (KeyCode::Char('G'), KeyModifiers::NONE, Some(Action::Bottom)),
            (KeyCode::End, KeyModifiers::NONE, Some(Action::Bottom)),
            (
                KeyCode::Char('p'),
                KeyModifiers::NONE,
                Some(Action::TogglePause),
            ),
            (KeyCode::Tab, KeyModifiers::NONE, Some(Action::NextPeer)),
            (KeyCode::BackTab, KeyModifiers::NONE, Some(Action::PrevPeer)),
            (
                KeyCode::Char('c'),
                KeyModifiers::NONE,
                Some(Action::ClearFilter),
            ),
            (
                KeyCode::Char('t'),
                KeyModifiers::NONE,
                Some(Action::ToggleStreams),
            ),
            (KeyCode::Char('x'), KeyModifiers::NONE, None),
            (
                KeyCode::Enter,
                KeyModifiers::NONE,
                Some(Action::ToggleDetail),
            ),
            (
                KeyCode::Char('e'),
                KeyModifiers::NONE,
                Some(Action::ToggleDetail),
            ),
            (
                KeyCode::Char('v'),
                KeyModifiers::NONE,
                Some(Action::ToggleMode),
            ),
            (
                KeyCode::Char('E'),
                KeyModifiers::NONE,
                Some(Action::ToggleMode),
            ),
            (
                KeyCode::Char('f'),
                KeyModifiers::NONE,
                Some(Action::CycleProvider),
            ),
            (
                KeyCode::Char('u'),
                KeyModifiers::NONE,
                Some(Action::ToggleMetric),
            ),
            (
                KeyCode::Char('w'),
                KeyModifiers::NONE,
                Some(Action::ToggleWindow),
            ),
            (
                KeyCode::Char('s'),
                KeyModifiers::NONE,
                Some(Action::ToggleStack),
            ),
            (
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                Some(Action::SelectFleet),
            ),
            (
                KeyCode::Char('o'),
                KeyModifiers::NONE,
                Some(Action::SortNext),
            ),
            (
                KeyCode::Char('O'),
                KeyModifiers::NONE,
                Some(Action::SortPrev),
            ),
            (
                KeyCode::Char('r'),
                KeyModifiers::NONE,
                Some(Action::SortReverse),
            ),
        ];
        for (code, modifiers, expected) in cases {
            let event = KeyEvent::new(code, modifiers);
            assert_eq!(map_key(event), expected, "{code:?} with {modifiers:?}");
        }
    }

    #[test]
    fn u_and_w_flip_the_gauge_metric_and_window_and_keep_the_rest() {
        let mut state = UiState::default();
        assert_eq!(state.gauge, GaugeMode::default());
        state.apply(Action::ToggleMetric, 0, 1);
        assert_eq!(state.gauge.metric, GaugeMetric::Tokens);
        assert_eq!(state.gauge.window, GaugeWindow::Hour);
        assert_eq!(state.chart_usage, UsageMetric::Limited);
        state.apply(Action::ToggleWindow, 0, 1);
        assert_eq!(state.gauge.window, GaugeWindow::Minutes);
        state.apply(Action::ToggleMetric, 0, 1);
        assert_eq!(state.gauge.metric, GaugeMetric::Requests);
        assert_eq!(state.chart_usage, UsageMetric::Tokens);
        state.apply(Action::ToggleWindow, 0, 1);
        assert_eq!(state.gauge, GaugeMode::default());
        state.apply(Action::ToggleMetric, 0, 1);
        assert_eq!(state.chart_usage, UsageMetric::Requests, "wraps");
    }

    #[test]
    fn the_matrix_hints_name_the_new_keys_on_one_line_within_100_columns() {
        assert!(!HINTS_MATRIX.contains('\n'));
        assert!(HINTS_MATRIX.contains("u metric"), "{HINTS_MATRIX:?}");
        assert!(HINTS_MATRIX.contains("w window"), "{HINTS_MATRIX:?}");
        assert!(HINTS_MATRIX.contains("m chart"), "{HINTS_MATRIX:?}");
        assert!(HINTS_MATRIX.contains("a all"), "{HINTS_MATRIX:?}");
        assert!(HINTS_MATRIX.contains("t streams"), "{HINTS_MATRIX:?}");
        assert!(HINTS_MATRIX.contains("? keys"), "{HINTS_MATRIX:?}");
        assert!(
            fw_tui::text::display_width(HINTS_MATRIX) <= 98,
            "fits the panel"
        );
    }

    fn sort_columns() -> Vec<PeerColumn> {
        vec![PeerColumn::Name, PeerColumn::Version, PeerColumn::Load]
    }

    #[test]
    fn o_steps_through_the_columns_and_back_to_unsorted() {
        let columns = sort_columns();
        let mut state = UiState::default();
        let mut seen = Vec::new();
        for _ in 0..4 {
            state.cycle_sort(&columns, true);
            seen.push(state.peer_sort.clone());
        }
        let sort = |column: PeerColumn| {
            Some(PeerSort {
                column,
                descending: false,
            })
        };
        assert_eq!(
            seen,
            [
                sort(PeerColumn::Name),
                sort(PeerColumn::Version),
                sort(PeerColumn::Load),
                None,
            ],
        );
    }

    #[test]
    fn capital_o_steps_backwards_and_starts_from_the_last_column() {
        let columns = sort_columns();
        let mut state = UiState::default();
        state.cycle_sort(&columns, false);
        assert_eq!(
            state.peer_sort.as_ref().map(|s| &s.column),
            Some(&PeerColumn::Load)
        );
        state.cycle_sort(&columns, false);
        assert_eq!(
            state.peer_sort.as_ref().map(|s| &s.column),
            Some(&PeerColumn::Version)
        );
        state.cycle_sort(&columns, false);
        assert_eq!(
            state.peer_sort.as_ref().map(|s| &s.column),
            Some(&PeerColumn::Name)
        );
        state.cycle_sort(&columns, false);
        assert_eq!(state.peer_sort, None);
    }

    #[test]
    fn a_sort_on_a_vanished_column_steps_from_unsorted() {
        let mut state = UiState {
            peer_sort: Some(PeerSort {
                column: PeerColumn::Provider("gone".to_owned()),
                descending: true,
            }),
            ..UiState::default()
        };
        state.cycle_sort(&sort_columns(), true);
        assert_eq!(
            state.peer_sort,
            Some(PeerSort {
                column: PeerColumn::Name,
                descending: false,
            }),
        );
    }

    #[test]
    fn r_reverses_the_sort_and_does_nothing_while_unsorted() {
        let mut state = UiState::default();
        assert!(!state.reverse_sort(), "nothing to reverse");
        assert_eq!(state.peer_sort, None);
        state.cycle_sort(&sort_columns(), true);
        assert!(state.reverse_sort());
        assert!(state.peer_sort.as_ref().is_some_and(|s| s.descending));
        assert!(state.reverse_sort());
        assert!(state.peer_sort.as_ref().is_some_and(|s| !s.descending));
        state.cycle_sort(&sort_columns(), true);
        assert!(
            state.peer_sort.as_ref().is_some_and(|s| !s.descending),
            "moving to a column sorts it ascending"
        );
    }

    #[test]
    fn o_and_o_step_through_the_stream_columns_and_back_to_default() {
        let columns = stream_columns();
        let mut state = UiState::default();
        let mut seen = Vec::new();
        for _ in 0..columns.len() + 1 {
            state.cycle_stream_sort(&columns, true);
            seen.push(state.stream_sort);
        }
        let sort = |column| {
            Some(StreamSort {
                column,
                descending: false,
            })
        };
        assert_eq!(
            seen,
            [
                sort(StreamColumn::Name),
                sort(StreamColumn::Usage),
                sort(StreamColumn::Chain),
                sort(StreamColumn::Rate),
                sort(StreamColumn::Expiry),
                None,
            ]
        );
        state.cycle_stream_sort(&columns, false);
        assert_eq!(state.stream_sort, sort(StreamColumn::Expiry));
        for _ in 0..columns.len() - 1 {
            state.cycle_stream_sort(&columns, false);
        }
        assert_eq!(state.stream_sort, sort(StreamColumn::Name));
        state.cycle_stream_sort(&columns, false);
        assert_eq!(state.stream_sort, None, "past the first is default order");
    }

    #[test]
    fn r_reverses_the_stream_sort_only_while_sorted() {
        let mut state = UiState::default();
        assert!(!state.reverse_stream_sort());
        state.cycle_stream_sort(&stream_columns(), true);
        assert!(state.reverse_stream_sort());
        assert_eq!(
            state.stream_sort,
            Some(StreamSort {
                column: StreamColumn::Name,
                descending: true,
            })
        );
        state.cycle_stream_sort(&stream_columns(), true);
        assert_eq!(
            state.stream_sort,
            Some(StreamSort {
                column: StreamColumn::Usage,
                descending: false,
            }),
            "moving to a column sorts it ascending"
        );
    }

    #[test]
    fn key_releases_are_ignored_even_for_quit_keys() {
        for (code, modifiers) in [
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Char('j'), KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
            (KeyCode::Esc, KeyModifiers::NONE),
        ] {
            let event = KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Release);
            assert_eq!(map_key(event), None, "{code:?} release");
        }
    }

    #[test]
    fn the_default_screen_is_the_matrix_with_an_automatic_detail_pane() {
        let state = UiState::default();
        assert_eq!(state.mode, Mode::Matrix);
        assert!(state.detail_open(true), "a problem peer opens it");
        assert!(!state.detail_open(false));
    }

    #[test]
    fn moving_the_selection_opens_the_detail_of_a_healthy_peer() {
        let mut state = UiState::default();
        assert!(!state.detail_open(false), "nothing chosen yet");
        state.selected = Some(1);
        assert!(state.detail_open(false), "following the selection");
        state.toggle_detail(false);
        assert!(!state.detail_open(false), "Enter closes it");
    }

    #[test]
    fn enter_toggles_the_detail_pane_against_what_is_showing() {
        let mut state = UiState::default();
        state.toggle_detail(true);
        assert_eq!(state.detail, Some(false), "closing an auto-open pane");
        assert!(!state.detail_open(true));
        state.toggle_detail(true);
        assert!(state.detail_open(true), "reopens");
        let mut quiet = UiState::default();
        quiet.toggle_detail(false);
        assert!(quiet.detail_open(false), "opens a healthy peer on request");
    }

    #[test]
    fn v_switches_between_matrix_and_full_and_resets_scroll() {
        let mut state = UiState {
            scroll: 5,
            ..UiState::default()
        };
        state.toggle_mode();
        assert_eq!(state.mode, Mode::Full);
        assert_eq!(state.scroll, 0, "each view starts at the top");
        state.toggle_mode();
        assert_eq!(state.mode, Mode::Matrix);
    }

    #[test]
    fn t_opens_the_streams_view_and_returns_to_the_matrix() {
        let mut state = UiState {
            scroll: 5,
            ..UiState::default()
        };
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)),
            Some(Action::ToggleStreams)
        );
        state.apply(Action::ToggleStreams, 100, 10);
        assert_eq!(state.mode, Mode::Streams);
        assert_eq!(state.scroll, 0);
        assert_eq!(state.selected_stream, None, "the first row is the default");
        state.apply(Action::ToggleStreams, 100, 10);
        assert_eq!(state.mode, Mode::Matrix);
    }

    /// The data one matrix frame is drawn from, owned so tests can lend it out.
    struct Fixture {
        providers: Vec<String>,
        matrix: Vec<view::Line>,
        rows: usize,
        cooling: Vec<view::Line>,
        detail: Option<Vec<view::Line>>,
        chart_title: String,
        chart: Vec<ChartSeries>,
        chart_interval: Duration,
    }

    impl Fixture {
        fn sample() -> Self {
            Self {
                providers: vec!["gemini".to_owned(), "zen".to_owned()],
                matrix: vec![
                    line("Fleet 2/2 fresh   gemini 2/2   zen 1/2"),
                    line("  peer-a  ■ ······"),
                    line("  peer-b  · ◐ ▁▂▃"),
                ],
                rows: 2,
                cooling: Vec::new(),
                detail: None,
                chart_title: "Load · fleet".to_owned(),
                chart: vec![ChartSeries {
                    name: "load".to_owned(),
                    points: vec![Some(1.0), Some(2.0), Some(3.0)],
                    color: Color::Cyan,
                }],
                chart_interval: Duration::from_secs(1),
            }
        }

        fn frame(&self) -> MatrixFrame<'_> {
            MatrixFrame {
                version: "9.8.7",
                clock: "12:34:56Z",
                status: "2 of 2 peers fresh",
                providers: &self.providers,
                matrix: &self.matrix,
                rows: self.rows,
                selected: None,
                cooling: &self.cooling,
                detail: self.detail.as_deref(),
                chart_title: &self.chart_title,
                chart_series: &self.chart,
                chart_interval: self.chart_interval,
                keys: [5, 2, 1],
            }
        }
    }

    fn rendered_matrix(fixture: &Fixture, state: &UiState, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("a test terminal");
        terminal
            .draw(|frame| draw_matrix(frame, &fixture.frame(), state))
            .expect("drawing succeeds");
        terminal.backend().buffer().clone()
    }

    fn all_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|r| row_text(buffer, r))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_matrix_frame_draws_header_tabs_peers_and_footer_at_100x30() {
        let buffer = rendered_matrix(&Fixture::sample(), &UiState::default(), 100, 30);
        let title = row_text(&buffer, 0);
        assert!(title.contains("reiki top v9.8.7"), "{title:?}");
        assert!(title.contains("12:34:56Z"), "{title:?}");
        assert!(row_text(&buffer, 1).contains("live · 2 of 2 peers fresh"));
        assert!(row_text(&buffer, 2).contains("t streams"), "hints");
        let tabs = row_text(&buffer, 4);
        assert!(
            tabs.contains("All") && tabs.contains("gemini") && tabs.contains("zen"),
            "{tabs:?}"
        );
        let text = all_text(&buffer);
        assert!(text.contains("Peers"), "{text}");
        assert!(text.contains("Keys"), "charts at 30 rows: {text}");
        assert!(text.contains("Load · fleet"), "{text}");
        let footer = row_text(&buffer, 29);
        assert!(footer.contains("[□ detail]"), "{footer:?}");
        assert!(footer.contains("[req 1h]"), "{footer:?}");
        assert!(footer.contains("[chart: load]"), "{footer:?}");
        assert!(
            !footer.contains("paused") && !footer.contains("filter"),
            "{footer:?}"
        );
    }

    #[test]
    fn the_tab_and_footer_follow_the_provider_filter_and_pause() {
        let state = UiState {
            paused: true,
            filter: Filter {
                peer: None,
                provider: Some("zen".to_owned()),
            },
            detail: Some(true),
            ..UiState::default()
        };
        let mut fixture = Fixture::sample();
        fixture.detail = Some(vec![line("peer-a card")]);
        let buffer = rendered_matrix(&fixture, &state, 100, 30);
        let tabs = row_text(&buffer, 4);
        let zen_at = tabs.find("zen").expect("zen tab");
        assert!(tabs.contains("All"));
        assert!(tabs[zen_at..].starts_with("zen"));
        let footer = row_text(&buffer, 29);
        assert!(footer.contains("[■ detail]"), "{footer:?}");
        assert!(footer.contains("[filter: zen]"), "{footer:?}");
        assert!(footer.contains("[paused]"), "{footer:?}");
    }

    #[test]
    fn the_cooling_panel_appears_only_with_a_cooling_key() {
        let closed = rendered_matrix(&Fixture::sample(), &UiState::default(), 100, 30);
        assert!(!all_text(&closed).contains("Cooling"));
        let mut cooling = Fixture::sample();
        cooling.cooling = vec![line("  peer-b  zen  ◐ cooling  [▁▂]  45s")];
        let open = rendered_matrix(&cooling, &UiState::default(), 100, 30);
        let text = all_text(&open);
        assert!(text.contains("Cooling"), "{text}");
        assert!(text.contains("peer-b  zen  ◐ cooling"), "{text}");
    }

    #[test]
    fn the_chart_row_needs_twenty_eight_rows() {
        let fixture = Fixture::sample();
        let short = all_text(&rendered_matrix(&fixture, &UiState::default(), 100, 27));
        assert!(!short.contains("Keys"), "no chart row at 27 rows");
        assert!(!short.contains("Load · fleet"));
        let tall = all_text(&rendered_matrix(&fixture, &UiState::default(), 100, 28));
        assert!(tall.contains("Keys") && tall.contains("Load · fleet"));
    }

    #[test]
    fn an_open_peer_card_takes_the_chart_row_and_keeps_its_place() {
        let mut fixture = Fixture::sample();
        fixture.detail = Some(vec![
            line("peer-b  instance peer-b"),
            line("    b  cooling"),
        ]);
        let state = UiState::default();
        let buffer = rendered_matrix(&fixture, &state, 40, 20);
        // Body is 19 rows: header 4, tabs 1, the card's third (6) at the bottom.
        assert!(
            row_text(&buffer, 13).starts_with('╭'),
            "{:?}",
            row_text(&buffer, 13)
        );
        assert!(row_text(&buffer, 13).contains("Detail"));
        assert_eq!(
            row_text(&buffer, 14),
            format!("│{:<38}│", "peer-b  instance peer-b")
        );
        assert!(
            !all_text(&buffer).contains("Keys"),
            "the card replaces the charts"
        );
        fixture.detail = None;
        let closed = rendered_matrix(&fixture, &state, 40, 20);
        assert!(!(0..20).any(|r| row_text(&closed, r).contains("Detail")));
    }

    #[test]
    fn the_detail_scroll_shows_its_own_window() {
        let mut fixture = Fixture::sample();
        fixture.detail = Some(numbered(12));
        let state = UiState {
            scroll: 4,
            ..UiState::default()
        };
        let buffer = rendered_matrix(&fixture, &state, 40, 20);
        assert_eq!(row_text(&buffer, 14), format!("│{:<38}│", "line 4"));
        let far = UiState {
            scroll: 99,
            ..UiState::default()
        };
        let buffer = rendered_matrix(&fixture, &far, 40, 20);
        // The pane has four inner rows, from row 14; the last page starts at 12 - 4.
        assert_eq!(
            row_text(&buffer, 17),
            format!("│{:<38}│", "line 11"),
            "clamped to the last page"
        );
    }

    #[test]
    fn the_help_overlay_lists_the_keys_and_any_key_closes_it() {
        let state = UiState {
            help: true,
            ..UiState::default()
        };
        let text = all_text(&rendered_matrix(&Fixture::sample(), &state, 100, 30));
        assert!(text.contains(" Keys "), "{text}");
        assert!(text.contains("any key closes it"), "{text}");
        let full = all_text(&rendered(&numbered(3), &state, "ok", 100, 30));
        assert!(full.contains(" Keys "), "{full}");

        let mut open = state.clone();
        let x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(dismiss_help(&mut open, x));
        assert!(!open.help);
        let mut open = state.clone();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!dismiss_help(&mut open, ctrl_c), "Ctrl-C still quits");
        assert!(open.help);
        let mut closed = UiState::default();
        assert!(!dismiss_help(&mut closed, x));
    }

    #[test]
    fn question_mark_and_m_map_to_the_overlay_and_the_chart() {
        let q = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        assert_eq!(map_key(q), Some(Action::ToggleHelp));
        let m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);
        assert_eq!(map_key(m), Some(Action::CycleChart));
        let mut state = UiState::default();
        state.apply(Action::ToggleHelp, 0, 1);
        assert!(state.help);
        state.apply(Action::ToggleHelp, 0, 1);
        assert!(!state.help);
        let order: Vec<ChartMetric> = (0..5)
            .map(|_| {
                let before = state.chart;
                state.apply(Action::CycleChart, 0, 1);
                before
            })
            .collect();
        assert_eq!(
            order,
            [
                ChartMetric::Load,
                ChartMetric::Disk,
                ChartMetric::Sessions,
                ChartMetric::Requests,
                ChartMetric::Load,
            ]
        );
    }

    #[test]
    fn s_stacks_and_a_returns_to_the_fleet_row() {
        let s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(map_key(s), Some(Action::ToggleStack));
        assert_eq!(map_key(a), Some(Action::SelectFleet));
        let mut state = UiState {
            selected: Some(3),
            detail: Some(true),
            scroll: 4,
            ..UiState::default()
        };
        state.apply(Action::ToggleStack, 10, 3);
        assert!(state.stacked);
        state.apply(Action::ToggleStack, 10, 3);
        assert!(!state.stacked);
        state.apply(Action::SelectFleet, 10, 3);
        assert_eq!(state.selected, Some(0));
        assert_eq!(state.detail, None);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn bands_drop_charts_then_cooling_then_tabs_then_fold_the_header() {
        // Three peers, so the bare matrix is five rows with its border.
        for height in 0..=60 {
            let b = bands(height, 3, 3, false);
            let total = u32::from(b.header)
                + u32::from(b.tabs)
                + u32::from(b.peers)
                + u32::from(b.cooling)
                + u32::from(b.charts)
                + u32::from(b.footer);
            assert!(total <= u32::from(height), "{height}: {b:?}");
            if height >= CHARTS_MIN_HEIGHT {
                assert_eq!(b.charts, CHART_ROWS, "{height}");
            } else {
                assert_eq!(b.charts, 0, "{height}");
            }
            if b.tabs == 0 {
                assert_eq!(
                    (b.cooling, b.charts),
                    (0, 0),
                    "tabs go after the panels: {height}"
                );
            }
            if b.header == 1 {
                assert_eq!(b.tabs, 0, "the header folds after the tabs: {height}");
            }
            if b.header == HEADER_ROWS && height >= 12 {
                assert!(b.peers >= 5, "the whole matrix shows at {height}: {b:?}");
            }
        }
        assert_eq!(
            bands(27, 3, 3, false).cooling,
            5,
            "the cooling panel stays at 27 rows"
        );
        assert_eq!(
            bands(12, 3, 3, false).cooling,
            0,
            "and goes before the matrix is cut"
        );
        assert_eq!(bands(12, 3, 3, false).peers, 6);
        assert_eq!(bands(0, 0, 0, true), Bands::default());
    }

    #[test]
    fn a_fleet_too_tall_for_the_panels_loses_them_and_clips_the_matrix_last() {
        let b = bands(30, 40, 3, false);
        assert_eq!((b.charts, b.cooling, b.tabs, b.header), (0, 0, 0, 1));
        assert_eq!(b.peers, 28, "the Peers panel takes the rest");
    }

    #[test]
    fn utc_clock_formats_the_time_of_day() {
        assert_eq!(utc_clock(0), "00:00:00Z");
        assert_eq!(utc_clock(3661), "01:01:01Z");
        assert_eq!(utc_clock(86_400 + 5), "00:00:05Z");
        assert_eq!(utc_clock(u64::MAX).len(), "00:00:00Z".len());
    }

    fn requests_fixture() -> Fixture {
        let mut fixture = Fixture::sample();
        fixture.chart_title = "Requests · fleet · req/interval".to_owned();
        fixture.chart_interval = Duration::from_secs(60);
        let latest = |value: f64| {
            (0..12)
                .map(|i| Some(if i == 11 { value } else { 0.0 }))
                .collect()
        };
        fixture.chart = vec![
            ChartSeries {
                name: "zen".to_owned(),
                points: (0..12)
                    .map(|i| Some(if i == 11 { 12.0 } else { 1.0 }))
                    .collect(),
                color: Color::Cyan,
            },
            ChartSeries {
                name: "openrouter".to_owned(),
                points: latest(3.0),
                color: Color::Green,
            },
            ChartSeries {
                name: "sample1".to_owned(),
                points: latest(1.0),
                color: Color::Yellow,
            },
        ];
        fixture
    }

    fn symbol_colors(buffer: &Buffer) -> Vec<Color> {
        buffer
            .content()
            .iter()
            .filter(|cell| "●◆▲■○◇△□".contains(cell.symbol()) && cell.fg != Color::Reset)
            .map(|cell| cell.fg)
            .collect()
    }

    #[test]
    fn the_requests_chart_names_every_provider_with_distinct_colours_and_a_real_axis() {
        let buffer = rendered_matrix(&requests_fixture(), &UiState::default(), 100, 30);
        let text = all_text(&buffer);
        assert!(text.contains("Requests · fleet · req/interval"), "{text}");
        assert!(text.contains("■ zen 12"), "legend: {text}");
        assert!(text.contains("■ openrouter 3"), "legend: {text}");
        assert!(text.contains("■ sample1 1"), "legend: {text}");
        assert!(text.contains("-12m"), "x axis: {text}");
        assert!(text.contains("now"), "x axis: {text}");
        assert!(text.contains("0"), "y baseline: {text}");
        let colors = symbol_colors(&buffer);
        for color in [Color::Cyan, Color::Green, Color::Yellow] {
            assert!(colors.contains(&color), "{color:?} missing: {colors:?}");
        }
        let distinct: std::collections::HashSet<Color> = colors.into_iter().collect();
        assert!(distinct.len() >= 3, "one colour per provider: {distinct:?}");
    }

    #[test]
    fn all_and_peer_selections_draw_distinct_chart_titles() {
        let mut fixture = requests_fixture();
        fixture.matrix = vec![
            line("Fleet 2/3 fresh"),
            line("> ALL     ■ ······"),
            line("  peer-a  ■ ······"),
        ];
        fixture.rows = 2;
        fixture.chart_title = "Requests · fleet · req/interval · fleet 2 of 3 fresh".to_owned();
        let fleet = all_text(&rendered_matrix(&fixture, &UiState::default(), 100, 30));
        assert!(fleet.contains("Requests · fleet"), "{fleet}");
        assert!(fleet.contains("> ALL"), "{fleet}");
        assert!(fleet.contains("fleet 2 of 3 fresh"), "{fleet}");

        fixture.matrix[1] = line("  ALL     ■ ······");
        fixture.matrix[2] = line("> peer-a  ■ ······");
        fixture.chart_title = "Requests · peer-a · req/interval".to_owned();
        let peer = all_text(&rendered_matrix(&fixture, &UiState::default(), 100, 30));
        assert!(peer.contains("Requests · peer-a"), "{peer}");
        assert!(peer.contains("> peer-a"), "{peer}");
        assert!(!peer.contains("fleet 2 of 3 fresh"), "{peer}");
    }

    #[test]
    fn s_switches_the_requests_chart_from_lines_to_a_stacked_total() {
        let fixture = requests_fixture();
        let overlaid = all_text(&rendered_matrix(&fixture, &UiState::default(), 100, 30));
        assert!(overlaid.contains('●'), "overlaid points: {overlaid}");
        let overlaid_blocks = overlaid.matches('█').count();
        let state = UiState {
            stacked: true,
            ..UiState::default()
        };
        let stacked = all_text(&rendered_matrix(&fixture, &state, 100, 30));
        assert!(
            stacked.matches('█').count() > overlaid_blocks,
            "stacked segments: {stacked}"
        );
        assert!(!stacked.contains('●'), "the lines are replaced: {stacked}");
    }

    #[test]
    fn a_single_provider_draws_its_legend_and_all_zero_series_sits_on_the_baseline() {
        let mut fixture = requests_fixture();
        fixture.chart = vec![ChartSeries {
            name: "zen".to_owned(),
            points: vec![Some(0.0); 12],
            color: Color::Cyan,
        }];
        let buffer = rendered_matrix(&fixture, &UiState::default(), 100, 30);
        let text = all_text(&buffer);
        assert!(text.contains("■ zen 0"), "{text}");
        assert!(
            text.contains('─'),
            "an all-zero chart still draws its baseline: {text}"
        );
        assert!(text.contains("0"), "{text}");
    }

    #[test]
    fn the_requests_chart_draws_from_twenty_to_forty_columns_without_panicking() {
        let fixture = requests_fixture();
        for width in 20..=40 {
            let buffer = rendered_matrix(&fixture, &UiState::default(), width, 30);
            assert_eq!(buffer.area.width, width);
        }
    }

    #[test]
    fn every_matrix_size_draws_without_panicking() {
        let mut fixture = Fixture::sample();
        fixture.cooling = numbered(9);
        let card = numbered(12);
        let states = [
            UiState::default(),
            UiState {
                paused: true,
                help: true,
                detail: Some(true),
                ..UiState::default()
            },
        ];
        for (width, height) in [
            (0, 0),
            (1, 1),
            (20, 5),
            (60, 12),
            (100, 30),
            (200, 60),
            (1, 40),
            (40, 1),
        ] {
            for state in &states {
                rendered_matrix(&fixture, state, width, height);
                fixture.detail = Some(card.clone());
                rendered_matrix(&fixture, state, width, height);
                fixture.detail = None;
                let buffer = rendered(&numbered(3), state, "ok", width, height);
                assert_eq!(buffer.area.height, height);
            }
        }
    }

    #[test]
    fn tiny_matrix_screens_do_not_panic() {
        let mut fixture = Fixture::sample();
        fixture.detail = Some(numbered(4));
        for (width, height) in [(0, 0), (1, 1), (3, 2), (2, 1), (40, 1), (40, 4), (40, 9)] {
            let buffer = rendered_matrix(&fixture, &UiState::default(), width, height);
            assert_eq!(buffer.area.width, width);
            assert_eq!(buffer.area.height, height);
        }
    }

    #[test]
    fn three_lines_draw_in_a_40x6_terminal() {
        let lines = vec![line("one"), line("two"), line("three")];
        let buffer = rendered(&lines, &UiState::default(), "ok", 40, 6);
        assert_eq!(row_text(&buffer, 0), format!("┌{}┐", "─".repeat(38)));
        for (row, text) in [(1, "one"), (2, "two"), (3, "three")] {
            assert_eq!(row_text(&buffer, row), format!("│{text:<38}│"));
        }
        assert_eq!(row_text(&buffer, 4), format!("└{}┘", "─".repeat(38)));
        let footer: String = format!("ok  {HINTS}").chars().take(40).collect();
        assert_eq!(row_text(&buffer, 5), footer);
    }

    #[test]
    fn the_scroll_window_shows_the_right_slice() {
        let lines = numbered(10);
        let state = UiState {
            scroll: 4,
            ..UiState::default()
        };
        let buffer = rendered(&lines, &state, "ok", 40, 6);
        for (row, n) in [(1, 4), (2, 5), (3, 6)] {
            assert_eq!(
                row_text(&buffer, row),
                format!("│{:<38}│", format!("line {n}"))
            );
        }

        let last_page = UiState {
            scroll: 7,
            ..UiState::default()
        };
        let buffer = rendered(&lines, &last_page, "ok", 40, 6);
        for (row, n) in [(1, 7), (2, 8), (3, 9)] {
            assert_eq!(
                row_text(&buffer, row),
                format!("│{:<38}│", format!("line {n}"))
            );
        }
    }

    #[test]
    fn an_out_of_range_scroll_still_shows_the_last_page() {
        let lines = numbered(10);
        let state = UiState {
            scroll: 99,
            ..UiState::default()
        };
        let buffer = rendered(&lines, &state, "ok", 40, 6);
        assert_eq!(row_text(&buffer, 1), format!("│{:<38}│", "line 7"));
        assert_eq!(row_text(&buffer, 3), format!("│{:<38}│", "line 9"));
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for (width, height) in [(0, 0), (1, 1), (3, 2), (2, 1)] {
            let buffer = rendered(&numbered(3), &UiState::default(), "ok", width, height);
            assert_eq!(buffer.area.width, width);
            assert_eq!(buffer.area.height, height);
        }
    }

    #[test]
    fn empty_lines_draw_only_the_frame_and_footer() {
        let buffer = rendered(&[], &UiState::default(), "ok", 40, 6);
        assert_eq!(row_text(&buffer, 1), format!("│{}│", " ".repeat(38)));
        assert!(row_text(&buffer, 5).starts_with("ok  q quit"));
    }

    #[test]
    fn the_paused_marker_is_visible() {
        let state = UiState {
            paused: true,
            ..UiState::default()
        };
        let buffer = rendered(&numbered(3), &state, "ok", 40, 6);
        assert!(
            row_text(&buffer, 5).contains("(paused)"),
            "paused marker in {:?}",
            row_text(&buffer, 5)
        );
        let running = rendered(&numbered(3), &UiState::default(), "ok", 40, 6);
        assert!(!row_text(&running, 5).contains("(paused)"));
    }

    #[test]
    fn the_footer_shows_the_status_and_the_key_hints() {
        let buffer = rendered(&numbered(3), &UiState::default(), "Fleet: 2 peers", 60, 6);
        let footer = row_text(&buffer, 5);
        assert!(footer.starts_with("Fleet: 2 peers"), "{footer:?}");
        assert!(footer.contains("q quit"), "{footer:?}");
        assert!(footer.contains("j/k scroll"), "{footer:?}");
        assert!(footer.contains("p pause"), "{footer:?}");
        assert!(footer.contains("Tab next peer"), "{footer:?}");
    }

    fn rendered_streams(
        frame: &StreamsFrame<'_>,
        state: &UiState,
        width: u16,
        height: u16,
    ) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("a test terminal");
        terminal
            .draw(|f| draw_streams(f, frame, state))
            .expect("drawing succeeds");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn the_streams_screen_renders_both_panes_without_a_token_value() {
        let secret = ["rsk", "_", &"d".repeat(64)].concat();
        let mut snapshot = crate::streams::StreamsView::from_json(&format!(
            r#"{{
              "enabled": true,
              "streams": [
                {{"name":"quiet","dynamic":false,"chain":["zen"],"limits":{{"rpm":30}},
                  "tokens":[],"rpm":{{"current":1}},"concurrency":{{"current":0}},
                  "last_used":1000,"state":"active"}},
                {{"name":"busy","dynamic":true,"chain":["zen"],"limits":{{"rpm":10}},
                  "tokens":[{{"id":"abcdef1234567890","expires_at":1900}}],"expiry":1900,
                  "rpm":{{"current":9,"limit":10}},"concurrency":{{"current":2,"limit":2}},
                  "last_used":1150,"state":"active","token":"{secret}"}}
              ],
              "audit":[{{"seq":1,"time":"2026-11-23T00:01:00Z","action":"create",
                "stream":"busy","outcome":"success","reason":"setup","changes":[]}}]
            }}"#
        ))
        .expect("fixture parses");
        let table = view::streams_table(&snapshot, Some("busy"), None, 1155, 60);
        let detail = view::stream_detail(&snapshot, Some("busy"), 1155, 40);
        let status = view::streams_status(&snapshot, None, 100);
        let frame = StreamsFrame {
            table: &table,
            detail: &detail,
            status: &status,
            selected: Some(0),
        };
        let buffer = rendered_streams(&frame, &UiState::default(), 100, 24);
        let text = all_text(&buffer);
        assert!(text.contains("Streams"), "the table pane has a title");
        assert!(text.contains("busy"), "the sorted table renders");
        assert!(text.contains("abcdef12"), "a short credential id renders");
        assert!(text.contains("create"), "recent audit renders");
        assert!(!text.contains(&secret), "no token value on screen");
        assert!(text.contains("t matrix"), "the Streams hints name t");
        assert!(
            text.contains("o sort"),
            "the Streams hints name the sort key"
        );

        snapshot.enabled = false;
        let status = view::streams_status(&snapshot, Some("admin unreachable"), 100);
        let frame = StreamsFrame {
            status: &status,
            ..frame
        };
        let text = all_text(&rendered_streams(&frame, &UiState::default(), 100, 24));
        assert!(
            text.contains("stream_admin is disabled"),
            "clear disabled message"
        );
        assert!(
            text.contains("admin unreachable"),
            "degraded refresh is visible"
        );
        assert!(!text.contains(&secret));
    }
}
