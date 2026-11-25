//! `reiki top`: a live terminal view of the key health of every configured peer.
//!
//! The feed layer follows each peer's `/v1/health/stream` with the same pinned client, token
//! and SSE parser as `reiki peers --watch`, and falls back to one `/v1/health` poll when a
//! stream fails. The reducer turns those messages into a [`Fleet`]; the view model and the
//! ratatui layer live in the dependency-free `reiki-top` crate.

use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reiki_top::matrix::{
    History, HostSummary, Matrix, Metric, Sample, fleet_series, is_fleet_row, is_problem,
    key_counts, order_peers, peer_columns, resolve, usage_by_provider,
};
use reiki_top::streams::{StreamSort, StreamsView, order_rows, stream_columns};
use reiki_top::ui::{self, ChartMetric, ChartSeries, crossterm, ratatui};
use reiki_top::{Fleet, Health, view};
use tokio::sync::mpsc;

use crate::config::{Config, PeerEntry, PeersConfig};
use crate::peers::client::{self, Chunk, MAX_BODY, SseItem, SseParser};

/// A failed stream-console refresh retries at most once a minute.
const STREAMS_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Unix seconds for the labels that count down token and cooldown deadlines.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

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
    /// The local instance's admin answer: full rows for one published instance.
    LocalFailures {
        instance: String,
        failures: Vec<reiki_top::Failure>,
    },
    /// The local instance's estimated cost, from its admin listener. Not a peer event.
    LocalCost(reiki_top::LocalCost),
    /// The local read-only stream console, from the same admin listener. Not a peer event.
    LocalStreams(StreamsView),
    /// Why the local stream console could not be refreshed.
    LocalStreamsError(String),
}

/// The first non-empty of the read-only and full admin tokens; reading needs only the former.
fn local_token(read_only: Option<String>, full: Option<String>) -> Option<String> {
    [read_only, full]
        .into_iter()
        .flatten()
        .find(|token| !token.is_empty())
}

/// Where the local instance's admin listener is, and which published name is ours.
#[derive(Clone)]
pub struct LocalFailures {
    base: String,
    token: String,
    instance: String,
}

impl LocalFailures {
    /// The local admin address and token, or `None` without `[admin]` and either
    /// `REIKI_ADMIN_READ_TOKEN` or `REIKI_ADMIN_TOKEN`. `reiki top` only reads, so the read-only
    /// token is preferred when both are set. With the read-only token the failure rows come
    /// without their body snippets. Remote peers keep counts only.
    pub fn from_config(config: &Config) -> Option<Self> {
        let admin = config.admin.as_ref()?;
        let token = local_token(
            std::env::var("REIKI_ADMIN_READ_TOKEN").ok(),
            std::env::var("REIKI_ADMIN_TOKEN").ok(),
        )?;
        let instance = config
            .peers
            .as_ref()
            .and_then(|peers| peers.instance.clone())
            .unwrap_or_else(|| "reiki".to_owned());
        Some(Self {
            base: format!("http://{}", admin.listen.replace("0.0.0.0", "127.0.0.1")),
            token,
            instance,
        })
    }

    async fn fetch(&self) -> Result<Vec<reiki_top::Failure>, String> {
        let response = reqwest::Client::new()
            .get(format!("{}/v1/failures", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let body = response.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("the admin listener answered {status}"));
        }
        serde_json::from_str(&body).map_err(|e| format!("bad failure list: {e}"))
    }

    /// Fetches estimated spend from the cost plugin. A 404 means the plugin is not compiled
    /// in or not configured, which is normal and must not look like a peer failure.
    async fn fetch_cost(&self) -> Result<reiki_top::LocalCost, String> {
        let response = reqwest::Client::new()
            .get(format!("{}/v1/cost", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let body = response.text().await.map_err(|e| e.to_string())?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err("cost plugin is not enabled".to_owned());
        }
        if !status.is_success() {
            return Err(format!("the admin listener answered {status}"));
        }
        reiki_top::LocalCost::from_json(&body)
    }

    /// Fetches the read-only stream console with the ordinary admin token. The stream-admin
    /// token is never read here: this route cannot create, rotate or revoke anything.
    async fn fetch_streams(&self) -> Result<StreamsView, String> {
        let response = reqwest::Client::new()
            .get(format!("{}/v1/stream-console", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("cannot reach the admin listener: {e}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("cannot read the admin answer: {e}"))?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err("this instance does not serve the stream console".to_owned());
        }
        if !status.is_success() {
            return Err(format!("the admin listener answered {status}"));
        }
        StreamsView::from_json(&body)
    }
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
    run_with_local(peers, None, once, interval, full, provider).await
}

/// [`run`] with the local instance's admin target, so its full failure rows can be shown while
/// every remote peer keeps counts only.
pub async fn run_with_local(
    peers: Option<&PeersConfig>,
    local: Option<LocalFailures>,
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
        run_once_with_local(peers, local.as_ref(), full, provider).await
    } else {
        run_live_with_local(peers, local, interval, full, provider).await
    }
}

/// Polls every peer once, prints the fleet as plain text and returns 0 when at least one peer
/// answered, 1 when none did.
pub async fn run_once(peers: &PeersConfig, full: bool, provider: Option<&str>) -> i32 {
    run_once_to(peers, full, provider, &mut io::stdout(), &mut io::stderr()).await
}

pub async fn run_once_with_local(
    peers: &PeersConfig,
    local: Option<&LocalFailures>,
    full: bool,
    provider: Option<&str>,
) -> i32 {
    run_once_to_with_local(
        peers,
        local,
        full,
        provider,
        &mut io::stdout(),
        &mut io::stderr(),
    )
    .await
}

/// [`run_once`] with the output handles passed in, so tests need no terminal.
async fn run_once_to(
    peers: &PeersConfig,
    full: bool,
    provider: Option<&str>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> i32 {
    run_once_to_with_local(peers, None, full, provider, out, err).await
}

async fn run_once_to_with_local(
    peers: &PeersConfig,
    local: Option<&LocalFailures>,
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

    if let Some(local) = local {
        // A failed local fetch is not a peer error; the counts still come from health.
        if let Ok(failures) = local.fetch().await {
            fleet.set_local_failures(&local.instance, failures);
        }
        if let Ok(cost) = local.fetch_cost().await {
            fleet.set_local_cost(cost);
        }
    }

    // No history on a single poll, so the matrix draws no sparklines.
    let lines = if full {
        view::render(&fleet, &names, now, 100)
    } else {
        let matrix = Matrix::build_fleet(&fleet, &names, now);
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
        if let Some(cost) = view::cost_line(&fleet, 100) {
            lines.push(cost);
        }
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
        FeedEvent::LocalFailures { instance, failures } => {
            fleet.set_local_failures(&instance, failures)
        }
        FeedEvent::LocalCost(cost) => fleet.set_local_cost(cost),
        FeedEvent::LocalStreams(streams) => fleet.set_local_streams(streams),
        FeedEvent::LocalStreamsError(reason) => fleet.set_streams_error(reason),
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

/// Refreshes the local instance's failure rows on the same cadence as health polls.
async fn feed_local(local: LocalFailures, poll: Duration, tx: mpsc::Sender<(String, FeedEvent)>) {
    loop {
        if let Ok(failures) = local.fetch().await {
            let event = FeedEvent::LocalFailures {
                instance: local.instance.clone(),
                failures,
            };
            if tx.send((local.instance.clone(), event)).await.is_err() {
                return;
            }
        }
        if let Ok(cost) = local.fetch_cost().await
            && tx
                .send((local.instance.clone(), FeedEvent::LocalCost(cost)))
                .await
                .is_err()
        {
            return;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Refreshes the local stream console. A failure doubles the wait up to a minute, so an admin
/// listener that is down cannot turn the view into a tight polling loop.
async fn feed_streams(local: LocalFailures, poll: Duration, tx: mpsc::Sender<(String, FeedEvent)>) {
    let poll = poll.max(Duration::from_secs(1));
    let mut backoff = poll;
    loop {
        let (event, delay) = match local.fetch_streams().await {
            Ok(streams) => (FeedEvent::LocalStreams(streams), poll),
            Err(reason) => {
                let delay = backoff;
                backoff = backoff.saturating_mul(2).min(STREAMS_BACKOFF_MAX);
                (FeedEvent::LocalStreamsError(reason), delay)
            }
        };
        if tx.send((local.instance.clone(), event)).await.is_err() {
            return;
        }
        tokio::time::sleep(delay).await;
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
    run_live_with_local(peers, None, interval, full, provider).await
}

async fn run_live_with_local(
    peers: &PeersConfig,
    local: Option<LocalFailures>,
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
    if let Some(local) = local {
        tasks.push(tokio::spawn(feed_local(local.clone(), poll, tx.clone())));
        tasks.push(tokio::spawn(feed_streams(local, poll, tx.clone())));
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
        // More than one configured peer starts on ALL, so the chart is the fleet view until the
        // user moves. `ALL` is the first display row.
        selected: (all.len() > 1).then_some(0),
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
                let (total, viewport) = match measure(
                    terminal,
                    shown,
                    shown_history,
                    shown_at,
                    &all,
                    &state,
                    redraw,
                ) {
                        Ok(pair) => pair,
                        Err(e) => {
                            eprintln!("reiki: cannot size the terminal: {e}");
                            return 1;
                        }
                    };
                let sorting = matches!(
                    action,
                    ui::Action::SortNext | ui::Action::SortPrev | ui::Action::SortReverse
                );
                if state.mode == ui::Mode::Matrix {
                    let matrix = shown_matrix(shown, &all, shown_at, &state);
                    follow_cursor(&mut state, &matrix);
                }
                let keep = if state.mode == ui::Mode::Streams {
                    let names = stream_names(shown.local_streams(), state.stream_sort.as_ref());
                    apply_streams_action(&mut state, action, &names, total, viewport)
                } else if sorting {
                    if state.mode == ui::Mode::Matrix {
                        let matrix = shown_matrix(shown, &all, shown_at, &state);
                        apply_sort(&mut state, action, &matrix);
                    }
                    true
                } else {
                    let problem = selected_peer(shown, &all, shown_at, &state)
                        .is_some_and(|(_, problem)| problem);
                    if action == ui::Action::CycleProvider {
                        if state.mode == ui::Mode::Matrix {
                            let matrix = Matrix::build_fleet(shown, &all, shown_at);
                            let providers: Vec<String> =
                                matrix.columns.iter().map(|c| c.upstream.clone()).collect();
                            state.cycle_provider(&providers);
                        }
                        true
                    } else {
                        apply_action(&mut state, action, &all, total, viewport, problem)
                    }
                };
                // A sort keeps the cursor on its peer through the draw; any other key records
                // which peer the cursor landed on.
                if state.mode == ui::Mode::Matrix && !sorting {
                    let matrix = shown_matrix(shown, &all, shown_at, &state);
                    record_cursor(&mut state, &matrix);
                }
                if !keep {
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
            if state.mode == ui::Mode::Matrix {
                let matrix = shown_matrix(shown, &all, shown_at, &state);
                follow_cursor(&mut state, &matrix);
            }
            let shown = Shown {
                fleet: shown,
                history: shown_history,
                now: shown_at,
                paused_for,
            };
            if let Err(e) = render_screen(terminal, &shown, &all, &state, redraw) {
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

/// Stream names in display order. Keeping this in one place makes key movement, the table and the
/// detail pane agree on which row is selected after a refresh or a re-sort.
fn stream_names(view: Option<&StreamsView>, sort: Option<&StreamSort>) -> Vec<String> {
    view.map_or_else(Vec::new, |console| {
        order_rows(&console.rows, sort)
            .into_iter()
            .map(|i| console.rows[i].name.clone())
            .collect()
    })
}

/// The selected stream's display position. Before a stream is chosen the first row is selected;
/// a stream that left the snapshot starts again from that row.
fn stream_position(names: &[String], state: &ui::UiState) -> Option<usize> {
    if names.is_empty() {
        return None;
    }
    Some(
        state
            .selected_stream
            .as_deref()
            .and_then(|name| names.iter().position(|candidate| candidate == name))
            .unwrap_or(0),
    )
}

/// The selected display row and its stream name. An explicit choice wins; before one is made the
/// first row is shown. Both the table marker and the detail pane use the same answer.
fn selected_stream<'a>(
    view: &'a StreamsView,
    order: &[usize],
    state: &ui::UiState,
) -> Option<(usize, &'a str)> {
    let position = state
        .selected_stream
        .as_deref()
        .and_then(|name| order.iter().position(|&i| view.rows[i].name == name))
        .or_else(|| (!order.is_empty()).then_some(0))?;
    Some((position, view.rows[order[position]].name.as_str()))
}

/// Applies one key action on the Streams screen. Movement keys pick a stream by name and reset the
/// detail scroll; paging keys scroll the detail. Sorting keys work like the matrix's, with the
/// default usage order between the last and first columns. Nothing here can change a stream.
fn apply_streams_action(
    state: &mut ui::UiState,
    action: ui::Action,
    names: &[String],
    detail_lines: usize,
    viewport: usize,
) -> bool {
    match action {
        ui::Action::Quit => return false,
        ui::Action::SortNext | ui::Action::SortPrev => {
            state.cycle_stream_sort(&stream_columns(), action == ui::Action::SortNext);
            return true;
        }
        ui::Action::SortReverse => {
            state.reverse_stream_sort();
            return true;
        }
        ui::Action::ScrollDown | ui::Action::NextPeer => {
            let current = stream_position(names, state);
            state.selected_stream = (!names.is_empty()).then(|| {
                let next = current.map_or(0, |row| if row + 1 < names.len() { row + 1 } else { 0 });
                names[next].clone()
            });
            state.scroll = 0;
        }
        ui::Action::ScrollUp | ui::Action::PrevPeer => {
            let current = stream_position(names, state);
            state.selected_stream = (!names.is_empty()).then(|| {
                let last = names.len() - 1;
                let prev = current.map_or(last, |row| row.checked_sub(1).unwrap_or(last));
                names[prev].clone()
            });
            state.scroll = 0;
        }
        ui::Action::PageUp => {
            state.scroll = state.scroll.saturating_sub(viewport.max(1));
        }
        ui::Action::PageDown => {
            state.scroll = state.scroll.saturating_add(viewport.max(1));
        }
        ui::Action::Top => state.scroll = 0,
        ui::Action::Bottom => state.scroll = detail_lines.saturating_sub(viewport.max(1)),
        ui::Action::ToggleStreams => {
            state.apply(action, detail_lines, viewport);
        }
        _ => {
            state.apply(action, detail_lines, viewport);
        }
    }
    state.scroll = state
        .scroll
        .min(detail_lines.saturating_sub(viewport.max(1)));
    true
}

/// The matrix the screen shows: every configured peer, narrowed to the provider filter.
fn shown_matrix(fleet: &Fleet, all: &[String], now: Instant, state: &ui::UiState) -> Matrix {
    let full = Matrix::build_fleet(fleet, all, now);
    match state.filter.provider.as_deref() {
        Some(p) => full.filter_provider(p).unwrap_or(full),
        None => full,
    }
}

/// Applies one sort key to the matrix. The columns are the ones on screen, so `o` never lands on
/// a provider the filter hides.
fn apply_sort(state: &mut ui::UiState, action: ui::Action, matrix: &Matrix) {
    match action {
        ui::Action::SortReverse => {
            state.reverse_sort();
        }
        _ => state.cycle_sort(&peer_columns(matrix), action == ui::Action::SortNext),
    }
}

/// Moves the cursor back onto the peer it was on. A refresh or a sort can move that peer to
/// another row; the position is only re-taken when the peer has left the list.
fn follow_cursor(state: &mut ui::UiState, matrix: &Matrix) {
    if state.selected.is_none() {
        state.selected_peer = None;
        return;
    }
    let order = order_peers(matrix, state.peer_sort.as_ref());
    let found = state
        .selected_peer
        .as_ref()
        .and_then(|name| order.iter().position(|&i| matrix.rows[i].peer == *name));
    match found {
        Some(pos) => state.selected = Some(pos),
        None => record_cursor(state, matrix),
    }
}

/// Remembers the peer under the cursor, so `follow_cursor` can find it again.
fn record_cursor(state: &mut ui::UiState, matrix: &Matrix) {
    let order = order_peers(matrix, state.peer_sort.as_ref());
    state.selected_peer = state
        .selected
        .and_then(|pos| resolve(Some(pos), order.len()))
        .map(|pos| matrix.rows[order[pos]].peer.clone());
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
    // `a` is the matrix's shortcut back to the fleet row.
    if action == ui::Action::SelectFleet {
        if state.mode == ui::Mode::Matrix {
            state.apply(action, total, viewport);
        }
        return true;
    }
    // The gauge and the chart only exist in the matrix, so the full view ignores their keys.
    if matches!(
        action,
        ui::Action::ToggleMetric
            | ui::Action::ToggleWindow
            | ui::Action::CycleChart
            | ui::Action::ToggleStack
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
    let matrix = shown_matrix(fleet, all, now, state);
    let order = order_peers(&matrix, state.peer_sort.as_ref());
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
    status: Vec<view::Line>,
    providers: Vec<String>,
    chart_title: String,
    chart_series: Vec<ChartSeries>,
    chart_interval: Duration,
    keys: [u64; 3],
}

fn build_parts(
    fleet: &Fleet,
    history: &History,
    all: &[String],
    now: Instant,
    state: &ui::UiState,
    width: usize,
    sample_interval: Duration,
) -> Parts {
    match state.mode {
        ui::Mode::Full => Parts {
            lines: view::render(fleet, &visible_cts(all, state), now, width),
            ..Parts::default()
        },
        ui::Mode::Streams => {
            // Both panes draw inside a border. The detail pane only exists on a wide frame.
            let inner = width.saturating_sub(2);
            let detail_width = if width >= 60 {
                width.saturating_sub(width * 3 / 5).saturating_sub(2)
            } else {
                0
            };
            let Some(console) = fleet.local_streams() else {
                return Parts {
                    lines: view::streams_waiting(fleet.streams_error(), inner),
                    detail: Some(Vec::new()),
                    ..Parts::default()
                };
            };
            let now = unix_now();
            let order = order_rows(&console.rows, state.stream_sort.as_ref());
            let selected_stream = selected_stream(console, &order, state);
            let (selected, name) = match selected_stream {
                Some((position, name)) => (Some(position), Some(name)),
                None => (None, None),
            };
            Parts {
                lines: view::streams_table(console, name, state.stream_sort.as_ref(), now, inner),
                rows: console.rows.len(),
                selected,
                detail: Some(view::stream_detail(console, name, now, detail_width)),
                status: view::streams_status(console, fleet.streams_error(), inner),
                ..Parts::default()
            }
        }
        ui::Mode::Matrix => {
            // The panels draw inside a border.
            let inner = width.saturating_sub(2);
            let full = Matrix::build_fleet(fleet, all, now);
            let (fresh, total) = full.fresh_counts();
            let providers = full.columns.iter().map(|c| c.upstream.clone()).collect();
            let (matrix, filter_name): (Matrix, Option<&str>) =
                match state.filter.provider.as_deref() {
                    Some(p) => match full.filter_provider(p) {
                        Some(fm) => (fm, Some(p)),
                        None => (full, None),
                    },
                    None => (full, None),
                };
            let order = order_peers(&matrix, state.peer_sort.as_ref());
            let pos = resolve(state.selected, order.len());
            let row = pos.map(|pos| &matrix.rows[order[pos]]);
            let detail = row.and_then(|row| {
                state.detail_open(is_problem(row)).then(|| {
                    if is_fleet_row(row) {
                        view::fleet_detail(fleet, all, now, inner)
                    } else {
                        view::peer_detail(fleet, &row.peer, now, inner)
                    }
                })
            });
            // The chart follows the user's own selection; ALL (the default) is the fleet view.
            let chart_row = state.selected.and(row);
            let fleet_view = chart_row.is_none_or(is_fleet_row);
            let (chart_series, chart_interval) =
                chart_series(state, history, &matrix, chart_row, sample_interval);
            let chart_title = if state.chart == ChartMetric::Requests {
                let subject = match chart_row {
                    Some(row) if !fleet_view => format!("Requests · {}", row.peer),
                    _ => "Requests · fleet".to_owned(),
                };
                let mut title = format!("{} · {}", subject, state.chart_usage.unit());
                if fleet_view && fresh < total {
                    title.push_str(&format!(" · fleet {fresh} of {total} fresh"));
                }
                title
            } else {
                match (state.chart, fleet_view) {
                    (ChartMetric::Disk, true) => format!("{} · fleet max", state.chart.title()),
                    (_, true) => format!("{} · fleet", state.chart.title()),
                    (_, false) => format!("{} · {}", state.chart.title(), chart_row.unwrap().peer),
                }
            };
            Parts {
                lines: view::render_matrix_mode(
                    &matrix,
                    history,
                    pos,
                    inner,
                    filter_name,
                    state.gauge,
                    state.peer_sort.as_ref(),
                ),
                rows: matrix.rows.len(),
                selected: pos,
                // The panel's own title is drawn by the frame, so its first line is dropped.
                cooling: view::cooling_panel(fleet, all, filter_name, now, inner)
                    .into_iter()
                    .skip(1)
                    .collect(),
                detail,
                status: Vec::new(),
                providers,
                chart_title,
                chart_series,
                chart_interval,
                keys: key_counts(fleet, all, now),
            }
        }
    }
}

/// The chart's lines and the time one point covers. Requests get one coloured line per provider;
/// the other metrics keep the single fleet or selected-peer history series.
fn chart_series(
    state: &ui::UiState,
    history: &History,
    matrix: &Matrix,
    row: Option<&reiki_top::matrix::Row>,
    sample_interval: Duration,
) -> (Vec<ChartSeries>, Duration) {
    if state.chart == ChartMetric::Requests {
        let source = match row.filter(|r| !is_fleet_row(r)) {
            Some(row) => Matrix {
                columns: matrix.columns.clone(),
                rows: vec![row.clone()],
                hidden_columns: matrix.hidden_columns,
            },
            None => matrix.clone(),
        };
        let series = usage_by_provider(&source, state.chart_usage);
        let interval = series
            .first()
            .map_or(sample_interval, |s| s.series.interval);
        let lines = series
            .into_iter()
            .enumerate()
            .map(|(i, provider)| ChartSeries {
                name: provider.upstream,
                points: provider.series.points.into_iter().map(Some).collect(),
                color: ui::chart_color(i),
            })
            .collect();
        return (lines, interval);
    }
    let metric = match state.chart {
        ChartMetric::Load => Metric::Load1,
        ChartMetric::Disk => Metric::DiskPercent,
        ChartMetric::Sessions => Metric::Sessions,
        ChartMetric::Requests => unreachable!("handled above"),
    };
    let points = match row.filter(|r| !is_fleet_row(r)) {
        Some(row) => history
            .values(&row.peer, metric)
            .into_iter()
            .map(Some)
            .collect(),
        None => {
            let peers: Vec<String> = matrix
                .rows
                .iter()
                .filter(|r| !is_fleet_row(r))
                .map(|r| r.peer.clone())
                .collect();
            fleet_series(history, &peers, metric)
        }
    };
    (
        vec![ChartSeries {
            name: state.chart.label().to_owned(),
            points,
            color: ui::chart_color(0),
        }],
        sample_interval,
    )
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
    sample_interval: Duration,
) -> io::Result<(usize, usize)> {
    let size = terminal.size()?;
    let parts = build_parts(
        fleet,
        history,
        all,
        now,
        state,
        usize::from(size.width),
        sample_interval,
    );
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
        (ui::Mode::Streams, Some(detail)) => {
            let body = usize::from(size.height.saturating_sub(2));
            let viewport = if usize::from(size.width) >= 60 {
                body.saturating_sub(2)
            } else {
                0
            };
            (detail.len(), viewport)
        }
        (ui::Mode::Streams, None) => (0, 0),
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
    sample_interval: Duration,
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
    let clock = ui::utc_clock(unix_now());
    terminal.draw(|frame| {
        let parts = build_parts(
            shown.fleet,
            shown.history,
            all,
            shown.now,
            state,
            usize::from(frame.area().width),
            sample_interval,
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
                    chart_series: &parts.chart_series,
                    chart_interval: parts.chart_interval,
                    keys: parts.keys,
                },
                state,
            ),
            ui::Mode::Streams => ui::draw_streams(
                frame,
                &ui::StreamsFrame {
                    table: &parts.lines,
                    detail: parts.detail.as_deref().unwrap_or(&[]),
                    status: &parts.status,
                    selected: parts.selected,
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

    const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

    const DOC: &str = r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : "2xx",
         "name" : "zen",
         "soonest_cooldown_seconds" : null
      }
   ]
}
"#;

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
                data: r#"{
   "age_seconds" : 0,
   "observed_at" : "t",
   "upstream" : {
      "cooling_combinations" : 0,
      "cooling_keys" : 1,
      "disabled_keys" : 0,
      "healthy_keys" : 0,
      "last_status_class" : "4xx",
      "name" : "zen",
      "soonest_cooldown_seconds" : null
   }
}
"#
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
        let parts = build_parts(&fleet, &history, &all, now, &state, 100, SAMPLE_INTERVAL);
        assert_eq!(
            parts.chart_title, "Disk · fleet max",
            "no selection, so the fleet"
        );
        assert_eq!(parts.chart_series[0].points, vec![Some(40.0)]);
        assert!(parts.keys.iter().sum::<u64>() > 0, "{:?}", parts.keys);
        assert_eq!(parts.providers, vec!["zen".to_owned()]);

        state.chart = ui::ChartMetric::Load;
        state.selected = Some(1); // 0 is the ALL row
        let parts = build_parts(&fleet, &history, &all, now, &state, 100, SAMPLE_INTERVAL);
        assert_eq!(parts.chart_title, "Load · peer-a");
        assert_eq!(parts.chart_series[0].points, vec![Some(1.5)]);
        assert_eq!(parts.chart_series[0].name, "load");
    }

    #[test]
    fn the_all_view_sums_providers_into_a_fleet_chart_and_opens_fleet_detail() {
        let now = Instant::now();
        let mut fleet = fleet();
        let usage = |name: &str, zen: &str, sample1: &str| {
            format!(
                r#"{{"instance":"{name}","reiki_version":"0.10.3","observed_at":"t","age_seconds":0,
                    "observed" : [],
                    "failures":{{}},"upstreams":[
                    {{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                      "cooling_combinations":0,"soonest_cooldown_seconds":null,"last_status_class":"2xx",
                      "requests_1h":10,"limited_1h":2,"request_buckets":[0,0,0,0,0,0,0,0,0,0,1,9],
                      "tokens_1h":10,"token_buckets":[0,0,0,0,0,0,0,0,0,0,1,9],
                      "request_minutes":{zen},"token_minutes":{zen}}},
                    {{"name":"sample1","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                      "cooling_combinations":0,"soonest_cooldown_seconds":null,"last_status_class":"2xx",
                      "requests_1h":3,"limited_1h":1,"request_buckets":[0,0,0,0,0,0,0,0,0,0,0,3],
                      "tokens_1h":3,"token_buckets":[0,0,0,0,0,0,0,0,0,0,0,3],
                      "request_minutes":{sample1},"token_minutes":{sample1}}}]}}"#
            )
        };
        apply_feed(
            &mut fleet,
            "peer-a",
            FeedEvent::Snapshot(usage(
                "peer-a",
                "[1,1,1,1,1,1,1,1,1,1]",
                "[0,0,0,0,0,0,0,0,0,1]",
            )),
            now,
        );
        apply_feed(
            &mut fleet,
            "peer-b",
            FeedEvent::Snapshot(usage(
                "peer-b",
                "[0,0,0,0,0,0,0,0,0,2]",
                "[0,0,0,0,0,0,0,0,0,2]",
            )),
            now,
        );
        apply_feed(
            &mut fleet,
            "peer-c",
            FeedEvent::Snapshot(usage(
                "peer-c",
                "[9,9,9,9,9,9,9,9,9,9]",
                "[9,9,9,9,9,9,9,9,9,9]",
            )),
            now - Duration::from_secs(91),
        );
        let all = vec![
            "peer-a".to_owned(),
            "peer-b".to_owned(),
            "peer-c".to_owned(),
        ];
        let history = History::new(0);
        let state = ui::UiState {
            selected: Some(0),
            detail: Some(false),
            chart: ui::ChartMetric::Requests,
            ..ui::UiState::default()
        };
        let parts = build_parts(
            &fleet,
            &history,
            &all,
            now,
            &state,
            100,
            Duration::from_secs(1),
        );
        assert!(
            parts.chart_title.contains("Requests · fleet"),
            "{:?}",
            parts.chart_title
        );
        assert!(
            parts.chart_title.contains("fleet 2 of 3 fresh"),
            "{:?}",
            parts.chart_title
        );
        assert_eq!(parts.chart_interval, Duration::from_secs(60));
        assert_eq!(
            parts
                .chart_series
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["sample1", "zen"]
        );
        assert_eq!(parts.chart_series[0].points[11], Some(3.0));
        assert!(
            parts.detail.is_none(),
            "ALL does not open a detail by itself"
        );

        let mut opened = state.clone();
        opened.detail = Some(true);
        let card = build_parts(&fleet, &history, &all, now, &opened, 100, SAMPLE_INTERVAL)
            .detail
            .expect("Enter on ALL opens the fleet detail");
        assert!(
            card[0].plain_text().contains("ALL · fleet"),
            "{:?}",
            card[0]
        );

        let mut peer = state.clone();
        peer.selected = Some(2);
        let parts = build_parts(&fleet, &history, &all, now, &peer, 100, SAMPLE_INTERVAL);
        assert_eq!(parts.chart_title, "Requests · peer-a · req/interval");
        assert_eq!(parts.chart_series[0].points[11], Some(1.0), "one peer only");

        apply_action(&mut peer, ui::Action::SelectFleet, &all, 10, 3, false);
        assert_eq!(peer.selected, Some(0), "a jumps back to ALL");
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
        let parts = build_parts(&fleet, &history, &all, now, &state, 100, SAMPLE_INTERVAL);
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
                r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "peer-b",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 1,
         "disabled_keys" : 0,
         "healthy_keys" : 0,
         "last_status_class" : "4xx",
         "name" : "zen",
         "soonest_cooldown_seconds" : 60
      }
   ]
}
"#
                .to_owned(),
            ),
            now,
        );
        let all = vec!["peer-a".to_owned(), "peer-b".to_owned()];
        let history = History::new(0);
        let state = ui::UiState {
            selected: Some(1), // ALL is row 0; peer-b is the first problem peer
            ..ui::UiState::default()
        };
        let parts = build_parts(&fleet, &history, &all, now, &state, 100, SAMPLE_INTERVAL);
        let card = parts
            .detail
            .expect("peer-b is a problem, so the pane opens");
        assert!(
            card[0].plain_text().starts_with("peer-b  instance"),
            "{card:?}"
        );

        let mut closed = state.clone();
        closed.detail = Some(false);
        assert!(
            build_parts(&fleet, &history, &all, now, &closed, 100, SAMPLE_INTERVAL)
                .detail
                .is_none()
        );
        let mut full = state.clone();
        full.toggle_mode();
        assert!(
            build_parts(&fleet, &history, &all, now, &full, 100, SAMPLE_INTERVAL)
                .detail
                .is_none()
        );
    }

    #[test]
    fn samples_are_pushed_once_per_interval_not_per_event() {
        let every = Duration::from_secs(1);
        let t0 = Instant::now();
        let mut fleet = fleet();
        let doc = r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "sessions" : {
      "active_sessions" : 4
   },
   "upstreams" : []
}
"#;
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
        let doc = r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "sessions" : {
      "active_sessions" : 4
   },
   "upstreams" : []
}
"#;
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
    fn the_read_only_token_is_preferred_for_top() {
        assert_eq!(
            local_token(Some("read".into()), Some("full".into())).as_deref(),
            Some("read")
        );
        assert_eq!(
            local_token(Some(String::new()), Some("full".into())).as_deref(),
            Some("full")
        );
        assert_eq!(local_token(None, Some(String::new())), None);
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
        let doc_a = r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "limited_1h" : 0,
         "name" : "zen",
         "request_buckets" : [
            1,
            1,
            2,
            2,
            3,
            3,
            4,
            4,
            5,
            5,
            6,
            6
         ],
         "request_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "requests_1h" : 42,
         "soonest_cooldown_seconds" : null,
         "token_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "token_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "tokens_1h" : 0
      }
   ]
}
"#;
        let doc_b = r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "peer-b",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "limited_1h" : 0,
         "name" : "zen",
         "request_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "request_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "requests_1h" : 0,
         "soonest_cooldown_seconds" : null,
         "token_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "token_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "tokens_1h" : 0
      }
   ]
}
"#;
        let doc_c = r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "peer-c",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "limited_1h" : 2,
         "name" : "zen",
         "request_buckets" : [
            6,
            6,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "request_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "requests_1h" : 12,
         "soonest_cooldown_seconds" : null,
         "token_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "token_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "tokens_1h" : 0
      }
   ]
}
"#;
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
        let doc_a = r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : "2xx",
         "name" : "zen",
         "soonest_cooldown_seconds" : null
      },
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : "2xx",
         "name" : "openrouter",
         "soonest_cooldown_seconds" : null
      }
   ]
}
"#;
        let doc_b = r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "peer-b",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 1,
         "disabled_keys" : 0,
         "healthy_keys" : 0,
         "last_status_class" : "4xx",
         "name" : "openrouter",
         "soonest_cooldown_seconds" : 60
      }
   ]
}
"#;
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
    const COOLING_DOC: &str = r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "peer-b",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 1,
         "disabled_keys" : 0,
         "healthy_keys" : 0,
         "last_status_class" : "4xx",
         "name" : "zen",
         "soonest_cooldown_seconds" : 90
      }
   ]
}
"#;

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
            &Matrix::build_fleet(&f, &all, now),
            &history,
            Some(0),
            98,
            None,
            state.gauge,
            None,
        ));
        let parts = build_parts(&f, &history, &all, now, &state, 100, SAMPLE_INTERVAL);
        assert_eq!(
            plain(&parts.lines),
            matrix,
            "the matrix lines are unchanged"
        );
        assert_eq!(parts.rows, 3, "ALL plus the two peers");
        assert_eq!(parts.cooling.len(), 1, "{:?}", plain(&parts.cooling));
        assert!(plain(&parts.cooling)[0].contains("peer-b"));
        assert!(!plain(&parts.cooling).iter().any(|l| l == "Cooling"));

        let healthy_only = build_parts(&f, &history, &all[..1], now, &state, 100, SAMPLE_INTERVAL);
        assert!(healthy_only.cooling.is_empty());
    }

    #[tokio::test]
    async fn run_once_prints_the_cooling_panel_after_the_matrix() {
        let doc_a = r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "name" : "zen",
         "soonest_cooldown_seconds" : null
      }
   ]
}
"#;
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

    fn runtime_token() -> String {
        ["rsk", "_", &"c".repeat(64)].concat()
    }

    fn streams_json(secret: &str) -> String {
        streams_json_with_usage(secret, 9, 1)
    }

    fn streams_json_with_usage(secret: &str, busy_rate: u64, quiet_rate: u64) -> String {
        format!(
            r#"{{
              "enabled": true,
              "streams": [
                {{"name": "busy", "dynamic": true, "chain": ["zen"],
                  "limits": {{"rpm": 10, "concurrent": 2}},
                  "tokens": [{{"id": "abcdef1234567890", "expires_at": 4102444800}}],
                  "expiry": 4102444800, "rpm": {{"current": {busy_rate}, "limit": 10}},
                  "concurrency": {{"current": 2, "limit": 2}},
                  "last_used": 1000, "state": "active",
                  "token": "{secret}", "hash": "not shown"}},
                {{"name": "quiet", "dynamic": false, "chain": ["zen", "openrouter"],
                  "limits": {{}}, "tokens": [], "rpm": {{"current": {quiet_rate}, "limit": 30}},
                  "concurrency": {{"current": 0, "limit": 2}},
                  "last_used": null, "state": "active"}}
              ],
              "audit": [
                {{"seq": 2, "time": "2026-11-23T00:01:00Z", "action": "token.add",
                  "stream": "busy", "outcome": "success", "reason": "rotation",
                  "changes": []}}
              ]
            }}"#
        )
    }

    #[test]
    fn streams_feed_keeps_the_last_good_snapshot_on_a_refresh_failure() {
        let now = Instant::now();
        let mut fleet = fleet();
        let streams = StreamsView::from_json(&streams_json(&runtime_token())).expect("fixture");
        apply_feed(&mut fleet, "local", FeedEvent::LocalStreams(streams), now);
        assert_eq!(fleet.local_streams().unwrap().rows.len(), 2);
        assert_eq!(fleet.streams_error(), None);
        apply_feed(
            &mut fleet,
            "local",
            FeedEvent::LocalStreamsError("admin unreachable".to_owned()),
            now,
        );
        assert_eq!(fleet.streams_error(), Some("admin unreachable"));
        assert_eq!(fleet.local_streams().unwrap().rows.len(), 2);
    }

    #[test]
    fn streams_keys_move_the_row_and_page_the_detail_without_leaving_the_view() {
        let mut state = ui::UiState {
            mode: ui::Mode::Streams,
            ..ui::UiState::default()
        };
        let names = ["a".to_owned(), "b".to_owned(), "c".to_owned()];
        apply_streams_action(&mut state, ui::Action::NextPeer, &names, 20, 5);
        assert_eq!(
            (state.selected_stream.as_deref(), state.scroll),
            (Some("b"), 0)
        );
        apply_streams_action(&mut state, ui::Action::PrevPeer, &names, 20, 5);
        assert_eq!(state.selected_stream.as_deref(), Some("a"));
        apply_streams_action(&mut state, ui::Action::ScrollUp, &names, 20, 5);
        assert_eq!(
            state.selected_stream.as_deref(),
            Some("c"),
            "selection wraps"
        );
        apply_streams_action(&mut state, ui::Action::PageDown, &names, 20, 5);
        assert_eq!(state.scroll, 5, "paging scrolls the detail");
        apply_streams_action(&mut state, ui::Action::Bottom, &names, 20, 5);
        assert_eq!(state.scroll, 15);
        apply_streams_action(&mut state, ui::Action::Top, &names, 20, 5);
        assert_eq!(state.scroll, 0);
        assert!(!apply_streams_action(
            &mut state,
            ui::Action::Quit,
            &names,
            20,
            5
        ));
        assert_eq!(state.mode, ui::Mode::Streams, "Quit leaves the state alone");
    }

    #[test]
    fn streams_sort_keys_follow_the_same_o_o_r_rule_as_the_matrix() {
        use reiki_top::streams::StreamColumn;

        let mut state = ui::UiState {
            mode: ui::Mode::Streams,
            ..ui::UiState::default()
        };
        let names = ["busy".to_owned(), "quiet".to_owned()];
        apply_streams_action(&mut state, ui::Action::SortReverse, &names, 20, 5);
        assert_eq!(state.stream_sort, None, "r is a no-op while unsorted");
        apply_streams_action(&mut state, ui::Action::SortNext, &names, 20, 5);
        assert_eq!(
            state.stream_sort,
            Some(StreamSort {
                column: StreamColumn::Name,
                descending: false,
            })
        );
        apply_streams_action(&mut state, ui::Action::SortReverse, &names, 20, 5);
        assert_eq!(
            state.stream_sort,
            Some(StreamSort {
                column: StreamColumn::Name,
                descending: true,
            })
        );
        apply_streams_action(&mut state, ui::Action::SortNext, &names, 20, 5);
        assert_eq!(
            state.stream_sort,
            Some(StreamSort {
                column: StreamColumn::Usage,
                descending: false,
            }),
            "a new column starts ascending"
        );
        apply_streams_action(&mut state, ui::Action::SortPrev, &names, 20, 5);
        assert_eq!(
            state.stream_sort,
            Some(StreamSort {
                column: StreamColumn::Name,
                descending: false,
            })
        );
        apply_streams_action(&mut state, ui::Action::SortPrev, &names, 20, 5);
        assert_eq!(state.stream_sort, None, "before the first is default order");
        assert_eq!(
            state.selected_stream, None,
            "sorting does not pick a stream"
        );
    }

    #[test]
    fn a_stream_stays_selected_across_a_refresh_and_a_resort() {
        use reiki_top::streams::StreamColumn;

        let secret = runtime_token();
        let mut fleet = fleet();
        fleet.set_local_streams(StreamsView::from_json(&streams_json(&secret)).expect("fixture"));
        let mut state = ui::UiState {
            mode: ui::Mode::Streams,
            selected_stream: Some("quiet".to_owned()),
            ..ui::UiState::default()
        };
        let parts = build_parts(
            &fleet,
            &History::new(0),
            &[],
            Instant::now(),
            &state,
            100,
            SAMPLE_INTERVAL,
        );
        assert_eq!(parts.selected, Some(1), "quiet is second by default");
        assert!(
            plain(parts.detail.as_ref().unwrap())
                .join("\n")
                .contains("quiet")
        );

        fleet.set_local_streams(
            StreamsView::from_json(&streams_json_with_usage(&secret, 0, 5)).expect("fixture"),
        );
        let parts = build_parts(
            &fleet,
            &History::new(0),
            &[],
            Instant::now(),
            &state,
            100,
            SAMPLE_INTERVAL,
        );
        assert_eq!(
            parts.selected,
            Some(0),
            "the refresh moved quiet to the top"
        );
        assert!(
            plain(parts.detail.as_ref().unwrap())
                .join("\n")
                .contains("quiet")
        );

        state.stream_sort = Some(StreamSort {
            column: StreamColumn::Name,
            descending: false,
        });
        let names = stream_names(fleet.local_streams(), state.stream_sort.as_ref());
        assert_eq!(names, ["busy", "quiet"]);
        let parts = build_parts(
            &fleet,
            &History::new(0),
            &[],
            Instant::now(),
            &state,
            100,
            SAMPLE_INTERVAL,
        );
        assert_eq!(parts.selected, Some(1), "the re-sort moved quiet again");
        assert!(
            plain(parts.detail.as_ref().unwrap())
                .join("\n")
                .contains("quiet")
        );
        assert!(
            !plain(parts.detail.as_ref().unwrap())
                .join("\n")
                .contains("busy (")
        );
        assert_eq!(state.selected_stream.as_deref(), Some("quiet"));
    }

    #[test]
    fn streams_parts_show_columns_and_never_render_a_token_value() {
        let secret = runtime_token();
        let mut fleet = fleet();
        let streams = StreamsView::from_json(&streams_json(&secret)).expect("fixture");
        fleet.set_local_streams(streams);
        let state = ui::UiState {
            mode: ui::Mode::Streams,
            selected_stream: Some("busy".to_owned()),
            ..ui::UiState::default()
        };
        let parts = build_parts(
            &fleet,
            &History::new(0),
            &[],
            Instant::now(),
            &state,
            100,
            SAMPLE_INTERVAL,
        );
        let table = plain(&parts.lines).join("\n");
        let detail = plain(parts.detail.as_ref().unwrap()).join("\n");
        assert!(
            table.contains("stream") && table.contains("dynamic"),
            "{table}"
        );
        assert!(table.contains("9/10") && table.contains("2/2"), "{table}");
        assert!(
            detail.contains("abcdef12"),
            "ids are shown, not values: {detail}"
        );
        assert!(detail.contains("token.add"), "{detail}");
        assert!(!format!("{table}\n{detail}").contains(&secret));
        assert!(parts.status.is_empty());
    }

    #[test]
    fn streams_parts_wait_without_a_local_admin_answer() {
        let state = ui::UiState {
            mode: ui::Mode::Streams,
            ..ui::UiState::default()
        };
        let parts = build_parts(
            &fleet(),
            &History::new(0),
            &[],
            Instant::now(),
            &state,
            100,
            SAMPLE_INTERVAL,
        );
        assert!(
            plain(&parts.lines)[0].contains("waiting for the local admin listener"),
            "{:?}",
            plain(&parts.lines)
        );
    }

    fn three_peers(now: Instant) -> Fleet {
        let mut f = fleet();
        for name in ["peer-a", "peer-b", "peer-c"] {
            apply_feed(&mut f, name, FeedEvent::Snapshot(DOC.to_owned()), now);
        }
        f
    }

    fn peer_names() -> Vec<String> {
        ["peer-a", "peer-b", "peer-c"].map(str::to_owned).to_vec()
    }

    #[test]
    fn the_cursor_stays_on_its_peer_through_a_resort() {
        let now = Instant::now();
        let f = three_peers(now);
        let all = peer_names();
        let mut state = ui::UiState {
            selected: Some(1),
            ..ui::UiState::default()
        };
        record_cursor(&mut state, &Matrix::build_fleet(&f, &all, now));
        assert_eq!(state.selected_peer.as_deref(), Some("peer-a"));

        apply_sort(
            &mut state,
            ui::Action::SortNext,
            &Matrix::build_fleet(&f, &all, now),
        );
        follow_cursor(&mut state, &Matrix::build_fleet(&f, &all, now));
        assert_eq!(
            state.selected,
            Some(1),
            "name ascending keeps peer-a second"
        );

        apply_sort(
            &mut state,
            ui::Action::SortReverse,
            &Matrix::build_fleet(&f, &all, now),
        );
        follow_cursor(&mut state, &Matrix::build_fleet(&f, &all, now));
        assert_eq!(state.selected, Some(3), "descending puts peer-a last");
        assert_eq!(state.selected_peer.as_deref(), Some("peer-a"));
    }

    #[test]
    fn the_cursor_stays_on_its_peer_through_a_refresh_that_reorders_problems() {
        let now = Instant::now();
        let mut f = three_peers(now);
        let all = peer_names();
        let mut state = ui::UiState {
            selected: Some(1),
            ..ui::UiState::default()
        };
        record_cursor(&mut state, &Matrix::build_fleet(&f, &all, now));
        assert_eq!(state.selected_peer.as_deref(), Some("peer-a"));

        apply_feed(
            &mut f,
            "peer-c",
            FeedEvent::Snapshot(COOLING_DOC.to_owned()),
            now,
        );
        follow_cursor(&mut state, &Matrix::build_fleet(&f, &all, now));
        assert_eq!(
            state.selected,
            Some(2),
            "peer-c is a problem, so it moved up"
        );
        assert_eq!(state.selected_peer.as_deref(), Some("peer-a"));
    }

    #[test]
    fn a_peer_that_left_the_list_keeps_the_cursor_position() {
        let now = Instant::now();
        let f = three_peers(now);
        let all = peer_names();
        let mut state = ui::UiState {
            selected: Some(2),
            selected_peer: Some("peer-gone".to_owned()),
            ..ui::UiState::default()
        };
        follow_cursor(&mut state, &Matrix::build_fleet(&f, &all, now));
        assert_eq!(state.selected, Some(2));
        assert_eq!(state.selected_peer.as_deref(), Some("peer-b"));
    }

    #[test]
    fn clearing_the_cursor_clears_the_peer_it_was_on() {
        let now = Instant::now();
        let f = three_peers(now);
        let all = peer_names();
        let mut state = ui::UiState {
            selected: None,
            selected_peer: Some("peer-a".to_owned()),
            ..ui::UiState::default()
        };
        follow_cursor(&mut state, &Matrix::build_fleet(&f, &all, now));
        assert_eq!(state.selected, None, "the cursor does not come back");
        assert_eq!(state.selected_peer, None);
    }
}
