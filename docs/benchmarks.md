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
`rg` on the generated fixture corpus under `tests/fixtures/` — never a real repository. A sample
dev-box run, p50:

| workload | p50 | vs. reference |
|---|---|---|
| `guide` | 1.1 ms | — |
| `hook classify`, 200-char command | 1.2 ms | 1.34× `bash -n` |
| `show`, one file ≤ 200 lines | 1.2 ms | — |
| `find`, literal, 2,000 files | 8.8 ms | 1.34× `rg` |
| `edit`, one replacement + structural check | 2.7 ms | — |
| `edit --from -`, 10 files | 24 ms | — |
| `transform --set`, 500-line YAML | 3.8 ms | — |

Gate thresholds, the CI margin and the allocation baselines live in `bench/gates.rs`; that file
carries the exact measurement each threshold was set from.

Measured once, outside the bench-gate harness, on the large-repo trial's 17.7k-file monorepo:
`show` at 2–4 ms, `find` at roughly 110 ms across the whole tree, and a symbol lookup plus an edit
on its largest file (17.9k lines) at 116 ms and 183 ms.
