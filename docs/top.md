# reiki top

`reiki top` is a live terminal view of key health for every instance under `[peers.NAME]`. It reads the same `/v1/health` and `/v1/health/stream` as `reiki peers`, with the same per-peer token and CA rules.

The default screen is a peers-by-providers matrix: a synthetic `ALL` row for the fleet, then one row per peer, one column per provider, then host load, disk and sessions. `ALL` is selected by default when more than one peer is configured; problem peers sort first after it. Around it the screen has a header, a row of provider tabs, a cooling panel when something is cooling, and at 28 rows or more a chart row. The selected row's detail card (keys, cooldowns, class, last error, or fleet totals for `ALL`) opens in a bordered pane where the chart row is.

It is read-only: nothing here changes a key or serves traffic.

## Build

Release binaries and `make install` include `reiki top`. A plain `cargo build` leaves it out: it is the `top` cargo feature, not in the default set, and implies `peers`:

```bash
cargo build --features top
```

Without the feature the subcommand and its `--help` entry are absent.

## Configure

It needs `[peers] enabled = true` and at least one `[peers.NAME]` table, exactly as `reiki peers`; each entry must point at an instance that publishes health and at a token file that instance issued (see [provider health](provider-health.md)).

The Streams screen additionally uses the local instance's `[admin] listen` address and an admin token, the same configuration as the local failure rows. `reiki top` reads only, so it prefers the read-only token from `REIKI_ADMIN_READ_TOKEN` and falls back to `REIKI_ADMIN_TOKEN`. With the read-only token the failure rows show their time, key, status and class but no body snippet. It never reads or uses the separate stream-admin token (see [stream administration routes](monitoring.md#stream-administration-routes)).

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
╭reiki top v0.14.0─────────────────────────────────────────12:00:00Z╮
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

- The header panel has the version, the UTC clock, the live or paused state with the fresh count, and the key hints. When the matrix does not fit beside it, the header folds to one line with the title, state and clock (see [narrow terminals](#narrow-terminals)).
- The tab row lists `All` and every provider column. The selected tab follows the `f` filter.
- The Peers panel holds the matrix. Its first line is the fleet line described below.
- The Cooling panel appears only while some fresh peer has a cooling key. It lists the same lines `--once` prints, without its title.
- The chart row appears at 28 rows or more, and only while no peer detail is open. Its rows are 10, borders included. `m` cycles load, disk, sessions and requests; the chart follows the selected row, so `ALL` (the default) shows the fleet and a peer row shows only that peer. The right panel counts keys by state across the fleet: healthy, cooling and disabled. Fresh peers only; every provider counts.
- The Requests chart draws one coloured line per upstream, including credential upstreams such as `sample1` and `sample2`, in provider-column order. The legend shows the latest value per upstream: `■ zen 12  ■ openrouter 3  ■ sample1 1`, clipped to the panel width. The title carries the unit (`req/interval`); the X axis is labelled from the real sample interval and point count (`-12m` on the left, `now` on the right), and the Y axis starts at `0` and ends at a rounded step. An all-zero series sits on the baseline. `s` toggles between overlaid lines and a stacked total. While Requests is selected, `u` also cycles the chart series requests, limited and tokens.
- The chart uses twelve one-minute points when any fresh peer publishes them, otherwise twelve five-minute buckets from the last hour. Stale peers are excluded, and a fleet chart that is missing peers adds `fleet N of M fresh` to its title.
- The footer shows chips: `[■ detail]` or `[□ detail]` for the peer card, `[req 1h]` for the gauge, `[chart: <metric>]`, `[filter: <provider>]` when a filter is set, and `[paused]`. With the Requests chart it also shows `[stack]` when the stacked total is on and `[series: <metric>]` for the chart series.
- `?` opens the key list over the screen. The next key closes it; Ctrl-C still quits.

### The matrix

Inside the Peers panel:

- The first line counts fresh peers, then one `provider healthy/with` tally per provider. Only peers that publish a provider count towards it. When every peer that has a provider is cooling or disabled (a stale peer counts as unknown), it adds `ALERT <provider> all cooling[, soonest <time>]` (up to three, then `+N more`). With `--provider` or the `f` filter it shows `filter: <provider>` and omits alerts.
- The first row is `ALL`, the fleet pseudo-peer. Its provider cells sum the request gauges of fresh peers and carry the worst key state seen for that provider; it is not a peer, never raises an alert, and is left out of the tallies. It is selected by default when more than one peer is configured. `Enter` opens a detail card with per-upstream totals and per-peer `(peer, upstream, requests, limited, state)` tuples, skipping stale peers; `a` returns to `ALL` from any matrix row.
- Cells: `■` healthy (green); `◐` partly healthy, with the soonest cooldown (yellow); `■` with a cooldown, no healthy key (red); `⊘` every key out of rotation (red); `·` the peer does not use this provider (dim); `?` the peer is stale (dim).
- After the state glyph, six cells show the last hour of attempts in ten-minute steps, from `▁` to `█`. They scale to the busiest cell across the whole fleet for that provider, so rows compare at a glance; `·` is a zero cell. A gauge turns yellow when any attempt in the hour was rate-limited or began a cooldown. A peer that publishes no figures for the chosen metric and window draws `·` cells rather than an error.
- `u` switches the gauge between requests and tokens; `w` switches the window between the last hour and the last five minutes, drawn as five cells, one per minute. The fleet line ends with the mode, such as `[req 1h]` or `[tok 5m]`. Both modes take the same column width.
- The cooling panel lists each (peer, provider) with cooling keys: its drain bar, time left and last status class, soonest first, six lines then `+N more`. `--once` prints it too.
- A `+N` after a cell is the number of cooling (route, model) combinations on that provider, when the peer publishes it; the request total is never in the matrix.
- The detail pane adds `last hour: N requests, M limited` and a twelve-cell five-minute trend for each upstream. When peers publish gateway counts (`expose_usage`), `ALL` adds a `gateway (N peers)` line with admitted requests, charged tokens and store errors; a peer card adds `gateway N req, N tok, N store errors`. These are counts only; no stream or model names appear.
- With the local admin token, the same local-only fetch adds one `cost (period currency, estimate)` line: the estimated current-month spend and request count. Unpriced requests and an unhealthy cost state are named when present. It comes from the local `GET /v1/cost` answer; peer health documents never carry prices, spend or model names, and no cost line appears when the plugin is off. See [cost](cost.md#reiki-top).
- Observed passthrough entries appear as display-only columns named `observed/{account}` when a peer publishes them. They show the last observed status class and retry hint, carry no key state, and are left out of key counts and fleet tallies. Duplicate observed entries for one account are collapsed by worst state; the detail pane says `observed` and lists the account, status class, remaining retry and quota figures.
- Sorting: `o` sorts the peer rows by the next column and `O` by the previous one; past the last column (or before the first) the rows return to the default order. The columns, left to right, are the peer name, each provider, `Ver`, `load`, `disk` and `sess`. `r` reverses the direction. The first press sorts ascending. A sorted header shows its direction: `▲` ascending, `▼` descending. Names sort case-insensitively and naturally (`peer2` before `peer10`); numbers sort by value; a provider column sorts by its requests in the last hour. A value the screen does not show, such as a missing version, a `-` figure, or host figures of a stale peer, sorts last in both directions. Equal values keep the default order. `ALL` stays first. The rows follow the sort instead of the default problem-first order, and the cursor stays on the same peer when the rows reorder. The provider filter narrows the columns that `o` steps through. A column that is hidden at a narrow width still sorts, but its header mark is not drawn.
- `Ver` is the version each peer publishes (`reiki_version`), after the provider columns and before the host figures, clipped to twelve characters. `?` means the peer does not publish one. The detail card shows it after the instance name, as `v0.10.3`.
- `load` is the one-minute load with a sparkline of recent samples. `disk` is the fullest disk: green under 75%, yellow to 89%, red from 90%. `sess` is active sessions. A `-` means the peer does not publish that figure.
- A stale row shows `stale <age>` in place of host figures. Its error goes to the detail pane.
- The sparkline is built from samples taken once per redraw interval while the screen is live. `--once` has no history, so it shows the number only.
- Mismatched providers: a peer that does not use a provider shows `·`, and the tally leaves it out of both counts.

### Failures

Every peer's health document carries a `failures` object: counts of recent key failures by class over the peer's in-memory ring (`failure_ring_size`, 200 by default). The detail card prints those counts as `failures: rate_limited 2  transient 1`; names, streams, actions and bodies are never published, so a remote peer can only be compared, not investigated.

A peer with summary export enabled also publishes `requestlog_export_dropped_total`. When it is above zero, the detail card prints it as `export drops: N`; older peers without the field count as zero. See [summary export](requestlog.md#summary-export).

The full recent-failure list is shown only for the local instance, and only when `REIKI_ADMIN_READ_TOKEN` or `REIKI_ADMIN_TOKEN` holds one of its admin tokens (together with `[admin] listen` in the config). `reiki top` finds the local peer by matching its published `instance` to `[peers] instance`, or `reiki` when that is not set. For that peer the card adds one line per failure: time, stream, key, status, class, action, cooldown left and the scrubbed body snippet. With the read-only token the action is still shown; only the snippet is withheld. A remote peer's card keeps the counts, even when the local admin listener is configured. If the admin listener is missing, unreachable or refuses the token, the counts still appear and only the rows are absent.

See [`GET /v1/failures`](monitoring.md#get-v1failures) for the fields and the `reiki failures` command.

### Streams

Press `t` to open a read-only view of the local instance's streams; `t` returns to the matrix. It reads [`GET /v1/stream-console`](monitoring.md#get-v1stream-console) with the admin token `reiki top` was given (the read-only one when set). A remote peer's stream names never enter this screen.

The table lists every configured stream, static and dynamic. Its default order is current request rate, then concurrency, then name. Its columns are `stream`, `type` (`static` or `dynamic`), `chain` (the upstream order), `expires` (countdown to the next live token), `rpm` and `conc` (current/limit, `-` for no limit), `last` (time since the last admitted request) and `state`. `j`/`k`, the arrows and `Tab`/`Shift-Tab` move the row selection. The selection follows a stream by name, so a refresh that changes the usage order, or a re-sort, does not move it to a different stream.

`o` sorts by the next column and `O` by the previous one; past the last column, or before the first, the table returns to the default usage order. The columns are the stream name, usage (`conc`), chain, rate (`rpm`) and expiry (`expires`), in that order. The first press sorts ascending and `r` reverses the direction. The sorted header shows `▲` or `▼`, and equal values keep the default order. A value the screen does not show, such as an empty chain or no expiry, sorts last in both directions. `type`, `last` and `state` are display-only columns.

The detail pane shows the selected stream's numeric limits, then its credentials by short id with expiry and active/expired state, then recent audit records for that stream, newest first. Credential values, hashes, audit-chain hashes, file paths and the actor hash are never sent to the screen. `PgUp`/`PgDn`, `g`/`Home` and `G`/`End` scroll the detail.

When `[stream_admin]` is not configured the screen says `stream_admin is disabled; dynamic streams and audit are unavailable` and keeps the static rows. A failed refresh shows a short reason and keeps the last good answer on screen; retries back off from the health poll interval to at most one minute. Without `[admin]` or either token variable the table says it is waiting for the local admin listener. Nothing on this screen can create, rotate or revoke a stream or a token.

### Keys

In the matrix:

- `j`/`k`, the arrows, `Tab`/`Shift-Tab`: move the selection between `ALL` and the peer rows. With more rows than fit the list scrolls with the selection and the header stays in place.
- `f`: cycle the provider filter (all, then each provider, then all). The filtered view shows only that column and peers that have it, and falls back to all if the provider disappears. `ALL` stays first.
- `u`: switch the gauge between requests and tokens; while the Requests chart is showing it also cycles its series requests, limited and tokens. `w`: switch the gauge window between the last hour and the last five minutes.
- `Enter` or `e`: open or close the detail pane. On `ALL` this shows fleet totals and the per-peer breakdown; a problem peer's pane opens by itself, and Enter flips it.
- `PgUp`/`PgDn`, `g`/`Home`, `G`/`End`: scroll the detail pane.
- `m`: cycle the chart metric: load, disk, sessions, requests. `s`: toggle the Requests chart between overlaid lines and a stacked total. `a`: select the `ALL` row and close the detail pane.
- `o` and `O`: sort the peer rows by the next or previous column, or back to the default order (see [the matrix](#the-matrix)). `s` stays the stacked-chart toggle, so sorting uses `o`. `r`: reverse the sort. Both are no-ops until a column is sorted.
- `c`: clear the provider filter and the selection, so the chart shows the fleet again.
- `v` or `E`: switch to the full view.
- `t`: open the read-only Streams view; `t` there returns to the matrix.
- `p`: freeze the screen as a snapshot and show its age; updates keep arriving, and resuming shows the live view. The frozen snapshot keeps its sparklines.
- `?`: list every key. Any key closes the list.
- `q` or `Esc` quits.

In the full view (`v` or `--full`):

- `j`/`k`, the arrows, `PgUp`/`PgDn`, `g`/`Home` and `G`/`End` scroll.
- `Tab`/`Shift-Tab` filter to the next or previous peer; `c` clears the filter.
- `p` freezes, `v` returns to the matrix, `t` opens Streams, `?` lists the keys, `q` or `Esc` quits.

On the Streams screen (`t`):

- `j`/`k`, the arrows and `Tab`/`Shift-Tab` move the stream selection; `PgUp`/`PgDn`, `g`/`Home` and `G`/`End` scroll the detail pane.
- `o`/`O` step through the sortable columns and back to the default order; `r` reverses the direction (see [Streams](#streams)).
- `p` freezes the screen, `t` returns to the matrix, `?` lists the keys, `q` or `Esc` quits.

Live mode needs stdout to be a terminal; otherwise it prints one line and exits 2, so use `--once`.

The frame's widgets come from the `fw-tui` crate, pinned by revision in `crates/reiki-top/Cargo.toml`. Colour follows `NO_COLOR`.

### Names and columns

Peer and provider names come from the network. Control characters and invisible format characters (bidi overrides, zero-width marks) are shown as `?`, and widths are measured in terminal columns, so wide characters stay aligned. At most 32 provider columns are drawn; the fleet line adds `+N more` for the rest.

### Narrow terminals

The matrix drops the `Ver` column first, then the last-hour gauge, then shortens provider headers to six, then three characters, drops the sparkline, and as a last resort prints one chip line per peer with only the providers it uses. Below 20 columns it draws the fleet line and peer names. It never draws past the terminal width.

The screen gives up its panels before it cuts the matrix. As the height drops it loses, in order: the chart row (below 28 rows), the cooling panel (when the bare matrix would no longer fit beside it), the tab row, and then the header folds to one line. The Peers panel shrinks last, and only when even the bare matrix does not fit does it clip rows. The detail pane keeps its bottom third of the screen as before. Widths below 20 columns still draw the names-only matrix. Any size down to 0 by 0 draws without panicking.

### `--once`

`--once` polls each peer once and prints the matrix as plain text, 100 columns wide and without colour, including the `ALL` fleet row. `--once --full` prints the per-peer text view instead. It exits 0 when at least one peer answered, 1 when none did, and 2 on a config problem.

The full text view also prints `Streams: N static, M dynamic` when peers publish stream counts with `expose_usage` (on by default). It sums fresh peers that publish the counts; peers that do not are left out, and no stream names are shown.
