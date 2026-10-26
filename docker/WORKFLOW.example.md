---
# Container-oriented starting point. Copy to docker/config/WORKFLOW.md and edit.
# Full contract: SPEC.md and the workflow reference in docs/configuration.md.
tracker:
  kind: linear
  provider:
    project_slug: "your-project-slug"
    api_key: $LINEAR_API_KEY # read host-side; never exposed to the Codex child
  active_states: [Todo, In Progress, Rework]
  terminal_states: [Done, Closed, Cancelled, Canceled, Duplicate]
polling:
  interval_ms: 10000
workspace:
  # Must live on the /workspaces volume so workspaces survive container restarts.
  root: /workspaces
hooks:
  after_create: |
    git clone --depth 1 "$SOURCE_REPO_URL" .
agent:
  max_concurrent_agents: 4
  max_turns: 20
codex:
  # Safe defaults: no network inside the turn sandbox, sandbox escalations rejected. Opt in only
  # if your workflow needs it, e.g. `turn_sandbox_policy: {type: workspaceWrite, networkAccess: true}`.
  command: codex app-server
  thread_sandbox: workspace-write
server:
  # The container binds 0.0.0.0, so the API needs a token: set SYMPHONY_API_TOKEN in docker/.env.
  api_token: $SYMPHONY_API_TOKEN
budget:
  max_tokens_per_issue: 5000000
  max_tokens_per_day: 50000000
---

You are working on tracker issue `{{ issue.identifier }}`: {{ issue.title }}

{% if issue.description %}
{{ issue.description }}
{% else %}
No description provided.
{% endif %}

Work only inside the current workspace. Report completed actions and blockers when done.
