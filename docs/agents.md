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
> | several `cat`/`sed -n`/`grep` calls, Read | `lets show a.ts b.ts:10-40 c.ts#computeFee` |
> | a definitions-only skim | `lets show f.ts --outline` |
> | `sed -i 's/a/b/'`, Edit | `lets edit f.ts --old a --new b` |
> | edit JSON/YAML/TOML | `lets transform f.json --set version=1.4.0` |
> | `cat > new.ts <<'EOF'` | `lets write new.ts <<'EOF'` |
>
> Several edits and the build in one call; each `old` is exact text that occurs once:
>
> ```
> lets edit --from - --check @auto <<'LETS'
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

A `cat`, `sed -n` or `grep` the agent types anyway is rewritten silently and faithfully by the
`PreToolUse` hook below, so this table only needs to teach what a rewrite cannot do: combining
several files, ranges or `#symbol` lookups into one call, and batching several edits (and the
build that checks them) into one.

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

The `PreToolUse` hook (`lets hook classify`) rewrites an exact `cat`, `head -n` or `sed -n` read
that a single `lets show` would reproduce line for line, and a `grep`/`rg` search that `lets find`
translates exactly — alone, or as one or more segments of a `&&`/`||`/`;` chain, with every other
byte of the command kept as typed. A search whose exit status a later `&&` or `||`, a `$?` or
`PIPESTATUS` expansion in any form, `set -e` (or `shopt -o errexit`) or an `ERR` trap reads is
rewritten too, not denied: `lets find` gets `--cap-exit-0` added so it
exits 0 over its hit cap and when every hit lands in a file it skips, matching what `grep`/`rg`
would have exited, so a chain that branches on that status still takes the same branch after the
rewrite. A read `lets show` would not print exactly (a budget cut, a normalized match, a file
named twice, which one `lets show` prints once) is allowed through unmodified rather than
rewritten or denied — a wrong rewrite is worse than none.

Case follows what the agent does with the result. A search whose hits it reads keeps `lets find`'s
smart case — a pattern with no capital matches any case — since the agent sees every hit, and the
footer names how many hits match only ignoring case. A search whose exit status is read, and a
count (`-c`) or a file list (`-l`), keeps `grep`'s exact case with `-s` unless the original asked
for `-i`: there a number or a branch stands in for the hits, and smart case could change it.
Faithful exit status and counts outrank smart case. An `rg` that set its own case keeps it, the
last of `-i`, `-s` (`--case-sensitive`) and `-S` (`--smart-case`) winning as it does in `rg`;
`grep`'s `-s` is `--no-messages`, not a case flag.

A `sed -i 's/…/…/g'` global substitution, a heredoc-fed `cat > file`, and a sensitive path (a
dotfile, key or credential) keep the deny, which always names a runnable `lets` command and keeps
every other segment of the chain. Everything the classifier does not recognize — an `awk`, `less`,
`nl` or `tee` read, `tail -n`, `echo >`, an unrecognized `grep`/`rg` flag, or a `sed -i`
substitution missing its trailing `g` — is not classified at all and runs exactly as typed, neither
rewritten nor denied. Codex gets the identical classification Claude Code does, rendered with
`hookSpecificOutput.permissionDecision: "allow"` alongside `updatedInput` for a rewrite (Codex
requires the field; Claude Code's own rewrite carries none) and `"deny"` for a block. Before a
rewrite, or before naming a path in a deny, the hook reads the `Read` and `Edit` deny and ask rules
from every Claude Code settings tier (managed, user, project, local) and steps aside — allowing the
original command through — when one of those rules already covers the path, so Claude Code's own
rule decides, not the hook's. The limit: a rule passed only through `--settings`,
`--disallowedTools`, or set for one session, is invisible to the hook, which reads only the
settings files on disk.

A rewrite verdict is any hook stdout JSON line containing `hookSpecificOutput.updatedInput`, with
no `additionalContext` — the stable signal a program reading `lets`'s hook output should use to
tell a rewrite from a block or an allow.

## The PostToolUse check

`lets hooks install claude-code` adds a `PostToolUse` hook on `Edit|Write`, and `lets hooks
install codex` one on `apply_patch` (Codex names the tool `apply_patch` in a hook's stdin, and
sends the patch text as `tool_input.command`). Both run `lets hook classify`, which checks the file
the tool just wrote — `tool_input.file_path`, or the first `*** Update File:`/`*** Add File:` path
of a patch — and prints the result as `additionalContext`, so the agent does not run the check by
hand. A post-write hook has no before-state, so it reports the whole file, not only the edit.

- Every recognized file gets the structural check `lets edit` runs: `check: structure ok`,
  `check: structure failed`, `check: json invalid`, and so on. A failure is reported as it is.
- A `.go` file inside a module that passes it is then built: `go build` for a package file, `go
  vet` for a `_test.go` file, which `go build` does not compile. A pass reads `go build: ok` (or
  `go vet: ok`); a failure carries the compiler's first 20 lines and names how many more it cut.
  go builds packages in parallel, so its per-package blocks are put in package order before the
  cut: the same errors survive every run.
- Only a file `go list` names is built. One the build leaves out — another OS or arch suffix, a
  `//go:build` or `// +build` constraint, a leading `_` or `.`, cgo turned off — reports the
  structural result, since a build would say ok about code it never compiled.
- `go list` and the build share 10 s. One still running then is stopped along with every process
  it started, and reports `go build: timed out after 10 s` (or `go list: …`) above the structural
  result, so the answer says why it is only structural.
- A missing `go`, a file outside any module, a path that is not a regular file, a file over 1 MiB,
  and an unrecognized extension fall back or stay silent; the hook never blocks and never errors.

Codex trusts each hook entry separately, so the install report says "installed and approved" only
when all four entries (`PreToolUse`, `SessionStart`, `SubagentStart`, `PostToolUse`) are trusted,
and otherwise names the ones still waiting.

## The SubagentStart line

`--append-system-prompt` does not reach a non-fork subagent, so the same paragraph (spec.md §
Discovery) arrives instead as SubagentStart `additionalContext`:

> # File work: use `lets` through Bash
>
> | Instead of | Run |
> |---|---|
> | several `cat`/`sed -n`/`grep` calls, Read | `lets show a.ts b.ts:10-40 c.ts#computeFee` |
> | a definitions-only skim | `lets show f.ts --outline` |
> | `sed -i 's/a/b/'`, Edit | `lets edit f.ts --old a --new b` |
> | edit JSON/YAML/TOML | `lets transform f.json --set version=1.4.0` |
> | `cat > new.ts <<'EOF'` | `lets write new.ts <<'EOF'` |
>
> Several edits and the build in one call; each `old` is exact text that occurs once:
>
> ```
> lets edit --from - --check @auto <<'LETS'
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

## The Codex paragraph

Codex has no Read or Edit tool to name, so `lets hooks install codex` delivers the same table with
those two mentions dropped, as `SessionStart` and `SubagentStart` plain-text stdout (Codex treats
non-JSON-looking stdout on exit 0 as `additionalContext` directly for both events — no
`hookSpecificOutput` wrapper needed the way Claude Code's `SubagentStart` requires). This is the
one place `src/install/codex.rs`'s `CODEX_SESSION_START_PARAGRAPH` constant quotes:

> # File work: use `lets` through Bash
>
> | Instead of | Run |
> |---|---|
> | several `cat`/`sed -n`/`grep` calls | `lets show a.ts b.ts:10-40 c.ts#computeFee` |
> | a definitions-only skim | `lets show f.ts --outline` |
> | `sed -i 's/a/b/'` | `lets edit f.ts --old a --new b` |
> | edit JSON/YAML/TOML | `lets transform f.json --set version=1.4.0` |
> | `cat > new.ts <<'EOF'` | `lets write new.ts <<'EOF'` |
>
> Several edits and the build in one call; each `old` is exact text that occurs once:
>
> ```
> lets edit --from - --check @auto <<'LETS'
> @@ a.ts
> <<<<<<< old
> cap = 10
> ======= new
> cap = 20
> >>>>>>>
> LETS
> ```
