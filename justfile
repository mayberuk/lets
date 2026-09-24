# Pinned because formatting output drifts between nightlies; bump it in its own commit.
nightly := "nightly-2026-09-19"

build:
    cargo build --locked

fmt:
    cargo +{{ nightly }} fmt

lint:
    cargo +{{ nightly }} fmt --check
    cargo clippy --all-targets --locked -- -D warnings
    @command -v cargo-deny >/dev/null 2>&1 || { echo "cargo-deny is not installed: cargo install cargo-deny --locked" >&2; exit 1; }
    cargo deny check
    scripts/deps-gate.sh
    @command -v cargo-machete >/dev/null 2>&1 || { echo "cargo-machete is not installed: cargo install cargo-machete --locked" >&2; exit 1; }
    cargo machete
    scripts/comments-gate.sh

test:
    cargo nextest run --locked
    cargo test --manifest-path tests/corpusgen/Cargo.toml
    @[ -s target/cmd-skipped.txt ] && cat target/cmd-skipped.txt || true

docs:
    scripts/gen-examples.sh

docs-check: docs
    git diff --exit-code docs/examples

shellcheck:
    shellcheck install.sh scripts/*.sh .githooks/pre-commit

# real `claude -p`, two arms, on demand; never CI
smoke-agent *args:
    scripts/smoke-agent.sh {{args}}

bench-gate:
    cargo build --release --locked
    cargo run --manifest-path tests/corpusgen/Cargo.toml --release
    cargo bench --bench wall_clock --locked
    cargo bench --bench alloc --locked

bench-baseline:
    cargo build --release --locked
    cargo run --manifest-path tests/corpusgen/Cargo.toml --release
    cargo bench --bench alloc --locked -- --save-baseline

# A skip exits 0 locally; with RELEASE_CHECK_STRICT=1 or CI set, a skipped step is a failure.
release-check:
    #!/usr/bin/env bash
    set -euo pipefail
    strict=${RELEASE_CHECK_STRICT:-${CI:+1}}
    skip() {
      echo "release-check: $1" >&2
      if [ "${strict:-0}" = 1 ]; then
        echo "release-check: strict mode, a skipped step fails" >&2
        exit 1
      fi
    }
    expr=$(sed -n 's/^pub const BINARY_MAX_BYTES: u64 = \(.*\);$/\1/p' bench/gates.rs)
    if [ -z "$expr" ]; then
      echo "release-check: BINARY_MAX_BYTES not found in bench/gates.rs" >&2
      exit 1
    fi
    limit=$((expr))
    if rustup target list --installed | grep -q x86_64-unknown-linux-musl \
        && command -v musl-gcc >/dev/null 2>&1; then
      cargo build --release --locked --target x86_64-unknown-linux-musl
      size=$(stat -c%s target/x86_64-unknown-linux-musl/release/lets)
      echo "release-check: stripped musl binary is $size bytes against the $limit-byte ceiling"
      if [ "$size" -ge "$limit" ]; then
        echo "release-check: over bench/gates.rs BINARY_MAX_BYTES" >&2
        exit 1
      fi
    else
      skip "musl target or musl-gcc not installed, skipping the musl build (rustup target add x86_64-unknown-linux-musl)"
    fi
    if command -v dist >/dev/null 2>&1; then
      dist plan
    else
      skip "dist not installed, skipping dist plan (cargo install dist --locked)"
    fi

check: lint test
