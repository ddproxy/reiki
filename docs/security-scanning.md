# Security scanning

The workflows in `.github/workflows` run the checks below. Findings appear as pull-request checks, as code scanning alerts or on releases; there is no separate dashboard to maintain.

| Workflow | Runs on | Checks | Where results appear |
| --- | --- | --- | --- |
| `ci` | pushes, pull requests, manual | Formatting, clippy, tests with and without `top` | `test` check on the commit or pull request |
| `codeql` | pushes to `main`, pull requests, weekly | CodeQL for Rust with build mode `none` | `Analyze (rust)` check and code scanning alerts in the Security tab |
| `scorecard` | pushes to `main`, weekly | OpenSSF Scorecard, with results published to the Scorecard API | `Scorecard` check, code scanning alerts, the [README badge](../README.md) and a `scorecard-results` artifact kept for 5 days |
| `dependency-review` | pull requests | New high or critical vulnerabilities, and licences outside `deny.toml`'s allow-list | `dependency-review` check and its summary on the pull request |
| `audit` | pushes, pull requests, daily | `cargo deny check` for advisories, bans, licences and sources | `Audit` check and its job log |
| `release` | `v*` tags | Builds archives, generates a CycloneDX JSON SBOM with checksums, and attests the archives | Release assets, release summary and GitHub's attestation store |

The dependency-review licence list mirrors `deny.toml`; keep both lists together when adding a licence. Scorecard publishes results, which is what keeps the badge current. Maintainers can check workflow YAML, SHA pinning and job permissions with `cargo run --quiet --manifest-path scripts/check-workflows/Cargo.toml`.

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
