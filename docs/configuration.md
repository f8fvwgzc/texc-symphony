# WORKFLOW.md reference

Symphony is configured by one file, `WORKFLOW.md`: YAML front matter for the runtime settings,
followed by a Liquid template that becomes the first prompt of every agent run. This page is the
complete reference for that file. Command-line flags and environment variables are covered in
[`cli.md`](cli.md); the behavioural contract is [`SPEC.md`](../SPEC.md).

Examples: [`../WORKFLOW.md`](../WORKFLOW.md) (this repository's own workflow) and
[`../docker/WORKFLOW.example.md`](../docker/WORKFLOW.example.md) (a container starting point).

- [File format](#file-format)
- [Prompt template](#prompt-template)
- [Value rules](#value-rules) (types, `null`, `$VAR` references)
- [Settings](#settings): [`tracker`](#tracker), [`polling`](#polling), [`workspace`](#workspace),
  [`worker`](#worker), [`agent`](#agent), [`codex`](#codex), [`hooks`](#hooks),
  [`observability`](#observability), [`server`](#server)
- [Tracker kinds](#tracker-kinds): [Linear](#linear), [GitHub Issues](#github-issues),
  [GitLab](#gitlab), [Jira Cloud](#jira-cloud), [Asana](#asana), [memory](#memory)
- [Tracker secrets](#tracker-secrets)
- [Validation errors](#validation-errors)
- [Reloading](#reloading)

## File format

```md
---
tracker:
  kind: linear
  provider:
    project_slug: "my-project-0123abcd"
workspace:
  root: ~/code/workspaces
hooks:
  after_create: |
    git clone --depth 1 https://github.com/your-org/your-repo .
---

You are working on {{ issue.identifier }}: {{ issue.title }}

{{ issue.description }}
```

- The file must be UTF-8. Lines may end in LF or CRLF.
- **Front matter** is present only when the very first line is exactly `---`. It ends at the next
  line that is exactly `---`. Without a closing `---`, everything after the opener is front
  matter and the prompt is empty. Without an opening `---`, the whole file is the prompt and the
  configuration is empty (which then fails with `missing_tracker_kind`).
- The front matter must decode to a YAML mapping. Empty or comment-only front matter is an empty
  mapping.
- YAML is parsed with YAML 1.2 core rules: `yes`/`no`/`on`/`off` are strings, not booleans;
  `0x1F` is an integer; `010` is the string `"010"`. Duplicate keys are a parse error. Integers
  must fit in 64 bits.
- **The prompt** is everything after the closing `---`, with surrounding whitespace trimmed. If
  it is blank, the [default prompt](#default-prompt) is used.
- Unknown keys are ignored, at every level. A misspelled key is therefore silently ignored; check
  the effective values in the dashboard or with `GET /api/v1/state` when in doubt.

## Prompt template

The prompt body is a [Liquid](https://shopify.github.io/liquid/) template, rendered once per agent
run for turn 1. Later turns of the same run (up to `agent.max_turns`) send a fixed
"Continuation guidance" message instead, because the original instructions are already in the
Codex thread.

### Variables

The template sees exactly two top-level variables:

| Variable | Type | Meaning |
|---|---|---|
| `attempt` | integer or nil | nil on the first run of an issue; `1`, `2`, ... on continuations and retries. Always defined, so `{% if attempt %}` is safe. |
| `issue` | object | The normalized tracker issue. Every field below is always defined (nil when the tracker has no value). |

| `issue.` field | Type | Notes |
|---|---|---|
| `id` | string | Stable tracker id (Linear UUID, GitHub issue number, GitLab IID, Jira id, Asana gid). |
| `identifier` | string | Human key: `MT-123` (Linear), `GH-12` (GitHub), `GL-7` (GitLab), the Jira issue key. Also names the workspace directory. |
| `title` | string | |
| `description` | string | Body text (Jira's rich text is flattened to plain text). |
| `priority` | integer | Linear priority; nil elsewhere. |
| `state` | string | Tracker-native spelling (`In Progress`, `open`, a Jira status, an Asana section name). |
| `branch_name` | string | Linear's suggested branch name; nil elsewhere. |
| `url` | string | Web URL of the issue. |
| `assignee_id` | string | Tracker id of the assignee. |
| `labels` | list of strings | Trimmed, lowercased, deduplicated. |
| `blocked_by` | list of objects | Each with `id`, `identifier`, `state`. |
| `dispatchable` | boolean | The adapter's eligibility verdict. |
| `native_ref` | object | Non-secret provider ids used by the tracker tools. |
| `created_at`, `updated_at` | string | ISO-8601 in UTC, for example `2026-02-26T18:06:48Z`. |

Rendering is strict:

- An undefined variable or field (`{{ issue.foo }}`, `{{ ticket }}`) fails the render.
- An unknown filter fails the render. The standard Liquid filters (`join`, `downcase`, `default`,
  `size`, ...) are available.
- A nil value renders as an empty string, and `{% if issue.description %}` is false for nil.
- A list rendered directly is concatenated without a separator (`{{ issue.labels }}` gives
  `backendui`); use `{{ issue.labels | join: ", " }}`.

A render failure fails the agent run with `template_render_error: ...` (or
`template_parse_error: ... template="..."` for a syntax error); the orchestrator then retries the
issue with backoff. Fix the template and the next attempt uses the reloaded file.

### Default prompt

Used when the prompt body is blank:

```liquid
You are working on an issue from the configured tracker.

Identifier: {{ issue.identifier }}
Title: {{ issue.title }}

Body:
{% if issue.description %}
{{ issue.description }}
{% else %}
No description provided.
{% endif %}
```

## Value rules

These rules apply to every key below.

- **`null` means absent.** `key: null` or `key: ~` behaves exactly like leaving the key out, so
  the default applies.
- **Integers** accept YAML integers and strings that are entirely an integer (`"30000"`,
  `"+5"`). Floats, booleans, `" 5"` and other strings are `is invalid`.
- **Booleans** accept `true`/`false` and the strings `"true"`, `"false"`, `"1"`, `"0"`.
- **Strings** accept only strings; `""` is kept as an empty string (not treated as absent).
- **Lists of strings** accept only lists whose every element is a string. A `null` element is
  `is invalid`.
- **Maps** accept only mappings.
- **Sections** (`tracker`, `polling`, ...) must be mappings; `polling: 5` is
  `polling is invalid`.

### `$VAR` references

Some string values may name an environment variable instead of holding the value. The table in
each section says which keys support this.

- Only a value that is **entirely** a reference is resolved: `$NAME`, where `NAME` matches
  `[A-Za-z_][A-Za-z0-9_]*`. There is no interpolation: `https://$HOST/x`, `${NAME}` and the old
  `env:NAME` syntax are not references.
- Linear keys and `workspace.root` follow one set of rules; the GitHub, GitLab, Jira and Asana
  keys follow another. They differ as follows:

| Case | Linear `api_key`, `assignee` | `workspace.root` | GitHub / GitLab / Jira / Asana keys |
|---|---|---|---|
| Key absent | default env var (`LINEAR_API_KEY` / `LINEAR_ASSIGNEE`) | default root | default env var, if the key has one |
| `$NAME`, variable set | its value | its value | its value, trimmed |
| `$NAME`, variable set to `""` | no value (no fallback) | default root | no value (no fallback) |
| `$NAME`, variable unset | default env var | default root | default env var, if the key has one |
| `$` + invalid name (`$1X`) | kept literally | kept literally | no value |
| Literal string | kept as is (no trimming); `""` = no value | kept as is; `""` = default root | trimmed; blank = no value |

"No value" for a required key fails validation (for example `missing_linear_api_token`).

`codex.command` and the `hooks.*` scripts are never resolved by Symphony: they are shell code, so
`$NAME` there is expanded by the shell at run time.

## Settings

The **Reload** column says when a change takes effect: **live** values are read again on every
use (see [Reloading](#reloading)); **restart** values are read once at startup.

### `tracker`

| Key | Type | Default | Validation and notes | Reload |
|---|---|---|---|---|
| `tracker.kind` | string | none (required) | One of `linear`, `github`, `gitlab`, `jira`, `asana`, `memory`. Case-sensitive (`Linear` is unsupported). | live |
| `tracker.provider` | map | `{}` | Adapter-owned settings; see [Tracker kinds](#tracker-kinds). Unknown keys are kept and ignored. | live |
| `tracker.required_labels` | list of strings | `[]` | An issue must carry every listed label to be dispatched or to keep running. Compared after trim + lowercase; duplicates removed. A blank entry matches no issue. | live |
| `tracker.active_states` | list of strings | Linear and memory: `[Todo, In Progress]`; other kinds: none | States that make an issue a dispatch candidate. Compared case-insensitively after trimming. Required for GitHub, GitLab, Jira and Asana. | live |
| `tracker.terminal_states` | list of strings | Linear and memory: `[Closed, Cancelled, Canceled, Duplicate, Done]`; other kinds: none | States that end a run and remove the issue's workspace. Same comparison. Required for GitHub, GitLab, Jira and Asana. | live |
| `tracker.endpoint`, `tracker.api_key`, `tracker.project_slug`, `tracker.assignee` | string | none | Legacy flat aliases for the Linear `provider` keys of the same name. A `provider` key wins over its alias. Ignored by the other kinds. | live |

An issue whose state is neither active nor terminal is left alone; if it was running, the run is
stopped but its workspace is kept.

### `polling`

| Key | Type | Default | Validation and notes | Reload |
|---|---|---|---|---|
| `polling.interval_ms` | integer | `30000` | `> 0`. Time between tracker polls. `POST /api/v1/refresh` triggers an immediate poll. | live |

### `workspace`

| Key | Type | Default | Validation and notes | Reload |
|---|---|---|---|---|
| `workspace.root` | string | `<system temp dir>/symphony_workspaces` (`TMPDIR` is honoured) | Supports `$VAR` (see the table above). `~` is expanded, and a relative path is resolved against the directory that contains `WORKFLOW.md`. On SSH workers the value is passed to the remote shell unchanged, so `~` means the remote home. | live |

Each issue gets the directory `<root>/<key>`, where `<key>` is the issue identifier with every byte
outside `[A-Za-z0-9._-]` replaced by `_`; when anything was replaced, `--` and 16 hex characters
of the identifier's SHA-256 are appended so that distinct identifiers never share a directory. The
canonical workspace path must stay strictly inside the canonical root (symlinks are resolved);
otherwise the run fails.

### `worker`

| Key | Type | Default | Validation and notes | Reload |
|---|---|---|---|---|
| `worker.ssh_hosts` | list of strings | `[]` | Run agents on these hosts over SSH instead of locally. Entries are `host`, `user@host`, `host:port` or `user@[ipv6]:port`. The `ssh` client reads your normal SSH config, or the file named by `SYMPHONY_SSH_CONFIG` (`ssh -F`). | live |
| `worker.max_concurrent_agents_per_host` | integer | none (no per-host cap) | `> 0`. When every host is at the cap, dispatch waits; it never falls back to running locally. | live |

Remote hosts need `bash`, `git`, the Codex CLI and whatever the hooks use. Hooks and Codex run
there through `bash -lc`.

### `agent`

| Key | Type | Default | Validation and notes | Reload |
|---|---|---|---|---|
| `agent.max_concurrent_agents` | integer | `10` | `> 0`. Global cap on running agents. | live |
| `agent.max_concurrent_agents_by_state` | map of state to integer | `{}` | Per-state caps; keys are compared after trim + lowercase. Every key must be non-blank (`state names must not be blank`) and every value a positive YAML integer (`limits must be positive integers`; `"2"` is rejected here). One bad entry rejects the whole file. A state without an entry uses `max_concurrent_agents`. | live |
| `agent.max_turns` | integer | `20` | `> 0`. Maximum back-to-back Codex turns in one agent run while the issue stays active. | live |
| `agent.max_retry_backoff_ms` | integer | `300000` | `> 0`. Cap for the retry delay after a failed run: `min(10 s * 2^(attempt-1), max_retry_backoff_ms)`. A run that ends normally is re-checked after 1 s instead. | live |

Counts must fit in an unsigned 32-bit integer.

### `codex`

| Key | Type | Default | Validation and notes | Reload |
|---|---|---|---|---|
| `codex.command` | string | `codex app-server` | Must not be blank (`can't be blank`). Run as `bash -lc "<command>"` in the workspace, so `~` and `$NAME` are expanded by the shell, not by Symphony. | live |
| `codex.approval_policy` | string or map | `{reject: {sandbox_approval: true, rules: true, mcp_elicitations: true}}` | Passed to Codex unchanged; no local check of the value (`""` is accepted). Current Codex versions accept `untrusted`, `on-failure`, `on-request`, `never` or a `reject` map. | live |
| `codex.thread_sandbox` | string | `workspace-write` | Passed to Codex unchanged (`read-only`, `workspace-write`, `danger-full-access`). | live |
| `codex.turn_sandbox_policy` | map | see below | When set, passed to Codex unchanged (keys as Codex expects them, for example `type`, `networkAccess`). | live |
| `codex.turn_timeout_ms` | integer | `3600000` | `> 0`. Inactivity limit while a turn streams: every Codex message restarts it. It is not a cap on total turn time. Also bounds each dynamic tool call. | live |
| `codex.read_timeout_ms` | integer | `5000` | `> 0`. How long to wait for Codex to answer a request (`initialize`, `thread/start`, ...). | live |
| `codex.stall_timeout_ms` | integer | `300000` | `>= 0`. When a running issue shows no Codex activity for this long, the run is stopped and retried with backoff (`Issue stalled ... restarting with backoff`). `0` disables stall detection. | live |

When `turn_sandbox_policy` is not set, each turn uses:

```yaml
type: workspaceWrite
writableRoots: [<the issue workspace>]
readOnlyAccess: { type: fullAccess }
networkAccess: false
excludeTmpdirEnvVar: false
excludeSlashTmp: false
```

Network access is off in that default. Workflows whose agents install packages or call external
services should set an explicit policy with `networkAccess: true`.

Changes apply to the next Codex session; running sessions keep the settings they started with.

### `hooks`

Hooks are shell scripts run in the issue workspace with `sh -lc` (with `bash -lc` on SSH
workers). Each run is limited to `hooks.timeout_ms`; on timeout the hook's whole process group is
killed. Hook output is logged, truncated to 2 KiB.

| Key | Type | Default | When it runs | On failure or timeout | Reload |
|---|---|---|---|---|---|
| `hooks.after_create` | string | none | Once, after the workspace directory is newly created. | The new directory is removed and the attempt fails (retried with backoff, so the hook runs again). | live |
| `hooks.before_run` | string | none | Before every agent run. | The attempt fails. | live |
| `hooks.after_run` | string | none | After every agent run, including failed and cancelled ones. | Logged and ignored. | live |
| `hooks.before_remove` | string | none | Before a workspace is deleted (the issue reached a terminal state, or startup cleanup found a terminal issue). | Logged and ignored; the workspace is still deleted. | live |
| `hooks.timeout_ms` | integer | `60000` | Limit for each hook run; must be `> 0`. | | live |

This repository's `before_remove` hook calls `symphony workspace before-remove` to close the
branch's open pull requests; see [`cli.md`](cli.md#symphony-workspace-before-remove).

### `observability`

Settings for the terminal status dashboard (see [`cli.md`](cli.md#terminal-status-dashboard)).

| Key | Type | Default | Validation and notes | Reload |
|---|---|---|---|---|
| `observability.dashboard_enabled` | boolean | `true` | The dashboard runs only when this is true **and** stdout is a terminal. | live off only: switching it to `false` stops a running dashboard; switching it on needs a restart (it also decides at startup whether logs go to stdout) |
| `observability.refresh_ms` | integer | `1000` | `> 0`. Periodic refresh interval (it also refreshes on every state change). | live |
| `observability.render_interval_ms` | integer | `16` | `> 0`. Minimum time between two redraws. | live |

### `server`

The HTTP server serves the web dashboard, the JSON API and the event stream
([`api/README.md`](api/README.md)). It has no authentication.

| Key | Type | Default | Validation and notes | Reload |
|---|---|---|---|---|
| `server.port` | integer | none: no HTTP server | `0..65535` (`must be greater than or equal to 0`, `must be less than or equal to 65535`). `0` binds a free port. `--port` and `SYMPHONY_PORT` override it. | restart |
| `server.host` | string | `127.0.0.1` | IP literal or DNS name. `--host` and `SYMPHONY_HOST` override it. Use `0.0.0.0` only inside a container or behind a firewall or authenticating proxy. | restart |

## Tracker kinds

`tracker.kind` selects an adapter. Adapter settings live under `tracker.provider`; the states and
labels stay directly under `tracker`. Each adapter also gives the agent one **tracker tool**, a raw
API passthrough that Symphony executes host-side with the configured credential (so the agent never
needs the token). The tool can reach whatever the credential can reach, not only the configured
project.

The checks run in the order listed; the first failure is reported. The error tags are explained in
[Validation errors](#validation-errors).

### Linear

```yaml
tracker:
  kind: linear
  provider:
    project_slug: "my-project-0123abcd"   # the last segment of the project URL
    api_key: $LINEAR_API_KEY               # optional: LINEAR_API_KEY is the default
    assignee: me                           # optional
```

| `provider.` key | Default | `$VAR` | Validation |
|---|---|---|---|
| `endpoint` | `https://api.linear.app/graphql` | no | non-blank, else `invalid_linear_endpoint` |
| `api_key` | env `LINEAR_API_KEY` | yes | required, else `missing_linear_api_token` |
| `project_slug` | none | no | required, non-blank, else `missing_linear_project_slug` |
| `assignee` | env `LINEAR_ASSIGNEE` | yes | if set, a non-blank string, else `invalid_linear_assignee` |

- The legacy flat keys `tracker.endpoint`, `tracker.api_key`, `tracker.project_slug` and
  `tracker.assignee` fill in any `provider` key that is absent.
- `assignee` limits dispatch to issues assigned to that Linear user id; `me` means the owner of
  the API key (looked up on every poll).
- A `Todo` issue with a blocker that is not in a terminal state is not dispatched.
- States default to `active_states: [Todo, In Progress]` and
  `terminal_states: [Closed, Cancelled, Canceled, Duplicate, Done]`. Workflows that use extra
  statuses (`Rework`, `Human Review`, `Merging`) must list them.
- Tool: `linear_graphql` (a GraphQL query string, or `{query, variables}`).

### GitHub Issues

```yaml
tracker:
  kind: github
  active_states: [open]
  terminal_states: [closed]
  provider:
    repo: your-org/your-repo
    token: $GITHUB_TOKEN
```

| `provider.` key | Default | `$VAR` | Validation |
|---|---|---|---|
| `api_url` | `https://api.github.com` | no | `https://` with a host, else `invalid_github_api_url` |
| `repo` | env `GITHUB_REPO` | yes | required, else `missing_github_repo`; `owner/name` without spaces, else `invalid_github_repo` |
| `token` | env `GITHUB_TOKEN` | yes | required, else `missing_github_token` |

- `active_states` and `terminal_states` are required (`missing_github_active_states`,
  `missing_github_terminal_states`); every active entry must be `open` and every terminal entry
  `closed` (case-insensitive), else `invalid_github_states`.
- `issue.identifier` is `GH-<number>`. Pull requests returned by the Issues API are never
  dispatched.
- Tool: `github_api` (relative REST path, optional `params` and JSON `body`).

### GitLab

```yaml
tracker:
  kind: gitlab
  active_states: [opened]
  terminal_states: [closed]
  provider:
    project_path: your-group/your-project
    api_key: $GITLAB_PAT
```

| `provider.` key | Default | `$VAR` | Validation |
|---|---|---|---|
| `api_url` | `https://gitlab.com/api/v4` | no | `https://` with a host, else `invalid_gitlab_api_url` |
| `project_path` | env `GITLAB_PROJECT_PATH` | yes | required, else `missing_gitlab_project_path`; no spaces, tabs, line breaks or NUL, else `invalid_gitlab_project_path` |
| `api_key` | env `GITLAB_PAT` | yes | required, else `missing_gitlab_api_key` |

- States are required (`missing_gitlab_active_states`, `missing_gitlab_terminal_states`); active
  entries must be `opened` and terminal entries `closed`, else `invalid_gitlab_states`.
- `issue.identifier` is `GL-<iid>`.
- Tool: `gitlab_api`.

### Jira Cloud

```yaml
tracker:
  kind: jira
  active_states: [To Do, In Progress]
  terminal_states: [Done]
  provider:
    base_url: https://your-site.atlassian.net
    email: $JIRA_EMAIL
    api_token: $JIRA_API_TOKEN
    project_key: ENG
```

| `provider.` key | Default | `$VAR` | Validation |
|---|---|---|---|
| `base_url` | env `JIRA_BASE_URL` | yes | `https://` with a host and no query or fragment, else `invalid_jira_base_url` |
| `email` | env `JIRA_EMAIL` | yes | required, else `missing_jira_email` |
| `api_token` | env `JIRA_API_TOKEN` | yes | required, else `missing_jira_api_token` |
| `project_key` | none | yes | required, else `missing_jira_project_key` |

- States are Jira status names. Both lists are required (`missing_jira_active_states`,
  `missing_jira_terminal_states`) and must not contain blank entries (`invalid_jira_states`).
- `issue.identifier` is the issue key (`ENG-42`). Issues in Jira's "new" status category wait
  until their blockers reach a terminal state.
- Tool: `jira_rest` (requests under `/rest/api/3/`).

### Asana

```yaml
tracker:
  kind: asana
  active_states: [Ready, Doing]
  terminal_states: [Done]
  provider:
    project_gid: "1200000000000000"
    api_key: $ASANA_PAT
```

| `provider.` key | Default | `$VAR` | Validation |
|---|---|---|---|
| `endpoint` | `https://app.asana.com/api/1.0` | no | `https://` with a host, else `invalid_asana_endpoint` |
| `api_key` | env `ASANA_PAT` | yes | required, else `missing_asana_api_key` |
| `project_gid` | none | yes | required, else `missing_asana_project_gid` |

- States are the names of the project's sections. Both lists are required
  (`missing_asana_active_states`, `missing_asana_terminal_states`) and must not contain blank
  entries (`invalid_asana_states`). Completed tasks are never dispatched.
- Tool: `asana_api`.

### memory

An in-process tracker for demos and local testing; it needs no credentials and has no tracker tool.
States default to the Linear defaults. Issues can be listed under `tracker.provider.issues`:

```yaml
tracker:
  kind: memory
  provider:
    issues:
      - id: "1"
        identifier: DEMO-1
        title: Add a health check
        description: Expose GET /healthz returning 200.
        state: Todo
        labels: [backend]
      - id: "2"
        identifier: DEMO-2
        title: Not ready yet
        state: Todo
        dispatchable: false
```

Each entry is an issue object with the fields listed under [Variables](#variables) (`id`,
`identifier`, `title`, `description`, `state`, `url`, `labels`, `priority`, `branch_name`,
`blocked_by`, `assignee_id`, `created_at`, `updated_at`, ...). `dispatchable` defaults to `true`
when omitted; the memory tracker does not evaluate `blocked_by`, so set `dispatchable: false` to
hold an issue back. The list is loaded at startup and applied again whenever `WORKFLOW.md` changes, so
editing an issue's `state` in the file moves it through the workflow. Without `provider.issues` the
tracker is empty.

## Tracker secrets

Tracker credentials are used only by Symphony itself. Before launching Codex, Symphony removes the
credential variables from the child environment and also `unset`s them in the login shell that
starts Codex (so a shell profile cannot put them back). The agent reaches the tracker only through
the tracker tool, which Symphony executes with the credential.

| Kind | Variables removed from the Codex environment |
|---|---|
| `linear` | `LINEAR_API_KEY`, plus the variable named by a `$NAME` in `provider.api_key` (or the flat `api_key`) |
| `github` | `GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_ENTERPRISE_TOKEN`, `GH_ENTERPRISE_TOKEN`, plus a `$NAME` in `provider.token` |
| `gitlab` | `GITLAB_PAT`, `GITLAB_ACCESS_TOKEN`, `GITLAB_TOKEN`, `OAUTH_TOKEN`, plus a `$NAME` in `provider.api_key` |
| `jira` | `JIRA_API_TOKEN`, plus a `$NAME` in `provider.api_token` |
| `asana` | `ASANA_PAT`, plus a `$NAME` in `provider.api_key` |
| `memory` | none |

Keep credentials out of the file itself: write `$NAME` and set the variable in Symphony's
environment. A literal token in a `WORKFLOW.md` that is committed to the repository the agents
clone is readable by the agents. Symphony redacts credentials in its own logs and error messages.

Agents that push code need their own git credentials (for example an SSH deploy key, or a
`gh`/git credential helper); the tracker token is not passed on.

## Validation errors

`WORKFLOW.md` is loaded and validated at startup and on every reload, in this order: read the
file, parse the front matter, check every key's type and range, then check `tracker.kind` and the
adapter settings. At startup any error stops Symphony (see [`cli.md`](cli.md#exit-codes)); later it
only affects dispatch (see [Reloading](#reloading)).

| Problem | Message |
|---|---|
| File missing or unreadable | `Missing WORKFLOW.md at <path>: :enoent` (or another POSIX reason) |
| YAML syntax error | `Failed to parse WORKFLOW.md: <parser message>` |
| Front matter is not a mapping | `Failed to parse WORKFLOW.md: workflow front matter must decode to a map` |
| Wrong type or range | `Invalid WORKFLOW.md config: <key> <message>[, <key> <message>...]` |
| `tracker.kind` absent | `Invalid WORKFLOW.md config: :missing_tracker_kind` |
| Unsupported kind | `Invalid WORKFLOW.md config: {:unsupported_tracker_kind, "Linear"}` |
| Adapter setting | `Invalid WORKFLOW.md config: :<tag>`, for example `:missing_linear_api_token` |

Type and range errors are collected for all keys and reported together, sections in the order of
this page. The messages are:

| Message | Meaning |
|---|---|
| `is invalid` | Wrong type, a section that is not a mapping, or a count too large for 32 bits. |
| `must be greater than 0` | A `> 0` integer is `0` or negative. |
| `must be greater than or equal to 0` | A negative `codex.stall_timeout_ms` or `server.port`. |
| `must be less than or equal to 65535` | `server.port` out of range. |
| `can't be blank` | `codex.command` is empty or whitespace. |
| `state names must not be blank` | A blank key in `agent.max_concurrent_agents_by_state`. |
| `limits must be positive integers` | A non-integer or non-positive value in `agent.max_concurrent_agents_by_state`. |

For example:

```text
Invalid WORKFLOW.md config: polling.interval_ms must be greater than 0, agent.max_turns is invalid, server.port must be less than or equal to 65535
```

The adapter tags are listed with each [tracker kind](#tracker-kinds).

## Reloading

Symphony watches `WORKFLOW.md` by polling: every second, and before each use, it compares the
file's modification time, size and content hash with the last good load, and reloads when any of
them changed. There is no need to restart or signal the process.

- **Valid change:** the new settings and prompt replace the old ones atomically. Values marked
  **live** apply to their next use: the next poll (interval, states, labels, concurrency), the
  next workspace operation (root, hooks), the next agent run (prompt, Codex settings, tracker
  settings). Running agent sessions are not restarted; each keeps the Codex settings and tracker
  tool credentials it started with. For `tracker.kind: memory`, `provider.issues` is applied
  again.
- **Invalid change:** Symphony keeps running with the **last known good** configuration and logs

  ```text
  Failed to reload workflow path=<path> reason=<reason>; keeping last known good configuration
  ```

  where `<reason>` is the error tag, for example
  `invalid_workflow_config: polling.interval_ms must be greater than 0` or
  `missing_github_token`. An unchanged failure is logged again at most every 30 s. While the file
  is invalid, every poll skips dispatching new work (logging the validation error) but keeps
  reconciling running issues, so terminal issues are still stopped and cleaned up. Fixing the
  file resumes dispatch on the next poll.
- **Restart-only values:** `server.*`, and turning `observability.dashboard_enabled` back on.
  Changing them in a running instance has no effect until the next start (`refresh_ms` and
  `render_interval_ms` apply on the dashboard's next tick, and `dashboard_enabled: false` stops a
  running dashboard).
- Environment variables referenced with `$NAME` are read at load time; changing Symphony's own
  environment requires a restart.
