---
title: lets hooks and lets hook classify
description: How lets reaches an agent without asking it to remember a prose rule — installing the integration, and what the PreToolUse classifier blocks.
order: 11
group: Setup
commands: [hooks, "hooks install", "hooks uninstall", hook, "hook classify"]
---

# `lets hooks` and `lets hook classify`

How `lets` reaches an agent without asking it to remember a prose rule. Two commands: `lets
hooks install|uninstall <claude-code|codex>` wires the integration into the agent's own settings;
`lets hook classify` is the program those hooks call on every shell command.

```
lets hooks install <claude-code|codex>
lets hooks uninstall <claude-code|codex>
lets hook classify
```

## What `hooks install` does

**`claude-code`** merges four hooks into `~/.claude/settings.json`:

- A `SessionStart` hook (matcher `startup|resume|clear|compact|fork`) that prints a short
  paragraph explaining `lets` and its verbs at the start of every session — including after
  compaction, since injected context does not survive compaction the way a system prompt does.
- A `SubagentStart` hook that delivers the same paragraph as `additionalContext`, since
  `--append-system-prompt` does not reach a non-fork subagent.
- A `PreToolUse` hook on `Bash`, matcher covering every shell call, that pipes the command to
  `lets hook classify`. A bare `cat`, `head -n` or `sed -n` read, or a `grep`/`rg` search, of a
  repo file whose stdout goes to the tool result (not into a pipe or a subshell) is rewritten in
  place — no permission prompt — when one `lets show` or `lets find` call reproduces it exactly,
  alone or as a segment of a `&&`/`||`/`;` chain, with the rest of the command kept byte for byte.
  A bare `sed -i 's/OLD/NEW/g'` substitution against an in-tree file is blocked and replaced with
  `lets edit <path> --old '<OLD>' --new '<NEW>' --all`; a search whose exit status a later `&&`,
  `||`, `$?`/`PIPESTATUS`, `set -e` or `ERR` trap reads is rewritten too, with `-s` and `--cap-exit-0` added so
  its hits and exit code still match what `grep`/`rg` would have produced.
- A `PostToolUse` hook on `Edit|Write` that checks the file the tool just wrote and hands the
  result back as `additionalContext` (see below).

**`codex`** merges four entries into `hooks.json` (default `$CODEX_HOME/hooks.json`, or
`~/.codex/hooks.json`): the same `PreToolUse` classifier, `SessionStart` and `SubagentStart`
entries that print the file-work paragraph the same way Claude Code's do, and a `PostToolUse`
entry on `apply_patch` that runs the same file check on the first file a patch names. Codex
requires a human to trust a hook before it runs it, and trusts each entry separately: the install
report names the current trust status on every run — "installed and approved" only when all four
are trusted, otherwise "not yet approved", naming the entries still untrusted when some already
are — until you open Codex and choose "Trust all and continue" when prompted, press `t` in the
hooks browser, or pass `--dangerously-bypass-hook-trust` for one run.

Both installers only ever write into the agent's own settings; nothing modifies your shell
profile. Installing twice is a no-op — the settings file is byte-identical across a reinstall.
`hooks uninstall` removes exactly what `hooks install` added and leaves any of the user's own
hooks in the same file untouched.

## What `lets hook classify` does

Reads one `PreToolUse` JSON event (`session_id`, `cwd`, `hook_event_name`, `tool_name`,
`tool_input.command`) on stdin. For an allowed command it prints nothing and exits 0 — the
command runs unmodified. For a rewrite it prints one JSON line naming the `lets` replacement in
`hookSpecificOutput.updatedInput`: on Claude Code with no `permissionDecision`, so its own
permission flow still runs on the rewritten command; on Codex with `permissionDecision: "allow"`,
since Codex's own hook contract requires the field on every decision. For a blocked command it
prints one JSON line naming the `lets` replacement in `hookSpecificOutput.permissionDecision:
"deny"`.

`cat`, `head -n`, `sed -n` and `grep`/`rg` reads are rewritten to one `lets show` or `lets find`
call, alone or as a segment of a `&&`/`||`/`;` chain, with the rest of the command kept byte for
byte. A search whose exit status a later `&&` or `||`, a `$?` or `PIPESTATUS` expansion in any
form, `set -e` (or `shopt -o errexit`) or an `ERR` trap reads is rewritten too, with
`--cap-exit-0` added: `lets find` exits 1 over its hit cap and when every hit
lands in a file it skips, where `grep`/`rg` exit 0, so without the flag a chain that branches on
that status could take a different branch after the rewrite. A read `lets show` would not
reproduce exactly (a budget cut, a normalized match, a file named twice, which one `lets show`
prints once) is allowed through unmodified instead of rewritten or blocked — a wrong rewrite is
worse than none. Codex gets the identical classification Claude Code does, rendered with
`permissionDecision: "allow"` for a rewrite and `"deny"` for a block. A `sed -i` substitution is
always blocked, on both agents, never rewritten: no `lets edit` call reproduces an in-place edit
exactly.

Case follows what the agent does with the result. A search whose hits it reads keeps `lets find`'s
smart case — a pattern with no capital matches any case — since the agent sees every hit, and the
footer names how many hits match only ignoring case. A search whose exit status is read, and a count
(`-c`) or file list (`-l`), keeps `grep`'s exact case with `-s` unless the original asked for
`-i`: there a number or a branch stands in for the hits, and smart case could change it. An
`rg` that set its own case keeps it, the last of `-i`, `-s` (`--case-sensitive`) and `-S`
(`--smart-case`) winning as it does in `rg`; `grep`'s `-s` is `--no-messages`, not a case flag.

The classifier parses the command with a real bash grammar, walks pipelines, `&&`/`;`/`||` lists
and substitutions, and rewrites or blocks only what it can translate with confidence. It fails
open: `lets` missing, crashing, or mid-update degrades every hook to allow, never an error that
could wedge an agent's turn.

For a `PostToolUse` event it checks the file an `Edit`/`Write` (`tool_input.file_path`) or a Codex
`apply_patch` (the first `*** Update File:`/`*** Add File:` path in `tool_input.command`) just
wrote, and prints the result as `additionalContext` — the check a model would otherwise run by
hand. Every recognized file gets the structural check `lets edit` runs (`check: structure ok`,
`check: json invalid`, …). A `.go` file inside a module that passes it is then built with `go
build`, or type-checked with `go vet` for a `_test.go` file, which `go build` skips; a build that
passes reads `go build: ok`, one that fails carries the compiler's first 20 lines and names how
many more it cut, with go's per-package blocks in package order so the cut keeps the same errors
every run. Only a file `go list` names is built: one the build leaves out — another OS or arch
suffix, a `//go:build` or `// +build` constraint, a leading `_` or `.`, cgo turned off — reports
the structural result, since a build would say ok about code it never compiled. `go list` and the
build share 10 s; one still running then is stopped with every process it started, and reports
`go build: timed out after 10 s` (or `go list: …`) above the structural result. A path that is not a regular file, a file over 1 MiB, or
an unrecognized extension gets no answer at all.

What passes unblocked, deliberately: output piped into another program (`cat f | jq`, `cat f |
wc`), a command substitution or process substitution (`$(cat f)`, `<(cat f)`), a heredoc sent to
another program's stdin, an unrecognized flag, and any path outside the working tree.

A `sed -i` substitution is blocked only in its narrowest form: a bare `sed -i 's/OLD/NEW/g'`
(the `g` flag is required), every target path in-tree, non-glob, resolvable, and neither a
directory nor duplicated, and a substitution that is literal — no regex metacharacters, no
line-range prefix. Anything looser (a different `-i` suffix, a non-global substitution, a regex
with `.*` or `&`) passes through unblocked, since a wrong translation is worse than none.

## Flags

Both `hooks install`/`hooks uninstall` and `hook classify` take only the shared flags
(`--json`, `--jsonl`, `--budget`, `--max-bytes`, `--max-file-bytes`, `--no-ignore`,
`--allow-outside`, `--no-check`, `-q/--quiet`); `hook classify` takes its event as stdin, not as a
flag.

## Exit codes

| Exit | Slug | Means |
|---|---|---|
| 0 | — | installed, uninstalled, or nothing to remove |
| 1 | `path_conflict` | a different `lets` comes first on `PATH`; named in the message, nothing written |
| 1 | `not_on_path` | no `lets` reachable on `PATH` at all, so a hook could not run it |
| 7 | `io_error` | the settings file could not be read or written |

## Examples

Installing for Claude Code adds all four hooks in one call:

```console
$ lets hooks install claude-code
added the PreToolUse hook
added the SubagentStart hook
added the SessionStart hook
added the PostToolUse hook
```

A second install is a no-op, reported as such, and the settings file does not change:

```console
$ lets hooks install claude-code
the PreToolUse hook was already installed
the SubagentStart hook was already installed
the SessionStart hook was already installed
the PostToolUse hook was already installed
```

Installing when a different `lets` shadows this one on `PATH` refuses and names it:

```console
$ lets hooks install claude-code
? 1
a different `lets` comes first on PATH at [CWD]/fake-bin/lets — this one is [..]/lets · remove the other, or put this one's directory ahead of it on PATH
ERROR_CODE=path_conflict
```

`lets hook classify` rewriting a bare `cat` on Claude Code — `updatedInput` carries the
replacement, there is no `permissionDecision`, and Claude Code's own permission flow runs on the
command it names instead:

```console
$ printf '{"session_id":"s","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat src/usage.ts"}}' "$(pwd)" | lets hook classify
{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"lets show src/usage.ts --all --no-header --no-numbers"}}}
```

`lets hook classify` blocking a `sed -i` substitution instead — no exact `lets` command reproduces
an in-place edit, so it always names one to run by hand:

```console
$ printf '{"session_id":"s","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sed -i '"'"'s/a/b/g'"'"' src/usage.ts"}}' "$(pwd)" | lets hook classify
{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"lets edit replaces the exact text and shows the changed lines.\nrun: lets edit src/usage.ts --old 'a' --new 'b' --all"}}
```

`lets hook classify` letting a piped `cat` through untouched — its stdout never reaches the tool
result, so there is nothing to replace:

```console
$ printf '{"session_id":"s","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat config/app.json | wc -l"}}' "$(pwd)" | lets hook classify
```

(stdout is empty, exit code 0 — the command runs unmodified.)

Installing for Codex adds all four entries and reports the trust status:

```console
$ lets hooks install codex
added the PreToolUse hook
added the SessionStart hook
added the SubagentStart hook
added the PostToolUse hook
hook: installed, not yet approved · open Codex and choose 'Trust all and continue' when prompted, press t in the hooks browser, or pass --dangerously-bypass-hook-trust for one run
For reading, finding and editing files, use `lets` (run `lets guide` once) instead of `cat`,
`grep` or `sed -n`. It reads several files or ranges in one call, returns bounded numbered
output, and its edits return the changed region — so do not follow a `lets` call with a `cat` or
`sed -n` to check the result.
an unapproved hook is skipped entirely · Codex runs the command unhooked, never through lets, until it is approved
```
