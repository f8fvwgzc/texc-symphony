#!/bin/sh
# Normal handshake, then after the turn/start result sends $FAKE_CODEX_MESSAGE (a server request or
# notification). The next client line (the reply, if any) completes the turn.
trace_file="${SYMP_TEST_CODEX_TRACE:-/dev/null}"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  case "$count" in
    1) printf '%s\n' '{"id":1,"result":{}}' ;;
    2) ;;
    3) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-89"}}}' ;;
    4)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-89"}}}'
      printf '%s\n' "$FAKE_CODEX_MESSAGE"
      ;;
    5) printf '%s\n' '{"method":"turn/completed"}'; exit 0 ;;
    *) exit 0 ;;
  esac
done
