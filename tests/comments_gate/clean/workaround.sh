#!/usr/bin/env sh
# Retries three times: the sandboxed runner's tmpfs occasionally reports ENOENT right after
# creation, and each retry re-stats rather than trusting the first result.
set -eu
