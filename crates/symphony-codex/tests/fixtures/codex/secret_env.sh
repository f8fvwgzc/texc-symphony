#!/bin/sh
# Records whether the login profile ran and whether tracker secrets reached the child.
trace_file="$SYMP_TEST_CODEX_TRACE"
printf 'PROFILE_LOADED:%s\n' "$SYMP_TEST_BASH_PROFILE_LOADED" >> "$trace_file"
printf 'CANONICAL_SECRET:%s\n' "$LINEAR_API_KEY" >> "$trace_file"
printf 'CUSTOM_SECRET:%s\n' "$SYMP_CUSTOM_LINEAR_API_KEY" >> "$trace_file"
count=0
while IFS= read -r _line; do
  count=$((count + 1))
  case "$count" in
    1) printf '%s\n' '{"id":1,"result":{}}' ;;
    2) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-secret"}}}' ;;
    3) printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-secret"}}}' ;;
    4) printf '%s\n' '{"method":"turn/completed"}'; exit 0 ;;
    *) exit 0 ;;
  esac
done
