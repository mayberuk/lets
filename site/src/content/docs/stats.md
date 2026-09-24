---
title: lets stats
description: Aggregate session-transcript counts for the hook and the lets calls it drove — what each field counts, --dir, --since and --json.
order: 13
group: Setup
commands: [stats]
---

# `lets stats`

```
lets stats [OPTIONS]
```

Scans Claude Code session transcripts (`.jsonl` files) and reports how much `lets` and its hook
were used — no transcript text, path or content leaves the scan; only counts and verb names reach
the report.

## Flags

| Flag | Meaning | Default |
|---|---|---|
| `--dir <DIR>` | directory to scan for `.jsonl` transcripts | `~/.claude/projects` |
| `--since <N d\|h>` | only scan files modified within this window, e.g. `7d` or `12h` — days or hours only | unset (all files) |
| `--json` / `--jsonl` | print the raw `StatsReport` object instead of a text table | off |
| `--budget`, `--max-bytes`, `--max-file-bytes`, `--no-ignore`, `--allow-outside`, `--no-check`, `-q/--quiet` | shared flags; `stats` has no files to check or bound, so most have no effect | see `lets stats --help` |

## What it counts

| Field | Means |
|---|---|
| `sessions` | number of transcript files scanned |
| `bash_calls` | total `Bash` tool calls seen |
| `lets_calls` | `Bash` calls whose command was a `lets` invocation, by verb |
| `hook_blocks` | `Bash` tool results that are a hook denial (text starting `run: `) |
| `blocks_followed` | of those blocks, how many were followed by a `Bash` call matching the suggested verb |
| `calls_saved` | sum of `targets - 1` over every multi-target `lets` call — `show a.ts b.ts c.ts` saves 2 |
| `read_calls` / `read_bytes` | `Read` tool calls and the bytes they returned |
| `skipped` | malformed or non-UTF-8 lines, shown only when nonzero (`malformed_lines`, `non_utf8_lines`, `unreadable_files`, `walk_errors`) |

## Output shape

Text table:

```console
$ lets stats --dir .
sessions          1
bash calls        5
lets calls        3
  edit            2
  show            1
hook blocks       1
blocks followed   1
calls saved       1
read calls        1
read bytes       10
```

`--json` prints the `StatsReport` struct directly, not wrapped in the `{"…":…,
"omitted":[],"stats":{...}}` envelope other verbs use — there is no per-call cost to report, since
`stats` is itself the report:

```console
$ lets stats --dir . --json
{"sessions":1,"bash_calls":1,"lets_calls":{"find":1},"hook_blocks":0,"blocks_followed":0,"calls_saved":0,"read_calls":1,"read_bytes":3}
```

With skips:

```console
$ lets stats --dir .
sessions         1
bash calls       1
lets calls       1
  show           1
hook blocks      0
blocks followed  0
calls saved      0
read calls       0
read bytes       0
── skipped: 1 non-UTF-8 line

$ lets stats --dir . --json
{"sessions":1,"bash_calls":1,"lets_calls":{"show":1},"hook_blocks":0,"blocks_followed":0,"calls_saved":0,"read_calls":0,"read_bytes":0,"skipped":{"non_utf8_lines":1}}
```

## `LETS_NO_STATS` and `LETS_TOKEN_RATIO`

These two env vars govern a different, unrelated field: the per-call `stats.tokens_est` estimate
that `show`, `find`, `edit`, `transform` and `write` carry in their own output (bytes ÷ 4, or ÷
`LETS_TOKEN_RATIO` if set; `null` under `LETS_NO_STATS=1`). They do not affect what `lets stats`
itself counts — the name is a coincidence, not a shared mechanism. See [--json and --jsonl](/docs/json/)
for `stats.tokens_est`.
