#!/bin/sh
# Stands in for `ssh` (copied into a temp dir by the tests). Traces `ARGV:$*` (and `ENV_SECRET:` with
# the value of FAKE_SECRET) to `ssh.trace` next to itself, then:
#   - remote workspace prepare (argv mentions __SYMPHONY_WORKSPACE__): fails with exit 75 for hosts
#     listed in `fail_hosts`, otherwise prints the marker line with the path from `remote_path`;
#   - Codex launch (argv mentions app-server): behaves like a one-turn fake app-server;
#   - a command containing `sleep-forever`: sleeps (timeout tests);
#   - anything else: exits 0.
dir=$(dirname "$0")
trace_file="$dir/ssh.trace"
printf 'ARGV:%s\n' "$*" >> "$trace_file"
printf 'ENV_SECRET:%s\n' "${FAKE_SECRET:-}" >> "$trace_file"

case "$*" in
  *__SYMPHONY_WORKSPACE__*)
    if [ -f "$dir/fail_hosts" ]; then
      while IFS= read -r host; do
        case "$*" in
          *" $host "*) printf '%s prepare failed\n' "$host" >&2; exit 75 ;;
        esac
      done < "$dir/fail_hosts"
    fi
    remote_path="/remote/home/.symphony-remote-workspaces/MT-SSH-WS"
    if [ -f "$dir/remote_path" ]; then
      remote_path=$(cat "$dir/remote_path")
    fi
    printf '__SYMPHONY_WORKSPACE__\t1\t%s\n' "$remote_path"
    exit 0
    ;;
  *app-server*)
    count=0
    while IFS= read -r line; do
      count=$((count + 1))
      printf 'JSON:%s\n' "$line" >> "$trace_file"
      case "$count" in
        1) printf '%s\n' '{"id":1,"result":{}}' ;;
        2) ;;
        3) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-remote"}}}' ;;
        4) printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-remote"}}}'
           printf '%s\n' '{"method":"turn/completed"}' ;;
        *) exit 0 ;;
      esac
    done
    exit 0
    ;;
  *sleep-forever*)
    sleep 30
    ;;
esac
exit 0
