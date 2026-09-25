# Benchmarks

How the numbers in the README's "Measured" section were produced, and the caveats that go with
them.

## Method

Two arms, each a fresh Claude Code session against a pinned commit of the target repository:

- **none** — a stock session, no `lets` on `PATH` and no hooks installed.
- **lean** — `lets hooks install claude-code`: the `SessionStart` paragraph plus the
  `PreToolUse` classifier, nothing appended to the system prompt.

Five tasks are sent as five turns of one `claude -p` conversation, so later turns see the file
state earlier turns left behind, the way a real session does. Each turn is graded afterward by a
checker whose expected values are written from the task requirement, never from a run of the
model's own output. Cost is the dollar figure Claude Code itself reports as billed for the
session.

Two repositories were used: a generated small TypeScript fixture, and a private 17.7k-file Go
monorepo. Neither the task text, the fixture, the monorepo's contents, nor any session transcript
is published here — only the aggregate numbers below are.

## Large-repo trial

Five tasks, 38 checks per session, on the private 17.7k-file Go monorepo. Sessions ran in three
rounds as the `SessionStart` paragraph and `find`'s cap-and-fallback behaviour were revised;
`none` is pooled across the two rounds it ran in, `lean` across all three.

| model | arm | n | cost, mean | tool calls | wall time (s) | checks passed |
|---|---|---|---|---|---|---|
| Sonnet 5 | none | 6 | $1.356 | 76.5 | 451 | 223/228 (97.8%) |
| Sonnet 5 | lean | 9 | $1.349 | 64.6 | 403 | 339/342 (99.1%) |
| Opus 5.5 | none | 6 | $0.929 | 27.7 | 408 | 228/228 (100%) |
| Opus 5.5 | lean, round 1 | 3 | $1.057 | 24.3 | 331 | 114/114 (100%) |
| Opus 5.5 | lean, round 2 | 3 | $0.949 | 22.0 | 390 | 114/114 (100%) |

Reading: Sonnet lean is cost-neutral against vanilla (−0.5%, $1.349 vs $1.356) with 14–16% fewer
tool calls and a higher check-pass rate. Opus lean cuts tool calls by 12–19% and wall time by
5–19% depending on the round; round 1's cost (+14%) was pulled up by one $1.263 outlier session,
and round 2 came in at +2% ($0.949 vs $0.929) once the `SessionStart` paragraph and `find`'s
over-cap behaviour had been revised.

Tool-call batching — the share of model responses that issue more than one tool call in the same
turn — stayed under vanilla in both arms on this harness (about 11% lean vs 21% vanilla for
Sonnet, near zero for Opus in both arms). `lets`'s effect here is fewer, more targeted calls per
turn, not more calls batched into one response; Claude Code's own batching behavior, not `lets`,
decides that.

### Caveats

- Small n (3–9 sessions per arm) against real spread: vanilla Sonnet's per-session cost ranged
  about $1.19–$1.76 (sd ≈ $0.20), so resolving a 5% difference at that variance would need
  roughly 140 sessions per arm.
- The single most expensive session in either arm — in both this trial and the small-fixture one
  below — came from the model delegating a task to a background subagent rather than doing the
  work directly, a source of variance independent of `lets`.
- Sessions ran in rounds as the paragraph and `find`'s fallback behavior were revised; only the
  final round reflects the shipped behavior, but the aggregate above pools every round run so
  far, so it understates lean's current numbers on the earlier-round sessions and overstates them
  on the latest.

## Small-fixture session trial

An earlier trial, five turns (two larger tasks, three smaller ones), 111 checks, Sonnet 5 only,
on a generated small TypeScript fixture, run before the large-repo trial and against an earlier
form of the `SessionStart` paragraph.

| arm | n | cost, mean | model requests | tool calls | wall time (s) | checks passed |
|---|---|---|---|---|---|---|
| none | 3 | $0.4188 | 50.0 | 45.0 | 101 | 110/111 |
| lean | 3 | $0.4429 | 28.7 | 23.7 | 86 | 110/111 |

Its request and tool-call reduction agrees in direction with the large-repo trial, but its small
n and its higher-than-expected per-turn cost (more model requests than the later, larger-repo
trial, despite the smaller fixture) are why it is reported separately rather than pooled into the
headline numbers above.

## Local latency

`just bench-gate`'s wall-clock target spawns the built `lets` binary against `bash -n`, `cat` and
`rg` on the generated fixture corpus under `tests/fixtures/` — never a real repository. Median
p50 across three passes, 2026-09-24, on an 8-core/16-thread AMD Ryzen 7 5700X3D (kernel
6.17.4-76061704-generic) — a shared machine with other builds and test runs going at the same
time, 1-minute load 2–4 rather than idle:

| workload | p50 | vs. reference |
|---|---|---|
| `guide` | 0.8 ms | — |
| `hook classify`, 200-char command | 1.1 ms | 1.17× `bash -n` |
| `show`, one file ≤ 200 lines | 0.9 ms | 1.64× `cat` |
| `find`, literal, 2,000 files | 7.8 ms | 1.14× `rg` |
| `edit`, one replacement + structural check | 2.3 ms | — |
| `edit --from -`, 10 files | 23.0 ms | — |
| `transform --set`, 500-line YAML | 3.2 ms | — |

Gate thresholds, the CI margin and the allocation baselines live in `bench/gates.rs`; that file
carries the exact measurement each threshold was set from.

Measured once, outside the bench-gate harness, on the large-repo trial's 17.7k-file monorepo:
`show` at 2–4 ms, `find` at roughly 110 ms across the whole tree, and a symbol lookup plus an edit
on its largest file (17.9k lines) at 116 ms and 183 ms.

## 0.0.1 → 0.0.2

Same machine as above. `lets` 0.0.1 (tag `v0.0.1`) and 0.0.2 (commit `7e3dde4`) were each built as
`cargo build --locked --profile dist --target x86_64-unknown-linux-musl` from their own `git
worktree`, then run against the same generated fixture corpus and, for the repo-root row, a
worktree checkout of this repo. Each row's arms — both `lets` versions and, where named, `rg`,
`cat` or `bash -n` — were interleaved one run at a time (one run of arm 1, one of arm 2, one of
arm 3, repeat), so a load burst from the other work sharing the machine lands in every arm's
sample equally rather than biasing whichever arm happened to run during it; times come from a
small Python harness (`subprocess.run`, no shell) rather than hyperfine, because hyperfine runs
all of arm 1's samples before starting arm 2. 20 warmup runs plus 200 measured runs per arm; p50
and p99 use the same integer-percentile formula as `benches/wall_clock.rs`. Load ranged 2.06–4.10
across the run; every number below was taken while other processes shared the machine.

| workload | 0.0.1 p50 | 0.0.2 p50 | reference p50 | 0.0.2 vs 0.0.1 | 0.0.2 vs reference |
|---|---|---|---|---|---|
| `show`, 199-line TypeScript | 1.415 ms | 0.977 ms | `cat` 0.592 ms | 1.45× faster | 1.65× `cat` |
| `show`, 8 MiB file, default window | 16.650 ms | 4.065 ms | `cat` 1.097 ms | 4.10× faster | 3.71× `cat` |
| `show`, 8 MiB file, `--all` | 102.070 ms | 35.674 ms | `cat` 1.423 ms | 2.86× faster | 25.07× `cat` |
| `show path#symbol`, 1999-line TypeScript | 15.587 ms | 11.623 ms | — | 1.34× faster | — |
| `find`, literal, whole corpus | 12.304 ms | 7.253 ms | `rg` 6.967 ms | 1.70× faster | 1.04× `rg` |
| `find`, literal, this repo's root (3 hits, 2 files) | 22.242 ms | 10.516 ms | `rg` 8.755 ms | 2.11× faster | 1.20× `rg` |
| `find`, repo root, 0.0.2 with expansion on | — | 16.510 ms | — | — | +57% p50 over no-expand |
| `hook classify`, 200-byte command | 1.573 ms | 1.217 ms | `bash -n` 1.015 ms | 1.29× faster | 1.20× `bash -n` |
| `guide` (startup) | 1.196 ms | 0.845 ms | — | 1.42× faster | — |
| `--version` (startup) | 1.114 ms | 0.815 ms | — | 1.37× faster | — |
| `edit` + structural check, 199 lines | 3.261 ms | 2.385 ms | — | 1.37× faster | — |
| `transform --set`, 503-line YAML | 4.492 ms | 3.264 ms | — | 1.38× faster | — |

0.0.1's `find` has no symbol-expansion feature at all, so its numbers above are directly
comparable to 0.0.2's `find --no-expand`; the "expansion on" row measures 0.0.2 alone, against
itself, to show what expansion costs. `show --all` on the 8 MiB file needed `--max-bytes
16777216`: the default 64 KiB budget refuses `--all` on a file that size on both versions
(exit 4, `over_budget`).
