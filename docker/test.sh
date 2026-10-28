#!/usr/bin/env bash
# Image test: the shipped layout keeps Symphony's secrets away from its agents.
#
#   docker/test.sh [image]          # default: symphony:dev
#
# Starts the image as it ships (entrypoint, user, environment, health check) with a marker API
# token and a task-only workflow, waits for Docker's own health check to pass, then checks, from
# a process of the image's user outside any sandbox (what an unconfined agent, a Claude Code
# `Bash` call or a hook is):
#   * PID 1 is `symphony init`, and neither its environment nor Symphony's is readable;
#   * no other process has the token in a readable environment (the health probe starts with an
#     empty one), while a control process's is found (so the scan is real);
#   * an orphaned process is reaped by PID 1;
#   * `docker stop` (SIGTERM to PID 1) reaches Symphony, which stops cleanly with exit 0.
set -euo pipefail

image="${1:-symphony:dev}"
# A marker, not a real secret.
token="image-test-token-$RANDOM$RANDOM$RANDOM"
workflow="$(mktemp)"
container=""
cleanup() {
  rm -f "$workflow"
  if [ -n "$container" ]; then docker rm -f "$container" >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT
fail() {
  if [ -n "$container" ]; then docker logs "$container" >&2 || true; fi
  echo "FAIL: $*" >&2
  exit 1
}

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

# Docker runs the probe with the container's environment: it must clear it before wget starts.
probe="$(docker inspect -f '{{join .Config.Healthcheck.Test " "}}' "$container")"
case "$probe" in *"env -i wget"*) ;; *) fail "the health check keeps the environment: $probe" ;; esac

docker start "$container" >/dev/null
status=""
for _ in $(seq 1 120); do
  status="$(docker inspect -f '{{.State.Health.Status}}' "$container")"
  [ "$status" = healthy ] && break
  [ "$(docker inspect -f '{{.State.Running}}' "$container")" = true ] || break
  sleep 1
done
[ "$status" = healthy ] || fail "the health check did not pass (status: $status)"

# As the image's user, in the container's PID namespace, outside any sandbox. `docker exec` would
# give the script the container's environment (the token included), so it starts empty.
read -r -d '' checks <<'SH' || true
set -u
token=$1
fail() { echo "FAIL: $*" >&2; exit 1; }
[ "$(cat /proc/1/comm)" = symphony ] || fail "PID 1 is $(cat /proc/1/comm), not symphony init"
tr '\0' ' ' < /proc/1/cmdline | grep -q '^symphony init -- symphony ' || fail "PID 1 is not symphony init"
pids=""
for dir in /proc/[0-9]*; do
  [ "$(cat "$dir/comm" 2>/dev/null)" = symphony ] && pids="$pids ${dir#/proc/}"
done
[ "$(echo "$pids" | wc -w)" -ge 2 ] || fail "expected symphony init and symphony, found:$pids"
for pid in $pids; do
  if cat "/proc/$pid/environ" > /dev/null 2>&1; then fail "/proc/$pid/environ is readable"; fi
done
# Processes whose readable environment holds the token.
holders() {
  for dir in /proc/[0-9]*; do
    if tr '\0' '\n' < "$dir/environ" 2>/dev/null | grep -qF "$token"; then
      echo "${dir#/proc/}:$(cat "$dir/comm" 2>/dev/null)"
    fi
  done
}
env SYMPHONY_IMAGE_TEST_MARKER="$token" sleep 30 & control=$!
# Retried: until env has exec'd sleep (and while execve runs) the marker is not there yet.
found_control=no
for _ in $(seq 1 20); do
  if holders | grep -q "^$control:"; then found_control=yes; break; fi
  sleep 0.1
done
[ "$found_control" = yes ] || fail "the scan does not find the token in a readable environment"
kill $control; wait $control 2>/dev/null
found="$(holders)"
[ -z "$found" ] || fail "the token is readable in the environment of: $found"
bash -c "sleep 0.2 &"
sleep 1
for stat in /proc/[0-9]*/stat; do
  case "$(sed 's/^.*) //' "$stat" 2>/dev/null)" in "Z 1 "*) fail "PID 1 left a zombie: $stat";; esac
done
echo "ok: PID 1 is symphony init; init and symphony environments unreadable (pids$pids); no other readable environment holds the token; orphans reaped"
SH
docker exec -u 10001:10001 "$container" \
  env -i PATH=/usr/local/bin:/usr/bin:/bin bash -c "$checks" check "$token" ||
  fail "the in-container checks failed"

docker stop -t 30 "$container" >/dev/null
code="$(docker inspect -f '{{.State.ExitCode}}' "$container")"
[ "$code" = 0 ] || fail "exit code $code after docker stop (137: SIGTERM did not reach symphony)"
if docker logs "$container" 2>&1 | grep -qF "$token"; then
  fail "the API token appears in the logs"
fi
echo "ok: docker stop reached symphony (exit 0)"
