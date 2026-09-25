# Changelog

All notable changes to `lets` are documented here. Format:
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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
- The Claude Code `SessionStart` paragraph is shorter and gives a runnable batch-edit example
  again.

### Fixed

- `hooks install` recognises an earlier install's `SessionStart` hook (including v0.0.1's) by
  more than exact text, so upgrading no longer leaves both the old and new paragraph installed.
- `transform` resolves an unquoted dotted key to an existing literal key instead of only the
  nested reading, and exits 2 naming every candidate when the key is ambiguous.
- A TOML table's span ends at its own last key, not the next table's header, so a comment
  introducing the next table stays with that table instead of the one before it.

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
