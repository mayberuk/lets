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
  `&&` chain, and after a `lets` call in the same command), displayed `grep`/`rg`, `sed -i`,
  `cat > file <<`, `python -c` writing a file, `xargs cat` displayed.
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
✅ DO    cat src/a.ts && cat src/b.ts            → block: "run: lets show src/a.ts src/b.ts"
✅ DO    jq . - <<'JSON'                          → allow  (heredoc-to-stdin)
❌ DON'T $(cat VERSION)                          → block  (data flow; must allow)
❌ DON'T cat f.ts                                → block: "use lets"  (no runnable command)
```

Why: the corpus pattern is two separate Bash calls, the hook is the one lever that moves it, and
one false block teaches the agent to route around the tool for the rest of the session.
