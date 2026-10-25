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
queued ──▶ running ──▶ succeeded
   ▲          │  └───▶ failed ──────┐
   │          └──────▶ blocked ─────┤  POST /unblock (optional reply)
   └────────────────────────────────┘
queued / blocked / running ──▶ cancelled   (POST /cancel)
```

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

## Endpoints

| Method and path | What it does |
|---|---|
| `POST /api/v1/tasks` | Queue a task. `201` with the task. |
| `GET /api/v1/tasks?status=&limit=&before_id=` | List, newest first. |
| `GET /api/v1/tasks/{id}` | One task. |
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
is). Unknown fields are refused.

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

`create`, `get`, `list`, `cancel`, `unblock`, `watch` and `wait` map one-to-one onto the endpoints;
errors carry the API's `code` (`ClientError::Api`).
