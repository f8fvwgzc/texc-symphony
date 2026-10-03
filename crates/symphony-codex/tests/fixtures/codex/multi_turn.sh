#!/bin/sh
# Two turns on one thread; every turn/start uses id 3. Traces client lines.
trace_file="${SYMP_TEST_CODEX_TRACE:-/dev/null}"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  case "$count" in
    1) printf '%s\n' '{"id":1,"result":{}}' ;;
    2) ;;
    3) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-multi"}}}' ;;
    4)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-a"}}}'
      printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"total":{"inputTokens":8,"outputTokens":3,"totalTokens":11}}}}'
      printf '%s\n' '{"method":"turn/completed"}'
      ;;
    5)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-b"}}}'
      printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"total":{"inputTokens":10,"outputTokens":4,"totalTokens":14}}}}'
      printf '%s\n' '{"method":"turn/completed","usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}'
      ;;
    *) exit 0 ;;
  esac
done
