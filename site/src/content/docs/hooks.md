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

**`claude-code`** merges three hooks into `~/.claude/settings.json`:

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
  `lets edit <path> --old '<OLD>' --new '<NEW>' --all`; a search is also blocked, instead of
  rewritten, when a later `&&`, `||` or `set -e` reads its exit status.

**`codex`** merges a `PreToolUse` entry into `hooks.json` (default `$CODEX_HOME/hooks.json`, or
`~/.codex/hooks.json`) and prints one paragraph to add to `~/.codex/AGENTS.md` by hand — Codex has
no session-start hook to inject a paragraph automatically the way Claude Code does.

Both installers only ever write into the agent's own settings; nothing modifies your shell
profile. Installing twice is a no-op — the settings file is byte-identical across a reinstall.
`hooks uninstall` removes exactly what `hooks install` added and leaves any of the user's own
hooks in the same file untouched.

## What `lets hook classify` does

Reads one `PreToolUse` JSON event (`session_id`, `cwd`, `hook_event_name`, `tool_name`,
`tool_input.command`) on stdin. For an allowed command it prints nothing and exits 0 — the
command runs unmodified. For a rewrite, on Claude Code only, it prints one JSON line naming the
`lets` replacement in `hookSpecificOutput.updatedInput`, with no `permissionDecision`, so Claude
Code's own permission flow still runs on the rewritten command. For a blocked command it prints
one JSON line naming the `lets` replacement in `hookSpecificOutput.permissionDecision: "deny"`.

`cat`, `head -n`, `sed -n` and `grep`/`rg` reads, and a `sed -i` substitution, are the forms the
classifier can replace. Codex — identified by its event's `turn_id` field — always gets the block;
Claude Code gets the same block only when a rewrite would not be safe (a sensitive path, a search
whose exit status a later `&&`, `||` or `set -e` reads, or output `lets show`/`lets find` would not
reproduce exactly) — otherwise it gets the rewrite instead, alone or as a segment of a
`&&`/`||`/`;` chain, with the rest of the command kept byte for byte. A `sed -i` substitution is
always blocked, never rewritten.

The classifier parses the command with a real bash grammar, walks pipelines, `&&`/`;`/`||` lists
and substitutions, and rewrites or blocks only what it can translate with confidence. It fails
open: `lets` missing, crashing, or mid-update degrades every hook to allow, never an error that
could wedge an agent's turn.

What passes unblocked, deliberately: output piped into another program (`cat f | jq`, `cat f |
wc`), a command substitution or process substitution (`$(cat f)`, `<(cat f)`), a heredoc sent to
another program's stdin, an unrecognized flag, and any path outside the working tree.

`lets find` exits 1 over its hit cap and when every hit lands in a file it skips, where `grep`/`rg`
exit 0 — so a search stays a deny, instead of a rewrite, whenever a later `&&`, `||`, `set -e` or
an `ERR` trap could take a different branch on that difference.

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

Installing for Claude Code adds all three hooks in one call:

```console
$ lets hooks install claude-code
added the PreToolUse hook
added the SubagentStart hook
added the SessionStart hook
```

A second install is a no-op, reported as such, and the settings file does not change:

```console
$ lets hooks install claude-code
the PreToolUse hook was already installed
the SubagentStart hook was already installed
the SessionStart hook was already installed
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
{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"lets show src/usage.ts --all"},"additionalContext":"lets show reads several files and ranges in one call.\nran instead: lets show src/usage.ts --all"}}
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

Installing for Codex adds the `PreToolUse` hook and prints the one line to add by hand:

```console
$ lets hooks install codex
added the PreToolUse hook
For reading, finding and editing files, use `lets` (run `lets guide` once) instead of `cat`,
`grep` or `sed -n`. It reads several files or ranges in one call, returns bounded numbered
output, and its edits return the changed region — so do not follow a `lets` call with a `cat` or
`sed -n` to check the result.
add this to ~/.codex/AGENTS.md by hand
```
