# Install

This page is for operators putting Reiki on a host for the first time. The [quickstart](quickstart.md) starts from an installed binary and reaches a checkable proof.

## One line, once published with tuishou

```sh
curl -fsSL https://raw.githubusercontent.com/ddproxy/tuishou/main/install.sh | bash -s -- --components reiki
```

**Coming with tuishou.** The installer script may not exist yet. Until it does, the manual path below is the working path. Read any installer before you run it:

```sh
curl -fsSL https://raw.githubusercontent.com/ddproxy/tuishou/main/install.sh -o install.sh
less install.sh
bash install.sh --components reiki
```

The same line installs several components when needed: `--components reiki,lu`. Reiki's component places one binary on `PATH`; it does not create a config, key file, state directory or service unit.

## Manual install, works today

From a checkout with Rust 1.99 or newer:

```sh
git clone https://github.com/ddproxy/reiki.git
cd reiki
make install
```

`make install` runs `make build` (`cargo build --release --locked --features top`) and installs `reiki` with mode `0755` into `~/.local/bin`, or `/usr/local/bin` as root. Use `PREFIX=/opt/reiki make install` for another prefix and `DESTDIR=` to stage an install. `make uninstall` removes that one binary.

From a release archive: download the archive for your target from [releases](https://github.com/ddproxy/reiki/releases), check it against its `.sha256` file, extract it and copy `reiki` onto `PATH`:

```sh
tag=v0.14.0
gh release download "$tag" --repo ddproxy/reiki \
  --pattern '*.tar.gz' --pattern '*.sha256' --dir dist
(cd dist && shasum -a 256 -c *.sha256)
tar -xzf "dist/reiki-$tag-x86_64-unknown-linux-musl.tar.gz" -C dist
install -m 0755 dist/reiki-"$tag"-x86_64-unknown-linux-musl/reiki ~/.local/bin/reiki
```

Archives also ship a CycloneDX SBOM and build provenance attestations; [security scanning](security-scanning.md#verify-a-release) shows the verification commands.

## Development builds

A successful CI push to `main` that changes more than documentation also publishes a pre-release tagged `vX.Y.(Z+1)-dev.<commits since last stable tag>.g<short hash>`. The tag without the leading `v` is the version in that build's `Cargo.toml`; it sorts after the base stable version and before the next stable version. Dev archives, checksums, SBOM and attestations use the same layout as stable archives. Only the ten newest dev pre-releases are kept, and older dev tags are removed with them. A stable tag still has to equal the checked-in `Cargo.toml` version. Prefer stable releases for production.

## Verify the install

```sh
reiki --version
reiki --help
```

Then follow the [quickstart](quickstart.md). `reiki --config FILE check` validates a config without contacting a listener. `--components` is an installer flag, not a Reiki CLI flag.

## Build options

Plugins and their order are chosen at build time through `REIKI_PLUGINS`; see [plugins](plugins.md#choose-and-order-plugins-at-build-time). Cargo features add opt-in subsystems: `top` (the terminal fleet view), `plugin-cost` (billing estimates), `plugin-wasm` (runtime components), `hub` (parsed-only hub config) and `fuzzing` (parser entry points for fuzz targets, never in a release build). See [configuration](configuration.md#validation) and [security](security.md).

## Platforms

Prebuilt archives are produced for `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin` and `x86_64-apple-darwin`. The source build needs Rust 1.99 or newer. See the [compatibility matrix](../README.md#compatibility-matrix) for what was last checked.
