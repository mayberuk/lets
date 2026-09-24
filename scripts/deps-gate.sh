#!/bin/sh
# The grammar set counts as one entry: bundling them was one decision. Every table that ships in
# the binary counts, build and target-specific ones too; dev-dependencies never ship.
set -eu

LIMIT=18

manifest="${1:-$(dirname "$0")/../Cargo.toml}"
if [ ! -f "$manifest" ]; then
  echo "deps-gate: cannot find $manifest" >&2
  exit 1
fi

list=$(
  awk '
    /^\[/ {
      in_deps = ($0 == "[dependencies]" ||
        $0 == "[build-dependencies]" ||
        $0 ~ /^\[target\.[^]]+\.(dependencies|build-dependencies)\]$/)
      if ($0 ~ /^\[dependencies\.[^]]+\]$/ ||
          $0 ~ /^\[build-dependencies\.[^]]+\]$/ ||
          $0 ~ /^\[target\.[^]]+\.(dependencies|build-dependencies)\.[^]]+\]$/) {
        name = $0
        sub(/^\[/, "", name)
        sub(/\]$/, "", name)
        sub(/^.*\./, "", name)
        print name
      }
      next
    }
    !in_deps { next }
    /^[A-Za-z0-9_.-]+[ \t]*=/ {
      name = $0
      sub(/[ \t]*=.*/, "", name)
      print name
    }
  ' "$manifest" |
    awk '
      $0 ~ /^tree-sitter-./ && $0 != "tree-sitter-language" { grammars++; next }
      { print }
      END { if (grammars) printf "tree-sitter-* (%d grammar crates)\n", grammars }
    ' |
    LC_ALL=C sort
)

count=$(printf '%s\n' "$list" | grep -c .)

printf '%s\n' "$list" | sed 's/^/  /'
printf 'deps-gate: %d direct dependencies (limit %d)\n' "$count" "$LIMIT"

if [ "$count" -gt "$LIMIT" ]; then
  echo "deps-gate: over the limit — an unlisted dependency is a proposal to the plan, not an install" >&2
  exit 1
fi
