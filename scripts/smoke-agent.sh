#!/bin/sh
# Never CI: it costs real API calls. `--settings` loads an arm's hooks without touching the real
# HOME or its credentials. LETS_SMOKE_*_DIR let tests/smoke_agent.rs use a fake `claude`.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

command -v claude >/dev/null 2>&1 || { echo "smoke-agent: claude not on PATH" >&2; exit 1; }

bin_dir="${LETS_SMOKE_BIN_DIR:-$root/target/release}"
if [ ! -x "$bin_dir/lets" ]; then
  cargo build --release --locked
fi
PATH="$bin_dir:$PATH"
export PATH

run="$(date -u +%Y%m%dT%H%M%SZ)"
log_dir="${LETS_SMOKE_LOGS_DIR:-$root/logs}/agent-smoke/$run"
mkdir -p "$log_dir"

# The real installer writes the with-hooks settings, so that arm tests what users get.
install_home="$(mktemp -d)"
if ! env HOME="$install_home" XDG_RUNTIME_DIR="$install_home" lets hooks install claude-code \
  >"$log_dir/hooks-install.out" 2>&1; then
  echo "smoke-agent: lets hooks install claude-code failed — see $log_dir/hooks-install.out" >&2
  exit 1
fi
hooks_settings="$log_dir/hooks-settings.json"
cp "$install_home/.claude/settings.json" "$hooks_settings"
rm -rf "${install_home:?}"

{
  echo "claude-version=$(claude --version 2>&1 | head -1)"
  echo "lets-commit=$(git -C "$root" rev-parse HEAD)"
  echo "lets-binary=$bin_dir/lets"
  echo "lets-bytes=$(wc -c <"$bin_dir/lets" | tr -d ' ')"
  echo "lets-sha256=$(sha256sum "$bin_dir/lets" | cut -d' ' -f1)"
} >"$log_dir/meta.txt"

# git rewrites its index on a bare `git status`, and `run` is the agent's XDG_RUNTIME_DIR.
digest() {
  find . -path ./.git -prune -o -path ./run -prune -o -type f -exec sha256sum {} + | sort
}

task_prompt="This repository has three files: README.md, config/app.json, and src/usage.ts. Read them and answer, in your final message: (1) the numeric value of \`cap\` in src/usage.ts, (2) whether config/app.json's review.threads is greater than 1, and (3) the sentence under the 'Bottom Line' heading in README.md."

unset_secrets() {
  for name in $(env | awk -F= '/^[A-Za-z_][A-Za-z0-9_]*=/ { print $1 }'); do
    case "$name" in
      ANTHROPIC_API_KEY | ANTHROPIC_AUTH_TOKEN | CLAUDE_CODE_OAUTH_TOKEN) ;;
      AWS_*)
        [ -n "${CLAUDE_CODE_USE_BEDROCK:-}" ] || printf ' -u %s' "$name"
        ;;
      *_TOKEN | *_KEY | *_SECRET) printf ' -u %s' "$name" ;;
    esac
  done
}

for arm in baseline with-hooks; do
  work_dir="$(mktemp -d)"
  cp -R "$root/tests/fixtures/base/." "$work_dir/"
  (
    cd "$work_dir"
    git init -q
    mkdir -p "$work_dir/run"
    # The real HOME holds claude's credentials; LETS_NO_STATS and LETS_TOKEN_RATIO stay unpinned
    # so the agent sees what a user would.
    XDG_RUNTIME_DIR="$work_dir/run"
    export XDG_RUNTIME_DIR

    digest >"$log_dir/$arm.tree-before.sha"

    # Unquoted on purpose: each ` -u NAME` has to split into two arguments for `env`.
    # shellcheck disable=SC2046
    set -- $(unset_secrets) claude -p "$task_prompt" \
      --output-format stream-json --verbose \
      --setting-sources "" \
      --permission-mode bypassPermissions \
      --disallowed-tools Write,Edit,NotebookEdit
    if [ "$arm" = "with-hooks" ]; then
      set -- "$@" --settings "$hooks_settings"
    fi

    # `set -e` would abort before claude's exit status, the evidence here, is recorded.
    set +e
    env "$@" >"$log_dir/$arm.jsonl" 2>"$log_dir/$arm.stderr.log"
    echo "$?" >"$log_dir/$arm.exit"
    set -e

    digest >"$log_dir/$arm.tree-after.sha"
  )
  rm -rf "$work_dir"
done

status=0
for arm in baseline with-hooks; do
  if grep -q '"type":"result"' "$log_dir/$arm.jsonl"; then
    result_line=yes
  else
    result_line=no
    status=1
  fi
  if ! cmp -s "$log_dir/$arm.tree-before.sha" "$log_dir/$arm.tree-after.sha"; then
    echo "$arm: the fixture copy changed under a run that only reads it — compare $arm.tree-before.sha and $arm.tree-after.sha"
    status=1
  fi
  echo "$arm exit=$(cat "$log_dir/$arm.exit") bytes=$(wc -c <"$log_dir/$arm.jsonl" | tr -d ' ') result-line=$result_line"
done

echo "$log_dir"
echo "grade with scripts/smoke-judge.md"
exit "$status"
