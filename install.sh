#!/bin/sh
# curl -fsSL https://raw.githubusercontent.com/mayberuk/lets/main/install.sh | sh -s -- [options]
set -eu

REPO="https://github.com/mayberuk/lets"

VERSION_TAG=""
HOOKS_LIST=""
NO_HOOKS=""
CHECK_ONLY=""
UNINSTALL=""
ASSUME_YES=""

usage() {
  cat <<'USAGE'
Usage: install.sh [--version vX.Y.Z] [--hooks=claude-code,codex | --no-hooks] [--check] [--uninstall] [--yes]
Installs into $LETS_BIN_DIR, default ~/.local/bin.
USAGE
}

err() {
  printf '%s\n' "$1" >&2
}

have() {
  command -v "$1" >/dev/null 2>&1
}

detect_lets() {
  LETS_FOUND=0
  LETS_IS_OURS=0
  LETS_PATH=""
  if LETS_PATH=$(command -v lets 2>/dev/null); then
    LETS_FOUND=1
    if version_json=$("$LETS_PATH" version --json 2>/dev/null); then
      version=$(printf '%s' "$version_json" | sed -n 's/.*"version"[ \t]*:[ \t]*"\([^"]*\)".*/\1/p')
      case "$version" in
        [0-9]*.[0-9]*.[0-9]*) LETS_IS_OURS=1 ;;
      esac
    fi
  fi
}

detect_claude() {
  have claude || [ -d "${HOME:-}/.claude" ]
}

detect_codex() {
  have codex || [ -d "${CODEX_HOME:-${HOME:-}/.codex}" ]
}

# Per-command redirects, not `exec`: a missing /dev/tty fails the command, not the shell, and
# `2>/dev/null` comes first to silence the shell's own "cannot open".
ask_yes_no() {
  ASK_REPLY=""
  printf '%s' "$1" 2>/dev/null >/dev/tty && IFS= read -r ASK_REPLY 2>/dev/null </dev/tty
}

agent_step() {
  agent="$1"
  label="$2"
  if [ -n "$NO_HOOKS" ]; then
    return 0
  fi
  if [ -n "$HOOKS_LIST" ]; then
    case ",$HOOKS_LIST," in
      *",$agent,"*) "$LETS_BIN" hooks install "$agent" ;;
    esac
    return 0
  fi
  if [ -n "$ASSUME_YES" ]; then
    "$LETS_BIN" hooks install "$agent"
    return 0
  fi
  if ask_yes_no "Install lets hooks for $label? [y/N] "; then
    case "$ASK_REPLY" in
      [Yy]*) "$LETS_BIN" hooks install "$agent" ;;
    esac
  else
    printf 'lets hooks install %s\n' "$agent"
  fi
}

hooks_step() {
  [ -n "${LETS_BIN:-}" ] || return 0
  if detect_claude; then
    agent_step claude-code "Claude Code"
  fi
  if detect_codex; then
    agent_step codex "Codex"
  fi
}

installer_url() {
  if [ -n "$VERSION_TAG" ]; then
    printf '%s/releases/download/%s/lets-installer.sh' "$REPO" "$VERSION_TAG"
  else
    printf '%s/releases/latest/download/lets-installer.sh' "$REPO"
  fi
}

# Download, then run, never `curl | sh`: `sh` exits 0 on the empty input a failed download gives.
fresh_install() {
  url=$(installer_url)
  scratch=$(mktemp -d)
  script="$scratch/lets-installer.sh"
  if ! curl --proto '=https' --tlsv1.2 --fail -sSL -o "$script" "$url"; then
    err "downloading $url failed"
    rm -rf "$scratch"
    exit 1
  fi
  install_dir="${LETS_BIN_DIR:-$HOME/.local/bin}"
  # dist's installer reads these two before LETS_UNMANAGED_INSTALL, as a prefix it appends bin/ to.
  (
    unset LETS_INSTALL_DIR CARGO_DIST_FORCE_INSTALL_DIR
    LETS_UNMANAGED_INSTALL="$install_dir" sh "$script"
  )
  rm -rf "$scratch"

  if resolved=$(command -v lets 2>/dev/null); then
    LETS_BIN="$resolved"
  elif [ -x "$install_dir/lets" ]; then
    LETS_BIN="$install_dir/lets"
    printf '%s is not on PATH yet; add this line to your shell profile:\n' "$install_dir"
    # shellcheck disable=SC2016
    printf '  export PATH="%s:$PATH"\n' "$install_dir"
  else
    err "the installer ran but no lets binary was found in $install_dir"
    exit 1
  fi
}

refuse_foreign() {
  err "a \`lets\` already on PATH at $LETS_PATH is not this project's build - remove it, or put the right one earlier on PATH"
  exit 1
}

update_existing() {
  LETS_BIN="$LETS_PATH"
  if [ -n "$CHECK_ONLY" ]; then
    set +e
    "$LETS_BIN" update --check
    code=$?
    set -e
    exit "$code"
  fi
  set +e
  "$LETS_BIN" update --check
  code=$?
  set -e
  case "$code" in
    0) ;;
    1) "$LETS_BIN" update ;;
    *) exit "$code" ;;
  esac
}

do_install() {
  detect_lets
  if [ "$LETS_FOUND" -eq 0 ]; then
    if [ -n "$CHECK_ONLY" ]; then
      err "no lets on PATH to check"
      exit 1
    fi
    fresh_install
  elif [ "$LETS_IS_OURS" -eq 1 ]; then
    update_existing
  else
    refuse_foreign
  fi
  hooks_step
}

do_uninstall() {
  detect_lets
  if [ "$LETS_FOUND" -eq 0 ]; then
    err "no lets on PATH to uninstall"
    exit 1
  fi
  if [ "$LETS_IS_OURS" -eq 0 ]; then
    refuse_foreign
  fi
  if detect_claude; then
    "$LETS_PATH" hooks uninstall claude-code
  fi
  if detect_codex; then
    "$LETS_PATH" hooks uninstall codex
  fi
  if [ -e "$LETS_PATH" ]; then
    rm -f "$LETS_PATH"
    printf 'removed %s\n' "$LETS_PATH"
  fi
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      [ $# -ge 2 ] || { err "--version needs a value"; exit 64; }
      VERSION_TAG="$2"
      shift 2
      ;;
    --version=*)
      VERSION_TAG="${1#--version=}"
      shift
      ;;
    --hooks=*)
      HOOKS_LIST="${1#--hooks=}"
      shift
      ;;
    --no-hooks)
      NO_HOOKS=1
      shift
      ;;
    --check)
      CHECK_ONLY=1
      shift
      ;;
    --uninstall)
      UNINSTALL=1
      shift
      ;;
    --yes)
      ASSUME_YES=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      err "unknown argument: $1"
      usage >&2
      exit 64
      ;;
  esac
done

if [ -n "$UNINSTALL" ]; then
  do_uninstall
else
  do_install
fi
