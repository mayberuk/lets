#!/bin/sh
set -eu
if [ -n "${LETS_INSTALL_DIR:-}" ]; then
  dir="$LETS_INSTALL_DIR/bin"
elif [ -n "${CARGO_DIST_FORCE_INSTALL_DIR:-}" ]; then
  dir="$CARGO_DIST_FORCE_INSTALL_DIR/bin"
else
  dir="${LETS_UNMANAGED_INSTALL:?}"
fi
mkdir -p "$dir"
cp "${LETS_TEST_FAKE_LETS:?}" "$dir/lets"
chmod +x "$dir/lets"
printf 'installer wrote lets to %s\n' "$dir" >>"${LETS_TEST_LOG:?}"
