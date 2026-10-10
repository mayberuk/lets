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
tool. See [target grammar](/docs/targets/) (`path`, `path:40`, `path:40-80`,
`path@'regex'`, `path#name`).

## Flags

| Flag | Meaning | Default |
|---|---|---|
| `--window <N>` | show the first N lines per whole-file target; show all when total ≤ N + floor(N/2) | 100 |
| `--all` | disable the window; print a cost line before the content | off |
| `-A <N>` | lines of context after a `:line` or `@'regex'` target | — |
| `-B <N>` | lines of context before | — |
| `-C <N>` | lines of context on both sides | — |
| `--no-numbers` | omit line numbers, for content piped onward | off |
| `--no-header` | omit each target's header line, and the footer when nothing was left out | off |
| `--outline` | list each definition's line range and first line instead of the content; whole-file targets only | off |
| `--head <N>` | print the first N output lines; name any cut in a footer, preserving the original footer if removed; text only | unset |
| `--json` | one JSON object on stdout | off |
| `--jsonl` | one JSON object per target, plus a trailing stats/omitted record | off |
| `--budget <N>` | shape the whole answer to ~N tokens, trimming the largest target first | unset |
| `--max-bytes <N>` | refuse (exit 4) if content exceeds N bytes and no `--budget` given | 65536 |
| `--max-file-bytes <N>` | files larger than this are refused | 8388608 |
| `--no-ignore` | do not honor `.gitignore` (only matters when a target resolves through a directory scan) | off |
| `--allow-outside` | applies to writes; `show` reads outside the working tree without it | off |
| `--no-check` | no effect on `show` (shared flag; `show` never runs a checker) | off |
| `-q, --quiet` | (shared flag; `show` already prints only the footer plus content) | off |

## Output shape

```
── <target>  (<start>-<end> of <total>[ · window W · :x-y not shown][ · via R])[ · crlf][ · non-UTF-8 lines …]
 <n>	<line text>
...
── showed <N> targets · <lines> lines[ · <target>:x-y not shown][ · <cost>]
```

- A read carries no `sha:`. `edit` prints the file's hash after every edit, for `edit --if sha:…`.
- `--no-header` drops every header line and the summary. A read that left nothing out then prints
  only its lines, so `lets show f:2-4 --no-header --no-numbers` prints what `sed -n '2,4p' f`
  prints. A window cut, a skip or a lossy line still gets its footer line.
- An empty file is a valid target, not a missing one: it exits 0 with a zero-line block.
- Invalid UTF-8 bytes are replaced lossily for display; the header names which lines held them
  (`· non-UTF-8 lines 3, 7`, at most five numbers then `(+N more)`).
- A file whose dominant line ending is CRLF gets `· crlf` in its header, since the rendered lines
  otherwise hide `\r` entirely.
- A line over 1,000 bytes is cut at a UTF-8 boundary and marked with `…`; the footer names how
  many lines this touched (`N long lines cut`).
- `--json`/`--jsonl` bodies carry `omitted` (the machine-readable footer) and `stats` (`lines`,
  `bytes`, `tokens_est`) alongside the target-specific fields. See [--json and --jsonl](/docs/json/).

## Outline

`--outline` lists the definitions the file's grammar captures (the same ones a `#name` target can
resolve), one line each, in source order:

```
── <target>
<line>[-<end_line>]	<first line of the definition, indentation trimmed>
...
── showed <N> targets · <D> definitions[ · <omission>]
```

- The first line is the signature as written, cut at 120 characters with `…`; the footer counts
  the cut lines. A signature that wraps shows its first line only.
- The list stops before the first entry that would take it over `--max-bytes`, and the footer
  names how many were left out (`output over --max-bytes N: M lines not shown`).
- A file with no bundled grammar exits 1 with `no_grammar`. A `:line`, `:a-b`, `#symbol` or
  `@'regex'` target refuses the whole call with `usage`, since an outline reads the whole file.

## Exit codes

| Exit | Slug | Means |
|---|---|---|
| 0 | — | shown |
| 1 | `not_found` | a target matched nothing (other targets in the same call are still shown) |
| 1 | `no_grammar` | `--outline` on a file with no bundled grammar |
| 2 | `ambiguous` | a `#name` target matched more than one symbol; every candidate listed |
| 4 | `over_budget` | content exceeded `--max-bytes` and no `--budget` was given |
| 7 | `unsupported_file` | binary, hardlinked, or a directory given as a target |
| 64 | `usage` | malformed command line |

## Examples

Read several files in one call, numbered:

```console
$ lets show src/usage.ts src/config.ts
── src/usage.ts  (1-13 of 13)
 1   import { usageCap } from './config'
 2
 3   export function usage(id: string) {
 4     const now = Date.now()
 5     const cap = 10
 ...
13   }
── src/config.ts  (1-2 of 2)
 1   export const usageCap = 10
 2   export const retries = 3
── showed 2 targets · 15 lines
```

Read exactly one function, found by parsing the file, instead of guessing a line range:

```console
$ lets show src/usage.ts#usage
── src/usage.ts#usage  (3-9 of 13 · via tree-sitter)
3   export function usage(id: string) {
4     const now = Date.now()
5     const cap = 10
6     if (!id) return
7     if (count(id) > cap) return
8     return total(id, now)
9   }
── showed 1 target · 7 lines
```

Find a line by regex and read context around it in the same call:

```console
$ lets show "src/usage.ts@'const cap'" -A 2
── src/usage.ts@'const cap'  (5-7 of 13)
5     const cap = 20
6     if (!id) return
7     if (count(id) > cap) return
── showed 1 target · 3 lines
```

A bare file shows the first 100 lines, with files of up to 150 lines shown whole to absorb a small
remainder. `--window N` scales this threshold to N + floor(N/2). Pass a range, `#symbol` or `--all`
when you need more. A larger file is truncated and the footer names its target and omitted range:

```console
$ lets show big.ts
── big.ts  (1-100 of 212 · window 100 · :101-212 not shown)
...
── showed 1 target · 100 lines · big.ts:101-212 not shown
```

Two missing targets alongside one that resolved — the call still exits 1, but everything found
is still printed:

```console
$ lets show nope1.ts small.md:3 nope2.ts
? 1
── small.md:3  (3-3 of 15)
3 	One grammar every verb speaks.
── showed 1 target · 1 line · nope1.ts failed (not_found) · nope2.ts failed (not_found)
nope1.ts: No such file or directory (os error 2)
nope2.ts: No such file or directory (os error 2)
ERROR_CODE=not_found
```

An empty file is a normal result, not an error:

```console
$ lets show empty.md
── empty.md
── showed 1 target · 0 lines
```

List a file's definitions without reading its bodies:

```console
$ lets show store.go --outline
── store.go
44-46	func Open(path string) (*Store, error) {
213-215	func (s *Store) Open(ctx context.Context) error {
── showed 1 target · 2 definitions
```
