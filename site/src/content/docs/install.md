---
title: Install and update
description: The install.sh one-liner and its flags, the dist installer, and lets update / lets version.
order: 10
group: Setup
commands: [update, version]
---

# Install and update

```console
$ curl -fsSL https://raw.githubusercontent.com/mayberuk/lets/main/install.sh | sh
```

`install.sh` detects an existing `lets`, updates it in place if it's this project's build, and
offers to wire up agent hooks. It refuses, naming the one it found, if a different `lets` is
already first on `PATH`. Its own flags:

```
Usage: install.sh [--version vX.Y.Z] [--hooks=claude-code,codex | --no-hooks] [--check] [--uninstall] [--yes]
Installs into $LETS_BIN_DIR, default ~/.local/bin.
```

- `--version vX.Y.Z` installs a specific release instead of the latest.
- `--hooks=claude-code,codex` installs the named agent hooks non-interactively; `--no-hooks`
  skips hook setup entirely. With neither given, the script asks interactively (see `/docs/hooks/`
  for what each hook does).
- `--check` reports whether a newer release exists (via `lets update --check`) without installing
  anything.
- `--uninstall` removes any agent hooks this script installed, then deletes the binary.
- `--yes` assumes yes to every interactive prompt, for non-interactive installs.

Linux and macOS.

Or fetch a release binary directly, the `dist`-generated one-liner used by `install.sh` itself
under the hood:

```console
$ curl --proto '=https' --tlsv1.2 -LsSf https://github.com/mayberuk/lets/releases/latest/download/lets-installer.sh | sh
```

## `lets update`

```
lets update [OPTIONS]
```

| Flag | Meaning | Default |
|---|---|---|
| `--check` | report whether a newer release exists, without installing it | off |
| `--force` | reinstall the latest release even when this one is already it | off |

`update` refuses, naming the one it found, when a different `lets` comes first on `PATH`.
`update --check` only compares versions against the latest release and never touches `PATH`, so
it does not refuse this way.

| Exit | Slug | Means |
|---|---|---|
| 0 | — | up to date, or updated |
| 1 | `update_available` | `--check` found a newer release; nothing was installed |
| 1 | `path_conflict` | a different `lets` comes first on `PATH`; named in the message |
| 1 | `no_repository` | this build names no release repository |
| 7 | `update_failed` | no release for this platform, the download failed, or the installer exited non-zero |

```console
$ lets update --check
lets 0.0.1 is the latest release

$ lets update --check
? 1
lets 0.0.1 · latest 999.0.0
lets 0.0.1 · latest 999.0.0 · run `lets update`
ERROR_CODE=update_available

$ lets update
replaced with the latest release
```

## `lets version`

```console
$ lets version
lets 0.0.1

$ lets version --json
{"version":"0.0.1"}
```

`--json`/`--jsonl` carry the version string alone, with no `omitted`/`stats` — see `/docs/json/`.

## Agent hooks

`install.sh` can wire up an agent's own settings during install (`--hooks=`/`--no-hooks`/`--yes`
above), or it can be done separately at any time with `lets hooks install claude-code` or `lets
hooks install codex`. See `/docs/hooks/` for what each hook does and how to uninstall it.

## Uninstall

```console
$ curl -fsSL https://raw.githubusercontent.com/mayberuk/lets/main/install.sh | sh -s -- --uninstall
```

Removes any agent hooks `install.sh` added, then deletes the binary from `$LETS_BIN_DIR`.
