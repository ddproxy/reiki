//! `reiki top`: a live terminal view of the key health of every configured peer.
//!
//! The feed layer follows each peer's `/v1/health/stream` with the same pinned client, token
//! and SSE parser as `reiki peers --watch`, and falls back to one `/v1/health` poll when a
//! stream fails. The reducer turns those messages into a [`Fleet`]; the view model and the
//! ratatui layer live in the dependency-free `reiki-top` crate.

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use reiki_top::ui::{self, crossterm, ratatui};
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

/// The `reiki top` command: validates the `[peers]` config, then runs once or live.
pub async fn run(peers: Option<&PeersConfig>, once: bool, interval: Option<u64>) -> i32 {
    let peers = match client::ready(peers) {
        Ok(peers) => peers,
        Err(e) => {
            eprintln!("reiki: {e}");
            return 2;
        }
    };
    if once {
        run_once(peers).await
    } else {
        run_live(peers, interval).await
    }
}

/// Polls every peer once, prints the fleet as plain text and returns 0 when at least one peer
/// answered, 1 when none did.
pub async fn run_once(peers: &PeersConfig) -> i32 {
    run_once_to(peers, &mut io::stdout(), &mut io::stderr()).await
}

/// [`run_once`] with the output handles passed in, so tests need no terminal.
async fn run_once_to(peers: &PeersConfig, out: &mut impl Write, err: &mut impl Write) -> i32 {
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

    let lines = view::render(&fleet, &names, Instant::now(), 100);
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
pub async fn run_live(peers: &PeersConfig, interval: Option<u64>) -> i32 {
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
    let code = live_loop(&mut terminal, peers, rx, redraw).await;
    for task in tasks {
        task.abort();
    }
    code
}

async fn live_loop(
    terminal: &mut Screen,
    peers: &PeersConfig,
    mut rx: mpsc::Receiver<(String, FeedEvent)>,
    redraw: Duration,
) -> i32 {
    let poll = Duration::from_secs(peers.poll_interval_secs);
    let all: Vec<String> = peers.peers.keys().cloned().collect();
    let mut fleet = Fleet::new(poll.saturating_mul(3));
    let mut state = ui::UiState::default();

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
            _ = frame_tick.tick() => draw_now = !state.paused,
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
                let Some(action) = ui::map_key(key) else {
                    continue;
                };
                let (total, viewport) = match measure(terminal, &fleet, &all, &state) {
                    Ok(pair) => pair,
                    Err(e) => {
                        eprintln!("reiki: cannot size the terminal: {e}");
                        return 1;
                    }
                };
                if !apply_action(&mut state, action, &all, total, viewport) {
                    return 0;
                }
                draw_now = true;
            },
        }
        if draw_now {
            if let Err(e) = render_screen(terminal, &fleet, &all, &state) {
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

/// Applies one key action, turning a Tab selection into the one-peer filter.
fn apply_action(
    state: &mut ui::UiState,
    action: ui::Action,
    all: &[String],
    total: usize,
    viewport: usize,
) -> bool {
    let previous = state.selected;
    let keep = state.apply(action, total, viewport);
    if action == ui::Action::ClearFilter {
        state.selected = None;
    }
    if action == ui::Action::ClearFilter || state.selected != previous {
        state.filter.peer = match state.selected {
            Some(index) if !all.is_empty() => Some(all[index % all.len()].clone()),
            _ => None,
        };
    }
    keep
}

/// The peers the filter lets through, in the caller's order.
fn visible_cts(all: &[String], state: &ui::UiState) -> Vec<String> {
    match &state.filter.peer {
        Some(name) => vec![name.clone()],
        None => all.to_vec(),
    }
}

/// The line count and body height a scroll action has to stay inside.
fn measure(
    terminal: &Screen,
    fleet: &Fleet,
    all: &[String],
    state: &ui::UiState,
) -> io::Result<(usize, usize)> {
    let size = terminal.size()?;
    let visible = visible_cts(all, state);
    let lines = view::render(fleet, &visible, Instant::now(), usize::from(size.width));
    // One footer row and a two-row border.
    Ok((lines.len(), usize::from(size.height.saturating_sub(3))))
}

fn render_screen(
    terminal: &mut Screen,
    fleet: &Fleet,
    all: &[String],
    state: &ui::UiState,
) -> io::Result<()> {
    let visible = visible_cts(all, state);
    let now = Instant::now();
    let fresh = visible
        .iter()
        .filter(|name| fleet.peer(name, now).is_some_and(|peer| !peer.stale))
        .count();
    let status = format!("{fresh} of {} peers fresh", visible.len());
    terminal.draw(|frame| {
        let lines = view::render(fleet, &visible, now, usize::from(frame.area().width));
        ui::draw(frame, &lines, state, &status);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::config::Config;

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
        let mut state = ui::UiState::default();
        assert!(apply_action(&mut state, ui::Action::NextPeer, &all, 10, 3));
        assert_eq!(state.filter.peer.as_deref(), Some("peer-a"));
        assert_eq!(visible_cts(&all, &state), ["peer-a"]);
        apply_action(&mut state, ui::Action::NextPeer, &all, 10, 3);
        assert_eq!(state.filter.peer.as_deref(), Some("peer-b"));
        apply_action(&mut state, ui::Action::NextPeer, &all, 10, 3);
        assert_eq!(state.filter.peer.as_deref(), Some("peer-a"), "it wraps");
        apply_action(&mut state, ui::Action::ClearFilter, &all, 10, 3);
        assert_eq!(state.filter.peer, None);
        assert_eq!(visible_cts(&all, &state), all);
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
        let code = run_once_to(&peers, &mut out, &mut err).await;
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
        let code = run_once_to(&peers, &mut out, &mut err).await;
        let out = String::from_utf8(out).unwrap();
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 1);
        assert!(out.contains("Providers"), "{out}");
        assert!(out.contains("no data"), "{out}");
        assert!(err.contains("peer `peer-a`"), "{err}");
    }
}
