#!/bin/sh
# The initialize response is padded to ~1.1 MB (more than the BEAM's 1 MiB line chunk).
count=0
while IFS= read -r _line; do
  count=$((count + 1))
  case "$count" in
    1)
      padding=$(printf '%*s' 1100000 '' | tr ' ' a)
      printf '{"id":1,"result":{},"padding":"%s"}\n' "$padding"
      ;;
    2) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-91"}}}' ;;
    3) printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-91"}}}' ;;
    4) printf '%s\n' '{"method":"turn/completed"}'; exit 0 ;;
    *) exit 0 ;;
  esac
done
