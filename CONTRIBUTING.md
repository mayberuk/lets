# Contributing

## Toolchain

Stable Rust at or above the MSRV in `Cargo.toml`'s `rust-version`, plus the pinned nightly
`justfile` names for `just fmt` — `rustfmt.toml` uses nightly-only options, but the code itself
builds and lints on stable. Install both with `rustup`:

```console
$ rustup toolchain install stable --component rustfmt,clippy
$ rustup toolchain install "$(just --evaluate nightly)" --component rustfmt
```

`cargo-deny`, `cargo-machete` and `cargo-nextest` are needed too (`cargo install --locked
cargo-deny cargo-machete cargo-nextest`), and `dist` if you're touching the release pipeline
(`cargo install --locked cargo-dist`).

The test suite also runs `rg`, and it expects GNU `sed -i`. On macOS, run
`brew install ripgrep gnu-sed` and put `$(brew --prefix gnu-sed)/libexec/gnubin` first on `PATH`.

## Recipes

| Command | Does |
|---|---|
| `just build` | debug build |
| `just fmt` | `cargo +nightly fmt` |
| `just lint` | format check, clippy `-D warnings`, `cargo-deny`, `cargo-machete`, the deps-count gate, the comment gate |
| `just test` | `cargo nextest run`: unit, property, literate cases, scenarios, hook corpus, bytes, JSON snapshots |
| `just check` | `lint` then `test`; the pre-commit and CI gate |
| `just docs` | regenerate `docs/examples/` from the passing literate cases |
| `just docs-check` | regenerate, then fail if that changed anything uncommitted |
| `just shellcheck` | `shellcheck` over `install.sh`, `scripts/*.sh` and the pre-commit hook |
| `just bench-gate` | wall-clock and allocation gates against `bench/gates.rs` |
| `just bench-baseline` | regenerate `bench/baselines/`; review the diff as a contract change |
| `just smoke-agent` | real `claude -p`, two arms; on demand, never CI |
| `just release-check` | musl static build, binary size, `dist plan` |

Run `just check` before opening a pull request; it's what CI runs.

## Pre-commit hook

```console
$ git config core.hooksPath .githooks
```

The hook is just `just check`. It's opt-in because it isn't fast, but CI runs the same gate on
every pull request regardless.

## Testing

Every tier but unit and property drives the built `lets` binary as a subprocess, inside a
sandbox: a fresh copy of a fixture tree per case, `HOME` and the lock directory pointed into a
temp dir, `LETS_NO_STATS=1` and `LETS_TOKEN_RATIO` pinned so output is byte-stable. Nothing in
the suite reads or writes your real `HOME` or the repository's own working tree.

| Tier | Command | Proves |
|---|---|---|
| Unit + property | `cargo nextest run --lib` | `matcher`, `normalize`, `target`, `window` invariants; proptest round-trips |
| Literate cases | `cargo nextest run --test cmd` | one case per command shape in `tests/cmd/`; `just docs` renders the passing ones into `docs/examples/` |
| Scenario replay | `cargo nextest run --test scenarios` | ordered command sequences against `tests/scenarios/<name>/expected/`, byte-identical |
| Hook corpus | `cargo nextest run --test hook` | `tests/hook/`: command → verdict, with the replacement text |
| Bytes | `cargo nextest run --test bytes` | CRLF, BOM, multibyte boundaries, mode bits, symlinks, hardlinks |
| JSON snapshots | `cargo nextest run --test json_snapshots` | `--json` and `--jsonl` shape, via `insta` |
| Bench gate | `just bench-gate` | wall-clock and allocation gates, before any performance claim |
| Agent smoke | `just smoke-agent` | real `claude -p`, two arms; on demand only |

A case whose external tool is absent is skipped by name (`skipped (<tool> absent)`), never
silently dropped — each verb's `required.txt` lists the cases and scenarios that may not vanish.
Regenerate a golden or snapshot only deliberately (`TRYCMD=overwrite`, `LETS_GOLDEN=overwrite`,
`INSTA_UPDATE=always`) and review the diff as a contract change: the tool's whole promise is that
its output is the verification, so a suite that would pass on wrong output doesn't back that
promise. Derive every expected value from the requirement the test is proving, never from a run
of the code under test, and give every success case a negative control that must fail.

## Comments

A comment says what the code can't: an invariant, a non-obvious constraint, the measurement
behind a threshold, a rejected alternative, or a workaround naming its cause. Default to no
comment — names and types carry the what. The full rule, with examples, is
[`.claude/rules/comments.md`](.claude/rules/comments.md); `scripts/comments-gate.sh`, run by
`just lint`, enforces the mechanical half of it (no banner comments, no commented-out code, no
`TODO`/`FIXME`, no oversized comment blocks).

## Performance

`lets` is called hundreds of times a session, so startup and per-call latency are gated, not just
measured: `bench/gates.rs` is the single source for every threshold, each with the measurement
that set it. `just bench-gate` builds a release binary, generates the fixture corpus, and runs
both bench targets (`benches/wall_clock.rs` for CLI latency against reference tools,
`benches/alloc.rs` for allocation counts and bytes under `divan`), failing on the first workload
that misses its gate or its baseline. Quote its output in any pull request that claims a
performance change or touches `src/hook/`, `src/output.rs`, `src/matcher.rs`, `src/symbols/` or
`src/verbs/find.rs`.

## Releases

Pushing a tag matching `v*.*.*` triggers the release workflow, which runs `dist` to build and
publish binaries for every target in `dist-workspace.toml` and attach them to a GitHub release.
`just release-check` runs the same static-binary build and size check locally before you tag.
