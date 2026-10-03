#!/bin/sh
# Streams two item updates 0.15 s apart before completing (the silence timeout must reset).
count=0
while IFS= read -r _line; do
  count=$((count + 1))
  case "$count" in
    1) printf '%s\n' '{"id":1,"result":{}}' ;;
    2) ;;
    3) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-timeout"}}}' ;;
    4)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-timeout"}}}'
      sleep 0.15
      printf '%s\n' '{"method":"item/updated","params":{"item":{"id":"one"}}}'
      sleep 0.15
      printf '%s\n' '{"method":"item/updated","params":{"item":{"id":"two"}}}'
      sleep 0.15
      printf '%s\n' '{"method":"turn/completed"}'
      exit 0
      ;;
    *) exit 0 ;;
  esac
done
