# AGENTS.md

Instructions for humans and agents working in this repository. README.md is the user-facing entry point; this file is the build-and-test entry point.

## Commands

Run these from the repository root. Rust 1.99 or newer is required. `make help` prints the same list with one-line descriptions.

| Command | Purpose |
| --- | --- |
| `make build` | Release build with `reiki top` (`cargo build --release --locked --features top`). |
| `make check` | `cargo check --workspace --all-targets --locked`. |
| `make test` | Version script tests (when `python3` is present) and workspace tests with default features and with `--features top`. |
| `make test-scripts` | Version script tests; skipped locally without `python3`, always run in CI. |
| `make lint` | `cargo fmt --all --check` and clippy for all targets with `-D warnings`, default and `top`. |
| `make docs-check` | `cargo test --locked --test docs_examples`: validates documented config examples, CLI flags and links, and runs the quickstart against a throwaway instance. |
| `make ci` | All of the above. |
| `make install` | Install the release binary into `PREFIX/bin`. |

Optional checks when the tools are installed: `cargo deny check`, `vale --config ... docs/ README.md`, and the fuzz targets in `fuzz/`. Do not claim they passed if the tool was absent.

## Repo map

| Path | What lives there |
| --- | --- |
| `src/main.rs` | CLI definition (`clap`), command dispatch, exit codes. |
| `src/config.rs` | Every config table, field, default and validation rule. |
| `src/proxy.rs`, `src/serve.rs`, `src/forward.rs`, `src/intercept.rs` | Request path, listeners, upstream transport, TLS interception. |
| `src/auth.rs`, `src/chain.rs`, `src/classify.rs`, `src/quota.rs`, `src/state.rs` | Tokens, key chains, response rules, cooldowns, persisted key health. |
| `src/gateway.rs`, `src/shaper/`, `src/usage.rs` | Stream admission, limits, counters and self-service usage. |
| `src/stream_admin.rs` | Dynamic streams API, state file and hash-chained audit log. |
| `src/admin.rs`, `src/peers/`, `src/top.rs` | Admin routes, health sharing, terminal fleet view. |
| `crates/reiki-*` | Plugin API and built-in plugins: detect, phi, finance, controlled, placeholders, requestlog, metrics, toolpolicy, cost, wasm, health, top. |
| `docs/`, `README.md`, `CHANGELOG.md` | User documentation and release notes; `docs/README.md` is the index. |
| `tests/`, `tests/golden/` | Integration tests and golden admin documents; `tests/docs_examples.rs` checks documentation. |
| `scripts/check-workflows/`, `scripts/set-release-version.py`, `scripts/tests/` | Workflow policy checker and release version validation with unit tests. |
| `.github/workflows/` | CI, stable and dev release, CodeQL, audit, fuzz and docs checks. |

## Conventions

- Rust edition 2024; `unsafe_code = "forbid"` and clippy `all` denied by `Cargo.toml`. Keep it that way.
- Config is `deny_unknown_fields`: adding a key means adding it to the struct, validation, docs and a test. Renaming a key is a breaking change; no compatibility shims are kept.
- Stable vocabulary: "stream" is a tenant, "key" is a provider credential, "chain" is a list of keys. Do not alternate between words such as "suspicious" and "WARN" for one finding class.
- Every command, flag, default and limit in docs is verified against `src/main.rs` or `src/config.rs`. Every config snippet must pass `reiki --config FILE check`. `make docs-check` enforces this for the documented examples.
- No secret-shaped literals in docs or tests. Use obvious placeholders such as `<64 hex from the admin token>` and generate values at runtime. The controller scrubs patches, so a plausible-looking token mangles the diff.
- Commit messages are terse and describe the change. Do not put job names, ticket IDs or personal names in commits or release notes.
- Behavior changes need a CHANGELOG entry under the correct `Added`, `Changed`, `Fixed`, `Security` or `Removed` heading and an upgrade note when an operator must act. This docs job does not bump the Cargo version.

## Things not to touch without a separate decision

- `Cargo.toml` version and `Cargo.lock` dependency versions.
- Wire formats and golden documents in `tests/golden/`: peers, health and status readers are strict; changing a shape is a compatibility break.
- The `/v1`-only admin routes and removed legacy config aliases; they are intentionally gone.
- Release workflow pinned action SHAs and build targets.
- Audit and state file formats: append-only hash chain and mode-`0600` atomic state. A format change needs a migration plan and a new format version.

## Quickstart as a smoke test

`make docs-check` extracts the documented TOML examples, writes them to a throwaway directory with runtime-generated hashes and a fake key file, runs `reiki --config FILE check`, then starts an instance on two free loopback ports. It asserts the health probe, the `401` for a wrong stream token, and the `keys` and `failures` output from the README proof step. Run it after any change to the CLI, config, listeners or docs. A manual smoke test follows [docs/quickstart.md](docs/quickstart.md) with a real key file and no upstream traffic.

## Stable command list and exit codes

`reiki` uses `0` for success and `2` for argument-parser usage errors unless a command says otherwise.

| Command | Non-zero codes |
| --- | --- |
| server mode | `1` config, key, plugin, state-directory or bind failure; `143`/`130` after a second stop signal |
| `check` | `1` when any check fails |
| `keys` | `1` refused by the instance or a `--check` warning; `2` unreachable or config unreadable |
| `failures`, `reload` | `1` refused or failed; `2` unreachable or config unreadable |
| `hash-token` | `1` token too short or stdin unreadable |
| `scan` | `1` findings; `2` usage or read error |
| `streams` | `1` API refusal; `2` usage, config, transport or audit-verification error |
| `cost export` | `1` refused or malformed answer; `2` unreachable or config unreadable |
| `plugin inspect` | `1` unreadable file or invalid component; `2` usage error |

Full per-command reference: [docs/cli.md](docs/cli.md).
