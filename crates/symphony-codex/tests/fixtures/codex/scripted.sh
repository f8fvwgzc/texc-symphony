#!/bin/sh
# Generic fake app-server driven by a script file ($1). Each script line is "<n> <action>" and runs
# after client line <n> is read ("0 <action>" runs at startup). Actions:
#   @exit <code>      exit immediately
#   @sleep <secs>     sleep
#   @stderr <text>    print <text> to stderr
#   @spawn <pidfile>  start a detached `sleep 60` in this process group and record its pid
#   @ignore-eof       after stdin closes, keep running instead of exiting
#   <anything else>   printed verbatim to stdout (one line)
# Client lines are traced as `JSON:<line>` to $SYMP_TEST_CODEX_TRACE.
script="$1"
trace_file="${SYMP_TEST_CODEX_TRACE:-/dev/null}"
actions=$(mktemp "${TMPDIR:-/tmp}/scripted-codex.XXXXXX")
ignore_eof=0

run_actions() {
  grep "^$1 " "$script" | cut -d' ' -f2- > "$actions"
  while IFS= read -r action; do
    case "$action" in
      "@exit "*) rm -f "$actions"; exit "${action#@exit }" ;;
      "@sleep "*) sleep "${action#@sleep }" ;;
      "@stderr "*) printf '%s\n' "${action#@stderr }" >&2 ;;
      "@spawn "*) sleep 60 </dev/null >/dev/null 2>&1 & printf '%s\n' "$!" > "${action#@spawn }" ;;
      "@ignore-eof") ignore_eof=1 ;;
      *) printf '%s\n' "$action" ;;
    esac
  done < "$actions"
}

run_actions 0
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  run_actions "$count"
done
rm -f "$actions"
if [ "$ignore_eof" = 1 ]; then
  while :; do sleep 1; done
fi
exit 0
