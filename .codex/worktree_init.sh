#!/usr/bin/env bash
set -eo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
project_root="$(cd "$script_dir/.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required. Install Rust with rustup from https://rustup.rs (rust-toolchain.toml selects the pinned toolchain)." >&2
  exit 1
fi

if ! command -v pnpm >/dev/null 2>&1; then
  echo "pnpm is required. Install Node.js 22.12+ and run \`corepack enable\` (or see https://pnpm.io/installation)." >&2
  exit 1
fi

cd "$project_root"

# cargo fetch + pnpm install for the web dashboard.
make setup
