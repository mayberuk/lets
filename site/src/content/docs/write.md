---
title: lets write
description: Create a file from stdin, checked on creation, refusing a silent overwrite or a silent empty file.
order: 5
group: Verbs
commands: [write]
---

# `lets write`

Creates a file from stdin. The hook-visible replacement for `cat > file <<'EOF'`.

```
lets write [OPTIONS] <PATH>
```

## What it does

Writes stdin to a new file, reports lines and bytes written and the new `sha:`, and runs the same
layer-1 check `edit` runs on creation. It refuses to silently overwrite an existing file, and
refuses to silently create an empty file — both need an explicit flag, because a heredoc typo
that would clobber a file with `cat >` should not clobber one here either.

## Flags

| Flag | Meaning | Default |
|---|---|---|
| `--force` | overwrite an existing file | off |
| `--empty` | allow writing a file from empty stdin | off |
| `--json`, `--jsonl` | structured output | off |
| `--budget <N>` | shape the answer to ~N tokens | unset |
| `--max-bytes <N>` | content budget | 65536 |
| `--max-file-bytes <N>` | refuse content over this size | 8388608 |
| `--no-ignore` | shared flag; not meaningful for a single new path | off |
| `--allow-outside` | permit a path outside the working tree | off |
| `--no-check` | skip the layer-1 check on the new file | off |
| `-q, --quiet` | shared flag | off |

## Output shape

```
── <path> · created · <N> lines · <B> bytes · sha:<hash>
── check: <result>
```

An overwrite (`--force`) prints `overwritten` and both the old and new `sha:`, joined by `→`.

## Exit codes

| Exit | Slug | Means |
|---|---|---|
| 0 | — | created (or overwritten, with `--force`) |
| 1 | `exists` | the file already exists; pass `--force` |
| 1 | `empty_input` | stdin was empty; pass `--empty` |
| 6 | `outside_tree` | the path is outside the working tree; pass `--allow-outside` |
| 7 | `io_error` | any other I/O failure |

## Examples

Create a file, checked on creation:

```console
$ lets write scripts/new-check.sh <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
EOF
── scripts/new-check.sh · created · 2 lines · 38 bytes · sha:[..]
── check: structure ok
```

Refuses to clobber an existing file:

```console
$ lets write scripts/new-check.sh <<'EOF'
#!/usr/bin/env bash
echo hi
EOF
? 1
scripts/new-check.sh exists (2 lines, sha:[..]) · pass --force to overwrite
ERROR_CODE=exists
```

`--force` overwrites, and the header shows both hashes:

```console
$ lets write scripts/new-check.sh --force <<'EOF'
#!/usr/bin/env bash
echo hi
EOF
── scripts/new-check.sh · overwritten · 2 lines · 28 bytes · sha:954b9f40bbee→b53701120a2e
── check: structure ok
```

An empty file needs `--empty`, or it's refused as likely a mistake:

```console
$ lets write f.txt
? 1
f.txt: refuses empty stdin without --empty
ERROR_CODE=empty_input

$ lets write f.txt --empty
── f.txt · created · 0 lines · 0 bytes · sha:[..]
── check: skipped (no grammar for .txt)
```
