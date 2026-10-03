#!/bin/sh
# Stands in for `ssh` but runs the remote command (the last argv element, `bash -lc '<script>'`)
# locally, so the real remote scripts (prepare, hooks, removal) are exercised. Traces `ARGV:$*` to
# `ssh.trace` next to itself.
for last; do :; done
printf 'ARGV:%s\n' "$*" >> "$(dirname "$0")/ssh.trace"
exec sh -c "$last"
