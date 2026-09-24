---
title: lets show
description: Read files, line ranges, symbols and regex-anchored context — several targets per call, with a bounded window and an honest footer.
order: 1
group: Verbs
commands: [show]
---

# `lets show`

Reads files, line ranges, anchors and symbols. Several targets in one call, so an agent that
needs two files or two functions gets both in one round trip instead of two.

```
lets show [OPTIONS] <TARGETS>...
```

## What it does

Renders each target as a numbered block with a header naming the range shown out of the file's
total, and a footer summarizing the whole call. Replaces `cat`, `head`, `sed -n` and the Read
tool. See `/docs/targets/` for the target grammar (`path`, `path:40`, `path:40-80`,
`path@'regex'`, `path#name`).

## Flags

| Flag | Meaning | Default |
|---|---|---|
| `--window <N>` | cap lines shown per whole-file target | 200 |
| `--all` | disable the window; print a cost line before the content | off |
| `-A <N>` | lines of context after a `:line` or `@'regex'` target | — |
| `-B <N>` | lines of context before | — |
| `-C <N>` | lines of context on both sides | — |
| `--no-numbers` | omit line numbers, for content piped onward | off |
| `--json` | one JSON object on stdout | off |
| `--jsonl` | one JSON object per target, plus a trailing stats/omitted record | off |
| `--budget <N>` | shape the whole answer to ~N tokens, trimming the largest target first | unset |
| `--max-bytes <N>` | refuse (exit 4) if content exceeds N bytes and no `--budget` given | 65536 |
| `--max-file-bytes <N>` | files larger than this are refused | 8388608 |
| `--no-ignore` | do not honor `.gitignore` (only matters when a target resolves through a directory scan) | off |
| `--allow-outside` | permit a target outside the working tree | off |
| `--no-check` | no effect on `show` (shared flag; `show` never runs a checker) | off |
| `-q, --quiet` | (shared flag; `show` already prints only the footer plus content) | off |

## Output shape

```
── <target>  (<start>-<end> of <total>[ · window W · :x-y not shown][ · via R])[ · crlf][ · non-UTF-8 lines …] · sha:xxxxxxxxxxxx
 <n>	<line text>
...
── showed <N> targets · <lines> lines[ · <cost>]
```

- `sha:` is the first twelve hex of the file's blake3 hash. Pass it back to `edit --if sha:…` to
  refuse the edit if the file changed since this `show`.
- An empty file is a valid target, not a missing one: it exits 0 with a zero-line block.
- Invalid UTF-8 bytes are replaced lossily for display; the header names which lines held them
  (`· non-UTF-8 lines 3, 7`, at most five numbers then `(+N more)`).
- A file whose dominant line ending is CRLF gets `· crlf` in its header, since the rendered lines
  otherwise hide `\r` entirely.
- A line over 1,000 bytes is cut at a UTF-8 boundary and marked with `…`; the footer names how
  many lines this touched (`N long lines cut`).
- `--json`/`--jsonl` bodies carry `omitted` (the machine-readable footer) and `stats` (`lines`,
  `bytes`, `tokens_est`) alongside the target-specific fields. See `/docs/json/`.

## Exit codes

| Exit | Slug | Means |
|---|---|---|
| 0 | — | shown |
| 1 | `not_found` | a target matched nothing (other targets in the same call are still shown) |
| 2 | `ambiguous` | a `#name` target matched more than one symbol; every candidate listed |
| 4 | `over_budget` | content exceeded `--max-bytes` and no `--budget` was given |
| 6 | `outside_tree` | the target is outside the working tree; pass `--allow-outside` |
| 7 | `unsupported_file` | binary, hardlinked, or a directory given as a target |
| 64 | `usage` | malformed command line |

## Examples

Read several files in one call, numbered, with a size estimate:

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

Read exactly one function, found by parsing the file, instead of guessing a line range:

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

Find a line by regex and read context around it in the same call:

```console
$ lets show "src/usage.ts@'const cap'" -A 2
── src/usage.ts@'const cap'  (5-7 of 13) · sha:93b5daea8ace
5     const cap = 20
6     if (!id) return
7     if (count(id) > cap) return
── showed 1 target · 3 lines · ~16 tokens
```

A file over the default window is truncated and the footer names what was left out:

```console
$ lets show big.ts
── big.ts  (1-200 of 212 · window 200 · :201-212 not shown) · sha:e37232c09d01
...
── showed 1 target · 200 lines · :201-212 not shown
```

Two missing targets alongside one that resolved — the call still exits 1, but everything found
is still printed:

```console
$ lets show nope1.ts small.md:3 nope2.ts
? 1
── small.md:3  (3-3 of 15) · sha:[..]
3 	One grammar every verb speaks.
── showed 1 target · 1 line · nope1.ts failed (not_found) · nope2.ts failed (not_found)
nope1.ts: No such file or directory (os error 2)
nope2.ts: No such file or directory (os error 2)
ERROR_CODE=not_found
```

An empty file is a normal result, not an error:

```console
$ lets show empty.md
── empty.md · sha:af1349b9f5f9
── showed 1 target · 0 lines
```
