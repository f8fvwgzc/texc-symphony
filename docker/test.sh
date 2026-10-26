#!/usr/bin/env bash
# Image test: the shipped layout keeps Symphony's secrets away from its agents.
#
#   docker/test.sh [image]          # default: symphony:dev
#
# Starts the image as it ships (entrypoint, user, environment) with a marker API token and a
# task-only workflow, then checks, from a process of the image's user outside any sandbox (what an
# unconfined agent, a Claude Code `Bash` call or a hook is):
#   * PID 1 is `symphony init`, and neither its environment nor Symphony's is readable, while an
#     ordinary process's environment is (so the denial is real);
#   * an orphaned process is reaped by PID 1;
#   * `docker stop` (SIGTERM to PID 1) reaches Symphony, which stops cleanly with exit 0.
set -euo pipefail

image="${1:-symphony:dev}"
token="image-test-token-$RANDOM$RANDOM$RANDOM"
workflow="$(mktemp)"
container=""
cleanup() {
  rm -f "$workflow"
  if [ -n "$container" ]; then docker rm -f "$container" >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT

cat >"$workflow" <<'EOF'
---
tracker:
  kind: none
server:
  port: 4000
  api_token: $SYMPHONY_API_TOKEN
---
Image test.
EOF
# mktemp makes it 0600 and `docker cp` keeps the mode; the image's user must read it.
chmod 0644 "$workflow"

container="$(docker create -e SYMPHONY_API_TOKEN="$token" "$image" \
  --i-understand-that-this-will-be-running-without-the-usual-guardrails /config/WORKFLOW.md)"
docker cp "$workflow" "$container:/config/WORKFLOW.md"
docker start "$container" >/dev/null

healthy=false
for _ in $(seq 1 60); do
  if docker exec "$container" wget -q -T 2 -O /dev/null http://127.0.0.1:4000/api/v1/health 2>/dev/null; then
    healthy=true
    break
  fi
  [ "$(docker inspect -f '{{.State.Running}}' "$container")" = true ] || break
  sleep 1
done
if [ "$healthy" != true ]; then
  docker logs "$container" >&2
  echo "FAIL: symphony did not become healthy" >&2
  exit 1
fi

# As the image's user, in the container's PID namespace, outside any sandbox.
docker exec -u 10001:10001 "$container" bash -c '
  set -u
  fail() { echo "FAIL: $*" >&2; exit 1; }
  [ "$(cat /proc/1/comm)" = symphony ] || fail "PID 1 is $(cat /proc/1/comm), not symphony init"
  tr "\0" " " < /proc/1/cmdline | grep -q "^symphony init -- symphony " || fail "PID 1 is not symphony init"
  pids=""
  for dir in /proc/[0-9]*; do
    [ "$(cat "$dir/comm" 2>/dev/null)" = symphony ] && pids="$pids ${dir#/proc/}"
  done
  [ "$(echo $pids | wc -w)" -ge 2 ] || fail "expected symphony init and symphony, found:$pids"
  sleep 30 & control=$!
  tr "\0" "\n" < /proc/$control/environ | grep -q "^PATH=" || fail "an ordinary environment is unreadable"
  kill $control; wait $control 2>/dev/null
  for pid in $pids; do
    if cat /proc/$pid/environ > /dev/null 2>&1; then fail "/proc/$pid/environ is readable"; fi
  done
  bash -c "sleep 0.2 &"
  sleep 1
  for stat in /proc/[0-9]*/stat; do
    case "$(sed "s/^.*) //" "$stat" 2>/dev/null)" in "Z 1 "*) fail "PID 1 left a zombie: $stat";; esac
  done
  echo "ok: PID 1 is symphony init; init and symphony environments unreadable (pids$pids); orphans reaped"
'

docker stop -t 30 "$container" >/dev/null
code="$(docker inspect -f '{{.State.ExitCode}}' "$container")"
if [ "$code" != 0 ]; then
  docker logs "$container" >&2
  echo "FAIL: exit code $code after docker stop (137: SIGTERM did not reach symphony)" >&2
  exit 1
fi
if docker logs "$container" 2>&1 | grep -q "$token"; then
  echo "FAIL: the API token appears in the logs" >&2
  exit 1
fi
echo "ok: docker stop reached symphony (exit 0)"
