#!/bin/sh
# Stays silent for 0.4 s after the turn starts.
count=0
while IFS= read -r _line; do
  count=$((count + 1))
  case "$count" in
    1) printf '%s\n' '{"id":1,"result":{}}' ;;
    2) ;;
    3) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-silent"}}}' ;;
    4)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-silent"}}}'
      sleep 0.4
      printf '%s\n' '{"method":"turn/completed"}'
      exit 0
      ;;
    *) exit 0 ;;
  esac
done
