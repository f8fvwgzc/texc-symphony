#!/bin/sh
# Stands in for `ssh`: logs its argv, then acts as the remote app-server.
trace_file="${SYMP_TEST_SSH_TRACE:-/dev/null}"
printf 'ARGV:%s\n' "$*" >> "$trace_file"
printf 'ENV_SECRET:%s\n' "$LINEAR_API_KEY" >> "$trace_file"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  case "$count" in
    1) printf '%s\n' '{"id":1,"result":{}}' ;;
    2) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-remote"}}}' ;;
    3) printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-remote"}}}' ;;
    4) printf '%s\n' '{"method":"turn/completed"}'; exit 0 ;;
    *) exit 0 ;;
  esac
done
