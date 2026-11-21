# Security scanning

The workflows in `.github/workflows` run the checks below. Findings appear as pull-request checks, as code scanning alerts or on releases; there is no separate dashboard to maintain.

| Workflow | Runs on | Checks | Where results appear |
| --- | --- | --- | --- |
| `ci` | pushes, pull requests, manual | Formatting, clippy, tests with and without `top` | `test` check on the commit or pull request |
| `codeql` | pushes to `main`, pull requests, weekly | CodeQL for Rust with build mode `none` | `Analyze (rust)` check and code scanning alerts in the Security tab |
| `scorecard` | pushes to `main`, weekly | OpenSSF Scorecard, with results published to the Scorecard API | `Scorecard` check, code scanning alerts, the [README badge](../README.md) and a `scorecard-results` artifact kept for 5 days |
| `dependency-review` | pull requests | New high or critical vulnerabilities, and licences outside `deny.toml`'s allow-list | `dependency-review` check and its summary on the pull request |
| `audit` | pushes, pull requests, daily | `cargo deny check` for advisories, bans, licences and sources | `Audit` check and its job log |
| `fuzz` | weekly, manual | 10 cargo-fuzz targets, 120 seconds each, for untrusted-input parsers | `Fuzz (<target>)` checks, job logs and `fuzz-crash-<target>` artifacts on failure |
| `release` | `v*` tags | Builds archives, generates a CycloneDX JSON SBOM with checksums, and attests the archives | Release assets, release summary and GitHub's attestation store |

The dependency-review licence list mirrors `deny.toml`; keep both lists together when adding a licence. Scorecard publishes results, which is what keeps the badge current. Maintainers can check workflow YAML, SHA pinning and job permissions with `cargo run --quiet --manifest-path scripts/check-workflows/Cargo.toml`.

## Fuzzing

`fuzz/` is a separate Cargo workspace so libFuzzer's build artifacts stay out of the main build. Its targets cover the parsers that consume untrusted input:

- `sse_parser`: peer/event SSE lines and chunk boundaries.
- `scrub_forms`: form generation and stream scrubbing of secrets.
- `placeholders` and `redaction`: plugin masking, including escaped JSON terms.
- `json_pair`: the key-invalid marker in JSON bodies.
- `config_parse` and `key_file`: config and key-file/permission parsing.
- `path_normalization`: repeated percent-decoding and dot-segment checks.
- `tunnel_allowlist`: allow-list rules and `host:port` parsing.
- `headers`: bearer tokens and rate-limit header parsing.

Each target asserts that the parser does not panic, that secrets never survive scrubbing, and that its output stays bounded by the input. Hand-written seeds live in `fuzz/corpus/<target>/`; libFuzzer adds mutated cases there locally. Run one target for 30 seconds with:

```bash
cd fuzz
cargo +nightly fuzz run sse_parser -- -max_total_time=30
```

Install the pinned driver first with `cargo install cargo-fuzz --locked --version 0.13.2`. The `fuzz` workflow runs every target for 120 seconds weekly and on manual dispatch. A failing run uploads the reproducing bytes from `fuzz/artifacts/<target>/` as a `fuzz-crash-<target>` artifact; that directory is ignored by git. The `fuzzing` feature only exposes existing internals to the harnesses and does not add any `unsafe` code.

## Verify a release

Download the release assets and check the attestation, the checksums and the SBOM:

```bash
tag=v0.12.0
gh release download "$tag" --repo ddproxy/reiki \
  --pattern '*.tar.gz' --pattern '*.sha256' --pattern '*-sbom.json' --dir dist

archive="reiki-$tag-x86_64-unknown-linux-musl.tar.gz"
gh attestation verify "dist/$archive" --repo ddproxy/reiki

# Checksum files name files relative to dist/.
(cd dist && shasum -a 256 -c *.sha256)
```

`gh attestation verify` checks the archive against the provenance attested in the release workflow. The `.sha256` files cover the archives and the SBOM; inspect `dist/reiki-$tag-sbom.json` as CycloneDX JSON to see the components. Run one attestation verify per archive you use.

## Owner checklist (GitHub settings)

These settings live in GitHub, not in this repository's files. A repository owner must do them.

- [ ] Enable secret scanning and push protection under **Settings → Code security and analysis**.
- [ ] Enable Dependabot alerts and the dependency graph they require.
- [ ] Enable private vulnerability reporting under **Settings → Code security and analysis**.
- [ ] Protect `main`: require a pull request, require the status checks `test`, `Analyze (rust)`, `dependency-review` and `Audit`, and require branches to be up to date before merging. Do not require `Scorecard`: it runs on pushes to `main` and weekly, not on pull requests.
- [ ] Keep code scanning **default setup disabled**. CodeQL and Scorecard upload SARIF from the workflows above.
- [ ] Register the project for an [OpenSSF Best Practices badge](https://www.bestpractices.dev/) after the settings above are in place. The README has no Best Practices badge until registration is complete.
- [ ] Do not add a crates.io badge: `Cargo.toml` sets `publish = false`, so the crate has no crates.io release. Add one only if that changes.
