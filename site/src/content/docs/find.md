---
title: lets find
description: Search files or directories and print hits as path:line, capped at 50 hits, reporting instead of flooding.
order: 2
group: Verbs
commands: [find]
---

# `lets find`

Searches files or directories and prints hits as `path:line`, grouped by file. `locate` is an
alias for the same command.

```
lets find [OPTIONS] <PATTERN> [PATHS]...
```

## What it does

Regex search by default (Rust `regex` syntax), walked with the same rules as `ripgrep`:
`.gitignore`, `.ignore`, the global gitignore and `.git/info/exclude` are honored, and hidden
files are skipped unless asked for. Capped at 50 hits by default — over the cap, no hit lines are
printed; instead a bounded map of the files holding the most hits, so the next call can narrow to
one of them. Replaces `grep -rn` and `rg` run directly in the shell.

Case is smart by default, as in `ripgrep --smart-case`: insensitive when the pattern has no
uppercase letter, exact otherwise. `-i` forces insensitive; `-s` forces exact case. A hit that
matched only because case was ignored is named in the footer.

## Flags

| Flag | Meaning | Default |
|---|---|---|
| `-F, --fixed-string` | match the pattern literally, not as a regex | off |
| `-i, --ignore-case` | force case-insensitive | off |
| `-s, --case-sensitive` | force exact case, overriding smart case | off |
| `-w, --word` | match whole words only | off |
| `--cap <N>` | raise the hit cap (prints hits, not the over-cap map) | 50 |
| `-l, --files` | list matching files only, one bare path per line, no cap | off |
| `-c, --count` | print the footer first, then one `<count>  <path>` row per file | off |
| `-g, --glob <GLOB>` (alias `--include`) | narrow the walk to paths the glob matches; footer names it | — |
| `--exclude <GLOB>` | prune a path from the walk; exactly `-g '!GLOB'`, interleaved with `-g` in argv order | — |
| `--hidden` | include hidden files and directories | off |
| `-A <N>` | lines of context after a hit | — |
| `-B <N>` | lines of context before | — |
| `-C <N>` | lines of context on both sides | — |
| `--no-expand` | print hit lines only, never the enclosing symbol or the lines around a hit | off |
| `--no-ignore` | do not honor `.gitignore`/`.ignore`/global excludes | off |
| `--allow-outside` | permit a path outside the working tree | off |
| `--json` / `--jsonl` | structured output | off |
| `--budget <N>` | shape the answer to ~N tokens | unset |
| `--max-bytes <N>` | content budget | 65536 |
| `--max-file-bytes <N>` | files larger than this are skipped as unreadable | 8388608 |
| `-q, --quiet` | shared flag | off |

Grep-compatible no-ops, accepted so a command copied from `grep` runs unchanged: `-n`,
`--line-number`, `-r`, `-R`, `-E`, `-H` (find already behaves as if each were set). `-v,
--invert-match` has no equivalent — find only ever prints matching lines — so it is refused at
parse time, exit 64 `ERROR_CODE=usage`.

## Output shape

Under the cap:

```
── <path>
<n>:	<line with the match wrapped in «»>
<n>-	<context line>
── <H> hits in <F> files · searched <S> files[ · ignored …][ · skipped …][ · glob …]
```

Over the cap, no hit lines print. Instead:

```
<count>	<path>
...
── <H> hits in <F> files · over the 50-hit cap · narrow the pattern or the paths, or --files · top 10 files shown
```

- Matches are wrapped in `«»` so they survive a pipe.
- `--files` prints one bare path per line, no header, no line numbers, then the ordinary footer.
- `--count` prints the footer first, then `<count>  <path>` rows, count right-aligned to the
  widest.
- A path that does not exist is named in the footer alongside what the other paths produced:
  `· nope failed (not_found)`.
- A file with a NUL byte, or a UTF-16 file, is not text and is not searched; it is counted in
  `skipped N (binary a · too large b · unreadable c)`.
- A regex holding one of grep's BRE escapes (`\|`, `\(`, `\)`, `\{`, `\}`, `\+`, `\?`) that
  matches nothing is retried read grep-style, and the footer names the reading used:
  `· «A\|B» had no hits, read grep-style as «A|B»`.
- Smart case folded the search and one or more hits have no case-sensitive match of the pattern:
  `· 3 hits match only ignoring case (-s for exact case)`.

## Exit codes

| Exit | Slug | Means |
|---|---|---|
| 0 | — | hits found |
| 1 | `not_found` | no hits, or a searched path does not exist |
| 1 | `over_cap` | hit count exceeded the cap; the top-files map was printed instead |
| 1 | `invalid_pattern` | the regex does not parse |
| 4 | `over_budget` | content exceeded `--max-bytes` and no `--budget` was given |
| 6 | `outside_tree` | a path is outside the working tree; pass `--allow-outside` |
| 64 | `usage` | malformed flag, e.g. `-v` |

## Examples

A basic search, hits wrapped so they survive a pipe:

```console
$ lets find usageCap
── src/config.ts
1:  export const «usageCap» = 10
── src/usage.ts
1:  import { «usageCap» } from './config'
── 2 hits in 2 files · searched 3 files
```

Two patterns at once, with context:

```console
$ lets find 'Bottom line|Next' small.md
── small.md
 5:	## «Bottom line»
13:	## «Next»
── 2 hits in 1 file · searched 1 file
```

Over the cap: no hits printed, a map of where they are instead, exit 1:

```console
$ lets find needle many-hits.txt
? 1
64	many-hits.txt
── 64 hits in 1 file · searched 1 file · over the 50-hit cap · narrow the pattern or the paths, or --files · top 1 file shown
ERROR_CODE=over_cap
```

Narrowing the walk with a glob, and the control with no glob:

```console
$ lets find -g '*.ts' needle
── a.ts
1:	const «needle» = 1;
── 1 hit in 1 file · searched 1 file · glob *.ts

$ lets find needle
── a.ts
1:	const «needle» = 1;
── b.py
1:	«needle» = 1
── 2 hits in 2 files · searched 2 files
```

`--files` and `--count` shapes:

```console
$ lets find filler . --files
big.ts
store.go
── 2 files · searched 5 files · ignored 3 (gitignore 1 · hidden 2) · skipped 1 (binary 1)

$ lets find filler . --count
── 393 hits in 2 files · ignored 3 (gitignore 1 · hidden 2) · skipped 1 (binary 1)
185  big.ts
208  store.go
```

No hits still prints a footer and exits 1, rather than staying silent:

```console
$ lets find zzz_never_appears_zzz small.md
? 1
── 0 hits in 0 files · searched 1 file
no hits for «zzz_never_appears_zzz»
ERROR_CODE=not_found
```
