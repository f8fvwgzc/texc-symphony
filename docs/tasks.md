# Task API

Products assign work to Symphony directly, without a tracker issue. A task is a prompt plus,
optionally, a repository, a backend and limits. Symphony runs it once on an agent, in its own
workspace, under the same concurrency, budget and kill-switch rules as tracker issues, and keeps
the agent's final message as the result. Coding is optional: without a repository the workspace
starts empty.

The task API needs the run-history store (it is on by default; `503 store_disabled` with
`--no-db`). It is part of the HTTP API, so it needs `server.port` and, off loopback, the API
token. The contract is in [`api/openapi.yaml`](api/openapi.yaml) (tag `tasks`).

A workflow that only serves tasks sets `tracker.kind: none`:

```yaml
---
tracker:
  kind: none
server:
  port: 4000
  api_token: $SYMPHONY_API_TOKEN
agent:
  backend: sonnet
  max_concurrent_agents: 4
backends:
  sonnet:
    kind: api
    provider: anthropic
    model: claude-sonnet-4-5
    api_key_env: ANTHROPIC_API_KEY
budget:
  max_tokens_per_day: 20000000
---
```

## Lifecycle

```text
waiting ──(dependencies succeeded)──▶ queued ──▶ running ──▶ succeeded
   │                                    ▲          │  └───▶ failed ──────┐
   └──(a dependency failed)──▶ failed   │          └──────▶ blocked ─────┤  POST /unblock
                                        └────────────────────────────────┘
waiting / queued / blocked / running ──▶ cancelled   (POST /cancel)
```

- `depends_on` lists existing tasks that must succeed first. Until they have, the task is
  `waiting` and is not dispatched; it is dispatched as soon as the last one succeeds. When one of
  them fails or is cancelled, the task fails (`dependency task-<id> did not succeed`), and so do
  its own dependants. Creating a task on a dependency that already failed or was cancelled is
  refused (`409 task_conflict`), and so is unblocking a task before its failed dependency: unblock
  the dependency first, then the dependant, which waits again. Dependencies must exist when the
  task is created, so there are no cycles.
- A task runs exactly one agent turn (the API loop's turn may make up to `max_steps` model calls).
  It never retries by itself: a failure ends as `failed`, with `error`.
- `blocked`: the agent needs operator input or approval, or the per-issue budget is used up.
  `unblock` re-queues it; a `message` is appended to the prompt as `Operator reply:`.
  `unblock` also re-queues a `failed` task.
- The kill switch and an exhausted daily budget stop running tasks and put them back to `queued`;
  so does a restart (tasks that were `running` are queued again).
- A task's identifier is `task-<id>` (lowercase, so it cannot be a Linear or Jira key); its runs
  appear in the dashboard and in `GET /api/v1/runs?issue=task-<id>`. Its workspace is
  `<workspace.root>/.tasks/<id>`, a directory no tracker identifier maps to, and it is removed
  when the task succeeds, fails or is cancelled (a blocked task keeps it until it ends). The
  workflow's hooks do not run for tasks.
- `repo` is shallow-cloned into the empty workspace (`https://`, `ssh://` or `user@host:path`;
  local paths, `file://` and git transport helpers are refused). The agent's own git credentials
  apply; the tracker token is not passed on.

## Isolation

For tasks from several tenants, an [`isolation:`](configuration.md#isolation) section isolates
every task run: its own uid (`task_uids`, Symphony must run as root), `HOME` inside its workspace,
a GitHub App token scoped to its repository (`github_app`; used for the clone and given to the
agent for `git push` and `gh`, redacted from events, valid one hour) and a default-deny network
that reaches only the repository's git host on 443, the LLM gateway and `network.allow`. The
network rule is enforced for every backend: the `api` shell runs in bubblewrap, Codex and Claude
Code in a network jail (bubblewrap with the host's files but no network except the proxy). A
task that cannot be isolated as configured (no root for `task_uids`, an SSH worker, no
bubblewrap for the jail) fails rather than running unisolated. Tracker issues are not affected.

## Endpoints

| Method and path | What it does |
|---|---|
| `POST /api/v1/tasks` | Queue a task. `201` with the task. |
| `GET /api/v1/tasks?status=&limit=&before_id=` | List, newest first. |
| `GET /api/v1/tasks/{id}` | One task. |
| `POST /api/v1/tasks/plan` | Plan a set of tasks without creating or running anything (below). |
| `POST /api/v1/tasks/{id}/cancel` | Stop or cancel it (final tasks are returned unchanged). |
| `POST /api/v1/tasks/{id}/unblock` | Re-queue a blocked or failed task; body `{"message": "..."}` optional. `409 task_conflict` otherwise. |
| `GET /api/v1/tasks/{id}/events` | SSE: `task` events when the record changes, `run_event` events with its runs' (redacted) events; ends when the task is final. |

```sh
curl -sS -H "Authorization: Bearer $SYMPHONY_API_TOKEN" -H 'content-type: application/json' \
  -d '{"title":"README summary","prompt":"Read README.md and write a five-line summary.",
       "repo":"https://github.com/f8fvwgzc/texc-symphony","backend":"sonnet",
       "limits":{"max_tokens":200000,"timeout_ms":600000},"metadata":{"ticket":"PROD-17"}}' \
  http://127.0.0.1:4000/api/v1/tasks
```

Body fields: `prompt` (required, at most 256 KiB), `title` (at most 200 characters), `repo`,
`branch` (needs `repo`), `backend` (a `backends` entry; default `agent.backend`), `limits`
(`max_tokens`: input + output tokens across the task's runs, the run fails when it is reached;
`timeout_ms`: wall-clock limit of one run), `metadata` (a JSON object up to 16 KiB, returned as
is), `depends_on` (up to 100 existing task ids). Unknown fields are refused.

### Planning

`POST /api/v1/tasks/plan` is a dry run for a set of tasks with dependencies: nothing is created
or run. Each planned task has a `key`; its `depends_on` names other planned tasks by key (strings)
and existing tasks by id (integers):

```json
{"tasks": [{"key": "build"}, {"key": "test", "depends_on": ["build"]},
           {"key": "deploy", "depends_on": ["test", 41]}]}
```

The answer gives `waves` (wave 0 depends on no planned task, each later wave only on earlier
ones), `order` (the waves in sequence, request order within a wave) and, per task, its `wave`
and the `status` creating it now would give: `queued`, `waiting` with `waiting_for` (its planned
dependencies and the existing ones that have not succeeded), or `failed` with
`failed_dependency` (already failed or cancelled, directly or through a planned task; creating it
would be refused). An unknown key or id, a duplicate key or a cycle is
`400 invalid_parameter` naming it (`dependency cycle: a -> b -> a`). Create the tasks wave by
wave, mapping keys to the ids the server returns.

Browsers open the stream with a ticket: `POST /api/v1/events/tickets`, then
`GET /api/v1/tasks/{id}/events?ticket=<ticket>`.

## Rust client

```toml
[dependencies]
symphony-tasks-client = { git = "https://github.com/f8fvwgzc/texc-symphony", tag = "v0.2.0" }
```

```rust
use symphony_tasks_client::{NewTask, TasksClient};

let client = TasksClient::new("http://127.0.0.1:4000")?.with_token(token);
let task = client
    .create(&NewTask { prompt: "Write release notes for v0.2.0".into(), ..NewTask::default() })
    .await?;
let done = client.wait(task.id).await?; // or client.watch(task.id) for every event
println!("{:?}: {}", done.status, done.result.unwrap_or_default());
```

`create`, `get`, `list`, `cancel`, `unblock`, `plan`, `watch` and `wait` map one-to-one onto the
endpoints (`NewTask::depends_on` for dependencies); errors carry the API's `code`
(`ClientError::Api`).
