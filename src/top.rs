//! `reiki top`: a live terminal view of the key health of every configured peer.
//!
//! The feed layer follows each peer's `/v1/health/stream` with the same pinned client, token
//! and SSE parser as `reiki peers --watch`, and falls back to one `/v1/health` poll when a
//! stream fails. The reducer turns those messages into a [`Fleet`]; the view model and the
//! ratatui layer live in the dependency-free `reiki-top` crate.

use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use reiki_top::matrix::{
    fleet_series, is_problem, key_counts, order_peers, request_series, resolve, History,
    HostSummary, Matrix, Metric, Sample,
};
use reiki_top::ui::{self, crossterm, ratatui, ChartMetric};
use reiki_top::{view, Fleet, Health};
use tokio::sync::mpsc;

use crate::config::{PeerEntry, PeersConfig};
use crate::peers::client::{self, Chunk, SseItem, SseParser, MAX_BODY};

/// One message from a peer's feed task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FeedEvent {
    /// The raw `/v1/health` body: the first `snapshot` event, or a polled body.
    Snapshot(String),
    /// Any event after that first snapshot, applied as-is.
    Event { event: String, data: String },
    /// An SSE comment: the stream is alive even when nothing changed.
    Heartbeat,
    /// The stream and the fallback poll both failed; the text says why.
    Down(String),
}

/// The `reiki top` command: validates the `[peers]` config, then runs once or live. `full`
/// selects the per-peer text view instead of the matrix.
pub async fn run(
    peers: Option<&PeersConfig>,
    once: bool,
    interval: Option<u64>,
    full: bool,
    provider: Option<&str>,
) -> i32 {
    let peers = match client::ready(peers) {
        Ok(peers) => peers,
        Err(e) => {
            eprintln!("reiki: {e}");
            return 2;
        }
    };
    if once {
        run_once(peers, full, provider).await
    } else {
        run_live(peers, interval, full, provider).await
    }
}

/// Polls every peer once, prints the fleet as plain text and returns 0 when at least one peer
/// answered, 1 when none did.
pub async fn run_once(peers: &PeersConfig, full: bool, provider: Option<&str>) -> i32 {
    run_once_to(peers, full, provider, &mut io::stdout(), &mut io::stderr()).await
}

/// [`run_once`] with the output handles passed in, so tests need no terminal.
async fn run_once_to(
    peers: &PeersConfig,
    full: bool,
    provider: Option<&str>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> i32 {
    let poll = Duration::from_secs(peers.poll_interval_secs);
    let names: Vec<String> = peers.peers.keys().cloned().collect();
    let mut tasks = Vec::new();
    for (name, peer) in &peers.peers {
        let (name, peer) = (name.clone(), peer.clone());
        tasks.push(tokio::spawn(async move {
            (name, client::fetch_raw(&peer).await)
        }));
    }

    let now = Instant::now();
    let mut fleet = Fleet::new(poll.saturating_mul(3));
    let mut answered = false;
    for task in tasks {
        match task.await {
            Ok((name, Ok(raw))) => match Health::from_json(&raw) {
                Ok(health) => {
                    fleet.apply_snapshot(&name, health, now);
                    answered = true;
                }
                Err(e) => {
                    let reason = format!("not a /v1/health document: {e}");
                    let _ = writeln!(err, "reiki: peer `{name}`: {reason}");
                    fleet.set_error(&name, reason);
                }
            },
            Ok((name, Err(reason))) => {
                let _ = writeln!(err, "reiki: peer `{name}`: {reason}");
                fleet.set_error(&name, reason);
            }
            Err(_) => {
                let _ = writeln!(err, "reiki: a peer fetch task failed");
            }
        }
    }

    // No history on a single poll, so the matrix draws no sparklines.
    let lines = if full {
        view::render(&fleet, &names, now, 100)
    } else {
        let matrix = Matrix::build(&fleet, &names, now);
        let (matrix, filter_name): (Matrix, Option<&str>) = match provider {
            Some(p) if matrix.filter_provider(p).is_some() => {
                (matrix.filter_provider(p).unwrap(), Some(p))
            }
            Some(p) => {
                let _ = writeln!(err, "unknown provider: {p}");
                (matrix, None)
            }
            None => (matrix, None),
        };
        let mut lines = view::render_matrix(&matrix, &History::new(0), None, 100, filter_name);
        lines.extend(view::cooling_panel(&fleet, &names, filter_name, now, 100));
        lines
    };
    for line in lines {
        let _ = writeln!(out, "{}", line.plain_text());
    }
    let _ = out.flush();
    i32::from(!answered)
}

/// [`FeedEvent`] to [`Fleet`]: the whole live reducer, with no clock of its own.
pub(crate) fn apply_feed(fleet: &mut Fleet, name: &str, feed: FeedEvent, now: Instant) {
    match feed {
        FeedEvent::Snapshot(raw) => match Health::from_json(&raw) {
            Ok(health) => fleet.apply_snapshot(name, health, now),
            Err(e) => fleet.set_error(name, format!("bad health document: {e}")),
        },
        FeedEvent::Event { event, data } => {
            if let Err(e) = fleet.apply_event(name, &event, &data, now) {
                fleet.set_error(name, format!("bad {event} event: {e}"));
            }
        }
        FeedEvent::Heartbeat => fleet.touch(name, now),
        FeedEvent::Down(reason) => fleet.set_error(name, reason),
    }
}

/// Follows one peer for as long as the view runs: its stream, and one poll whenever that fails.
async fn feed_peer(
    name: String,
    peer: PeerEntry,
    poll: Duration,
    tx: mpsc::Sender<(String, FeedEvent)>,
) {
    loop {
        let reason = follow_peer(&name, &peer, &tx).await;
        if tx.is_closed() {
            return;
        }
        let feed = match client::fetch_raw(&peer).await {
            Ok(body) => FeedEvent::Snapshot(body),
            Err(e) => FeedEvent::Down(format!("stream: {reason}; poll: {e}")),
        };
        if tx.send((name.clone(), feed)).await.is_err() {
            return;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Streams one peer until it fails, sending the first snapshot raw and every later event as-is.
async fn follow_peer(
    name: &str,
    peer: &PeerEntry,
    tx: &mpsc::Sender<(String, FeedEvent)>,
) -> String {
    let mut response = match client::open_stream(peer).await {
        Ok(response) => response,
        Err(e) => return e,
    };
    let mut parser = SseParser::new(MAX_BODY);
    let mut snapshotted = false;
    loop {
        let chunk = match client::next_chunk(&mut response).await {
            Ok(Chunk::Data(chunk)) => chunk,
            Ok(Chunk::Ended) => return "the stream ended".to_owned(),
            Err(e) => return e,
        };
        let items = match parser.feed(&chunk) {
            Ok(items) => items,
            Err(e) => return e,
        };
        for item in items {
            let feed = match item {
                SseItem::Comment => FeedEvent::Heartbeat,
                SseItem::Event { event, data } => {
                    if event == "snapshot" && !snapshotted {
                        snapshotted = true;
                        FeedEvent::Snapshot(data)
                    } else {
                        FeedEvent::Event { event, data }
                    }
                }
            };
            if tx.send((name.to_owned(), feed)).await.is_err() {
                return "stopped".to_owned();
            }
        }
    }
}

/// The real terminal: the alternate screen and raw mode, both turned off by [`Guard`].
struct TerminalControl<W: Write> {
    out: W,
}

/// What a [`Guard`] turns on and undoes. A fake implementation tests the guard without a tty.
trait ScreenControl {
    fn enter(&mut self) -> io::Result<()>;
    fn restore(&mut self);
}

impl<W: Write> ScreenControl for TerminalControl<W> {
    fn enter(&mut self) -> io::Result<()> {
        crossterm::execute!(self.out, crossterm::terminal::EnterAlternateScreen)?;
        crossterm::terminal::enable_raw_mode()
    }

    fn restore(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            self.out,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = self.out.flush();
    }
}

/// Restores the terminal when dropped, on every exit path including a panic.
struct Guard<C: ScreenControl> {
    control: C,
}

impl<C: ScreenControl> Guard<C> {
    fn new(control: C) -> Self {
        Self { control }
    }

    fn enter(&mut self) -> io::Result<()> {
        self.control.enter()
    }
}

impl<C: ScreenControl> Drop for Guard<C> {
    fn drop(&mut self) {
        self.control.restore();
    }
}

type Screen = ratatui::Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>;

/// Follows the feeds and draws the fleet until the user quits.
pub async fn run_live(
    peers: &PeersConfig,
    interval: Option<u64>,
    full: bool,
    provider: Option<&str>,
) -> i32 {
    if !io::stdout().is_terminal() {
        eprintln!("reiki: stdout is not a terminal; use `reiki top --once`");
        return 2;
    }
    let poll = Duration::from_secs(peers.poll_interval_secs);
    let (tx, rx) = mpsc::channel(256);
    let mut tasks = Vec::new();
    for (name, peer) in &peers.peers {
        tasks.push(tokio::spawn(feed_peer(
            name.clone(),
            peer.clone(),
            poll,
            tx.clone(),
        )));
    }
    drop(tx);

    let mut guard = Guard::new(TerminalControl { out: io::stdout() });
    if let Err(e) = guard.enter() {
        eprintln!("reiki: cannot set up the terminal: {e}");
        return 1;
    }
    let mut terminal = match Screen::new(ratatui::backend::CrosstermBackend::new(io::stdout())) {
        Ok(terminal) => terminal,
        Err(e) => {
            eprintln!("reiki: cannot set up the terminal: {e}");
            return 1;
        }
    };
    let redraw = Duration::from_secs(interval.unwrap_or(1).max(1));
    let code = live_loop(&mut terminal, peers, rx, redraw, full, provider).await;
    for task in tasks {
        task.abort();
    }
    code
}

/// The picture pause keeps: the fleet and the sparkline history as they were when paused.
#[derive(Clone)]
struct Frozen {
    fleet: Fleet,
    history: History,
    at: Instant,
}

fn freeze(fleet: &Fleet, history: &History, at: Instant) -> Frozen {
    Frozen {
        fleet: fleet.clone(),
        history: history.clone(),
        at,
    }
}

async fn live_loop(
    terminal: &mut Screen,
    peers: &PeersConfig,
    mut rx: mpsc::Receiver<(String, FeedEvent)>,
    redraw: Duration,
    full: bool,
    provider: Option<&str>,
) -> i32 {
    let poll = Duration::from_secs(peers.poll_interval_secs);
    let all: Vec<String> = peers.peers.keys().cloned().collect();
    let mut fleet = Fleet::new(poll.saturating_mul(3));
    let mut history = History::new(History::DEFAULT_CAPACITY);
    let mut last_sample: HashMap<String, Instant> = HashMap::new();
    let mut state = ui::UiState {
        mode: if full {
            ui::Mode::Full
        } else {
            ui::Mode::Matrix
        },
        filter: ui::Filter {
            peer: None,
            provider: provider.map(str::to_owned),
        },
        ..ui::UiState::default()
    };
    // Pause freezes the picture, not just the redraws, so a peer that goes quiet cannot keep
    // looking fresh on a screen that is minutes old.
    let mut frozen: Option<Frozen> = None;

    #[cfg(unix)]
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    #[cfg(not(unix))]
    let mut terminate: Option<()> = None;
    let mut frame_tick = tokio::time::interval(redraw);
    frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut key_tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        let mut draw_now = false;
        tokio::select! {
            message = rx.recv() => match message {
                Some((name, feed)) => {
                    // Pausing stops redraws, not the data: dropping events would leave a
                    // wrong key state on screen after resuming.
                    apply_feed(&mut fleet, &name, feed, Instant::now());
                    draw_now = !state.paused;
                }
                None => return 0,
            },
            // Return so the guard puts the terminal back before the process ends.
            () = terminated(&mut terminate) => return 0,
            // Also while paused, so the age of the frozen picture keeps counting. Samples
            // come from the redraw tick, not from feed events, so a chatty peer gets no more.
            _ = frame_tick.tick() => {
                if frozen.is_none() {
                    fleet.advance(Instant::now());
                    sample_fleet(&mut history, &fleet, &all, Instant::now(), &mut last_sample, redraw);
                }
                draw_now = true;
            }
            _ = key_tick.tick() => loop {
                match crossterm::event::poll(Duration::ZERO) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => {
                        eprintln!("reiki: cannot read keys: {e}");
                        return 1;
                    }
                }
                let event = match crossterm::event::read() {
                    Ok(event) => event,
                    Err(e) => {
                        eprintln!("reiki: cannot read keys: {e}");
                        return 1;
                    }
                };
                let crossterm::event::Event::Key(key) = event else {
                    continue;
                };
                if ui::dismiss_help(&mut state, key) {
                    draw_now = true;
                    continue;
                }
                let Some(action) = ui::map_key(key) else {
                    continue;
                };
                let (shown, shown_history, shown_at) = match &frozen {
                    Some(f) => (&f.fleet, &f.history, f.at),
                    None => (&fleet, &history, Instant::now()),
                };
                let (total, viewport) =
                    match measure(terminal, shown, shown_history, shown_at, &all, &state) {
                        Ok(pair) => pair,
                        Err(e) => {
                            eprintln!("reiki: cannot size the terminal: {e}");
                            return 1;
                        }
                    };
                let problem = selected_peer(shown, &all, shown_at, &state)
                    .is_some_and(|(_, problem)| problem);
                if action == ui::Action::CycleProvider {
                    if state.mode == ui::Mode::Matrix {
                        let matrix = Matrix::build(shown, &all, shown_at);
                        let providers: Vec<String> =
                            matrix.columns.iter().map(|c| c.upstream.clone()).collect();
                        state.cycle_provider(&providers);
                    }
                } else if !apply_action(&mut state, action, &all, total, viewport, problem) {
                    return 0;
                }
                match (state.paused, frozen.is_some()) {
                    (true, false) => frozen = Some(freeze(&fleet, &history, Instant::now())),
                    (false, true) => frozen = None,
                    _ => {}
                }
                draw_now = true;
            },
        }
        if draw_now {
            let (shown, shown_history, shown_at, paused_for) = match &frozen {
                Some(f) => (&f.fleet, &f.history, f.at, Some(f.at.elapsed())),
                None => (&fleet, &history, Instant::now(), None),
            };
            let shown = Shown {
                fleet: shown,
                history: shown_history,
                now: shown_at,
                paused_for,
            };
            if let Err(e) = render_screen(terminal, &shown, &all, &state) {
                eprintln!("reiki: cannot draw: {e}");
                return 1;
            }
        }
    }
}

#[cfg(unix)]
async fn terminated(signal: &mut Option<tokio::signal::unix::Signal>) {
    match signal {
        Some(signal) => {
            signal.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

#[cfg(not(unix))]
async fn terminated(_signal: &mut Option<()>) {
    std::future::pending::<()>().await;
}

/// Applies one key action. In matrix view the arrows and j/k move the selection and Enter opens
/// the detail pane; in full view they scroll and Tab filters to one peer.
fn apply_action(
    state: &mut ui::UiState,
    action: ui::Action,
    all: &[String],
    total: usize,
    viewport: usize,
    problem_selected: bool,
) -> bool {
    let action = match (state.mode, action) {
        (ui::Mode::Matrix, ui::Action::ScrollUp) => ui::Action::PrevPeer,
        (ui::Mode::Matrix, ui::Action::ScrollDown) => ui::Action::NextPeer,
        (_, action) => action,
    };
    if action == ui::Action::ToggleDetail {
        if state.mode == ui::Mode::Matrix {
            state.toggle_detail(problem_selected);
        }
        return true;
    }
    // The gauge and the chart only exist in the matrix, so the full view ignores their keys.
    if matches!(
        action,
        ui::Action::ToggleMetric | ui::Action::ToggleWindow | ui::Action::CycleChart
    ) {
        if state.mode == ui::Mode::Matrix {
            state.apply(action, total, viewport);
        }
        return true;
    }
    // The first press has to move off the row that is selected by default.
    if state.mode == ui::Mode::Matrix
        && state.selected.is_none()
        && matches!(action, ui::Action::NextPeer | ui::Action::PrevPeer)
    {
        state.selected = Some(0);
    }
    let previous = state.selected;
    let keep = state.apply(action, total, viewport);
    // A detail choice and its scroll belong to the peer they were made on.
    if state.mode == ui::Mode::Matrix && state.selected != previous {
        state.detail = None;
        state.scroll = 0;
    }
    if action == ui::Action::ClearFilter {
        state.selected = None;
    }
    if state.mode == ui::Mode::Full
        && (action == ui::Action::ClearFilter || state.selected != previous)
    {
        state.filter.peer = match state.selected {
            Some(index) if !all.is_empty() => Some(all[index % all.len()].clone()),
            _ => None,
        };
    }
    keep
}

/// The peers on screen: the Tab filter applies to the full view only.
fn visible_cts(all: &[String], state: &ui::UiState) -> Vec<String> {
    match (&state.filter.peer, state.mode) {
        (Some(name), ui::Mode::Full) => vec![name.clone()],
        _ => all.to_vec(),
    }
}

/// The selected peer's name and whether it needs attention, in matrix display order.
fn selected_peer(
    fleet: &Fleet,
    all: &[String],
    now: Instant,
    state: &ui::UiState,
) -> Option<(String, bool)> {
    let matrix = Matrix::build(fleet, all, now);
    let matrix = if state.mode == ui::Mode::Matrix {
        match state.filter.provider.as_deref() {
            Some(p) => matrix.filter_provider(p).unwrap_or(matrix),
            None => matrix,
        }
    } else {
        matrix
    };
    let order = order_peers(&matrix);
    let row = &matrix.rows[order[resolve(state.selected, order.len())?]];
    Some((row.peer.clone(), is_problem(row)))
}

/// What one frame draws: the matrix lines and the panels' contents, or the full view's text.
#[derive(Default)]
struct Parts {
    lines: Vec<view::Line>,
    /// Peer rows in `lines`, and the selected row's display position.
    rows: usize,
    selected: Option<usize>,
    cooling: Vec<view::Line>,
    detail: Option<Vec<view::Line>>,
    providers: Vec<String>,
    chart_title: String,
    chart_name: &'static str,
    chart: Vec<Option<f64>>,
    keys: [u64; 3],
}

fn build_parts(
    fleet: &Fleet,
    history: &History,
    all: &[String],
    now: Instant,
    state: &ui::UiState,
    width: usize,
) -> Parts {
    match state.mode {
        ui::Mode::Full => Parts {
            lines: view::render(fleet, &visible_cts(all, state), now, width),
            ..Parts::default()
        },
        ui::Mode::Matrix => {
            // The panels draw inside a border.
            let inner = width.saturating_sub(2);
            let full = Matrix::build(fleet, all, now);
            let providers = full.columns.iter().map(|c| c.upstream.clone()).collect();
            let (matrix, filter_name): (Matrix, Option<&str>) =
                match state.filter.provider.as_deref() {
                    Some(p) => match full.filter_provider(p) {
                        Some(fm) => (fm, Some(p)),
                        None => (full, None),
                    },
                    None => (full, None),
                };
            let order = order_peers(&matrix);
            let pos = resolve(state.selected, order.len());
            let row = pos.map(|pos| &matrix.rows[order[pos]]);
            let detail = row.and_then(|row| {
                state
                    .detail_open(is_problem(row))
                    .then(|| view::peer_detail(fleet, &row.peer, now, inner))
            });
            // The chart follows the user's own selection; the fleet until they move it.
            let chart_row = state.selected.and(row);
            let (chart_name, chart) = chart_series(state.chart, history, &matrix, chart_row);
            let chart_title = match (state.chart, chart_row) {
                (ChartMetric::Disk, None) => format!("{} · fleet max", state.chart.title()),
                (_, None) => format!("{} · fleet", state.chart.title()),
                (_, Some(row)) => format!("{} · {}", state.chart.title(), row.peer),
            };
            Parts {
                lines: view::render_matrix_mode(
                    &matrix,
                    history,
                    pos,
                    inner,
                    filter_name,
                    state.gauge,
                ),
                rows: matrix.rows.len(),
                selected: pos,
                // The panel's own title is drawn by the frame, so its first line is dropped.
                cooling: view::cooling_panel(fleet, all, filter_name, now, inner)
                    .into_iter()
                    .skip(1)
                    .collect(),
                detail,
                providers,
                chart_title,
                chart_name,
                chart,
                keys: key_counts(fleet, all, now),
            }
        }
    }
}

/// The chart's legend name and points: the selected peer's history, or the fleet's when no
/// peer is selected. Requests come from the matrix rows, so a provider filter narrows them.
fn chart_series(
    metric: ChartMetric,
    history: &History,
    matrix: &Matrix,
    row: Option<&reiki_top::matrix::Row>,
) -> (&'static str, Vec<Option<f64>>) {
    let name = metric.label();
    let metric = match metric {
        ChartMetric::Requests => {
            let rows = row.map_or(&matrix.rows[..], std::slice::from_ref);
            return (name, request_series(rows));
        }
        ChartMetric::Load => Metric::Load1,
        ChartMetric::Disk => Metric::DiskPercent,
        ChartMetric::Sessions => Metric::Sessions,
    };
    match row {
        Some(row) => (
            name,
            history
                .values(&row.peer, metric)
                .into_iter()
                .map(Some)
                .collect(),
        ),
        None => {
            let peers: Vec<String> = matrix.rows.iter().map(|r| r.peer.clone()).collect();
            (name, fleet_series(history, &peers, metric))
        }
    }
}

/// Pushes one host sample for each fresh peer, at most once per `every`. Called from the redraw
/// tick, so the sparkline's time axis is the redraw interval whatever the feeds do.
fn sample_fleet(
    history: &mut History,
    fleet: &Fleet,
    names: &[String],
    now: Instant,
    last: &mut HashMap<String, Instant>,
    every: Duration,
) {
    // A tick that lands a few milliseconds early would skip a whole interval, so accept ninety
    // percent of one.
    let due_after = every - every / 10;
    for name in names {
        let Some(peer) = fleet.peer(name, now) else {
            continue;
        };
        if peer.stale {
            continue;
        }
        if last
            .get(name)
            .is_some_and(|t| now.saturating_duration_since(*t) < due_after)
        {
            continue;
        }
        let host = HostSummary::from_health(peer.health);
        history.push(
            name,
            Sample {
                load1: host.load1,
                disk_percent: host.disk_percent,
                sessions: host.sessions,
            },
        );
        last.insert(name.clone(), now);
    }
}

/// The line count and body height a scroll action has to stay inside. In matrix view only an
/// open detail pane scrolls.
fn measure(
    terminal: &Screen,
    fleet: &Fleet,
    history: &History,
    now: Instant,
    all: &[String],
    state: &ui::UiState,
) -> io::Result<(usize, usize)> {
    let size = terminal.size()?;
    let parts = build_parts(fleet, history, all, now, state, usize::from(size.width));
    Ok(match (state.mode, parts.detail) {
        (ui::Mode::Full, _) => (
            parts.lines.len(),
            // One footer row and a two-row border.
            usize::from(size.height.saturating_sub(3)),
        ),
        (ui::Mode::Matrix, Some(card)) => (
            card.len(),
            usize::from(ui::detail_height(size.height).saturating_sub(2)),
        ),
        (ui::Mode::Matrix, None) => (0, 0),
    })
}

/// The data one frame is drawn from: the live picture or the frozen one.
struct Shown<'a> {
    fleet: &'a Fleet,
    history: &'a History,
    now: Instant,
    paused_for: Option<Duration>,
}

/// The footer status. A paused screen is a snapshot, so it says how old that snapshot is.
fn status_line(fresh: usize, total: usize, paused_for: Option<Duration>) -> String {
    match paused_for {
        Some(age) => format!(
            "{fresh} of {total} peers fresh when paused {}s ago",
            age.as_secs()
        ),
        None => format!("{fresh} of {total} peers fresh"),
    }
}

fn render_screen(
    terminal: &mut Screen,
    shown: &Shown<'_>,
    all: &[String],
    state: &ui::UiState,
) -> io::Result<()> {
    let visible = visible_cts(all, state);
    let fresh = visible
        .iter()
        .filter(|name| {
            shown
                .fleet
                .peer(name, shown.now)
                .is_some_and(|peer| !peer.stale)
        })
        .count();
    let status = status_line(fresh, visible.len(), shown.paused_for);
    let clock = ui::utc_clock(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    );
    terminal.draw(|frame| {
        let parts = build_parts(
            shown.fleet,
            shown.history,
            all,
            shown.now,
            state,
            usize::from(frame.area().width),
        );
        match state.mode {
            ui::Mode::Full => ui::draw(frame, &parts.lines, state, &status),
            ui::Mode::Matrix => ui::draw_matrix(
                frame,
                &ui::MatrixFrame {
                    version: env!("CARGO_PKG_VERSION"),
                    clock: &clock,
                    status: &status,
                    providers: &parts.providers,
                    matrix: &parts.lines,
                    rows: parts.rows,
                    selected: parts.selected,
                    cooling: &parts.cooling,
                    detail: parts.detail.as_deref(),
                    chart_title: &parts.chart_title,
                    chart_name: parts.chart_name,
                    chart: &parts.chart,
                    keys: parts.keys,
                },
                state,
            ),
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::config::Config;
    use reiki_top::matrix::Metric;

    const DOC: &str = r#"{"instance":"peer-a","observed_at":"t","upstreams":[
        {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
         "soonest_cooldown_seconds":null,"last_status_class":"2xx"}]}"#;

    fn fleet() -> Fleet {
        Fleet::new(Duration::from_secs(30))
    }

    fn upstream(fleet: &Fleet, now: Instant) -> &reiki_top::Upstream {
        &fleet.peer("peer-a", now).unwrap().health.upstreams[0]
    }

    #[test]
    fn a_paused_status_line_reports_the_age_of_the_snapshot() {
        assert_eq!(status_line(2, 3, None), "2 of 3 peers fresh");
        assert_eq!(
            status_line(2, 3, Some(Duration::from_millis(12_900))),
            "2 of 3 peers fresh when paused 12s ago"
        );
    }

    #[test]
    fn a_frozen_fleet_does_not_follow_later_feeds() {
        let now = Instant::now();
        let mut live = fleet();
        apply_feed(
            &mut live,
            "peer-a",
            FeedEvent::Snapshot(DOC.to_owned()),
            now,
        );
        let frozen = live.clone();
        let later = now + Duration::from_secs(40);
        apply_feed(&mut live, "peer-a", FeedEvent::Heartbeat, later);
        assert!(frozen.stale("peer-a", later, Duration::from_secs(30)));
        assert!(!live.stale("peer-a", later, Duration::from_secs(30)));
    }

    #[test]
    fn a_snapshot_feed_fills_the_fleet() {
        let now = Instant::now();
        let mut fleet = fleet();
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Snapshot(DOC.to_owned()),
            now,
        );
        let peer = fleet.peer("peer-a", now).expect("the peer has data");
        assert_eq!(peer.health.instance, "peer-a");
        assert_eq!(peer.health.upstreams.len(), 1);
        assert!(!peer.stale);
    }

    #[test]
    fn a_key_event_feed_patches_one_upstream() {
        let now = Instant::now();
        let mut fleet = fleet();
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Snapshot(DOC.to_owned()),
            now,
        );
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Event {
                event: "key".to_owned(),
                data: r#"{"upstream":{"name":"zen","healthy_keys":0,"cooling_keys":1,
                    "disabled_keys":0,"last_status_class":"4xx"}}"#
                    .to_owned(),
            },
            now,
        );
        let upstream = upstream(&fleet, now);
        assert_eq!(upstream.healthy_keys, 0);
        assert_eq!(upstream.cooling_keys, 1);
        assert_eq!(upstream.last_status_class.as_deref(), Some("4xx"));
        assert_eq!(fleet.error("peer-a"), None);
    }

    #[test]
    fn a_heartbeat_keeps_a_quiet_ct_fresh() {
        let now = Instant::now();
        let mut fleet = fleet();
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Snapshot(DOC.to_owned()),
            now,
        );
        let later = now + Duration::from_secs(40);
        assert!(fleet.stale("peer-a", later, Duration::from_secs(30)));
        apply_feed(&mut fleet, "peer-a", FeedEvent::Heartbeat, later);
        assert!(!fleet.stale("peer-a", later, Duration::from_secs(30)));
    }

    #[test]
    fn a_down_feed_sets_the_error_until_a_snapshot() {
        let now = Instant::now();
        let mut fleet = fleet();
        let reason = "stream: the stream ended; poll: cannot connect";
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Down(reason.to_owned()),
            now,
        );
        assert_eq!(fleet.error("peer-a"), Some(reason));
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Snapshot(DOC.to_owned()),
            now,
        );
        assert_eq!(fleet.error("peer-a"), None);
    }

    #[test]
    fn a_malformed_snapshot_becomes_an_error_not_a_panic() {
        let now = Instant::now();
        let mut fleet = fleet();
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Snapshot("not json".to_owned()),
            now,
        );
        let error = fleet.error("peer-a").expect("the error is kept");
        assert!(error.contains("bad health document"), "{error}");
        assert!(fleet.peer("peer-a", now).is_none());
    }

    #[test]
    fn tab_filters_to_one_ct_and_c_clears_it() {
        let all = vec!["peer-a".to_owned(), "peer-b".to_owned()];
        let mut state = ui::UiState {
            mode: ui::Mode::Full,
            ..ui::UiState::default()
        };
        assert!(apply_action(
            &mut state,
            ui::Action::NextPeer,
            &all,
            10,
            3,
            false
        ));
        assert_eq!(state.filter.peer.as_deref(), Some("peer-a"));
        assert_eq!(visible_cts(&all, &state), ["peer-a"]);
        apply_action(&mut state, ui::Action::NextPeer, &all, 10, 3, false);
        assert_eq!(state.filter.peer.as_deref(), Some("peer-b"));
        apply_action(&mut state, ui::Action::NextPeer, &all, 10, 3, false);
        assert_eq!(state.filter.peer.as_deref(), Some("peer-a"), "it wraps");
        apply_action(&mut state, ui::Action::ClearFilter, &all, 10, 3, false);
        assert_eq!(state.filter.peer, None);
        assert_eq!(visible_cts(&all, &state), all);
    }

    #[test]
    fn tab_moves_the_selection_in_matrix_view_and_filters_in_full_view() {
        let all = vec!["peer-a".to_owned(), "peer-b".to_owned()];
        let mut state = ui::UiState::default();
        apply_action(&mut state, ui::Action::NextPeer, &all, 10, 3, false);
        assert_eq!(
            state.selected,
            Some(1),
            "the first press leaves the default row"
        );
        assert_eq!(state.filter.peer, None, "the matrix does not filter");
        state.toggle_mode();
        apply_action(&mut state, ui::Action::NextPeer, &all, 10, 3, false);
        assert_eq!(
            state.filter.peer.as_deref(),
            Some("peer-a"),
            "wraps to the first"
        );
        assert_eq!(state.selected, Some(2));
    }

    #[test]
    fn arrows_and_jk_move_the_selection_in_matrix_view_and_scroll_in_full_view() {
        let all = vec!["peer-a".to_owned(), "peer-b".to_owned()];
        let mut state = ui::UiState::default();
        apply_action(&mut state, ui::Action::ScrollDown, &all, 10, 3, false);
        assert_eq!((state.selected, state.scroll), (Some(1), 0));
        apply_action(&mut state, ui::Action::ScrollUp, &all, 10, 3, false);
        apply_action(&mut state, ui::Action::ScrollUp, &all, 10, 3, false);
        assert_eq!(state.selected, Some(0), "never underflows");
        state.toggle_mode();
        apply_action(&mut state, ui::Action::ScrollDown, &all, 10, 3, false);
        assert_eq!((state.selected, state.scroll), (Some(0), 1));
    }

    #[test]
    fn enter_toggles_the_detail_only_in_matrix_view() {
        let all = vec!["peer-a".to_owned()];
        let mut state = ui::UiState::default();
        apply_action(&mut state, ui::Action::ToggleDetail, &all, 10, 3, true);
        assert!(
            !state.detail_open(true),
            "closes the automatically open pane"
        );
        apply_action(&mut state, ui::Action::ToggleDetail, &all, 10, 3, true);
        assert!(state.detail_open(true));
        state.toggle_mode();
        let before = state.clone();
        apply_action(&mut state, ui::Action::ToggleDetail, &all, 10, 3, true);
        assert_eq!(state, before, "Enter does nothing in the full view");
    }

    #[test]
    fn m_cycles_the_chart_only_in_matrix_view_and_the_chart_follows_the_selection() {
        let all = vec!["peer-a".to_owned()];
        let mut state = ui::UiState::default();
        apply_action(&mut state, ui::Action::CycleChart, &all, 10, 3, false);
        assert_eq!(state.chart, ui::ChartMetric::Disk);
        state.toggle_mode();
        let before = state.clone();
        apply_action(&mut state, ui::Action::CycleChart, &all, 10, 3, false);
        assert_eq!(state, before, "the full view ignores m");
        state.toggle_mode();

        let now = Instant::now();
        let mut fleet = fleet();
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Snapshot(DOC.to_owned()),
            now,
        );
        let mut history = History::new(10);
        history.push(
            "peer-a",
            Sample {
                load1: Some(1.5),
                disk_percent: Some(40.0),
                sessions: Some(2),
            },
        );
        let parts = build_parts(&fleet, &history, &all, now, &state, 100);
        assert_eq!(
            parts.chart_title, "Disk · fleet max",
            "no selection, so the fleet"
        );
        assert_eq!(parts.chart, vec![Some(40.0)]);
        assert!(parts.keys.iter().sum::<u64>() > 0, "{:?}", parts.keys);
        assert_eq!(parts.providers, vec!["zen".to_owned()]);

        state.chart = ui::ChartMetric::Load;
        state.selected = Some(0);
        let parts = build_parts(&fleet, &history, &all, now, &state, 100);
        assert_eq!(parts.chart_title, "Load · peer-a");
        assert_eq!(parts.chart, vec![Some(1.5)]);
        assert_eq!(parts.chart_name, "load");
    }

    #[test]
    fn u_and_w_change_the_gauge_only_in_matrix_view_and_the_title_says_so() {
        let all = vec!["peer-a".to_owned()];
        let mut state = ui::UiState::default();
        apply_action(&mut state, ui::Action::ToggleMetric, &all, 10, 3, false);
        apply_action(&mut state, ui::Action::ToggleWindow, &all, 10, 3, false);
        assert_eq!(state.gauge.metric, view::GaugeMetric::Tokens);
        assert_eq!(state.gauge.window, view::GaugeWindow::Minutes);

        let now = Instant::now();
        let mut fleet = fleet();
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Snapshot(DOC.to_owned()),
            now,
        );
        let history = History::new(0);
        let parts = build_parts(&fleet, &history, &all, now, &state, 100);
        assert!(
            parts.lines[0].plain_text().ends_with("[tok 5m]"),
            "{:?}",
            parts.lines[0]
        );

        state.toggle_mode();
        let before = state.clone();
        apply_action(&mut state, ui::Action::ToggleMetric, &all, 10, 3, false);
        assert_eq!(state, before, "u does nothing in the full view");
    }

    #[test]
    fn the_matrix_screen_opens_the_detail_card_of_the_selected_problem_peer() {
        let now = Instant::now();
        let mut fleet = fleet();
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Snapshot(DOC.to_owned()),
            now,
        );
        apply_feed(
            &mut fleet,
            "peer-b",
            FeedEvent::Snapshot(
                r#"{"instance":"peer-b","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,
                 "soonest_cooldown_seconds":60,"last_status_class":"4xx"}]}"#
                    .to_owned(),
            ),
            now,
        );
        let all = vec!["peer-a".to_owned(), "peer-b".to_owned()];
        let history = History::new(0);
        let state = ui::UiState::default();
        let parts = build_parts(&fleet, &history, &all, now, &state, 100);
        let card = parts
            .detail
            .expect("peer-b is a problem, so the pane opens");
        assert!(
            card[0].plain_text().starts_with("peer-b  instance"),
            "{card:?}"
        );

        let mut closed = state.clone();
        closed.detail = Some(false);
        assert!(build_parts(&fleet, &history, &all, now, &closed, 100)
            .detail
            .is_none());
        let mut full = state.clone();
        full.toggle_mode();
        assert!(build_parts(&fleet, &history, &all, now, &full, 100)
            .detail
            .is_none());
    }

    #[test]
    fn samples_are_pushed_once_per_interval_not_per_event() {
        let every = Duration::from_secs(1);
        let t0 = Instant::now();
        let mut fleet = fleet();
        let doc = r#"{"instance":"peer-a","observed_at":"t","upstreams":[],
            "sessions":{"active_sessions":4}}"#;
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Snapshot(doc.to_owned()),
            t0,
        );
        let names = vec!["peer-a".to_owned()];
        let mut history = History::new(History::DEFAULT_CAPACITY);
        let mut last = HashMap::new();
        let count = |h: &History| h.series("peer-a", Metric::Sessions).len();

        sample_fleet(&mut history, &fleet, &names, t0, &mut last, every);
        assert_eq!(count(&history), 1);
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Heartbeat,
            t0 + Duration::from_millis(500),
        );
        sample_fleet(
            &mut history,
            &fleet,
            &names,
            t0 + Duration::from_millis(500),
            &mut last,
            every,
        );
        assert_eq!(count(&history), 1, "half an interval is not due");
        sample_fleet(
            &mut history,
            &fleet,
            &names,
            t0 + Duration::from_secs(1),
            &mut last,
            every,
        );
        assert_eq!(count(&history), 2);
        sample_fleet(
            &mut history,
            &fleet,
            &names,
            t0 + Duration::from_millis(1200),
            &mut last,
            every,
        );
        assert_eq!(
            count(&history),
            2,
            "the next is due one interval after the last"
        );
    }

    #[test]
    fn a_stale_peer_is_not_sampled() {
        let t0 = Instant::now();
        let mut fleet = Fleet::new(Duration::from_secs(30));
        let doc = r#"{"instance":"peer-a","observed_at":"t","upstreams":[],
            "sessions":{"active_sessions":4}}"#;
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Snapshot(doc.to_owned()),
            t0,
        );
        let mut history = History::new(History::DEFAULT_CAPACITY);
        let mut last = HashMap::new();
        sample_fleet(
            &mut history,
            &fleet,
            &["peer-a".to_owned()],
            t0 + Duration::from_secs(40),
            &mut last,
            Duration::from_secs(1),
        );
        assert!(history.series("peer-a", Metric::Sessions).is_empty());
    }

    #[test]
    fn a_frozen_screen_keeps_the_history_it_had_when_frozen() {
        let now = Instant::now();
        let mut history = History::new(History::DEFAULT_CAPACITY);
        history.push(
            "peer-a",
            Sample {
                sessions: Some(1),
                ..Sample::default()
            },
        );
        let frozen = freeze(&fleet(), &history, now);
        history.push(
            "peer-a",
            Sample {
                sessions: Some(2),
                ..Sample::default()
            },
        );
        assert_eq!(frozen.history.series("peer-a", Metric::Sessions), vec![8]);
        assert_eq!(history.series("peer-a", Metric::Sessions), vec![4, 8]);
    }

    #[tokio::test]
    async fn run_once_prints_the_matrix_unless_full_is_asked() {
        let url = health_server().await;
        let token = token_file("once-matrix");
        let peers = reader(&url, &token);
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run_once_to(&peers, false, None, &mut out, &mut err).await,
            0
        );
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Fleet 1/1 fresh"), "{out}");
        assert!(out.contains("zen"), "{out}");
        assert!(!out.contains("Providers"), "{out}");
    }

    #[tokio::test]
    async fn run_once_draws_a_gauge_for_each_peer_from_its_own_traffic() {
        let doc_a = r#"{"instance":"peer-a","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
             "requests_1h":42,"limited_1h":0,
             "request_buckets":[1,1,2,2,3,3,4,4,5,5,6,6]}]}"#;
        let doc_b = r#"{"instance":"peer-b","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
             "requests_1h":0,"limited_1h":0,
             "request_buckets":[0,0,0,0,0,0,0,0,0,0,0,0]}]}"#;
        let doc_c = r#"{"instance":"peer-c","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
             "requests_1h":12,"limited_1h":2,
             "request_buckets":[6,6,0,0,0,0,0,0,0,0,0,0]}]}"#;
        let url_a = health_server_with(doc_a).await;
        let url_b = health_server_with(doc_b).await;
        let url_c = health_server_with(doc_c).await;
        let token = token_file("once-gauges");
        let text = format!(
            "[peers]\nenabled = true\n\
             [peers.peer-a]\nurl = \"{url_a}\"\ntoken_file = \"{}\"\n\
             [peers.peer-b]\nurl = \"{url_b}\"\ntoken_file = \"{}\"\n\
             [peers.peer-c]\nurl = \"{url_c}\"\ntoken_file = \"{}\"\n",
            token.display(),
            token.display(),
            token.display()
        );
        let peers = Config::parse(&text).unwrap().peers.unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_once_to(&peers, false, None, &mut out, &mut err).await;
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("■ ▁▃▄▅▇█"), "peer-a: {out}");
        assert!(out.contains("■ ······"), "peer-b: {out}");
        assert!(out.contains("■ █·····"), "peer-c: {out}");
    }

    #[derive(Clone, Default)]
    struct FakeWriter {
        text: Rc<RefCell<String>>,
        fail: bool,
    }

    impl FakeWriter {
        fn text(&self) -> String {
            self.text.borrow().clone()
        }
    }

    impl ScreenControl for FakeWriter {
        fn enter(&mut self) -> io::Result<()> {
            self.text.borrow_mut().push_str("enter;");
            if self.fail {
                return Err(io::Error::other("no tty"));
            }
            Ok(())
        }

        fn restore(&mut self) {
            self.text.borrow_mut().push_str("restore;");
        }
    }

    #[test]
    fn the_guard_restores_the_screen_when_dropped() {
        let writer = FakeWriter::default();
        {
            let mut guard = Guard::new(writer.clone());
            guard.enter().unwrap();
            assert_eq!(writer.text(), "enter;");
        }
        assert_eq!(writer.text(), "enter;restore;");
    }

    #[test]
    fn the_guard_restores_the_screen_when_entering_fails() {
        let writer = FakeWriter {
            fail: true,
            ..FakeWriter::default()
        };
        {
            let mut guard = Guard::new(writer.clone());
            assert!(guard.enter().is_err());
        }
        assert_eq!(writer.text(), "enter;restore;");
    }

    fn token_file(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("reiki-top-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "token-for-top-tests").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn reader(url: &str, token: &std::path::Path) -> PeersConfig {
        let text = format!(
            "[peers]\nenabled = true\n[peers.peer-a]\nurl = \"{url}\"\ntoken_file = \"{}\"\n",
            token.display()
        );
        Config::parse(&text).unwrap().peers.unwrap()
    }

    async fn health_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .fallback(|| async { ([("content-type", "application/json")], DOC) });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn run_once_prints_the_provider_table_and_exits_zero() {
        let url = health_server().await;
        let token = token_file("once-ok");
        let peers = reader(&url, &token);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_once_to(&peers, true, None, &mut out, &mut err).await;
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        assert!(out.contains("Providers"), "{out}");
        assert!(out.contains("zen"), "{out}");
        assert!(out.contains("1/1"), "{out}");
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    }

    #[tokio::test]
    async fn run_once_exits_one_when_no_peer_answers() {
        let token = token_file("once-down");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let peers = reader(&format!("http://127.0.0.1:{port}"), &token);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_once_to(&peers, true, None, &mut out, &mut err).await;
        let out = String::from_utf8(out).unwrap();
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 1);
        assert!(out.contains("Providers"), "{out}");
        assert!(out.contains("no data"), "{out}");
        assert!(err.contains("peer `peer-a`"), "{err}");
    }

    #[test]
    fn moving_the_selection_drops_the_detail_choice_and_its_scroll() {
        let all = vec![
            "peer-a".to_owned(),
            "peer-b".to_owned(),
            "peer-c".to_owned(),
        ];
        let mut state = ui::UiState::default();
        apply_action(&mut state, ui::Action::NextPeer, &all, 10, 3, false);
        apply_action(&mut state, ui::Action::ToggleDetail, &all, 10, 3, false);
        state.scroll = 4;
        assert_eq!(
            state.detail,
            Some(false),
            "Enter closed the pane that followed"
        );
        apply_action(&mut state, ui::Action::NextPeer, &all, 10, 3, false);
        assert_eq!(state.detail, None, "the new peer decides for itself");
        assert_eq!(state.scroll, 0);
    }

    async fn health_server_with(doc: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .fallback(move || async move { ([("content-type", "application/json")], doc) });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn run_once_filters_to_one_provider_and_hides_absent_peers() {
        let doc_a = r#"{"instance":"peer-a","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,"soonest_cooldown_seconds":null,"last_status_class":"2xx"},
            {"name":"openrouter","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,"soonest_cooldown_seconds":null,"last_status_class":"2xx"}]}"#;
        let doc_b = r#"{"instance":"peer-b","observed_at":"t","upstreams":[
            {"name":"openrouter","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,"soonest_cooldown_seconds":60,"last_status_class":"4xx"}]}"#;
        let url_a = health_server_with(doc_a).await;
        let url_b = health_server_with(doc_b).await;
        let token = token_file("once-provider-filter");
        let text = format!(
            "[peers]\nenabled = true\n\
             [peers.peer-a]\nurl = \"{url_a}\"\ntoken_file = \"{}\"\n\
             [peers.peer-b]\nurl = \"{url_b}\"\ntoken_file = \"{}\"\n",
            token.display(),
            token.display()
        );
        let peers = Config::parse(&text).unwrap().peers.unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_once_to(&peers, false, Some("zen"), &mut out, &mut err).await;
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("filter: zen"), "{out}");
        assert!(out.contains("peer-a"), "{out}");
        assert!(!out.contains("peer-b"), "absent peer hidden: {out}");
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    }

    #[tokio::test]
    async fn run_once_unknown_provider_prints_stderr_and_shows_all() {
        let url = health_server().await;
        let token = token_file("once-provider-unknown");
        let peers = reader(&url, &token);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_once_to(&peers, false, Some("gone"), &mut out, &mut err).await;
        assert_eq!(code, 0);
        let out = String::from_utf8(out).unwrap();
        let err = String::from_utf8(err).unwrap();
        assert!(!out.contains("filter: gone"), "{out}");
        assert!(out.contains("zen"), "{out}");
        assert_eq!(err.trim(), "unknown provider: gone");
    }
    const COOLING_DOC: &str = r#"{"instance":"peer-b","observed_at":"t","upstreams":[
        {"name":"zen","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,
         "soonest_cooldown_seconds":90,"last_status_class":"4xx"}]}"#;

    fn plain(lines: &[view::Line]) -> Vec<String> {
        lines.iter().map(view::Line::plain_text).collect()
    }

    #[test]
    fn the_cooling_panel_is_separate_from_the_matrix_lines() {
        let now = Instant::now();
        let all = vec!["peer-a".to_owned(), "peer-b".to_owned()];
        let mut f = fleet();
        apply_feed(&mut f, "peer-a", FeedEvent::Snapshot(DOC.to_owned()), now);
        apply_feed(
            &mut f,
            "peer-b",
            FeedEvent::Snapshot(COOLING_DOC.to_owned()),
            now,
        );
        let state = ui::UiState {
            detail: Some(false),
            ..ui::UiState::default()
        };
        let history = History::new(0);

        let matrix = plain(&view::render_matrix_mode(
            &Matrix::build(&f, &all, now),
            &history,
            Some(0),
            98,
            None,
            state.gauge,
        ));
        let parts = build_parts(&f, &history, &all, now, &state, 100);
        assert_eq!(
            plain(&parts.lines),
            matrix,
            "the matrix lines are unchanged"
        );
        assert_eq!(parts.rows, 2);
        assert_eq!(parts.cooling.len(), 1, "{:?}", plain(&parts.cooling));
        assert!(plain(&parts.cooling)[0].contains("peer-b"));
        assert!(!plain(&parts.cooling).iter().any(|l| l == "Cooling"));

        let healthy_only = build_parts(&f, &history, &all[..1], now, &state, 100);
        assert!(healthy_only.cooling.is_empty());
    }

    #[tokio::test]
    async fn run_once_prints_the_cooling_panel_after_the_matrix() {
        let doc_a = r#"{"instance":"peer-a","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0}]}"#;
        let url_a = health_server_with(doc_a).await;
        let url_b = health_server_with(COOLING_DOC).await;
        let token = token_file("once-cooling");
        let text = format!(
            "[peers]\nenabled = true\n\
             [peers.peer-a]\nurl = \"{url_a}\"\ntoken_file = \"{}\"\n\
             [peers.peer-b]\nurl = \"{url_b}\"\ntoken_file = \"{}\"\n",
            token.display(),
            token.display()
        );
        let peers = Config::parse(&text).unwrap().peers.unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_once_to(&peers, false, None, &mut out, &mut err).await;
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        let out = String::from_utf8(out).unwrap();
        let cooling = out
            .find("Cooling")
            .unwrap_or_else(|| panic!("panel: {out}"));
        assert!(out.find("Fleet").unwrap() < cooling, "{out}");
        assert!(out[cooling..].contains("peer-b"), "{out}");
    }
}
