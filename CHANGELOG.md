# Changelog

All notable changes to `lets` are documented here. Format:
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- `lets show` and `lets find` read files outside the git checkout, such as `~/.claude/CLAUDE.md`
  or a scratch directory, without `--allow-outside`. `edit`, `write` and `transform` still refuse
  a target outside the checkout with exit 6 (`outside_tree`).
- A bare file target up to one and a half windows long (150 lines by default) shows whole instead
  of stopping at line 100.
- Each cut in the `show` footer names its file, as in `big.ts:101-212 not shown`, and the JSON
  `window` omission carries a `target` field.
- The `over_budget` message names `--max-bytes` and suggests a line range or fewer targets.
- The Codex session-start paragraph says a bare file shows its first 100 lines, and how to read
  more.

### Fixed

- An `edit` or `transform` value that starts with `-`, such as a Markdown bullet `- b` or a
  `-webkit-` CSS property, is taken as the value instead of being refused as an unknown flag.

## [0.0.8] - 2026-10-09

### Fixed

- A read after a `cd` out of the session's checkout into a directory no checkout holds, such as
  `cd ~/.ssh && cat known_hosts`, runs as typed again. In 0.0.7 the hook rewrote it to `lets`
  without judging the dot directory above the file.

### Upgrading

- Run `lets update`.

## [0.0.7] - 2026-10-09

### Fixed

- A read that starts with `cd` into another checkout, such as `cd ../other && sed -n 1,40p a.ts`,
  is now judged by that checkout's tree, so the hook rewrites it to `lets`. Before, the hook
  bounded it by the session's own checkout and let it run as typed. That shape was 58% of the
  raw reads in a day of 0.0.6 use. Deny and ask rules from both checkouts still apply.

### Changed

- `lets hooks install claude-code` installs the SessionStart and SubagentStart hooks again. They
  put the short lets note into the conversation, beside the table the mod puts in the Bash
  description.

### Upgrading

- Run `lets update`, then `lets hooks install claude-code` to add the start hooks. Start a new
  Claude Code session to pick them up.

## [0.0.6] - 2026-10-08

### Fixed

- In bypass and auto mode, Claude Code attaches a message telling the model to read files with
  `cat`, `head` or `sed -n` and to edit with `sed` or heredocs. The mod now rewrites that clause to
  point at `lets` and keeps the rest of the message. With 0.0.5, agents still typed `lets` for
  almost no reads.
- `lets show` and `lets find` accept `--max-lines N` as another name for `--head N`. Codex reached
  for it on its own.

### Upgrading

- `lets update` refreshes the mod. Start a new Claude Code session to pick it up.

## [0.0.5] - 2026-10-07

### Fixed

- The Claude Code mod now steers the lean prompt Claude Code serves Opus. That prompt's Bash
  description has no "avoid `cat`" paragraph, so in 0.0.4 the lets table never reached the model.
  The table now goes after the description's first paragraph whenever that paragraph is missing.
  The lean "Prefer the dedicated file/search tools" sentence is dropped as well.
- The mod remembers a change refused at a permission prompt or by a permission rule, not only one
  refused by auto mode. It keeps refusals per session, so `/clear` starts afresh, and it keeps
  other hooks' input changes when it drops its own.
- The installed classify guard exits 0 and prints nothing unless `lets` itself succeeds. Before,
  a `lets` exiting 2 blocked every Bash call.
- `lets hooks install codex` merges its entries and retires old ones in one write, so a failed
  install leaves the file untouched.
- The hook rewrites a command only into one with the same output and exit status. Otherwise the
  command runs as typed:
  - rg's `\|` stays a literal pipe.
  - `sed -i` on a symlink, a search naming a missing file, and bare `tail` all run as typed.
  - Bare `head f` becomes `lets show f:1-10`.
  - `rg -A`/`-B` keep their direction.
- A search with no path, or of a directory that a Read deny or ask rule could reach inside, runs as
  typed. settings.json written as JSONC is read rather than ignored.

### Upgrading

- After `lets update`, run `lets hooks install claude-code`, and `lets hooks install codex` if
  you use Codex. This writes the new guard. Codex asks you to approve the changed hook, and until
  you do it skips the hook rather than blocking.

## [0.0.4] - 2026-10-06

### Added

- A Claude Code mod, shipped inside the binary. `lets hooks install claude-code` writes it to
  `$XDG_DATA_HOME/lets/claude-code` and adds that directory to `CLAUDE_CODE_PLUGIN_DIRS` in
  `~/.claude/settings.json`. The mod:
  - replaces the Bash tool description's "avoid `cat`/`head`/`tail`, use Read" paragraph with the
    lets table, and drops "prefer dedicated tools over Bash" from the system prompt;
  - sends each Bash call through `lets hook classify`, runs a rewrite as the new command, and runs
    a deny's single `run:` line in place of the original, with a note to the model saying what ran;
  - passes the call through unchanged on any error.
- `show --head N` and `find --head N` cut the rendered output to N lines and name the cut in the
  footer. Refused with `--json`/`--jsonl` and for 0.

### Changed

- The `PreToolUse` hook no longer denies a command because one part of it reads a dot path, a key,
  a credential or a path outside the tree. That part runs as typed and the rest is still
  rewritten.
- A glob operand is expanded the way bash expands it, and the command is rewritten when every
  match is a plain in-tree file.
- `cat > f <<'EOF'` is rewritten to `lets write --force f` with the heredoc kept byte for byte,
  and `lets show|find … | head -N` to `--head N`, so the footer survives.
- On Claude Code, the install removes the `SessionStart` and `SubagentStart` lets hooks; the mod
  carries that text. Codex keeps both.
- `lets update` refreshes the installed mod through the new binary, but only while Claude Code
  settings still load it. It names every settings change the refresh made.
- `lets hooks uninstall claude-code` removes the mod and its `CLAUDE_CODE_PLUGIN_DIRS` entry. It
  deletes mod files only from the installer's own directory and names any it left.

### Upgrading

- Run `lets hooks install claude-code` once after updating to 0.0.4: a 0.0.3 binary's
  `lets update` does not know about the mod.

## [0.0.3] - 2026-09-28

### Added

- `find` names how many hits match ignoring case when an exact-case search finds none:
  `3 hits match only ignoring case (-s for exact case)`.
- `find --cap-exit-0` prints the same over-cap output but exits 0, so a rewritten `grep … && …`
  chain keeps the original's exit-status parity.
- `show --no-header`, and `find --no-numbers` with grep's `--` separator between non-adjacent
  context groups.

### Changed

- `show` no longer prints `sha:` on a read; only an `edit` result's footer carries the file's
  hash, as `sha:<unchanged>` or `sha:<before>→<after>`.
- `show --outline` lists one line per definition instead of the file's content.
- An edit whose match was exact and whose check passed or did not apply prints one line instead
  of the full echo; a normalized, guessed-span, confirmed or reverted edit still gets the full
  echo.
- The `PreToolUse` hook rewrites an exactly-translatable `cat`, `sed -n`, `head`/`tail`, `nl -ba`,
  `grep` or `rg` in place (`updatedInput`) on both Claude Code and Codex, instead of denying the
  command and suggesting a replacement. It denies only what has no exact `lets` translation:
  `sed -i` and similar in-place edits, or a path a settings deny/ask rule already covers.
- A search whose exit status is read (`&&`/`||`, `set -e`, `shopt -o errexit`, an `ERR` trap, or a
  later `$?`/`${?}`/`PIPESTATUS`) keeps exact case and uses `--cap-exit-0`, so the rewritten
  command's exit status still matches the original's; a plain displayed search keeps smart case.
- A file a command reads twice is left as typed instead of rewritten.
- `lets hooks install codex` adds `SessionStart`/`SubagentStart` context and prints the trust
  approval step; it never writes trust itself.

### Fixed

- `edit`, `write` and `transform` no longer fail inside Codex's sandbox. It mounts
  `XDG_RUNTIME_DIR` read-only, so the edit lock could never be taken and every edit failed with
  `lock unavailable: Read-only file system`. The lock now falls back to the temp directory when the
  runtime directory cannot be written, with the same ownership and symlink checks.

### Removed

- The `PostToolUse` check hook (structural check, then `go build`/`go vet` after an edit) and the
  `show --outline` row in the session-start paragraph are no longer part of the default install.
  Two pre-registered side trials on the lets-best-shot candidate found no benefit from either:
  the check hook cost +0.86% [95% CI −3.46%, +3.01%], 22 pairs; the outline row cost +2.93%
  [−3.45%, +8.48%], 28 pairs. Both remain available — the check hook by wiring the entry back by
  hand, `--outline` as a `show` flag.

## [0.0.2] - 2026-09-25

### Added

- `find` footer names every directory it skipped and why: `ignored dirs (gitignore target/ ·
  hidden .config/)`.
- A `find` with 10 or fewer hits in 3 or fewer files, and no `-A`/`-B`/`-C`/`--files`/`--count`/
  `--json`, expands each hit to its smallest enclosing function or, when that has no symbol or
  runs over 40 lines, to ±5 lines; `--no-expand` turns it off.
- Over the hit cap, `find` prints the top files by hit count, then the first 10 hits of the
  busiest file, before the over-cap error.
- `find` matches case-insensitively when the pattern has no uppercase letter, as `rg --smart-case`
  does; `-s`/`--case-sensitive` forces exact case.
- `edit --help` documents the `--from -` batch format, and `lets guide` gives it in two lines.
- `find --exclude GLOB` (repeatable) prunes a path from the walk, exactly `-g '!GLOB'`, in the
  order given relative to other `-g`/`--exclude` flags.
- `edit FILE --from -` applies a headerless batch (no `@@` line) to `FILE`, as if stdin opened
  `@@ FILE`; a positional target together with an `@@` header on stdin is refused with a message
  that says to pick one.

### Changed

- Text footers no longer print a `~N tokens` estimate; `--json`/`--jsonl` still carry it in
  `stats.tokens_est`.
- `show`'s default window drops from 200 to 100 lines.
- An `edit`'s echoed diff keeps 1 line of context instead of 2; a changed span over 6 lines
  shows only its first and last 2 lines and names the gap. A reverted edit (checker failure)
  echoes every line it tried to write, since those lines were never committed to disk.
- The `PreToolUse` hook rewrites an exact shell read into the equivalent `lets show`, including
  each exact-read segment inside a `&&`/`||`/`;` chain, instead of denying the whole command; it
  steps aside wherever a Read/Edit deny or ask rule in settings already covers the path. The
  installed hook command is guarded by `command -v lets` so a missing or broken install never
  blocks a command.
- The `PreToolUse` hook also rewrites an exact `grep`/`rg` search into `lets find`, alone or as a
  segment of a chain, unless a later `&&`, `||` or `set -e` reads its exit status — `lets find`
  exits 1 over its hit cap and when every hit lands in a skipped file, where `grep`/`rg` exit 0.
- The Claude Code `SessionStart` paragraph is shorter and gives a runnable batch-edit example
  again.

### Fixed

- `hooks install` recognises an earlier install's `SessionStart` hook (including v0.0.1's) by
  more than exact text, so upgrading no longer leaves both the old and new paragraph installed.
- `transform` resolves an unquoted dotted key to an existing literal key instead of only the
  nested reading, and exits 2 naming every candidate when the key is ambiguous.
- A TOML table's span ends at its own last key, not the next table's header, so a comment
  introducing the next table stays with that table instead of the one before it.
- A batch `@@` header now covers every edit block listed under it, where before a second block
  under one header was rejected.

### Performance

- `find` walks the tree once, applying its own ignore matchers instead of a second pass.
- `mimalloc` is the global allocator on musl release builds.
- The x86_64 musl binary links non-PIE.
- `show` borrows line text instead of copying it, and `edit`/`transform`/`show` size their output
  buffers up front instead of growing them.

## [0.0.1] - 2026-09-23

### Added

- `show`, `find` (alias `locate`), `edit`, `transform` and `write` verbs: bounded, numbered
  output with a footer that names every omission, and byte-splice edits with a structural check
  and automatic revert on failure.
- `stats`, `guide`, `hook` and `hooks` verbs: token-estimate accounting, the `lets guide` screen,
  the `PreToolUse` command classifier, and `lets hooks install|uninstall claude-code|codex`.
- `update` verb: `lets update --check` and `lets update` against GitHub releases, refusing when a
  different `lets` is already first on `PATH`.
- `--json` and `--jsonl` output for every verb, rendered from one output model alongside text.
- `install.sh` (detects an existing install, offers per-agent hooks) and the `dist`-generated
  `lets-installer.sh` one-liner.
- Linux (glibc and musl, x86_64 and aarch64) and macOS (Intel and Apple Silicon) release
  binaries.
