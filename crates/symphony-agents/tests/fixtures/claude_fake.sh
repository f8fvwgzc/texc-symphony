#!/usr/bin/env bash
# Fake Claude Code CLI for the claude_code backend tests.
#
# Turn N (counted in $FAKE_CLAUDE_DIR/count) records its argv (one per line), stdin and
# environment as argv.N, stdin.N and env.N, then runs $FAKE_CLAUDE_DIR/turn.N.sh when it exists,
# else prints $FAKE_CLAUDE_DIR/turn.N.jsonl (or turn.jsonl).
set -u
dir="${FAKE_CLAUDE_DIR:?FAKE_CLAUDE_DIR is not set}"
n=$(( $(cat "$dir/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$dir/count"
printf '%s\n' "$@" > "$dir/argv.$n"
cat > "$dir/stdin.$n"
env > "$dir/env.$n"
if [ -f "$dir/turn.$n.sh" ]; then
  # shellcheck disable=SC1090
  . "$dir/turn.$n.sh"
elif [ -f "$dir/turn.$n.jsonl" ]; then
  cat "$dir/turn.$n.jsonl"
else
  cat "$dir/turn.jsonl"
fi
