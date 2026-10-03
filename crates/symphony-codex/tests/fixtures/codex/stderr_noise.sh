#!/bin/sh
# Writes a warning to stderr before completing the turn.
count=0
while IFS= read -r _line; do
  count=$((count + 1))
  case "$count" in
    1) printf '%s\n' '{"id":1,"result":{}}' ;;
    2) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-92"}}}' ;;
    3) printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-92"}}}' ;;
    4)
      printf '%s\n' 'warning: this is stderr noise' >&2
      printf '%s\n' '{"method":"turn/completed"}'
      exit 0
      ;;
    *) exit 0 ;;
  esac
done
