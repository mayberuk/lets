# Changelog

All notable changes to `lets` are documented here. Format:
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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
