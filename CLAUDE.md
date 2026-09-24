# lets

Locate · Edit · Transform · Show: a file-operations CLI for coding agents. One Bash call reads,
locates, edits or transforms several files and returns bounded, numbered output whose footer
names everything left out, so no follow-up call is needed. Rust, one static binary per OS and
arch, no index, no daemon, no config file.

## Commands
| Command | Does |
|---|---|
| `just build` | debug build |
| `just fmt` | `cargo +nightly fmt`: the formatter runs on the pinned nightly because `rustfmt.toml` uses nightly-only options; the code still builds on stable |
| `just test` | `cargo nextest run`: unit, property, trycmd, scenarios, hook corpus, bytes, snapshots |
| `just lint` | `cargo +nightly fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo-deny`, `cargo-machete`, `scripts/deps-gate.sh` |
| `just check` | `lint` then `test`; the pre-commit and CI gate |
| `just docs` | regenerate `docs/examples/` from passing trycmd cases |
| `just site` | `bun install --frozen-lockfile` and build the website in `site/` to `site/dist/` |
| `just site-check` | `site`, then check the built site; its own workflow, `site.yml`, not part of `check` |
| `just bench-gate` | wall-clock and allocation gates against `bench/gates.rs` |
| `just bench-baseline` | regenerate `bench/baselines/`; the diff is reviewed |
| `just smoke-agent` | real `claude -p`, two arms, on demand; never CI |
| `just release-check` | musl static build, binary size, `dist plan` |

Harness detail and the sandbox contract: `CONTRIBUTING.md`.

## Rules: `.claude/rules/`, five global and four scoped
- **Comments**: why-not-what, default none. `comments.md`
- **Engineering**: YAGNI ladder, one `Error` enum, no one-impl trait, mechanical over prose. `engineering.md`
- **Stack**: the crate table, gate 17, ctx7 first. `stack.md`
- **Testing**: tiers, expected from the requirement, negative controls, sandbox. `testing.md`
- **Performance**: gates in `bench/gates.rs`, expected and forbidden optimizations. `performance.md`
- **Contract** (`src/output.rs`, `src/main.rs`, `tests/**`): footer names every omission, exit codes in one place, stream discipline. `contract.md`
- **Edit safety** (`edit`, `transform`, `write`, `fs`, `lock`, `matcher`, `check`, `normalize`): byte splices, sorted locks, validation-atomic, structure is not syntax. `edit-safety.md`
- **Hook** (`src/hook/**`, `tests/hook/**`): fail open, block only with a runnable replacement. `hook.md`
- **Layout** (`src/**`, `tests/**`, `bench/**`, `docs/**`): the decided tree, generated files never hand-edited. `structure.md`

## Dealbreakers (one confirmed instance in real use is stop-ship)
- The hook blocks when `lets` is missing, crashing or mid-update. Every error path degrades to
  allow.
- A footer omits a narrowing, skip or omission that happened.
- A revert or partial batch leaves a file in a state the output does not name.
- p50 over 50 ms for `show` or `edit` on a file under the default window, or `guide` over 5 ms.
- Non-deterministic stdout for identical input and file state.
- A block message with no runnable replacement command.
- A blocked heredoc-to-stdin, or an edit that changes bytes outside the matched span.

## Hard constraints
- Rust. `lets hooks install claude-code` and `lets hooks install codex` merge every discovery
  tier they support into that agent's own user-level settings; nothing is appended to the system
  prompt and no shell alias is needed.
- Never published to crates.io. The installer refuses when a different `lets` is already on
  `PATH`, naming it.
- stdout is the answer, stderr is diagnostics plus `ERROR_CODE`, stdin is accepted wherever
  content goes in. No output assumes a terminal.
