# reiki top

`reiki top` is a read-only live terminal view of key health for every instance under `[peers.NAME]`. It reads the same `/v1/health` and `/v1/health/stream` as `reiki peers`, with the same per-peer token and CA rules. Nothing here changes a key or serves traffic. See [provider health](provider-health.md).

The default screen is a peers-by-providers matrix: a synthetic `ALL` row for the fleet, one row per peer, one column per provider, then host load, disk and sessions. `ALL` is selected by default when more than one peer is configured; problem peers sort first after it. Around it the screen has a header, a row of provider tabs, a cooling panel when something is cooling, and at 28 rows or more a chart row. The selected row's detail card opens in a bordered pane where the chart row is.

## Build

Release binaries and `make install` include `reiki top`. A plain `cargo build` leaves it out: it is the `top` cargo feature, not in the default set, and implies `peers`:

```bash
cargo build --features top
```

Without the feature the subcommand and its `--help` entry are absent.

## Configure

It needs `[peers] enabled = true` and at least one `[peers.NAME]` table, exactly as `reiki peers`; each entry must point at an instance that publishes health and at a token file that instance issued.

The Streams screen additionally uses the local instance's `[admin] listen` address and an admin token. `reiki top` reads only, so it prefers the read-only token from `REIKI_ADMIN_READ_TOKEN` and falls back to `REIKI_ADMIN_TOKEN`. With the read-only token, failure rows show time, key, status and class but no body snippet. It never reads the separate stream-management token; see [stream administration routes](monitoring.md#stream-administration-routes).

## Use

```bash
reiki top                 # live matrix; q quits
reiki top --full          # live per-peer text view instead
reiki top --once          # poll every peer once, print the matrix as text and exit
reiki top --once --full   # poll once, print the per-peer text view
reiki top --provider zen  # show only the zen column and peers that have it
reiki top --interval 2    # redraw every two seconds instead of every second
```

Live mode needs stdout to be a terminal; otherwise it prints one line and exits 2, so use `--once`. `--once` polls each peer once, prints 100 columns without colour, including the `ALL` row, and exits 0 when at least one peer answered, 1 when none did and 2 on a config problem. `--once --full` prints the per-peer text view, which also prints `Streams: N static, M dynamic` when peers publish counts with `expose_usage` (on by default). No stream names appear. The frame widgets come from the `fw-tui` crate pinned in `crates/reiki-top/Cargo.toml`; colour follows `NO_COLOR`.

### The screen

```text
╭reiki top vX.Y.Z────────────────────────────────────────────12:00:00Z╮
│live · 3 of 3 peers fresh                                          │
│q quit  j/k  Enter detail  f filter  u metric  w window  m chart  s │
╰───────────────────────────────────────────────────────────────────╯
 All │ gemini │ zen │ openrouter
╭Peers──────────────────────────────────────────────────────────────╮
│Fleet 3/3 fresh   gemini 2/2   zen 1/2   openrouter 0/1   [req 1h] │
│          gemini        zen           openrouter  load  disk  sess │
│> ALL   ■ ······      ■ ······      ◐ ▁▂▃▄▅▆      -     -     -  │
│  peer-b  ■ ······      ·             ◐ ▁▂▃▄▅▆      -     -     -  │
│  peer-c  ■ ······+2    ■ ▂▃▄▅▆▇ 45s  ·             -     -     -  │
╰───────────────────────────────────────────────────────────────────╯
╭Cooling────────────────────────────────────────────────────────────╮
│  peer-c  zen  ◐ cooling  [▅▅▅░░]  45s  last 429                   │
╰───────────────────────────────────────────────────────────────────╯
╭Requests · fleet · req/interval─────╮╭Keys─────────────────────────╮
│■ zen 12  ■ openrouter 3  ■ sample1 1  ││ healthy cooling disab  │
│20                                     ││                        │
│   ●───────────────────────────────●   ││                        │
│0  ■                               ▲   ││                        │
│-12m                               now ││                        │
╰───────────────────────────────────────╯╰────────────────────────╯
                        [□ detail] [req 1h] [chart: requests]
```

- The header holds the version, UTC clock, live or paused state with the fresh count, and key hints. When the matrix does not fit beside it, the header folds to one line.
- The tab row lists `All` and every provider column; the selected tab follows the `f` filter.
- The Cooling panel appears only while some fresh peer has a cooling key, listing the same lines `--once` prints.
- The chart row appears at 28 rows or more and only while no detail is open. `m` cycles load, disk, sessions and requests; it follows the selected row. `s` toggles overlaid lines and a stacked total; while Requests is selected, `u` also cycles its series requests, limited and tokens. Twelve one-minute points are used when any fresh peer publishes them, otherwise twelve five-minute buckets from the last hour. Stale peers are excluded, and a fleet chart missing peers adds `fleet N of M fresh` to its title.
- The right chart panel counts healthy, cooling and disabled keys across fresh peers, every provider.
- The footer shows chips for the detail pane, gauge, chart, filter, stack and series where applicable. `?` opens the key list; the next key closes it and Ctrl-C still quits.

### The matrix

- The first line counts fresh peers, then one `provider healthy/with` tally per provider. Only peers that publish a provider count towards it. When every peer that has a provider is cooling or disabled it adds `ALERT <provider> all cooling[, soonest <time>]`, up to three then `+N more`. A filter shows `filter: <provider>` and omits alerts.
- `ALL` is the fleet pseudo-peer. Its cells sum request gauges of fresh peers and carry the worst key state for that provider; it never raises an alert and stays first. `Enter` opens a detail card with per-upstream totals and per-peer `(peer, upstream, requests, limited, state)` tuples.
- Cells: `■` healthy, `◐` partly healthy with soonest cooldown, `■` with a cooldown and no healthy key, `⊘` every key out of rotation, `·` provider unused, `?` peer stale.
- Six cells after the glyph show the last hour in ten-minute steps from `▁` to `█`, scaled to the busiest cell for that provider across the fleet; `·` is zero. A gauge turns yellow when an attempt was rate-limited or began a cooldown. `u` switches requests and tokens, `w` the last hour and the last five minutes; the fleet line ends with the mode, such as `[req 1h]` or `[tok 5m]`.
- `+N` after a cell is the number of cooling `(route, model)` combinations when the peer publishes it. The detail pane adds `last hour: N requests, M limited` and a twelve-cell trend per upstream, plus gateway counts when peers publish them. With the local admin token it adds a `cost` line from the local `GET /v1/cost` answer; peer health documents never carry prices or spend. See [cost](cost.md).
- Observed passthrough entries appear as display-only `observed/{account}` columns with no key state and no effect on fleet tallies; the detail pane lists status class, retry and quota figures.
- `o`/`O` sort peer rows by the next or previous column and back to the default order; `r` reverses. Columns are peer name, each provider, `Ver`, `load`, `disk`, `sess`. Missing values sort last, equal values keep the default order, and `ALL` stays first. A provider column sorts by its requests in the last hour. `Ver` shows the peer's published version clipped to twelve characters, `?` when absent. `load` is the one-minute load with a sparkline; `disk` is the fullest disk, green below 75%, yellow to 89%, red from 90%; `sess` is active sessions; `-` means unpublished. A stale row shows `stale <age>` and puts its error in the detail pane. Sparklines use one sample per redraw while live, so `--once` shows the number only.
- A peer that does not use a provider shows `·` and is left out of both tallies.

### Failures

Every peer's health document carries a `failures` object: counts of recent key failures by class over the peer's in-memory ring (`failure_ring_size`, 200 by default). The detail card prints them as `failures: rate_limited 2  transient 1`; names, streams, actions and bodies are never published. A peer with summary export enabled also publishes `requestlog_export_dropped_total`, printed as `export drops: N` when above zero. See [request log](requestlog.md).

The full recent-failure list is shown only for the local instance, and only when `REIKI_ADMIN_READ_TOKEN` or `REIKI_ADMIN_TOKEN` holds one of its admin tokens together with `[admin] listen` in the config. The local peer is found by matching its published `instance` to `[peers] instance`, or `reiki` when unset. Its card adds one line per failure: time, stream, key, status, class, action, cooldown left and the scrubbed body snippet. With the read-only token the action is still shown; only the snippet is withheld. Remote cards keep the counts, and missing or refused admin access removes only the rows. See [`GET /v1/failures`](monitoring.md#get-v1failures).

### Streams

Press `t` to open a read-only view of the local instance's streams; `t` returns to the matrix. It reads [`GET /v1/stream-console`](monitoring.md#get-v1stream-console) with the admin token `reiki top` was given. Remote peer stream names never enter this screen.

The table lists every configured stream, static and dynamic, ordered by current request rate, concurrency then name. Columns are `stream`, `type`, `chain`, `expires`, `rpm` and `conc` (current/limit, `-` when unlimited), `last` and `state`. `j`/`k`, arrows and `Tab`/`Shift-Tab` move the selection, which follows a stream by name across refreshes and re-sorts. `o`/`O` step through the sortable columns name, usage (`conc`), chain, rate (`rpm`) and expiry (`expires`) and back to the default order; `r` reverses. `type`, `last` and `state` are display-only. The detail pane shows numeric limits, credentials by short id with expiry and state, and recent audit records newest first; values, hashes, audit-chain hashes and file paths are never sent to the screen. `PgUp`/`PgDn`, `g`/`Home` and `G`/`End` scroll it.

Without `[stream_admin]` the screen says `stream_admin is disabled; dynamic streams and audit are unavailable` and keeps the static rows. A failed refresh shows a short reason and keeps the last good answer, backing off from the health poll interval to at most one minute. Without `[admin]` or either token variable it waits for the local admin listener. Nothing on this screen can create, rotate or revoke a stream or token.

### Keys

In the matrix:

- `j`/`k`, arrows, `Tab`/`Shift-Tab`: move selection; the list scrolls with it and the header stays in place.
- `f`: cycle the provider filter; `c` clears it and resets selection.
- `u`/`w`: switch gauge metric and window.
- `Enter` or `e`: toggle the detail pane; `a` selects `ALL`; `PgUp`/`PgDn`, `g`/`Home`, `G`/`End` scroll.
- `m`: cycle the chart metric; `s`: stack the Requests chart; `o`/`O` and `r` sort.
- `v` or `E`: full view; `t`: Streams; `p`: freeze the screen as an age-labelled snapshot, keeping sparklines.
- `?`: key list; `q` or `Esc`: quit.

In the full view, `j`/`k`, arrows and the scroll keys move; `Tab`/`Shift-Tab` filter peers; `p`, `v`, `t`, `?` and `q` behave as above. On Streams, `j`/`k`, arrows and `Tab` move the row, scroll keys scroll the detail, and `p`, `t`, `?` and `q` behave as above.

Peer and provider names come from the network: control characters and invisible format characters are shown as `?`, and widths are measured in terminal columns. At most 32 provider columns are drawn, with `+N more` on the fleet line.

### Narrow terminals

The matrix drops `Ver`, then the last-hour gauge, then shortens provider headers to six and three characters, drops the sparkline, and finally prints one chip line per peer. Below 20 columns it draws only the fleet line and peer names. As height drops the screen loses the chart row (below 28 rows), cooling panel, tab row and finally folds the header; the Peers panel shrinks last and clips rows only as a last resort. Any size down to 0 by 0 draws without panicking.
