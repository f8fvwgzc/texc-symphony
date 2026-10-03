#!/bin/sh
# Prints a truncated JSON protocol line, plain text, then a valid turn/completed.
count=0
while IFS= read -r _line; do
  count=$((count + 1))
  case "$count" in
    1) printf '%s\n' '{"id":1,"result":{}}' ;;
    2) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-93"}}}' ;;
    3) printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-93"}}}' ;;
    4)
      printf '%s\n' '{"method":"turn/completed"'
      printf '%s\n' 'plain progress text'
      printf '%s\n' '[1,2]'
      printf '%s\n' '{"method":"turn/completed"}'
      exit 0
      ;;
    *) exit 0 ;;
  esac
done
