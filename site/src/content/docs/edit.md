---
title: lets edit
description: Content-addressed, exactly-once text replacement, with the post-state and a syntax check returned in the same call.
order: 3
group: Verbs
commands: [edit]
---

# `lets edit`

Content-addressed, exactly-once replacement, with the post-state returned in the same call.
Replaces the Edit tool, `sed -i`, and one-off Python rewrite scripts.

```
lets edit [OPTIONS] [TARGET] [MORE_TARGETS]...
```

## What it does

`--old` is matched as an exact substring that must occur exactly once in the target (or the
target's line range); `--new` replaces it. The file is parsed before and after the edit and a
guardrail reverts the write if the edit introduced a new parse error. The output shows the
changed region with 2 lines of context and the check result, so the agent does not need to
re-read the file to confirm the edit landed. See [target forms](/docs/targets/).

## Flags

| Flag | Meaning | Default |
|---|---|---|
| `--old <S>` | exact text to find; `-` reads all of stdin as the content instead of matching it literally | — |
| `--new <S>` | replacement text; `-` reads stdin (shared with `--old -` is refused, since only one flag can drain stdin) | — |
| `--all` | replace every occurrence, not just the one match | off |
| `--expect <S>` | with a `:a-b` target, replace only if the named single line is exactly `S` (whitespace-trimmed) | — |
| `--expect-all` | confirm content for a multi-line range read from stdin | off |
| `--insert-after <target>` | insert `--new` after an anchor (`@'regex'` or `#symbol`); no line-number insert | — |
| `--insert-before <target>` | insert `--new` before an anchor | — |
| `--from -` | a fence-delimited or JSONL batch on stdin, one edit per entry; `-` is the only accepted value | — |
| `--if sha:<12+ hex>` | refuse (exit 5) if the file changed since the `show` that produced this hash | — |
| `--normalize` | also try a match with smart quotes, en/em dashes and non-breaking spaces folded to their ASCII forms | off |
| `--literal-newlines` | write `--new`'s line endings exactly as typed, instead of matching them to the target file's dominant line-ending convention | off |
| `--check <cmd\|@preset>` | layer-2 checker: a real command, or one of `@auto`, `@cargo`, `@go`, `@tsc`, `@py` | — |
| `--check-timeout <secs>` | how long the layer-2 checker may run before its result is `inconclusive (timed out)` | 60 |
| `--no-check` | skip both the built-in structural/format check and any `--check` | off |
| `--if`, `--json`, `--jsonl`, `--budget`, `--max-bytes`, `--max-file-bytes`, `--no-ignore`, `--allow-outside`, `-q/--quiet` | shared flags — see [--json and --jsonl](/docs/json/) | see `lets edit --help` |

Several file targets in one call (`lets edit f1.ts f2.ts --old a --new b`), and a `--from -`
batch, are both **validation-atomic**: every file is matched and pre-checked before any file is
written. If one target fails to match, nothing is written to any file, and the failure names
which target.

## The `--from -` batch format

Fence-delimited, no escaping needed for code bodies:

```
lets edit --from - <<'LETS'
@@ a.ts
<<<<<<< old
cap = 10
======= new
cap = 20
>>>>>>>
@@ b.ts insert-after @'^import'
======= new
import x from 'y'
>>>>>>>
LETS
```

A JSONL form is also accepted, one edit object per line: `{"file":"a.ts","old":"…","new":"…"}` or
`{"file":"b.ts","insert_after":"@'^import'","new":"import x from 'y'\n"}`. Every line is validated
against the edit shape before anything is written; an unknown key, a missing or non-string `new`,
or a line combining `old` with `insert_after`, fails the whole batch at exit 64 with nothing
written.

## Checks (the guardrail)

**Layer 1, default, automatic, per file.** For JSON, YAML, TOML and markdown frontmatter, this is
a real parser: `check: json ok` means the file is valid JSON. For the tree-sitter languages, it
is a **structural check** — the file is parsed before and after, and any new parse-error node
reverts the edit. This catches broken brackets and quotes, not wrong types. A file with no
bundled grammar is named as skipped, never guessed at: `check: skipped (no grammar for .vue)`.

**Layer 2, opt-in, per batch: `--check '<cmd>'`.** A real checker (`tsc --noEmit`, `go vet`,
`cargo check`) runs once before any write and once after the whole batch lands. Verdict is by
exit code only: `0 → 0` is `check: <cmd> ok`; `0 → non-zero` reverts the whole batch, exit 3;
`non-zero → non-zero` is `inconclusive (failed before and after)` — kept, not called verified.
`{}` in the command substitutes the list of edited files.

**Presets**, so `--check` needs no config file: `@auto` walks up from the edited files to the
first manifest it recognizes; `@cargo`, `@go`, `@tsc`, `@py` force one regardless of what manifest
is present.

| Manifest | Preset | Command |
|---|---|---|
| `Cargo.toml` | `@cargo` | `cargo check --workspace --quiet --all-targets` |
| `go.mod` | `@go` | `go build ./...` |
| `tsconfig.json` | `@tsc` | `npm run -s typecheck` (if `scripts.typecheck` exists) else `npx --no-install tsc --noEmit -p <dir>` |
| `pyproject.toml` or `setup.py` | `@py` | `python3 -m py_compile {}` |

`@auto` finding no manifest is not an error: `check: skipped (@auto found no manifest)`.

## Output shape

```
── <path> · <N> replacement(s) · line <n> · exact
<n>	<context line>
<n>+	<inserted line>
<n>~	<replaced line>
── check: <result> · sha:<before>→<after>[ · <cost>]
```

`+` marks an inserted line, `~` a replaced line, in the marker column right after the line
number. A reverted edit prints `REVERTED` after the line/replacement count and shows the rejected
line with `← parse error` (or `← invalid json`/`yaml`/`toml`), then `check: failed → reverted ·
file unchanged`. `--quiet` returns only the footer line.

## Exit codes

| Exit | Slug | Means |
|---|---|---|
| 0 | — | applied |
| 1 | `not_found` | `--old` (or the target) matched nothing; nearest candidate shown |
| 2 | `ambiguous` | `--old` matched more than once; every candidate listed as `path:line` |
| 2 | `expect_refused` | `--expect` can't confirm a multi-line range, or its content didn't match |
| 3 | `check_failed` | the guardrail reverted the edit; the file is unchanged |
| 5 | `changed` | the file changed since the `--if sha:…` given |
| 6 | `outside_tree` | the target is outside the working tree; pass `--allow-outside` |
| 7 | `unsupported_file` / `locked` / `read_only` / `io_error` | binary, non-UTF-8 region, over size; lock held by another `lets`; missing owner-write bit; other I/O failure |
| 8 | `partial_batch` | a batch partly landed; the footer names which files |
| 64 | `usage` | malformed command line or batch input |

## Examples

Edit and get the proof back in the same call — the `~` marks the changed line, and the check runs
before the output is printed, not after, in a separate call:

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

Ambiguous match — every candidate listed, nothing written:

```console
$ lets edit usage.ts --old 'return' --new 'return undefined'
? 2
usage.ts is ambiguous (3 candidates)
  usage.ts:4	  if (!id) return
  usage.ts:7	  if (now > cap) return
  usage.ts:9	  return total
ERROR_CODE=ambiguous
```

A broken edit is parsed, caught, and reverted in the same call:

```console
$ lets edit main.go --old 'func' --new 'fun'
? 3
── main.go · 1 replacement · line 2 · REVERTED
1 	package main
2~	fun usage(id string) int {          ← parse error
3 		cap := 10
4 		return cap + len(id)
── check: failed → reverted · file unchanged · sha:db91a17c0af6
structure check failed for main.go: failed
ERROR_CODE=check_failed
```

A project-level checker (`@cargo`) reverts a rename that a structural check alone would miss,
because it broke a caller in a sibling crate:

```console
$ lets edit a/src/lib.rs --old 'pub fn double' --new 'pub fn twice' --check @cargo
? 3
command check failed for a/src/lib.rs: `cargo check --workspace --quiet --all-targets` passed before the batch and failed after it[..] · 1 file reverted
ERROR_CODE=check_failed
```

Several edits across files in one call, validated before any file is written:

```console
$ lets edit --from - <<'LETS'
@@ src/config.ts
<<<<<<< old
export const usageCap = 10
======= new
export const usageLimit = 10
>>>>>>>
@@ src/usage.ts
<<<<<<< old
import { usageCap } from './config'
======= new
import { usageLimit } from './config'
>>>>>>>
LETS
── src/config.ts · 1 replacement · line 1 · exact
1~  export const usageLimit = 10
2   export const retries = 3
── src/usage.ts · 1 replacement · line 1 · exact
1~  import { usageLimit } from './config'
2
3   export function usage(id: string) {
── 2 files · 2 edits · all applied · checks: structure ok ×2 · ~60 tokens
```

A stale `--if` hash is refused rather than silently overwritten:

```console
$ lets edit usage.ts --old 'const cap = 10' --new 'const cap = 20' --if sha:000000000000
? 5
usage.ts changed since sha:000000000000 (now sha:c5525cc20b61)
ERROR_CODE=changed
```

Insert text anchored on a symbol, not a guessed line number:

```console
$ lets edit usage.ts --insert-before '#usage' --new '/** Returns the running total for id. */'
── usage.ts · inserted 1 line before #usage (line 3)
1 	import { usageCap } from './config'
2 	import { clock } from './clock'
3+	/** Returns the running total for id. */
4 	export function usage(id: string) {
5 	  if (!id) return
── check: structure ok · sha:c5525cc20b61→45b5b2745608
```
