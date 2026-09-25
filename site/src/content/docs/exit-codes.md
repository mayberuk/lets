---
title: Exit codes
description: The full table of exit codes and slugs every verb can return, and the rule for branching on the slug instead of the message text.
order: 21
group: Reference
commands: []
---

# Exit codes

Every non-zero exit writes one diagnostic line per failure to stderr, and the last line is always
`ERROR_CODE=<slug>`. Branch on the exit code (or the slug), not on the message text — the message
can change wording; the slug does not. Content and a non-zero exit are not exclusive: a partly
successful `show` or a batch that landed some of its edits prints what it did, then still exits
non-zero, so stdout stays worth reading even on failure.

| Exit | Slug | Means | Seen from |
|---|---|---|---|
| 0 | — | done | all verbs |
| 1 | `not_found` | a target, `--old`, or a pattern matched nothing; the nearest candidate is shown where one exists | `show`, `edit`, `transform`, `find` |
| 1 | `over_cap` | `find` had more than 50 hits (or `--cap`'s value) and printed the top-files map instead | `find` |
| 1 | `invalid_pattern` | the regex does not parse | `find` |
| 1 | `exists` | `write` would overwrite an existing file; pass `--force` | `write` |
| 1 | `empty_input` | `write` got empty stdin; pass `--empty` | `write` |
| 1 | `empty_file` | `edit`'s `--old` matched nothing because the target file is empty; write it first with `lets write --force` | `edit` |
| 1 | `mixed_endings` | `--old` spans more than one line and the file mixes CRLF and LF; match one line at a time, or type `\r\n` literally | `edit` |
| 1 | `no_grammar` | a `#symbol` target on a file whose extension has no bundled grammar and no plaintext match; the message names the extension and a fallback target form | `show`, `edit`, `transform` |
| 1 | `update_available` | `update --check` found a newer release; nothing was installed | `update` |
| 1 | `path_conflict` | `hooks install` or `update`: a different `lets` comes first on `PATH`; named in the message, nothing written | `hooks install`, `update` |
| 1 | `not_on_path` | `hooks install`: no `lets` reachable on `PATH` at all | `hooks install` |
| 1 | `no_repository` | `update`: this build names no release repository | `update` |
| 1 | `guessed_span` | `edit --insert-after` targeted a plaintext-heuristic symbol whose span end is a guess, not a parsed boundary; use `--insert-before` or anchor on the last line | `edit` |
| 2 | `ambiguous` | the target, `--old`, or an attribute selector matched more than once; every candidate listed as `path:line`, at most 20 with `(+N more)` | `show`, `edit`, `transform` |
| 2 | `expect_refused` | `--expect` can't confirm a multi-line range, or its content didn't match what's on disk | `edit` |
| 3 | `check_failed` | the guardrail failed after the edit; the file was reverted and is unchanged | `edit`, `transform` |
| 4 | `over_budget` | content exceeded `--max-bytes` and no `--budget` was given | `show`, `find` |
| 5 | `changed` | the file changed since the `--if sha:…` it was given | `edit`, `transform` |
| 6 | `outside_tree` | a target or write is outside the working tree; pass `--allow-outside` | `show`, `find`, `edit`, `transform`, `write` |
| 7 | `unsupported_file` | binary, hardlinked, non-UTF-8 in the matched region, over `--max-file-bytes`, a directory given as a target, or (for `transform`) not a structured format, or a key that can't be changed in place | `show`, `edit`, `transform` |
| 7 | `locked` | another `lets` process holds the file's lock past the 2-second retry | `edit`, `transform`, `write` |
| 7 | `read_only` | the file lacks the owner-write bit; `chmod u+w` is the fix named in the message | `edit`, `transform`, `write` |
| 7 | `io_error` | any other I/O failure; a stdout write that fails because the reader closed the pipe is exit 0, not this | `edit`, `transform`, `write`, `hooks` |
| 7 | `update_failed` | `update`: no release for this platform, the download failed, or the installer exited non-zero | `update` |
| 8 | `partial_batch` | some files of a batch landed; the footer names which | `edit`, `transform` |
| 64 | `usage` | the command line, or a batch's `--from -` input, is malformed — an unknown JSONL key, a short `--if`, an unrecognized `--check` preset, `find -v`, two conflicting stdin flags | all verbs |

A missing file reaches exit 1 (`not_found`) whichever way it failed, so a caller can branch on the
code alone without parsing the message. Exit 64 is reserved for a malformed request — never exit
2, which is reserved for an ambiguity or a content refusal about what's already on disk.

`update --check` (report only, no `not_on_path`/`path_conflict` refusal applies since nothing is
written) and `hooks uninstall` (exit 7 `io_error` if the settings file can't be read or written,
leaving it untouched) are not in this table's "seen from" column separately; they share the exits
above their sibling command uses.
