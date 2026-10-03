#!/bin/sh
# Answers each request by line number. Responses are sent "early" (the reply to line 2, the
# `initialized` notification, is the thread/start result) to check that the client only relies on
# reading lines in order. Traces every client line as `JSON:<line>` to $SYMP_TEST_CODEX_TRACE.
trace_file="${SYMP_TEST_CODEX_TRACE:-/dev/null}"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  case "$count" in
    1) printf '%s\n' '{"id":1,"result":{}}' ;;
    2) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-1001"}}}' ;;
    3) printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-1001"}}}' ;;
    4) printf '%s\n' '{"method":"turn/completed"}'; exit 0 ;;
    *) exit 0 ;;
  esac
done
