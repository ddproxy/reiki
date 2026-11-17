# reiki top

`reiki top` is a live terminal view of key health for every instance under `[peers.NAME]`. It reads the same `/v1/health` and `/v1/health/stream` as `reiki peers`, with the same per-peer token and CA rules.

The default screen is a peers-by-providers matrix: one row per peer, one column per provider, then host load, disk and sessions. Problem peers sort first. Around it the screen has a header, a row of provider tabs, a cooling panel when something is cooling, and at 28 rows or more a chart row. The selected peer's detail card (keys, cooldowns, class, last error) opens in a bordered pane where the chart row is.

It is read-only: nothing here changes a key or serves traffic.

## Build

Release binaries and `make install` include `reiki top`. A plain `cargo build` leaves it out: it is the `top` cargo feature, not in the default set, and implies `peers`:

```bash
cargo build --features top
```

Without the feature the subcommand and its `--help` entry are absent.

## Configure

It needs `[peers] enabled = true` and at least one `[peers.NAME]` table, exactly as `reiki peers`; each entry must point at an instance that publishes health and at a token file that instance issued (see [peers](peers.md)).

## Use

```bash
reiki top                 # live matrix; q quits
reiki top --full          # live per-peer text view instead
reiki top --once          # poll every peer once, print the matrix as text and exit
reiki top --once --full   # poll once, print the per-peer text view
reiki top --provider zen  # show only the zen column and peers that have it
reiki top --interval 2    # redraw every two seconds instead of every second
```

### The screen

```
╭reiki top v0.10.3─────────────────────────────────────────12:00:00Z╮
│live · 3 of 3 peers fresh                                          │
│q quit  j/k peer  Enter detail  f filter  u metric  w window  m ...│
╰───────────────────────────────────────────────────────────────────╯
 All │ gemini │ zen │ openrouter
╭Peers──────────────────────────────────────────────────────────────╮
│Fleet 3/3 fresh   gemini 2/2   zen 1/2   openrouter 0/1   [req 1h] │
│          gemini        zen           openrouter  load  disk  sess │
│  peer-b  ■ ······      ·             ◐ ▁▂▃▄▅▆      -     -     -  │
│> peer-c  ■ ······+2    ■ ▂▃▄▅▆▇ 45s  ·             -     -     -  │
╰───────────────────────────────────────────────────────────────────╯
╭Cooling────────────────────────────────────────────────────────────╮
│  peer-c  zen  ◐ cooling  [▅▅▅░░]  45s  last 429                   │
╰───────────────────────────────────────────────────────────────────╯
╭Load · fleet──────────────────────────╮╭Keys─────────────────────╮
│  (line chart of the fleet's load)    ││ healthy cooling disab  │
╰──────────────────────────────────────╯╰────────────────────────╯
                       [□ detail] [req 1h] [chart: load]
```

- The header panel has the version, the UTC clock, the live or paused state with the fresh count, and the key hints. When the matrix does not fit beside it, the header folds to one line with the title, state and clock (see [narrow terminals](#narrow-terminals)).
- The tab row lists `All` and every provider column. The selected tab follows the `f` filter.
- The Peers panel holds the matrix. Its first line is the fleet line described below.
- The Cooling panel appears only while some fresh peer has a cooling key. It lists the same lines `--once` prints, without its title.
- The chart row appears at 28 rows or more, and only while no peer detail is open. Its rows are 10, borders included. The left chart plots the selected peer's history, or the fleet's when no peer is selected: `m` cycles load, disk, sessions and requests. The right panel counts keys by state across the fleet: healthy, cooling and disabled. Fresh peers only; every provider counts.
- The footer shows chips: `[■ detail]` or `[□ detail]` for the peer card, `[req 1h]` for the gauge, `[chart: <metric>]`, `[filter: <provider>]` when a filter is set, and `[paused]`.
- `?` opens the key list over the screen. The next key closes it; Ctrl-C still quits.

### The matrix

Inside the Peers panel:

- The first line counts fresh peers, then one `provider healthy/with` tally per provider. Only peers that publish a provider count towards it. When every peer that has a provider is cooling or disabled (a stale peer counts as unknown), it adds `ALERT <provider> all cooling[, soonest <time>]` (up to three, then `+N more`). With `--provider` or the `f` filter it shows `filter: <provider>` and omits alerts.
- Cells: `■` healthy (green); `◐` partly healthy, with the soonest cooldown (yellow); `■` with a cooldown, no healthy key (red); `⊘` every key out of rotation (red); `·` the peer does not use this provider (dim); `?` the peer is stale (dim).
- After the state glyph, six cells show the last hour of attempts in ten-minute steps, from `▁` to `█`. They scale to the busiest cell across the whole fleet for that provider, so rows compare at a glance; `·` is a zero cell. A gauge turns yellow when any attempt in the hour was rate-limited or began a cooldown. A peer that publishes no figures for the chosen metric and window, or an older publisher, draws `·` cells rather than an error.
- `u` switches the gauge between requests and tokens; `w` switches the window between the last hour and the last five minutes, drawn as five cells, one per minute. The fleet line ends with the mode, such as `[req 1h]` or `[tok 5m]`. Both modes take the same column width.
- The cooling panel lists each (peer, provider) with cooling keys: its drain bar, time left and last status class, soonest first, six lines then `+N more`. `--once` prints it too.
- A `+N` after a cell is the number of cooling (route, model) combinations on that provider, when the peer publishes it; the request total is never in the matrix.
- The detail pane adds `last hour: N requests, M limited` and a twelve-cell five-minute trend for each upstream.
- `Ver` is the version each peer publishes (`reiki_version`), after the provider columns and before the host figures, clipped to twelve characters. `?` means the peer does not publish one. The detail card shows it after the instance name, as `v0.10.3`.
- `load` is the one-minute load with a sparkline of recent samples. `disk` is the fullest disk: green under 75%, yellow to 89%, red from 90%. `sess` is active sessions. A `-` means the peer does not publish that figure.
- A stale row shows `stale <age>` in place of host figures. Its error goes to the detail pane.
- The sparkline is built from samples taken once per redraw interval while the screen is live. `--once` has no history, so it shows the number only.
- Mismatched providers: a peer that does not use a provider shows `·`, and the tally leaves it out of both counts.

### Failures

Every peer's health document carries a `failures` object: counts of recent key failures by class over an in-memory ring of at most 64. The detail card prints those counts as `failures: rate_limited 2  transient 1`; names, streams and bodies are never published, so a remote peer can only be compared, not investigated.

The full recent-failure list is shown only for the local instance, and only when `REIKI_ADMIN_TOKEN` holds its admin token (together with `[admin] listen` in the config). `reiki top` finds the local peer by matching its published `instance` to `[peers] instance`, or `reiki` when that is not set. For that peer the card adds one line per failure: time, stream, key, status, class, cooldown left and the scrubbed body snippet. A remote peer's card keeps the counts, even when the local admin listener is configured. If the admin listener is missing, unreachable or refuses the token, the counts still appear and only the rows are absent.

See [`GET /failures`](monitoring.md#get-failures) for the fields and the `reiki failures` command.

### Keys

In the matrix:

- `j`/`k`, the arrows, `Tab`/`Shift-Tab`: move the selection to the next or previous peer. With more peers than rows the list scrolls with the selection and the header stays in place.
- `f`: cycle the provider filter (all, then each provider, then all). The filtered view shows only that column and peers that have it, and falls back to all if the provider disappears.
- `u`: switch the gauge between requests and tokens. `w`: switch the gauge window between the last hour and the last five minutes.
- `Enter` or `e`: open or close the detail pane. A problem peer's pane opens by itself; Enter flips it.
- `PgUp`/`PgDn`, `g`/`Home`, `G`/`End`: scroll the detail pane.
- `m`: cycle the chart metric: load, disk, sessions, requests.
- `c`: clear the provider filter and the selection, so the chart shows the fleet again.
- `v` or `E`: switch to the full view.
- `p`: freeze the screen as a snapshot and show its age; updates keep arriving, and resuming shows the live view. The frozen snapshot keeps its sparklines.
- `?`: list every key. Any key closes the list.
- `q` or `Esc` quits.

In the full view (`v` or `--full`):

- `j`/`k`, the arrows, `PgUp`/`PgDn`, `g`/`Home` and `G`/`End` scroll.
- `Tab`/`Shift-Tab` filter to the next or previous peer; `c` clears the filter.
- `p` freezes, `v` returns to the matrix, `?` lists the keys, `q` or `Esc` quits.

Live mode needs stdout to be a terminal; otherwise it prints one line and exits 2, so use `--once`.

The frame's widgets come from the `fw-tui` crate, pinned by revision in `crates/reiki-top/Cargo.toml`. Colour follows `NO_COLOR`.

### Names and columns

Peer and provider names come from the network. Control characters and invisible format characters (bidi overrides, zero-width marks) are shown as `?`, and widths are measured in terminal columns, so wide characters stay aligned. At most 32 provider columns are drawn; the fleet line adds `+N more` for the rest.

### Narrow terminals

The matrix drops the `Ver` column first, then the last-hour gauge, then shortens provider headers to six, then three characters, drops the sparkline, and as a last resort prints one chip line per peer with only the providers it uses. Below 20 columns it draws the fleet line and peer names. It never draws past the terminal width.

The screen gives up its panels before it cuts the matrix. As the height drops it loses, in order: the chart row (below 28 rows), the cooling panel (when the bare matrix would no longer fit beside it), the tab row, and then the header folds to one line. The Peers panel shrinks last, and only when even the bare matrix does not fit does it clip rows. The detail pane keeps its bottom third of the screen as before. Widths below 20 columns still draw the names-only matrix. Any size down to 0 by 0 draws without panicking.

### `--once`

`--once` polls each peer once and prints the matrix as plain text, 100 columns wide and without colour. `--once --full` prints the per-peer text view instead. It exits 0 when at least one peer answered, 1 when none did, and 2 on a config problem.
