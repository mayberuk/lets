#!/bin/sh
# A whole-repo run skips fixture trees and generated files; an explicit path is always checked,
# so the gate's own tests still see their violating fixtures.
# mawk has no `\<`/`\>` or `{n,}`: boundaries pad the line and bracket a non-word char.
set -eu

cd "$(git rev-parse --show-toplevel)"

MAX_BLOCK=6
DUP_MIN=40

exclude_dirs='^tests/(fixtures|cmd|hook|comments_gate)/|^docs/examples/|^bench/baselines/|^\.github/workflows/release\.yml$'

if [ "$#" -gt 0 ]; then
  files=$(find "$@" -type f 2>/dev/null | grep -E '\.(rs|sh|py|toml|ya?ml)$|(^|/)justfile$' || true)
else
  files=$(
    git ls-files --cached --others --exclude-standard \
      '*.rs' '*.sh' '*.py' '*.toml' '*.yml' '*.yaml' 'justfile' '**/justfile' |
      grep -vE "$exclude_dirs" || true
  )
fi
files=$(printf '%s\n' "$files" | grep -v '^$' | LC_ALL=C sort -u || true)
[ -n "$files" ] || exit 0

# shellcheck disable=SC2086
report=$(
  awk -v max="$MAX_BLOCK" -v dup_min="$DUP_MIN" -v todo_re='(^|[^A-Za-z])(TODO|FIXME|XXX|HACK)([^A-Za-z]|$)' '
    function flag(rule) { printf "%s:%d: %s: %s\n", FILENAME, FNR, rule, $0 }
    function close_block() {
      if (run > max) printf "%s:%d: block of %d comment lines (max %d): trim it, the reasoning belongs in the commit message\n", file, start, run, max
      run = 0
    }
    function unescaped_quotes(s,    t) {
      t = s
      gsub(/\\"/, "", t)
      gsub(/[^"]/, "", t)
      return length(t)
    }
    function trim(s) {
      sub(/^[ \t]+/, "", s)
      sub(/[ \t]+$/, "", s)
      return s
    }
    # Must agree with tests/scenarios.rs `heredoc_delimiter`.
    function heredoc_word(line,    idx, after, first, rest, endq, i, c, word) {
      idx = index(line, "<<")
      if (idx == 0) return ""
      if (substr(line, idx, 3) == "<<<") return ""
      after = substr(line, idx + 2)
      sub(/^-/, "", after)
      sub(/^[ \t]+/, "", after)
      if (after == "") return ""
      first = substr(after, 1, 1)
      if (first == "\047" || first == "\"") {
        rest = substr(after, 2)
        endq = index(rest, first)
        return (endq == 0) ? "" : substr(rest, 1, endq - 1)
      }
      word = ""
      for (i = 1; i <= length(after); i++) {
        c = substr(after, i, 1)
        if (c ~ /[A-Za-z0-9_]/) word = word c
        else break
      }
      return word
    }
    FNR == 1 {
      close_block()
      file = FILENAME
      marker = (file ~ /\.rs$/) ? "//" : "#"
      heredoc = 0
      instr = 0
    }
    # A heredoc body is data, not script comments.
    file ~ /\.sh$/ && heredoc {
      if (trim($0) == hd_tag) heredoc = 0
      next
    }
    file ~ /\.sh$/ && !heredoc {
      hd_candidate = heredoc_word($0)
      if (hd_candidate != "") { heredoc = 1; hd_tag = hd_candidate }
    }
    marker == "//" && $0 ~ /\/\/.*(^|[^A-Za-z])(TODO|FIXME|XXX|HACK)([^A-Za-z]|$)/ { flag("TODO-style marker") }
    marker == "#" && $0 ~ ("#.*" todo_re) {
      flag("TODO-style marker")
    }

    marker == "#" && $0 ~ /^[ \t]*#!/ { next }
    marker == "#" && $0 ~ /^[ \t]*#\[/ { next }
    marker == "#" && file ~ /^tests\/scenarios\/.*\/script\.sh$/ && $0 ~ /^#[ \t]*-+[ \t]*$/ { next }

    (marker == "//" && $0 ~ /^[ \t]*\/\//) || (marker == "#" && $0 ~ /^[ \t]*#/) {
      padded = " " $0 " "
      if (marker == "//") {
        if ($0 ~ /^[ \t]*\/\/+!?[ \t]*[-=*#~_][-=*#~_][-=*#~_][-=*#~_]/) flag("banner")
      } else {
        if ($0 ~ /^[ \t]*#+[ \t]*[-=*#~_][-=*#~_][-=*#~_][-=*#~_]/) flag("banner")
      }
      if (padded ~ /[^A-Za-z][Pp]hase [0-9]|[^A-Za-z][Ww]ave [0-9]|<done>|<task>|<owned>|[^A-Za-z]EARS[^A-Za-z]|[^A-Za-z]Assumption [0-9]|\.claude\/plans/) flag("plan reference")
      if (padded ~ /[^A-Za-z][Bb]efore (the|this) fix[^A-Za-z]|[^A-Za-z]the fix (for|under test)[^A-Za-z]|[^A-Za-z][Tt]his fix[^A-Za-z]|[^A-Za-z][Tt]he review[^A-Za-z\/]|[^A-Za-z]reviewer[^A-Za-z]|[^A-Za-z]test adversary[^A-Za-z]/) flag("history of the change, not a fact about the code")

      idline = padded
      gsub(/[Hh][1-6]/, "", idline)
      gsub(/[Pp](50|90|95|99)/, "", idline)
      if (idline ~ /\([A-Z][0-9][0-9]?[^A-Za-z0-9]/ || idline ~ /[^A-Za-z0-9][A-Z][0-9][0-9]?\)/ || idline ~ /[^A-Za-z0-9][A-Z][0-9][0-9]?:/) flag("standalone id")

      if ($0 ~ /\.md[ \t]+~[0-9]/) flag("doc pointer with a line number")
      if ($0 ~ /spec\.md|repo-setup\.md|examples-narrative\.md|brainstorm-living-doc\.md|briefing\.md|findings\.md|docs\/design|docs\/research|\.claude\/(plans|work|brainstorms|docs)/) flag("pointer to a non-shipping doc")

      if ($0 ~ /[Tt]ightened [0-9][0-9][0-9][0-9]-/ || padded ~ /[^A-Za-z]used to[^A-Za-z]/ || padded ~ /[^A-Za-z]no longer[^A-Za-z]/ || padded ~ /[^A-Za-z]bug [A-Z][0-9]/ || padded ~ /[^A-Za-z]the plan[^A-Za-z]/) flag("dated/history wording")

      if (marker == "//") {
        if ($0 ~ /^[ \t]*\/\/[ \t]+(let|fn|use|if|for|match|return|pub)[^A-Za-z_].*[;{][ \t]*$/) flag("commented-out code")
      } else {
        if ($0 ~ /^[ \t]*#[ \t]+(import [A-Za-z_]|from [A-Za-z_].*import )/) flag("commented-out code")
        else if ($0 ~ /^[ \t]*#[ \t]+(def|class|if|elif|for|while)[ \t(].*:[ \t]*$/) flag("commented-out code")
        else if ($0 ~ /^[ \t]*#[ \t]+(return|export|local)[ \t][A-Za-z_].*;[ \t]*$/) flag("commented-out code")
      }

      trimmed = $0
      if (marker == "//") { sub(/^[ \t]*\/\/!?[ \t]*/, "", trimmed) } else { sub(/^[ \t]*#[ \t]*/, "", trimmed) }
      if (length(trimmed) > dup_min) {
        if (trimmed in seen) flag("duplicated comment, also at " seen[trimmed])
        else seen[trimmed] = file ":" FNR
      }

      if (run == 0) start = FNR
      run++
      next
    }

    {
      close_block()
      pos = index($0, marker)
      # A `//` inside a string is content; parity carries over, as a Rust string can span lines.
      if (marker == "//") {
        qbefore = (pos > 0) ? unescaped_quotes(substr($0, 1, pos - 1)) : 0
        instr_at_marker = (instr + qbefore) % 2
      }
      if (pos > 0) {
        before = substr($0, 1, pos - 1)
        if (before !~ /^[ \t]*$/) {
          ok = 1
          if (marker == "//" && substr($0, pos - 1, 1) == ":") ok = 0
          if (marker == "//" && instr_at_marker) ok = 0
          # sh, TOML and YAML open a `#` comment only after whitespace; touching content it is data.
          if (marker == "#" && substr($0, pos - 1, 1) !~ /[ \t]/) ok = 0
          if (ok) flag("trailing comment")
        }
      }
      if (marker == "//") {
        is_url = (pos > 0 && substr($0, pos - 1, 1) == ":")
        # Past a real comment nothing opens or closes a string, so only the code before it counts.
        instr = (pos > 0 && !is_url && !instr_at_marker) ? 0 : (instr + unescaped_quotes($0)) % 2
      }
    }
    END { close_block() }
  ' $files
)

if [ -n "$report" ]; then
  printf '%s\n' "$report" >&2
  echo "comments-gate: $(printf '%s\n' "$report" | wc -l) comment(s) break .claude/rules/comments.md" >&2
  exit 1
fi
