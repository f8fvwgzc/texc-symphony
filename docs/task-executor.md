# texc as a standalone task executor

This page is the roadmap for using Symphony purely as a **task executor**: a service any team can
clone, run and call over REST, a Rust client, or gRPC to queue work for an AI agent and control it
while it runs. It records what exists today and what is still to build. It is a living plan, not a
contract; the behavioural contract stays [`SPEC.md`](../SPEC.md).

Run it as a task executor with `tracker.kind: none` and the run-history store on (the default). No
issue tracker is involved; work comes only from the task API.

## What exists today

### Clone and build with no credentials

As of the Tutela removal, every dependency is on crates.io. `git clone` then `make build` needs no
SSH key, no private repository and no secret. Authentication to the HTTP API is one bearer token
(`server.api_token` / `SYMPHONY_API_TOKEN`); without one the API is open on loopback only.

### Agent backends (the adapters)

Work runs on one of three backends behind the `AgentBackend` / `AgentSession` traits
([`crates/symphony-agents/src/lib.rs`](../crates/symphony-agents/src/lib.rs)):

- **`codex`** — the Codex `app-server`, over JSON-RPC on stdio ([`crates/symphony-codex`](../crates/symphony-codex)).
- **`claude_code`** — the Claude Code CLI, one process per turn.
- **`api`** — Symphony's own agent loop over any Anthropic Messages or OpenAI-compatible Chat
  Completions endpoint, with workspace tools and a sandboxed shell.

A new backend is an `AgentBackend` implementation; a new OpenAI-compatible provider is config.

### REST API (`crates/symphony-server`)

The task endpoints under `/api/v1` ([`docs/tasks.md`](tasks.md), [`docs/api/openapi.yaml`](api/openapi.yaml)):

| Action | Endpoint | Status |
|---|---|---|
| Queue a task (prompt, repo, backend, limits, dependencies) | `POST /api/v1/tasks` | ✅ |
| Plan a set of tasks (order, waves) without running | `POST /api/v1/tasks/plan` | ✅ |
| List / read tasks | `GET /api/v1/tasks`, `GET /api/v1/tasks/{id}` | ✅ |
| Cancel a running, queued or blocked task | `POST /api/v1/tasks/{id}/cancel` | ✅ |
| Unblock / re-queue, with an operator reply appended | `POST /api/v1/tasks/{id}/unblock` | ✅ |
| Watch a task and its run events over SSE | `GET /api/v1/tasks/{id}/events` | ✅ |
| Global kill switch (pause/resume all dispatch) | `POST /api/v1/pause` | ✅ |
| Live state snapshot / push | `GET /api/v1/state`, `GET /api/v1/events` | ✅ |

### Rust client

[`symphony-tasks-client`](../crates/symphony-tasks-client) wraps the API: `create`, `wait`,
`cancel`, `unblock`, `plan`, and an SSE watch.

### Automatic running, checking and retry (exists)

The orchestrator already does, per task:

- **Run and check:** a task moves `queued → running → succeeded | failed | blocked`; `waiting`
  while a dependency is unfinished; it fails if a dependency fails.
- **Automatic retry:** a per-task retry policy (`retry.max_attempts`, backoff) retries failures of
  the kinds it lists; a stall or a crashed replica is recovered as `interrupted`.
- **Provider limits wait:** a run a provider's rate/usage limit refused before any work waits and
  retries without counting an attempt (issue runs and, on this branch, task runs).
- **Stop / cancel:** `cancel` stops a running task's agent (process group killed) and marks it
  cancelled.
- **Multi-replica safety:** PostgreSQL dispatch leases, fenced by owner and attempt, so two
  replicas never run one task at once.

The control surface the server calls is the `ControlPlane` trait
([`crates/symphony-server/src/control.rs`](../crates/symphony-server/src/control.rs)):
`state`, `refresh`, `cancel_task`, `unblock_task`, `set_paused`.

## Remaining features (to build)

### 1. Live message injection into a running Codex session

**Goal:** `POST /api/v1/tasks/{id}/message {"text": "...", "interrupt": false}` reaches the agent
*while its turn runs* — add context to the live turn (steer), or stop the turn and start a new one
with the message (interrupt).

**State:** the Codex protocol supports it (`turn/steer`, `turn/interrupt`, verified against
codex-cli 0.160.0). The session plumbing is written on branch `feat/codex-steer` (an `Inbox`,
`AppServerSession::run_turn_steerable`, `AgentSession::can_steer`). **Not merged:** one turn-timeout
test regressed (the `select!` races the per-turn timeout) and the end-to-end wiring is not done.

**Remaining work:**
- Fix the turn-timeout regression on `feat/codex-steer`, then merge it.
- Orchestrator: give each running worker an `InboxSender`, keyed by task id, in its `WorkerContext`;
  the runner passes the inbox to `run_turn_steerable` and drains it between turns.
- `ControlPlane::send_message(id, text, interrupt)` → the orchestrator finds the running worker and
  sends on its inbox (requeue if the task is not running yet).
- REST endpoint and OpenAPI entry; SSE already shows the delivered-message event.
- `claude_code` and `api` backends cannot steer mid-turn: `can_steer` is false, the message is
  delivered as the next turn's prompt. Document that difference.

### 2. gRPC surface for every task action

**Goal:** the same actions as REST over gRPC, so non-Rust services get a typed client.

**State:** none exists. An earlier initiative scoped it (async-graphql + tonic over one shared
service layer); nothing was built.

**Remaining work:**
- Extract the task validation and dispatch now in `symphony-server/src/tasks.rs` into a transport-
  neutral service module (REST and gRPC call the same functions).
- `symphony.proto`: `CreateTask`, `GetTask`, `ListTasks`, `Cancel`, `Unblock`, `SendMessage`, and a
  server-streaming `Watch` (the SSE equivalent).
- A `tonic` server that shares the port (axum + tonic over hyper's HTTP/1+2), behind the same
  bearer token.
- A contract test over identical fixtures so REST and gRPC cannot drift.

### 3. Per-task pause / resume

**Goal:** pause one running task (its agent stops after the current turn) and resume it, distinct
from the global kill switch.

**State:** only the global kill switch exists (`set_paused` stops *all* dispatch).

**Remaining work:**
- A `paused` task status, or a `paused_at` column, that the orchestrator honours between turns.
- `POST /api/v1/tasks/{id}/pause` and `/resume`, a `ControlPlane` method, and gRPC parity.

## Suggested order

1. Land the Tutela removal and this page (this branch).
2. Fix and merge `feat/codex-steer`, then wire live message injection end to end (feature 1).
3. Extract the shared service layer, then add gRPC (feature 2).
4. Per-task pause/resume (feature 3).

Each is its own change with its own tests; none needs the others to be useful.
