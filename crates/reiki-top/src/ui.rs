//! Ratatui drawing, state and key mapping for `reiki top`. Compiled only with the `tui`
//! feature; everything here is still pure except [`draw`], which writes into a frame.

pub use crossterm;
pub use ratatui;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style as RatStyle};
use ratatui::text::{Line as RatLine, Span as RatSpan};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::view::{self, GaugeMetric, GaugeMode, GaugeWindow, Span, Style};

/// The key hints on the footer's right side, full view.
const HINTS: &str = "q quit  j/k scroll  p pause  Tab next peer  c all  v matrix";
/// The key hints on the footer's right side, matrix view.
const HINTS_MATRIX: &str =
    "q quit  j/k peer  Enter detail  PgUp/PgDn  f provider  u metric  w window  p pause  v full view";

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
    /// Cycles the matrix provider filter: all, then each column, then all.
    CycleProvider,
    /// Switches the matrix gauge between requests and tokens.
    ToggleMetric,
    /// Switches the matrix gauge between the last hour and the last five minutes.
    ToggleWindow,
}

/// Which screen the live view shows. The matrix is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Matrix,
    /// The per-peer text view, with the Tab peer filter.
    Full,
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
            Mode::Full => Mode::Matrix,
        };
        self.scroll = 0;
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
            // Needs the selected peer's problem flag, so the caller calls `toggle_detail`.
            Action::ToggleDetail => {}
            // Needs the current provider columns; the caller cycles the filter.
            Action::CycleProvider => {}
            Action::ToggleMetric => {
                self.gauge.metric = match self.gauge.metric {
                    GaugeMetric::Requests => GaugeMetric::Tokens,
                    GaugeMetric::Tokens => GaugeMetric::Requests,
                };
            }
            Action::ToggleWindow => {
                self.gauge.window = match self.gauge.window {
                    GaugeWindow::Hour => GaugeWindow::Minutes,
                    GaugeWindow::Minutes => GaugeWindow::Hour,
                };
            }
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
        KeyCode::Char('f') => Action::CycleProvider,
        KeyCode::Char('u') => Action::ToggleMetric,
        KeyCode::Char('w') => Action::ToggleWindow,
        _ => return None,
    };
    Some(action)
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
}

/// The detail pane's total height for a terminal of `screen_height` rows: a third of the body
/// above the footer, or none when that would be too short to hold a border and a line.
pub fn detail_height(screen_height: u16) -> u16 {
    let third = screen_height.saturating_sub(1) / 3;
    if third >= 3 {
        third
    } else {
        0
    }
}

/// Draws the matrix over the body, then the selected peer's detail card in a bordered pane
/// when `detail` is given. `state.scroll` scrolls the detail pane; the matrix is not scrolled.
pub fn draw_matrix(
    frame: &mut Frame,
    lines: &[view::Line],
    detail: Option<&[view::Line]>,
    state: &UiState,
    status: &str,
) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let body_height = area.height.saturating_sub(1);
    let footer_y = area.y.saturating_add(body_height);
    let detail_rows = detail.map_or(0, |_| detail_height(area.height));

    if body_height > 0 {
        let top_rows = body_height.saturating_sub(detail_rows);
        if top_rows > 0 {
            let top = Rect::new(area.x, area.y, area.width, top_rows);
            frame.render_widget(Paragraph::new(to_ratatui(lines)), top);
        }
        if let (Some(card), true) = (detail, detail_rows > 0) {
            let pane = Rect::new(
                area.x,
                area.y.saturating_add(top_rows),
                area.width,
                detail_rows,
            );
            let block = Block::default().borders(Borders::ALL);
            let inner = block.inner(pane);
            frame.render_widget(block, pane);
            if inner.width > 0 && inner.height > 0 {
                let viewport = usize::from(inner.height);
                let start = state.scroll.min(max_scroll(card.len(), viewport));
                let end = start.saturating_add(viewport).min(card.len());
                frame.render_widget(Paragraph::new(to_ratatui(&card[start..end])), inner);
            }
        }
    }

    frame.render_widget(
        Paragraph::new(footer_text(state, status, HINTS_MATRIX)),
        Rect::new(area.x, footer_y, area.width, 1),
    );
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
    use crossterm::event::KeyEvent;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

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
        state.apply(Action::ToggleWindow, 0, 1);
        assert_eq!(state.gauge.window, GaugeWindow::Minutes);
        state.apply(Action::ToggleMetric, 0, 1);
        assert_eq!(state.gauge.metric, GaugeMetric::Requests);
        state.apply(Action::ToggleWindow, 0, 1);
        assert_eq!(state.gauge, GaugeMode::default());
    }

    #[test]
    fn the_matrix_footer_names_the_gauge_keys_on_one_line() {
        assert!(!HINTS_MATRIX.contains('\n'));
        assert!(HINTS_MATRIX.contains("u metric"), "{HINTS_MATRIX:?}");
        assert!(HINTS_MATRIX.contains("w window"), "{HINTS_MATRIX:?}");
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

    fn rendered_matrix(
        lines: &[view::Line],
        detail: Option<&[view::Line]>,
        state: &UiState,
        status: &str,
        width: u16,
        height: u16,
    ) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("a test terminal");
        terminal
            .draw(|frame| draw_matrix(frame, lines, detail, state, status))
            .expect("drawing succeeds");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn the_matrix_footer_hints_the_matrix_keys() {
        let buffer = rendered_matrix(&numbered(1), None, &UiState::default(), "ok", 100, 8);
        let footer = row_text(&buffer, 7);
        assert!(footer.contains("Enter detail"), "{footer:?}");
        assert!(footer.contains("v full view"), "{footer:?}");
    }

    #[test]
    fn the_detail_pane_is_a_bordered_third_under_the_matrix() {
        let rows = numbered(3);
        let card = vec![line("peer-b  instance peer-b"), line("    b  cooling")];
        let buffer = rendered_matrix(&rows, Some(&card), &UiState::default(), "ok", 40, 20);
        // Body is 19 rows; the pane takes 19 / 3 = 6, so its top border is row 13.
        assert_eq!(row_text(&buffer, 13), format!("┌{}┐", "─".repeat(38)));
        assert_eq!(
            row_text(&buffer, 14),
            format!("│{:<38}│", "peer-b  instance peer-b")
        );
        assert_eq!(row_text(&buffer, 0), format!("{:<40}", "line 0"));
        let closed = rendered_matrix(&rows, None, &UiState::default(), "ok", 40, 20);
        assert!(!(0..20).any(|r| row_text(&closed, r).starts_with('┌')));
    }

    #[test]
    fn the_detail_scroll_shows_its_own_window() {
        let rows = numbered(2);
        let card = numbered(12);
        let state = UiState {
            scroll: 4,
            ..UiState::default()
        };
        let buffer = rendered_matrix(&rows, Some(&card), &state, "ok", 40, 20);
        assert_eq!(row_text(&buffer, 14), format!("│{:<38}│", "line 4"));
        let far = UiState {
            scroll: 99,
            ..UiState::default()
        };
        let buffer = rendered_matrix(&rows, Some(&card), &far, "ok", 40, 20);
        // The pane has four inner rows, from row 14; the last page starts at 12 - 4.
        assert_eq!(
            row_text(&buffer, 17),
            format!("│{:<38}│", "line 11"),
            "clamped to the last page"
        );
    }

    #[test]
    fn tiny_matrix_screens_do_not_panic() {
        let card = numbered(4);
        for (width, height) in [(0, 0), (1, 1), (3, 2), (2, 1), (40, 1), (40, 4), (40, 9)] {
            let buffer = rendered_matrix(
                &numbered(3),
                Some(&card),
                &UiState::default(),
                "ok",
                width,
                height,
            );
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
}
