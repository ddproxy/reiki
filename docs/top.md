# reiki top

`reiki top` is a live terminal view of key health for every instance under `[peers.NAME]`. It reads the same `/v1/health` and `/v1/health/stream` as `reiki peers`, with the same per-peer token and CA rules.

The default screen is a peers-by-providers matrix: one row per peer, one column per provider, then host load, disk and sessions. Problem peers sort first. Below the matrix, the selected peer's detail card (keys, cooldowns, class, last error) opens in a bordered pane.

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

### The matrix

```
Fleet 3/3 fresh   gemini 2/2   zen 1/2   openrouter 0/1
          gemini        zen           openrouter  load      disk           sess
  peer-b  ■ ······      ·             ◐ ▁▂▃▄▅▆      -         -              -
> peer-c  ■ ······+2    ■ ▂▃▄▅▆▇ 45s  ·             -         -              -
  peer-a  ·             ■ ▁▁▂▃▄▅      ·             0.4 ▁▅█▄   30% ██░░░░░░  1
```

- The first line counts fresh peers, then one `provider healthy/with` tally per provider. Only peers that publish a provider count towards it. When every peer that has a provider is cooling or disabled (a stale peer counts as unknown), it adds `ALERT <provider> all cooling[, soonest <time>]` (up to three, then `+N more`). With `--provider` or the `f` filter it shows `filter: <provider>` and omits alerts.
- Cells: `■` healthy (green); `◐` partly healthy, with the soonest cooldown (yellow); `■` with a cooldown, no healthy key (red); `⊘` every key out of rotation (red); `·` the peer does not use this provider (dim); `?` the peer is stale (dim).
- After the state glyph, six cells show the last hour of attempts in ten-minute steps, from `▁` to `█`. They scale to the busiest bucket across the whole fleet for that provider, so rows compare at a glance; `·` is a zero bucket. A gauge turns yellow when any attempt in the hour was rate-limited or began a cooldown. A peer that publishes no figures, or an older publisher, draws `·` cells rather than an error.
- A `+N` after a cell is the number of cooling (route, model) combinations on that provider, when the peer publishes it; the request total is never in the matrix.
- The detail pane adds `last hour: N requests, M limited` and a twelve-cell five-minute trend for each upstream.
- `load` is the one-minute load with a sparkline of recent samples. `disk` is the fullest disk: green under 75%, yellow to 89%, red from 90%. `sess` is active sessions. A `-` means the peer does not publish that figure.
- A stale row shows `stale <age>` in place of host figures. Its error goes to the detail pane.
- The sparkline is built from samples taken once per redraw interval while the screen is live. `--once` has no history, so it shows the number only.
- Mismatched providers: a peer that does not use a provider shows `·`, and the tally leaves it out of both counts.

### Keys

In the matrix:

- `j`/`k`, the arrows, `Tab`/`Shift-Tab`: move the selection to the next or previous peer. With more peers than rows the list scrolls with the selection and the header stays in place.
- `f`: cycle the provider filter (all, then each provider, then all). The filtered view shows only that column and peers that have it, and falls back to all if the provider disappears.
- `Enter` or `e`: open or close the detail pane. A problem peer's pane opens by itself; Enter flips it.
- `PgUp`/`PgDn`, `g`/`Home`, `G`/`End`: scroll the detail pane.
- `v` or `E`: switch to the full view.
- `p`: freeze the screen as a snapshot and show its age; updates keep arriving, and resuming shows the live view. The frozen snapshot keeps its sparklines.
- `q` or `Esc` quits.

In the full view (`v` or `--full`):

- `j`/`k`, the arrows, `PgUp`/`PgDn`, `g`/`Home` and `G`/`End` scroll.
- `Tab`/`Shift-Tab` filter to the next or previous peer; `c` clears the filter.
- `p` freezes, `v` returns to the matrix, `q` or `Esc` quits.

Live mode needs stdout to be a terminal; otherwise it prints one line and exits 2, so use `--once`.

### Names and columns

Peer and provider names come from the network. Control characters and invisible format characters (bidi overrides, zero-width marks) are shown as `?`, and widths are measured in terminal columns, so wide characters stay aligned. At most 32 provider columns are drawn; the fleet line adds `+N more` for the rest.

### Narrow terminals

The matrix drops the last-hour gauge first, then shortens provider headers to six, then three characters, drops the sparkline, and as a last resort prints one chip line per peer with only the providers it uses. Below 20 columns it draws the fleet line and peer names. It never draws past the terminal width.

### `--once`

`--once` polls each peer once and prints the matrix as plain text, 100 columns wide and without colour. `--once --full` prints the per-peer text view instead. It exits 0 when at least one peer answered, 1 when none did, and 2 on a config problem.
