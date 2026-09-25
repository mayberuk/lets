# lets, for a calling agent

What a program or an agent harness needs to read `lets` output: the exit codes and their slugs,
the `--json` and `--jsonl` shapes, and the footer contract. `docs/guide.md` is the one source of
the `lets guide` screen; the discovery text at the end of this file quotes spec.md's own words for
the Claude Code and SubagentStart paragraphs, so those two and the spec cannot drift apart.

## Exit codes

Every non-zero exit writes a diagnostic to stderr and `ERROR_CODE=<slug>` as the last line: one
line per failure (an ambiguity adds its candidates below it), so `show` or `find` with several
missing targets names each of them, and the first failure listed sets the slug and the code.
Stdout stays parseable: content and a non-zero exit are not exclusive, so a partly successful
call prints what it did and then fails.

| Exit | Slug | Means |
|---|---|---|
| 0 | — | done |
| 1 | `not_found` | a target, `--old` or a pattern matched nothing; the nearest candidate is shown, or `find` was over its hit cap |
| 1 | `exists` | `write` would overwrite; pass `--force` |
| 1 | `empty_input` | `write` got empty stdin; pass `--empty` |
| 1 | `path_conflict` | `hooks install` or `update`: a different `lets` comes first on PATH; the message names it. Nothing is written |
| 1 | `not_on_path` | `hooks install`: a bare `lets` on PATH does not reach this binary, so the hooks could not run it. Nothing is written |
| 1 | `no_repository` | `update`: this build names no release repository |
| 2 | `ambiguous` | the target or `--old` matched more than once; every candidate is listed as `path:line`, at most 20 with `(+N more)` |
| 3 | `check_failed` | the guardrail failed after the edit; the file is unchanged |
| 4 | `over_budget` | content exceeded `--max-bytes` and no `--budget` was given |
| 5 | `changed` | the file moved since the `--if sha:…` it was given |
| 6 | `outside_tree` | a write outside the working tree; pass `--allow-outside` |
| 7 | `unsupported_file` | binary, hardlinked, non-UTF-8 in the matched region, over `--max-file-bytes`, or the lock was unavailable; for `transform`, not a structured format, or a key that exists but cannot be changed in place (a tagged YAML node, or one reached through an alias) |
| 7 | `io_error` | any other I/O failure; `-` names stdout. A stdout write that fails because the reader closed the pipe is exit 0 with nothing on stderr |
| 7 | `update_failed` | `update`: no release for this platform, the download failed, or the installer exited non-zero; the message says which |
| 8 | `partial_batch` | some files of a batch landed; the footer names which (for `hooks install`, stdout does) |
| 64 | `usage` | the command line is malformed. Clap's own code for this is 2, which is `ambiguous` here, so it is remapped |

A missing file reaches exit 1 (`not_found`) whichever way it failed, so a caller branches on the
code, not on the message.

## `--json` and `--jsonl`

The same output model renders text, `--json` and `--jsonl`: a field exists in all three or in
none. `--json` is one object; `--jsonl` is one object per target, hit, file, count row or edit,
followed by a trailing `{"stats":…,"omitted":…}` record.

```jsonc
// show
{"targets":[{"target":"src/store/usage.ts#usage","path":"src/store/usage.ts","start":38,"end":61,
  "total":212,"resolver":"tree-sitter","sha":"e77be77be77b",
  "lines":[{"number":38,"marker":"none","text":"export function usage(id: string) {"}]}],
 "omitted":[],"stats":{"lines":24,"bytes":840,"tokens_est":210}}

// find: hits are target blocks without the range keys; markers are "hit" and "context".
// --files is {"files":[…]}, --count is {"counts":[{"count":9,"path":"…"}]}.

// edit, one file
{"path":"src/store/usage.ts","replacements":1,"lines":[42],"match":"exact",
 "region":{"start":40,"end":44,"lines":[…]},
 "check":{"layer":"structure","status":"ok","errors_before":0,"errors_after":0},
 "sha":{"before":"e77be77be77b","after":"b410b410b410"},
 "omitted":[],"stats":{…}}

// edit or transform, a batch: {"edits":[…],"omitted":[…],"stats":{…}}
// an insert carries "inserted" and "anchor" where a replace carries "replacements"
// write: {"path":"…","outcome":"created","lines":2,"bytes":34,"sha":"…","omitted":[],"stats":{…}}
// guide and version carry their text alone: {"guide":"…"} and {"version":"1.2.3"}
```

`omitted` is the machine-readable half of the footer: a unit omission is a string
(`"normalized"`), and one with detail is a single-key object
(`{"hit_cap":{"hits":312,"cap":50}}`, `{"partial_batch":{"written":["src/a.ts"]}}`). A target
or search root that produced nothing while others did is
`{"unresolved":{"target":"nope.ts","error":"not_found"}}`, and a budget trim names the range it
cut: `{"budget":{"budget":100,"trimmed_target":"big.txt","not_shown":[59,200]}}`.
`stats.tokens_est` is bytes ÷ 4 — `LETS_TOKEN_RATIO` overrides the divisor, and it is `null`
under `LETS_NO_STATS=1`.

## The footer contract

The footer is a contract, not a status line: if it does not name a narrowing, that narrowing did
not happen. Every window, budget trim, failed target, hit cap, `.gitignore` or hidden-file skip, skipped or
inconclusive check, normalised match and partly written batch is named there and in `omitted`,
which is why an agent can trust the result and skip the follow-up read. Status metadata —
headers, footers, `ERROR_CODE` — is outside `--budget` and `--max-bytes`, so no budget can trim
the record of what a budget did. Output is byte-identical for identical input and file state, and
carries no colour.

## The Claude Code paragraph

Delivered by a `SessionStart` hook alone (spec.md § Discovery): a 2026-09-23 trial found
`--append-system-prompt-file` text held tool-call batching to 0 of 192 model requests, against
roughly 26% for plain Claude Code, and cost the same or more, so nothing is appended to the
system prompt any more.

> # File work: use `lets` through Bash
>
> | Instead of | Run |
> |---|---|
> | `cat a.ts b.ts`, Read | `lets show a.ts b.ts` |
> | `sed -n '40,80p' f.ts` | `lets show f.ts:40-80` |
> | find one function | `lets show f.ts#computeFee` |
> | `grep -n -A 5 'x' f.ts` | `lets show "f.ts@'x'" -A 5` |
> | `grep -rn 'x' src`, `rg x src` | `lets find 'x' src` |
> | `sed -i 's/a/b/'`, Edit | `lets edit f.ts --old a --new b` |
> | edit JSON/YAML/TOML | `lets transform f.json --set version=1.4.0` |
> | `cat > new.ts <<'EOF'` | `lets write new.ts <<'EOF'` |
>
> Several edits in one call; each `old` is exact text that occurs once:
>
> ```
> lets edit --from - <<'LETS'
> @@ a.ts
> <<<<<<< old
> cap = 10
> ======= new
> cap = 20
> >>>>>>>
> <<<<<<< old
> floor = 1
> ======= new
> floor = 2
> >>>>>>>
> LETS
> ```
>
> Do not pipe `lets` through `head`/`tail` or add `2>/dev/null`: it cuts the footer and hides the fix. Keep Read for images and PDFs; use plain Bash for anything else that is not reading, searching or editing files.

`docs/design/system-append.md` holds a longer, alternative form, per verb, for a harness that can
afford it.

## The SessionStart hook

`lets hooks install claude-code` adds a SessionStart hook, matcher
`startup|resume|clear|compact|fork` (compaction drops injected context, so the paragraph is
re-printed on every way a session can begin, not just `startup`), whose command prints the
paragraph above and nothing else — not `lets guide`, whose command table the paragraph now
carries itself, so printing both would repeat about 1,000 tokens. The command first checks that
`lets` is on PATH and otherwise exits 0 with no output, so a missing or mid-update binary adds no
hook error to every session start. Nothing is written to `~/.claude/system-append.md` any more,
and no alias is printed to add by hand; a file an earlier install left there is kept, and named as
no longer used, on every following install.

## Rewrite versus deny

The `PreToolUse` hook (`lets hook classify`) rewrites only an exact `cat`, `head -n` or `sed -n`
read that a single `lets show` would reproduce line for line. A recognized `grep`/`rg` search, a
`sed -i 's/…/…/g'` global substitution, a heredoc-fed `cat > file`, a sensitive path (a dotfile,
key or credential), and any read `lets show` would not print exactly, all keep the deny instead,
which always names a runnable `lets` command. Everything the classifier does not recognize — an
`awk`, `less`, `nl` or `tee` read, `tail -n`, `echo >`, an unrecognized `grep`/`rg` flag, or a
`sed -i` substitution missing its trailing `g` — is not classified at all and runs exactly as
typed, neither rewritten nor denied. Codex always gets the deny — a rewrite is Claude
Code only. Before a rewrite, or before naming a path in a deny, the hook reads the `Read` and
`Edit` deny and ask rules from every Claude Code settings tier (managed, user, project, local) and
steps aside — allowing the original command through — when one of those rules already covers the
path, so Claude Code's own rule decides, not the hook's. The limit: a rule passed only through
`--settings`, `--disallowedTools`, or set for one session, is invisible to the hook, which reads
only the settings files on disk.

## The SubagentStart line

`--append-system-prompt` does not reach a non-fork subagent, so the same paragraph (spec.md §
Discovery) arrives instead as SubagentStart `additionalContext`:

> # File work: use `lets` through Bash
>
> | Instead of | Run |
> |---|---|
> | `cat a.ts b.ts`, Read | `lets show a.ts b.ts` |
> | `sed -n '40,80p' f.ts` | `lets show f.ts:40-80` |
> | find one function | `lets show f.ts#computeFee` |
> | `grep -n -A 5 'x' f.ts` | `lets show "f.ts@'x'" -A 5` |
> | `grep -rn 'x' src`, `rg x src` | `lets find 'x' src` |
> | `sed -i 's/a/b/'`, Edit | `lets edit f.ts --old a --new b` |
> | edit JSON/YAML/TOML | `lets transform f.json --set version=1.4.0` |
> | `cat > new.ts <<'EOF'` | `lets write new.ts <<'EOF'` |
>
> Several edits in one call; each `old` is exact text that occurs once:
>
> ```
> lets edit --from - <<'LETS'
> @@ a.ts
> <<<<<<< old
> cap = 10
> ======= new
> cap = 20
> >>>>>>>
> <<<<<<< old
> floor = 1
> ======= new
> floor = 2
> >>>>>>>
> LETS
> ```
>
> Do not pipe `lets` through `head`/`tail` or add `2>/dev/null`: it cuts the footer and hides the fix. Keep Read for images and PDFs; use plain Bash for anything else that is not reading, searching or editing files.
