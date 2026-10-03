---
# Container-oriented starting point. Copy to docker/config/WORKFLOW.md and edit.
# Full contract: SPEC.md and the workflow reference in the main README.
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
  command: codex app-server
  thread_sandbox: workspace-write
  turn_sandbox_policy:
    type: workspaceWrite
    networkAccess: true
---

You are working on tracker issue `{{ issue.identifier }}`: {{ issue.title }}

{% if issue.description %}
{{ issue.description }}
{% else %}
No description provided.
{% endif %}

Work only inside the current workspace. Report completed actions and blockers when done.
