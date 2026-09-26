# Changelog

All notable changes to `lets` are documented here. Format:
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.0.3] - 2026-09-26

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
