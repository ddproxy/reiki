# Notes

- Verified against `src/main.rs`, `src/state.rs`, and `src/config.rs`.
- `src/state.rs` already had `check_writable` for the state directory probe (`create_new` + `O_NOFOLLOW`, removed after open). It did not write a few KB; the probe now writes 4 KiB and `sync_all`s before removal to exercise write permission and free space.
- Startup state-dir probe ran only in server mode and always failed on error. It now respects `[gateway].on_store_error`: `closed` fails fast, `open` logs one `WARN` and continues.
- `[log].file` directory writability was not probed at startup; `logging::init` failed on file open. Startup now probes the log directory before opening the log file, with the same `on_store_error` policy. In `open` mode a log-dir probe failure disables the log file for that run and logs one `WARN`.
- `reiki check` does not bind listeners or write state; the new probes are server-startup checks.
- `gateway.on_store_error` default remains `closed` (`src/config.rs`).
- No secret-shaped literals were added.
