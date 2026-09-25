---
paths:
  - src/hook/**
  - tests/hook/**
---
# PreToolUse classifier (`lets hook classify`)

Every error path inside `lets` itself degrades to allow. A block with no runnable replacement,
or a blocked heredoc-to-stdin, is a dealbreaker.

## Always
- Parse the command with the bundled bash grammar and walk pipelines, `&&`/`;`/`||` lists,
  command and process substitutions, and redirections. Classify a read only when its stdout
  reaches the tool result rather than another command.
- Block only what has a `lets` replacement, and put the runnable replacement command in the
  block message.
- Return allow (exit 0, no block JSON) on parse failure, panic, missing binary, timeout, unknown
  builtin, `eval`/`bash -c`/`sh <<EOF` bodies, paths outside the working tree, and anything not
  classified with confidence.
- Pass: `cat a | jq`, `$(cat f)`, `<(cat f)`, `xargs cat | sort`, `head -c`, `tail -f`, any
  `cmd - <<'EOF'` heredoc-to-stdin, any `lets …` call.
- Block: a bare displayed `cat`, `head`, `tail` or `sed -n` of a repo file (including last in an
  `&&` chain, and after a `lets` call in the same command), a displayed `grep`/`rg` search whose
  exit status is read after it (`&&`, `||`, `set -e`, an `ERR` trap) or that `lets find` would not
  translate exactly, `sed -i`, `cat > file <<`, `python -c` writing a file, `xargs cat` displayed.
- Rewrite (`updatedInput`, no `permissionDecision`) only on Claude Code, and only when one
  `lets show` prints every line the original would, of named in-tree files that are not dotfiles,
  keys or credentials, or when one `lets find` call reproduces a `grep`/`rg` search exactly —
  alone, or as one or more segments of a `&&`/`||`/`;` chain, with every other byte kept as typed;
  deny everything else a block covers, and always fail open. "Prints every line" is checked on the
  file itself, after any `cd` and symlink resolves: under `--max-bytes`, no line `show` cuts, no
  range starting past the end.
- Before a rewrite, or a block whose `run:` line names a path, match every named path against
  the `Read` and `Edit` deny and ask rules of each Claude Code settings tier (managed, user,
  project, local). A match, or a settings file or rule that cannot be read, is allow: Claude
  Code judges `lets`, not the read it replaced, so only its own rule can decide the original.
  Load settings on that path only, never on the allow path.
- Add every new verdict to `tests/hook/` as command → verdict with the replacement text. The
  corpus is the spec of this module.
- Stay under the `hook classify` gate in `bench/gates.rs`: startup plus one bash parse, no
  grammar loaded but bash.

## Never
- Block on a regex over the command string.
- Block a command whose stdout is consumed by another command.
- Emit a block message without a copy-pasteable `lets` command in it.
- Let a panic, I/O error or missing grammar surface as a block.

```text
✅ DO    cat src/a.ts && cat src/b.ts            → Claude Code rewrite: "lets show src/a.ts src/b.ts --all"
✅ DO    the same command from Codex             → block: "run: lets show src/a.ts src/b.ts"
✅ DO    rg cap src                              → Claude Code rewrite: "lets find -s 'cap' src"
✅ DO    set -e; grep -n x src/a.ts              → block: "run: set -e; lets find -s 'x' src/a.ts"  (exit status read)
✅ DO    jq . - <<'JSON'                          → allow  (heredoc-to-stdin)
❌ DON'T $(cat VERSION)                          → block  (data flow; must allow)
❌ DON'T cat f.ts                                → block: "use lets"  (no runnable command)
```

Why: the corpus pattern is two separate Bash calls, the hook is the one lever that moves it, and
one false block teaches the agent to route around the tool for the rest of the session.
