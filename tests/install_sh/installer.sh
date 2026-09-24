#!/bin/sh
set -eu
dir="${LETS_UNMANAGED_INSTALL:?}"
mkdir -p "$dir"
cp "${LETS_TEST_FAKE_LETS:?}" "$dir/lets"
chmod +x "$dir/lets"
printf 'installer wrote lets to %s\n' "$dir" >>"${LETS_TEST_LOG:?}"
