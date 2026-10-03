# Symphony (Rust)

This repository contains the Rust implementation of the Symphony orchestration service: it polls an
issue tracker, creates per-issue workspaces, runs Codex in app-server mode, and serves a live
dashboard and JSON API.

## Environment

- Rust `1.99.0` (pinned in `rust-toolchain.toml`; `rustup` installs it automatically).
- Node.js 22 + pnpm (version pinned in `web/package.json` `packageManager`) for the web dashboard.
- Main quality gate: `make all` (Rust fmt check, clippy `-D warnings`, tests, web typecheck/lint/test/build).

## Layout

- `crates/symphony-core` — `WORKFLOW.md` loading and reload, config casting/validation, prompts, path safety.
- `crates/symphony-trackers` — `Tracker` trait, HTTP transport, Linear/GitHub/GitLab/Jira/Asana/Memory, agent tools.
- `crates/symphony-codex` — Codex app-server client, events, dynamic tools, token accounting.
- `crates/symphony-runtime` — orchestrator actor, agent runner, workspaces and hooks, SSH workers.
- `crates/symphony-store` — SQLite run history (optional).
- `crates/symphony-server` — HTTP API, SSE stream, embedded web UI, OpenAPI.
- `crates/symphony` — the `symphony` binary: CLI, bootstrap, terminal status dashboard.
- `xtask` — repository tooling (`pr-body-check`). `web/` — dashboard. `docs/` — reference docs.

Dependency direction: `symphony` → `server` → `runtime` → {`codex`, `trackers`, `store`} → `core`.

## Codebase-Specific Conventions

- Runtime config comes from `WORKFLOW.md` front matter through `symphony-core` (`WorkflowStore`,
  `Settings`). Add config access there instead of ad-hoc env reads.
- Keep the implementation aligned with [`SPEC.md`](SPEC.md) where practical.
  - The implementation may be a superset of the spec.
  - The implementation must not conflict with the spec.
  - If a change meaningfully alters intended behavior, update the spec in the same change.
- The HTTP API contract is [`docs/api/openapi.yaml`](docs/api/openapi.yaml); change the contract and
  the server (and its contract test) together.
- Workspace safety is critical:
  - Never run a Codex turn with its cwd in the source repository.
  - Workspaces must stay under the configured workspace root.
- Orchestrator behavior is stateful and concurrency-sensitive; preserve retry, reconciliation, and
  cleanup semantics. Child processes (codex, ssh, hooks) run in their own process group and must be
  killed and awaited before workspace cleanup.
- Simplicity is a project constraint: prefer the smallest coherent design with one clear owner and
  invariant. Push back on extra abstractions, duplicated policy, and speculative flexibility.
- For stateful changes, check startup, reload, restart, and failure recovery together before editing.
- Follow [`docs/logging.md`](docs/logging.md) for logging conventions and required issue/session fields.
- Never log secrets; tracker tokens are scrubbed in errors and stripped from child environments.

## Tests and Validation

Run targeted tests while iterating, then run the full gate before handoff.

- Prefer narrow tests that exercise real tasks and observable behavior (fake app-server scripts,
  `wiremock`, `tokio::time::pause()`) over mock-only or broad end-to-end coverage.
- For non-trivial changes, use an adversarial review early to challenge complexity and try to break
  adjacent lifecycle paths; a reproducible failure blocks landing even if other reviews are clean.
- If tests need repeated global restarts or bespoke cleanup, first fix the shared harness or
  ownership boundary.

```bash
cargo test -p <crate>     # while iterating
make all                  # before handoff
```

## Required Rules

- No `unsafe` (the workspace forbids it). No `unwrap()`/`expect()` in non-test code except for
  impossible states, with a comment explaining why.
- Public items are documented. Errors are typed (`thiserror`) and keep their stable snake_case tags.
- Dependencies use the latest stable release, pinned exactly in `[workspace.dependencies]`.
- Evaluate proposed directions instead of agreeing reflexively; surface simpler designs and material
  trade-offs early.
- Keep changes narrowly scoped; avoid unrelated refactors. Follow the existing module and style patterns.

## PR Requirements

- The PR body must follow [`.github/pull_request_template.md`](.github/pull_request_template.md) exactly.
- Validate a PR body locally when needed:

```bash
cargo run -p xtask -- pr-body-check --file /path/to/pr_body.md
```

## Docs Update Policy

If behavior or config changes, update docs in the same PR:

- `README.md` for the project concept, quick start and goals.
- `WORKFLOW.md` and `docs/configuration.md` for workflow/config contract changes.
- `docs/cli.md`, `docs/deployment.md`, `docs/api/` for CLI, deployment and API changes.
