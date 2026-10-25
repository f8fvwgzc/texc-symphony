#!/bin/sh
# Streams item updates 0.15 s apart for 0.75 s before completing: longer than the test's
# silence timeout (so it must reset on every update), with a wide margin per gap for slow CI.
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
      printf '%s\n' '{"method":"item/updated","params":{"item":{"id":"three"}}}'
      sleep 0.15
      printf '%s\n' '{"method":"item/updated","params":{"item":{"id":"four"}}}'
      sleep 0.15
      printf '%s\n' '{"method":"turn/completed"}'
      exit 0
      ;;
    *) exit 0 ;;
  esac
done
