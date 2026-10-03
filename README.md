# Symphony

Symphony turns project work into isolated, autonomous implementation runs, allowing teams to manage
work instead of supervising coding agents.

[![Symphony demo video preview](.github/media/symphony-demo-poster.jpg)](https://player.vimeo.com/video/1186371009?h=5626e4b899)

_In this [demo video](https://player.vimeo.com/video/1186371009?h=5626e4b899), Symphony monitors a Linear board for work and spawns agents to handle the tasks. The agents complete the tasks and provide proof of work: CI status, PR review feedback, complexity analysis, and walkthrough videos. When accepted, the agents land the PR safely. Engineers do not need to supervise Codex; they can manage the work at a higher level._

> [!WARNING]
> Symphony is a low-key engineering preview for testing in trusted environments.

## What it does

Symphony is a long-running service. It polls one issue tracker (Linear, GitHub Issues, GitLab,
Jira Cloud or Asana) for issues in your active states, gives each eligible issue its own workspace
directory, and runs a [Codex](https://developers.openai.com/codex/app-server/) `app-server` session
in it until the issue leaves those states. Everything is configured in one
[`WORKFLOW.md`](WORKFLOW.md) file: YAML front matter for the tracker, workspace hooks, concurrency
and Codex settings, and a Liquid template for the agent prompt. Agents reach the tracker through a
tool that Symphony executes with host-side credentials, so tracker tokens never enter the agent's
environment.

Symphony works best in codebases that have adopted
[harness engineering](https://openai.com/index/harness-engineering/).

This repository builds one self-contained Rust binary with the web dashboard embedded. The behavioural
contract is [`SPEC.md`](SPEC.md).

## Quick start

Outside Docker you need the [Codex CLI](https://developers.openai.com/codex/) installed and logged
in (`codex login`) and `git`; in every case, a token for your tracker. Then pick one way to get
`symphony`.

**A. Release binary** (Linux x86_64/arm64, macOS x86_64/arm64). Each release has raw executables
named `symphony-<vX.Y.Z|nightly>-<linux_x86_64|linux_arm64|macos_x86_64|macos_arm64>` with a
matching `.sha256` checksum file:

```sh
version=v0.1.0 target=linux_x86_64
base=https://github.com/f8fvwgzc/texc-symphony/releases/download/$version
curl -fL -O "$base/symphony-$version-$target" -O "$base/symphony-$version-$target.sha256"
shasum -a 256 -c "symphony-$version-$target.sha256"
install -m 0755 "symphony-$version-$target" /usr/local/bin/symphony
```

See [`docs/deployment.md`](docs/deployment.md) for verification, systemd and upgrades.

**B. From source with Cargo:**

```sh
cargo install --locked --path crates/symphony
```

The dashboard is embedded from `web/dist` at compile time; build it first (`make web`), otherwise
the server shows a placeholder page. `make build` (below) does both.

**C. Docker** (amd64 and arm64):

```sh
docker run --rm -p 127.0.0.1:4000:4000 -v "$PWD/WORKFLOW.md:/config/WORKFLOW.md:ro" -e LINEAR_API_KEY ghcr.io/f8fvwgzc/texc-symphony:latest
```

The image includes git and the Codex CLI. For Compose, persistent volumes, Codex login inside the
container and reverse proxies, see [`docker/README.md`](docker/README.md).

### First run

1. Copy [`WORKFLOW.md`](WORKFLOW.md) (or
   [`docker/WORKFLOW.example.md`](docker/WORKFLOW.example.md)) into your project and edit the
   tracker section, `workspace.root` and the `after_create` hook that clones your repository.
   [`docs/configuration.md`](docs/configuration.md) documents every key.
2. Export the tracker token, for example `export LINEAR_API_KEY=...`.
3. Start Symphony:

   ```sh
   symphony --i-understand-that-this-will-be-running-without-the-usual-guardrails --port 4000 ./WORKFLOW.md
   ```

4. Open <http://127.0.0.1:4000/>. Move an issue into an active state (`Todo` by default for Linear)
   and watch an agent pick it up.

The example workflow depends on the Linear statuses `Rework`, `Human Review` and `Merging`; add
them to your team's workflow in Linear, or adjust the states and the prompt. To try Symphony
without a tracker, use `tracker.kind: memory` with a list of issues in the file (see
[configuration.md](docs/configuration.md#memory)).

## Build from source

Requirements: Rust 1.99.0 (pinned by [`rust-toolchain.toml`](rust-toolchain.toml); rustup installs
it on first use), Node.js 22.12+ and pnpm (`corepack enable`).

```sh
make setup        # cargo fetch + pnpm install
make build        # web bundle (web/dist) + target/release/symphony with the dashboard embedded
make run          # build, then run ./WORKFLOW.md with the dashboard on :4000
```

`make run WORKFLOW=path/to/WORKFLOW.md PORT=8080` overrides the defaults. `make docker` builds a
local image.

## Project layout

```text
crates/
  symphony-core/      WORKFLOW.md loading and reload, config validation, issues, prompt rendering, path safety
  symphony-trackers/  tracker adapters (Linear, GitHub, GitLab, Jira, Asana, memory) and the agents' tracker tools
  symphony-codex/     Codex app-server client (JSON-RPC over stdio), events, token accounting
  symphony-store/     SQLite run history (runs, events, token usage, retention)
  symphony-runtime/   orchestrator, agent runner, workspaces and hooks, SSH workers
  symphony-server/    HTTP API, Server-Sent Events, OpenAPI document, embedded web dashboard
  symphony/           the `symphony` binary: CLI, bootstrap, logging, terminal dashboard
xtask/                repository tasks (pr-body-check)
web/                  web dashboard (Preact, Vite, TypeScript)
docker/               Dockerfile, Compose files, example workflow
docs/                 configuration, CLI, deployment, architecture, API and migration notes
SPEC.md               the Symphony specification
WORKFLOW.md           example workflow for running Symphony on this repository
elixir/               the original Elixir reference implementation, kept during the transition
```

How the crates fit together: [`docs/architecture.md`](docs/architecture.md).

## Live monitoring

- **Web dashboard** at `/` when the HTTP server runs (`--port`, `SYMPHONY_PORT` or `server.port`):
  running agents, retries, blocked issues, token usage and rate limits, plus run history.
- **Server-Sent Events** at `GET /api/v1/events`: a full state snapshot on every change. The
  dashboard falls back to polling `GET /api/v1/state` when the stream is blocked (for example by a
  buffering proxy).
- **Run history** from the SQLite store: `GET /api/v1/runs`, `/api/v1/runs/{id}`,
  `/api/v1/runs/{id}/events` and `/api/v1/totals`. History survives restarts.
- **Terminal dashboard** when stdout is a terminal (`observability.dashboard_enabled`).

The API is documented in [`docs/api/README.md`](docs/api/README.md), with the OpenAPI 3.1 contract
in [`docs/api/openapi.yaml`](docs/api/openapi.yaml); a running server also serves it at
`GET /api/openapi.json`. The HTTP API has no authentication: keep it on loopback or behind an
authenticating proxy.

## Development

```sh
make all          # = make: cargo fmt --check, cargo clippy -D warnings, cargo test, pnpm check (web)
make fmt          # format the Rust code
make test         # cargo test for the whole workspace
cargo run -p xtask -- pr-body-check --file pr_body.md   # or: make pr-body-check FILE=pr_body.md
```

`make all` is the gate CI runs (`.github/workflows/ci.yml`). Pull request descriptions follow
[`.github/pull_request_template.md`](.github/pull_request_template.md), which `pr-body-check`
validates. Repository-local Codex skills live in [`.codex/skills/`](.codex/skills/).

## Configuration and CLI

- [`docs/configuration.md`](docs/configuration.md): every `WORKFLOW.md` key, the prompt variables,
  validation errors and reload behaviour.
- [`docs/cli.md`](docs/cli.md): flags, environment variables, logging, exit codes and the
  `workspace before-remove` subcommand.
- [`docs/deployment.md`](docs/deployment.md): release binaries, Docker, systemd, reverse proxies,
  backups and upgrades.

## Rewritten from Elixir to Rust

Symphony started as an Elixir/OTP reference implementation. This repository ports it to Rust while
keeping [`SPEC.md`](SPEC.md) as the contract: the same `WORKFLOW.md` format, tracker behaviour and
HTTP API. It adds run history in SQLite, live updates over Server-Sent Events, a new web
dashboard and multi-arch container images. Behaviour differences and fixes are listed
in [`docs/migration-from-elixir.md`](docs/migration-from-elixir.md). The Elixir code stays in
[`elixir/`](elixir/) for reference during the transition.

You can also build your own: tell your favorite coding agent to implement Symphony according to
the [specification](https://github.com/openai/symphony/blob/main/SPEC.md).

## License

Licensed under the [Apache License 2.0](LICENSE). See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
