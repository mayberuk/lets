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
  `cmd - <<'EOF'` heredoc-to-stdin, any `lets …` call not piped into
  `head`.
- Rewrite (`updatedInput`, plus `permissionDecision: allow` on Codex, none on Claude Code) on
  both harnesses alike, whenever one `lets show` prints every line the original would, of named
  in-tree files that are not dotfiles, keys or credentials; when one `lets find` call reproduces a
  `grep`/`rg` search exactly; or when a `grep`/`rg` search whose exit status is read after it
  (`&&`, `||`, `$?`/`${?}`/`PIPESTATUS` in any form, `set -e` or `shopt -o errexit`, an `ERR`
  trap) can be rewritten with `lets find --cap-exit-0`,
  which exits 1 on zero hits so the status check still reads the same thing — alone, or as one or
  more segments of a `&&`/`||`/`;` chain, with every other byte kept as typed. "Prints every line"
  is checked on the file itself, after any `cd` and symlink resolves: under `--max-bytes`, no line
  `show` cuts, no range starting past the end. Also rewrite `lets show|find … | head[ -N]` to the
  same call with `--head N` when nothing reads the pipeline's exit status, so the footer survives;
  and `cat > f <<'EOF'` or `cat <<'EOF' > f` (operator `>` or `>|`) to `lets write --force f` with
  the heredoc operator, body and delimiter kept byte for byte. A glob operand is expanded as bash
  expands it (last component only, nullglob and dotglob off) and is rewritten only when every match
  is a regular in-tree file that is not a dotfile, key or credential, no dot-named entry matches,
  and the glob stays unquoted in the replacement.
- Keep grep's exact case (`-s`, unless the original chose its own case) on a search whose exit
  status is read and on a count or file list; leave smart case only on a search whose hits the
  agent sees. A faithful exit status or count outranks smart case. rg's last `-i`/`-s`/`-S` wins;
  grep's `-s` is `--no-messages`.
- Allow a read `lets` cannot reproduce exactly (a range `lets show` would cut, a search
  `--cap-exit-0` can't translate, anything past `--max-bytes`, a file named twice): the original
  command runs unmodified rather than losing the agent a read it needs. Also allow a path that is
  a dotfile, a dot directory, a key or a credential, or that leaves the tree, whether as typed or
  once `cd` and symlinks resolve. That statement runs as typed, and every other statement in the
  chain is still rewritten. A deny there withheld nothing, since its own `run:` line read the same
  file through `lets`. Also allow a glob this expansion cannot follow, or one that matches nothing,
  a directory, a dot entry, a `-`-led name or a sensitive file. Also allow a heredoc write when the
  command holds a `set` builtin, when the destination is hard-linked or is not a regular file, when
  the body is over `--max-file-bytes`, or when the operator is `&>`. Also allow a `lets … | head`
  whose exit status is read, and any `| tail`.
- Deny only an in-place edit with no exact `lets` translation but a runnable replacement
  (`sed -i 's/a/b/g'`, `python -c` writing a file). Put the runnable
  `lets edit` (or other) replacement in the reason on both harnesses — the deny JSON shape does
  not depend on which one is asking.
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
- Let a dot path, a glob, a heredoc write or a `lets … | head` pipe turn a command into a deny.

```text
✅ DO    cat src/a.ts && cat src/b.ts            → rewrite (both harnesses): "lets show src/a.ts src/b.ts --all"
✅ DO    grep -n leader f.go && ls               → rewrite: "lets find 'leader' f.go -s --cap-exit-0 && ls"  (exit status read)
✅ DO    grep -n leader f.go                     → rewrite: "lets find 'leader' f.go"  (hits shown: smart case)
✅ DO    sed -n '40,45p' a.ts, past the file's end → allow  (lets show would cut the range)
✅ DO    jq . - <<'JSON'                          → allow  (heredoc-to-stdin)
❌ DON'T $(cat VERSION)                          → deny  (data flow; must allow)
❌ DON'T sed -i 's/a/b/' f.ts                    → deny: "use lets"  (no runnable command)
✅ DO    the same sed -i                         → deny: "run: lets edit f.ts --old a --new b"
✅ DO    cat .claude/plans/p.md && cat src/a.ts  → rewrite: "cat .claude/plans/p.md && lets show src/a.ts --all --no-header --no-numbers"
✅ DO    cat src/*.ts  (every match plain)       → rewrite: "lets show src/*.ts --all --no-numbers"
✅ DO    cat > f.ts <<'EOF' … EOF                 → rewrite: "lets write --force f.ts <<'EOF' … EOF"
✅ DO    lets find cap src | head -20             → rewrite: "lets find cap src --head 20"
```

Why: the corpus pattern is two separate Bash calls, the hook is the one lever that moves it, and
one false block teaches the agent to route around the tool for the rest of the session.
