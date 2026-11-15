# reiki top

`reiki top` is a live terminal view of key health for every instance under `[peers.NAME]`. It reads the same `/v1/health` and `/v1/health/stream` as `reiki peers`, with the same per-peer token and CA rules, and draws one screen: a provider summary, then one card per peer with each upstream's healthy, cooling and disabled key counts, per-key labels and cooldowns, and host load and sessions when the publisher exposes them.

It is read-only: nothing here changes a key or serves traffic.

## Build

Release binaries include `reiki top`. From source it is the `top` cargo feature, which is not in the default set and implies `peers`:

```bash
cargo build --features top
```

Without the feature the subcommand and its `--help` entry are absent.

## Configure

It needs `[peers] enabled = true` and at least one `[peers.NAME]` table, exactly as `reiki peers`; each entry must point at an instance that publishes health and at a token file that instance issued (see [peers](peers.md)).

## Use

```bash
reiki top                 # live view; q quits
reiki top --once          # poll every peer once, print plain text and exit
reiki top --interval 2    # redraw every two seconds instead of every second
```

- Live mode follows each peer's stream and polls once when a stream cannot be opened, ends or goes quiet. Both failing puts the reason on that peer's card as a dim line. Data older than three `poll_interval_secs` is stale and drawn dim, never as healthy; a peer with no answer shows `no data`.
- Keys: `q` or `Esc` quits; `j`/`k`, the arrows, `PgUp`/`PgDn`, `g`/`Home` and `G`/`End` scroll; `p` pauses redraws (updates keep arriving; the screen catches up on resume); `Tab`/`Shift-Tab` filter to the next or previous peer; `c` clears the filter.
- Live mode needs stdout to be a terminal; otherwise it prints one line and exits 2, so use `--once`.
- `--once` polls each peer once and prints the same view as plain text, 100 columns wide and without colour. It exits 0 when at least one peer answered, 1 when none did, and 2 on a config problem.
