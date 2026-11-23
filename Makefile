.PHONY: help build install uninstall test lint check docs-check ci du clean clean-debug clean-release clean-stale

# root installs to /usr/local, anyone else to ~/.local; override with PREFIX=
PREFIX ?= $(if $(filter 0,$(shell id -u)),/usr/local,$(HOME)/.local)
BINDIR ?= $(PREFIX)/bin
TARGET_DIR ?= $(or $(CARGO_TARGET_DIR),target)


help:  ## list targets
	@grep -E '^[a-z-]+:.*##' Makefile | sed 's/:.*##/ -/'

build:  ## release build with reiki top
	cargo build --release --locked --features top

install: build  ## build and install reiki into PREFIX/bin (~/.local, or /usr/local as root; PREFIX= and DESTDIR= override)
	install -d '$(DESTDIR)$(BINDIR)'
	install -m 0755 '$(TARGET_DIR)/release/reiki' '$(DESTDIR)$(BINDIR)/reiki'
	@echo "installed $(DESTDIR)$(BINDIR)/reiki"

uninstall:  ## remove the installed binary
	rm -f '$(DESTDIR)$(BINDIR)/reiki'

fmt:  ## check Rust formatting
	cargo fmt --all --check

lint:  ## fmt check plus clippy with warnings denied, default and top features
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo clippy --workspace --all-targets --locked --features top -- -D warnings

test:  ## workspace tests, default and top features
	cargo test --locked --workspace
	cargo test --locked --workspace --features top

check:  ## compile every target without running tests
	cargo check --workspace --all-targets --locked

docs-check:  ## validate documented config examples, CLI flags and links
	cargo test --locked --test docs_examples

ci: check lint test docs-check  ## everything CI runs

du:  ## show target/ size breakdown (debug/release/musl), biggest first
	@du -sh target/* 2>/dev/null | sort -rh || echo "no target/ yet"

clean-debug:  ## cargo clean, debug profile only -- debug/ is almost always the bulk of target/ (stale incremental artifacts, never GC'd by cargo itself)
	cargo clean --profile dev

clean-release:  ## cargo clean, release profile only
	cargo clean --release

clean-stale:  ## remove build artifacts untouched for 14+ days, without a full clean (needs: cargo install cargo-sweep)
	cargo sweep --time 14 .

clean:  ## full cargo clean -- next build recompiles everything from scratch
	cargo clean
