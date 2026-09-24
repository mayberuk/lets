# lets

Locate · Edit · Transform · Show — a file-operations CLI for coding agents (Claude Code, Codex).
One Bash call reads, searches, edits or transforms several files and returns bounded, numbered
output whose footer names everything it left out. One static Rust binary. No index, no daemon,
no config file.

## What it is

`lets` is a small command-line tool that coding agents call instead of `cat`, `grep`, `sed` and
the built-in Read/Edit tools. The agent runs it through its normal shell tool, the same way it
already runs `cat`.

| Command | Replaces | Does |
|---|---|---|
| `lets show` | `cat`, `head`, `sed -n`, Read | reads files, line ranges, a function by name, a markdown section, or lines around a regex — several targets per call |
| `lets find` | `grep -rn`, `rg` | searches with line numbers, grouped by file, capped at 50 hits, says when it hit the cap |
| `lets edit` | Edit, `sed -i`, rewrite scripts | replaces text that must match exactly once, or inserts after an anchor; batches across files |
| `lets transform` | `jq`, `yq`, `sed` on config | sets, appends or deletes keys in JSON, YAML, TOML or markdown frontmatter, keeping comments and formatting |
| `lets write` | `cat > file <<EOF` | creates a file from stdin; won't overwrite an existing file without `--force` |

## Two ideas

**One call does the whole job.** Read five files, one function by name, or the lines around a
search hit, in a single call. An edit prints the changed lines and a syntax check in the same
output, so the agent doesn't need to re-read the file to confirm the edit landed.

**Output is bounded and honest.** 200 lines per file and 50 search hits by default. The last line
— the footer — names everything left out: lines past the window, hits over the cap, files skipped
by `.gitignore`. If the footer doesn't name a cut, nothing was cut, so the agent can trust the
answer and skip the follow-up read.

Why it matters: every extra round trip is another model turn, and under prompt-cache billing each
turn re-reads the whole held conversation. Unbounded output — `cat` on a 1,600-line file — fills
the context window and brings compaction sooner.

## Before and after

The "after" output below is real, run against a small demo project with the current binary.

### Read several files

Before — two turns, one file per call: `cat src/usage.ts` then `cat src/config.ts`.

```console
$ lets show src/usage.ts src/config.ts
── src/usage.ts  (1-13 of 13) · sha:75d31d847ffb
 1   import { usageCap } from './config'
 2
 3   export function usage(id: string) {
 4     const now = Date.now()
 5     const cap = 10
 ...
13   }
── src/config.ts  (1-2 of 2) · sha:4f49d457dfea
 1   export const usageCap = 10
 2   export const retries = 3
── showed 2 targets · 15 lines · ~74 tokens
```

### Read one function

Before — either the whole file to see 7 lines, or two turns: `grep -n 'function usage'
src/usage.ts`, then `sed -n '3,20p' src/usage.ts` while guessing where the function ends.

```console
$ lets show src/usage.ts#usage
── src/usage.ts#usage  (3-9 of 13 · via tree-sitter) · sha:75d31d847ffb
3   export function usage(id: string) {
4     const now = Date.now()
5     const cap = 10
6     if (!id) return
7     if (count(id) > cap) return
8     return total(id, now)
9   }
── showed 1 target · 7 lines · ~38 tokens
```

### Edit, and confirm it landed

Before — three turns: read for the exact text, edit, re-read to check it landed.

```console
$ lets edit src/usage.ts --old 'const cap = 10' --new 'const cap = 20'
── src/usage.ts · 1 replacement · line 5 · exact
3   export function usage(id: string) {
4     const now = Date.now()
5~    const cap = 20
6     if (!id) return
7     if (count(id) > cap) return
── check: structure ok · sha:75d31d847ffb→93b5daea8ace · ~45 tokens
```

`~` marks a changed line. Don't `cat` after a `lets` edit — the proof is already in the output.

### The text is there more than once

Before — the Edit tool fails with `Found 3 matches`, then the agent greps to pick one.

```console
$ lets edit src/usage.ts --old 'return' --new 'return undefined'
src/usage.ts is ambiguous (3 candidates)
  src/usage.ts:6    if (!id) return
  src/usage.ts:7    if (count(id) > cap) return
  src/usage.ts:8    return total(id, now)
ERROR_CODE=ambiguous
```

### An edit that breaks the file

Before — the edit lands with `sed -i`, and the break shows up several turns later in a test run,
or never.

```console
$ lets edit src/usage.ts --old 'return total(id, now)' --new 'return total(id, now))'
── src/usage.ts · 1 replacement · line 8 · REVERTED
 6     if (!id) return
 7     if (count(id) > cap) return
 8~    return total(id, now))          ← parse error
 9   }
── check: failed → reverted · file unchanged · sha:93b5daea8ace · ~40 tokens
ERROR_CODE=check_failed
```

The file is parsed before and after the edit; a new parse error reverts it in the same call. This
is a structural check, not a type check — it catches broken brackets and quotes, not wrong types.
For a real project checker, pass `--check '<command>'` (`tsc --noEmit`, `cargo check`, `go vet`):
it runs once per batch and reverts if the command newly fails. See [`/docs/edit/`](/docs/edit/).

## How it reaches an agent

The fix people try first is a rule in CLAUDE.md telling the agent to batch its reads. That doesn't
hold: prose rules get roughly 55% compliance in practice. `lets hooks install claude-code` (or
`codex`) wires the tool in at three points instead of asking nicely:

- A `SessionStart` paragraph explains `lets` and its verbs at the start of every session,
  including after compaction, since injected context doesn't survive compaction the way a system
  prompt does.
- A `SubagentStart` hook delivers the same paragraph to subagents, which a system-prompt append
  never reaches.
- A `PreToolUse` hook (`lets hook classify`) inspects each shell command before it runs. A bare
  `cat`, `sed -n` or `grep` of a repo file is blocked with the exact `lets` command to run
  instead:

  ```console
  agent runs:  sed -n '3,9p' src/usage.ts
  hook says:   lets show reads several files and ranges in one call.
               run: lets show src/usage.ts:3-9
  ```

  Output piped into another program, a heredoc sent to a program's stdin, and files outside the
  repo all pass through untouched. If `lets` is missing or crashes, the hook allows the original
  command — it can never wedge an agent's turn. Full detail: [`/docs/hooks/`](/docs/hooks/).

## Measured

A large-repo trial — five tasks sent as five turns of one Claude Code session, on a private
17.7k-file Go monorepo, checked against expected values derived from each task — compared a
stock session (`none`) against one with `lets hooks install claude-code` (`lean`), no other
change:

| model | n, lean vs none | tool calls | wall time | checks passed | cost |
|---|---|---|---|---|---|
| Sonnet 5 | 9 vs 6 | −16% (64.6 vs 76.5) | −11% (403 s vs 451 s) | 99.1% vs 97.8% (339/342 vs 223/228) | −0.5% ($1.349 vs $1.356), cost-neutral |
| Opus 5.5 | 6 vs 6 | −16% | −12% | 100% both arms | +8% mean ($1.003 vs $0.929; +4% median) |

Caveat: small samples (3–9 sessions per arm), so a smaller difference than this would need many
more sessions to resolve with confidence.

Local latency, p50 from `just bench-gate`'s wall-clock target against the generated fixture
corpus: `guide` 1.1 ms, `hook classify` 1.2 ms (1.34× `bash -n`), `show` on 200 lines 1.2 ms,
`find` on 2,000 files 8.8 ms (1.34× `rg`), `edit` plus a syntax check 2.7 ms, a 10-file batch edit
24 ms, `transform` on a YAML file 3.8 ms. Measured once on the same private monorepo, outside the
bench-gate harness: `show` at 2–4 ms, `find` at roughly 110 ms across the whole tree, and a
symbol lookup plus an edit on its largest file (17.9k lines) at 116 ms and 183 ms.

## Install

```console
$ curl -fsSL https://raw.githubusercontent.com/mayberuk/lets/main/install.sh | sh
$ lets hooks install claude-code
```

Linux and macOS. MIT or Apache-2.0. Version 0.0.1. Repo:
[github.com/mayberuk/lets](https://github.com/mayberuk/lets).

---

This page covers the product summary, before/after pairs, hooks and the measured numbers. The
full API — every verb, every flag, every exit code — is at [`/llms-full.txt`](/llms-full.txt), or
browse it verb by verb starting from [`/docs/show.md`](/docs/show.md). Nothing here was cut; a
markdown version of this exact page is what you're reading.
