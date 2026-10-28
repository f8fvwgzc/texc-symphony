# Agent backends

Symphony runs every issue and task on an **agent backend**. The runner treats them alike: it
starts a session in the workspace, runs turns while streaming events, and stops it. Backends are
named entries of `backends:` in `WORKFLOW.md`; `agent.backend` picks the default, and a task can
name its own.

```yaml
agent:
  backend: local            # default for issues and tasks; built-in default: codex
backends:
  claude:
    kind: claude_code
    model: sonnet
    allowed_tools: [Read, Edit, Write, Glob, Grep, "Bash(cargo:*)", "Bash(git:*)"]
  local:
    kind: api
    provider: openai
    base_url: http://localhost:11434/v1
    model: qwen2.5-coder:32b
  sonnet:
    kind: api
    provider: anthropic
    model: claude-sonnet-4-5
    api_key_env: ANTHROPIC_API_KEY
    pricing: {input_usd_per_million_tokens: 3, output_usd_per_million_tokens: 15}
```

| Kind | What runs | Tracker tools | Remote (SSH) workers |
|---|---|---|---|
| `codex` | `codex app-server` (configured by `codex:`) | dynamic tools | yes |
| `claude_code` | the Claude Code CLI, one `claude -p` process per turn | bridged over MCP | no |
| `api` | Symphony's own agent loop over a model API | native tools | no |

`codex` always exists; define `backends.codex` only to set its `model`, `prompt_template` or
`pricing`. With the platform's LLM connection contract (`LLM_*` environment,
[configuration.md](configuration.md#llm-connection-contract)) there is also a built-in `llm`
backend (an `api` backend from the contract), used where `agent.backend: llm` or a task names
it; the contract also supplies defaults to the other backends.

## Adding a provider (no code)

Any server that speaks OpenAI Chat Completions or Anthropic Messages is one entry:

```yaml
backends:
  openrouter:
    kind: api
    provider: openai                      # wire format
    base_url: https://openrouter.ai/api/v1
    model: anthropic/claude-sonnet-4.5
    api_key_env: OPENROUTER_API_KEY       # the variable's name; Symphony reads it host-side
    headers: {HTTP-Referer: https://example.com, X-Title: symphony}
```

| Provider | `provider` | `base_url` | Key |
|---|---|---|---|
| OpenAI | `openai` | `https://api.openai.com/v1` (default) | `OPENAI_API_KEY` |
| Anthropic | `anthropic` | `https://api.anthropic.com` (default) | `ANTHROPIC_API_KEY` |
| Ollama | `openai` | `http://localhost:11434/v1` | none |
| vLLM | `openai` | `http://<host>:8000/v1` | optional |
| LM Studio | `openai` | `http://localhost:1234/v1` | none |
| OpenRouter | `openai` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| Groq | `openai` | `https://api.groq.com/openai/v1` | `GROQ_API_KEY` |
| Together | `openai` | `https://api.together.xyz/v1` | `TOGETHER_API_KEY` |
| Gemini | `openai` | `https://generativelanguage.googleapis.com/v1beta/openai` | `GEMINI_API_KEY` |

The model must support tool calling for the workspace tools. `api_key_env` names a variable;
Symphony reads it, sends it (`x-api-key` for Anthropic, `Authorization: Bearer` otherwise), removes
it from every agent child environment and redacts its value in recorded events.

## Common keys

| Key | Default | Notes |
|---|---|---|
| `kind` | required | `codex`, `claude_code` or `api`. |
| `model` | `codex.model` / CLI default | Required for `api`. |
| `prompt_template` | none | Liquid template around the first turn's prompt: `{{ prompt }}` (the workflow prompt, or a task's prompt), `{{ issue }}`, `{{ attempt }}`. |
| `system_prompt` | built-in | `api`: the system message; `claude_code`: `--append-system-prompt`. |
| `pricing` | `budget` prices | `{input_usd_per_million_tokens, cached_input_usd_per_million_tokens, output_usd_per_million_tokens}` for this backend's runs ([budget](configuration.md#budget)); cache reads cost the input price unless set. |
| `turn_timeout_ms` | `3600000` | `claude_code`: silence timeout; `api`: limit of one whole turn. |

## `api`: the built-in agent loop

Each turn sends the prompt and loops: model call (streamed unless `stream: false`), tool calls,
results, until the model answers without tools (the turn's result) or `max_steps` model calls
were made (`agent_limit_exceeded`). The conversation persists across the turns of one run. 429,
5xx and connection errors are retried three times (1, 2, 4 s; `retry-after` wins, at most 30 s).

| Key | Default | Notes |
|---|---|---|
| `provider` | required | `openai` or `anthropic`. |
| `base_url` | provider default | `http://` allowed for local servers. |
| `api_key_env` | none | Variable holding the key; no key is sent without it. |
| `headers` | `{}` | Extra non-secret request headers. |
| `tools` | all | Subset of `read_file`, `write_file`, `edit_file`, `list_files`, `shell`, `tracker`. |
| `max_steps` | `50` | Model calls per turn. |
| `max_output_tokens` | `8192` | Per response. |
| `temperature` | provider default | `0..2`. |
| `stream` | `true` | `false` for servers with broken streaming. |
| `prompt_cache` | `true` | `anthropic`: prompt-cache breakpoints on the tools, the system prompt and the newest message, so each model call reads the conversation so far from the provider's cache (about a tenth of the input price). `false` for Anthropic-compatible servers that reject `cache_control`. OpenAI-compatible providers cache without markers. |
| `compact_after_tokens` | `100000` | Once the conversation is estimated (four characters a token) above this, tool results longer than 1,024 characters, except those of the four newest tool-result messages, are replaced by a stub with their first line; the agent runs the tool again if it needs one. Each cut invalidates the prompt cache from the first changed result, so it cuts everything older at once. Lower it for models with small context windows; `0` never cuts. |
| `request_timeout_ms` | `600000` | One model request. |
| `sandbox` | `auto` | `shell` confinement: `auto` (= `bubblewrap`), `bubblewrap`, `seatbelt` (macOS development only), `none` (needs `unconfined`). |
| `network` | `false` | Network access for `shell` commands (bubblewrap and seatbelt; a task's `isolation.network.mode: deny` wins). `false` cannot be set with `sandbox: none`, which cannot enforce it. |
| `unconfined` | `false` | Must be `true` with `sandbox: none` (commands reach the network and every file and process environment Symphony's user can read) and with `sandbox: seatbelt` (reads are not limited and the sandbox can be escaped). |
| `shell_timeout_ms` | `600000` | One command (a smaller per-call `timeout_ms` wins). |

Tools:

- `read_file` (`offset`/`limit` lines, 2000 lines and 256 KiB at most), `write_file`, `edit_file`
  (`old_string` must be unique unless `replace_all`), `list_files` (depth 3 by default, 1000
  entries, `.git` skipped). Paths are relative to the workspace; `..`, absolute paths outside it
  and symlink escapes are refused. Only regular files are opened (FIFOs and devices are refused
  without blocking), the opened file is checked to be the one inside the workspace, and files
  over 8 MiB are read only partly and not edited.
- `shell`: `bash -lc` in the workspace with stderr merged, the last 64 KiB of output kept (only
  the tail is ever buffered), host secrets removed from the environment and `unset` again after
  the login profile, a private temp directory as `$TMPDIR`, the whole process group killed on
  timeout or stop.
  - `bubblewrap` (Linux, the production sandbox): only system directories (`/usr`, `/bin`,
    `/lib*`, `/opt`, and the parts of `/etc` toolchains need) are bound read-only, so `$HOME`,
    `/run`, `/var`, the run-history database, other workspaces, credential files and
    filesystem sockets are not reachable; the workspace and a private `/tmp` are writable;
    every namespace is new (user, PID with its own `/proc`, IPC, UTS, cgroup, and network
    unless `network: true`); the environment is cleared and rebuilt from `PATH`, the locale
    variables and the backend's own variables. Needs unprivileged user namespaces.
  - `seatbelt` (macOS `sandbox-exec`): for development only, never chosen by `auto`, and it
    needs `unconfined: true`. It limits writes and the network, but reads are unrestricted and a
    copied launcher (`cp /bin/launchctl ./lc`) escapes it.
  - `auto` is `bubblewrap` and **refuses** to run commands without it.
  - `none` runs commands with Symphony's own access (files, network, the environments of
    Symphony's user's processes other than Symphony and the image's init, which are
    non-dumpable) and therefore needs `unconfined: true`. A container is no sandbox for this:
    Symphony, its run history and its secret files live in it.
- `tracker`: the tracker's scoped tools (`linear_graphql`, `github_api`, ...), executed by
  Symphony with the host-side credential ([agent tool scope](configuration.md#agent-tool-scope)).
  `workflow_reference`, which serves the prompt's reference sections, is offered whenever the
  prompt has sections, with or without `tracker`.

Events use the Codex vocabulary, so the dashboard, run history and budgets work unchanged:
`session_started`, `item/agentMessage/delta`, `agentMessage` / `commandExecution` / `fileChange`
items, `tool_call_completed` / `tool_call_failed`, cumulative `thread/tokenUsage/updated`
(input includes cache writes and reads, and cache reads are also reported as `cachedInputTokens`,
which token accounting meters apart), `turn_completed` or `turn_ended_with_error`.

## `claude_code`: the Claude Code CLI

Each turn runs, in the workspace:

```text
bash -lc 'exec <command> -p --output-format stream-json --verbose [--model M]
  --permission-mode <mode> --setting-sources user --strict-mcp-config [--allowedTools ...]
  [--disallowedTools ...] [--max-turns N] [--append-system-prompt S] [--mcp-config <file>]
  [--resume <session>] [extra_args...]'
```

with the prompt on stdin. The first turn starts a session; later turns resume it.

| Key | Default | Notes |
|---|---|---|
| `command` | `claude` | Executable. |
| `allowed_tools` / `disallowed_tools` | `[]` | Claude Code permission rules. |
| `permission_mode` | `acceptEdits` | `default`, `acceptEdits`, `plan`, `bypassPermissions`, `dontAsk`. |
| `max_turns` | CLI default | `--max-turns` per Symphony turn. |
| `api_key_env` | none | Passed to Claude Code as `ANTHROPIC_API_KEY`; without it Claude Code uses its own login. |
| `extra_args` | `[]` | Appended verbatim, except that `--setting-sources`, `--settings`, `--mcp-config`, `--strict-mcp-config` and `--dangerously-skip-permissions` are refused. |

Tracker tools reach Claude Code through MCP: Symphony serves them on a private Unix socket and
points Claude at `symphony agent-tools --socket <path>` (`--mcp-config`), adding
`mcp__symphony__<tool>` to `--allowedTools`. Calls run inside Symphony with the scoped tool
handler, so the tracker token never reaches Claude. Claude Code's own `Bash` tool is not
sandboxed by Symphony: keep `allowed_tools` narrow.

Claude Code only reads the operator's user settings (`--setting-sources user`) and only
Symphony's MCP config (`--strict-mcp-config`), so a cloned repository's `.claude/settings.json`
(permissions, hooks) or `.mcp.json` cannot widen `allowed_tools`; `extra_args` may not change
these flags. Tracker tokens and the other backends' keys are removed from its environment, but
Claude Code itself receives its own key (`api_key_env`, passed as `ANTHROPIC_API_KEY`, or the LLM
contract's credential), so commands its `Bash` tool runs can read that key. The `api_key_env`
variable is removed from every other child (hooks, Codex, the api shell) and redacted.

## Image and requirements

The Docker image ships the Codex CLI, the Claude Code CLI and `bubblewrap`
(`INSTALL_CODEX=false` / `INSTALL_CLAUDE_CODE=false` leave the CLIs out; the `api` backend needs
neither). Inside a container, `bubblewrap` needs unprivileged user namespaces: the default
seccomp profiles of Docker and Kubernetes (`RuntimeDefault`) block them, so the `api` backend's
shell needs a seccomp profile that allows `clone`/`unshare` with `CLONE_NEWUSER` (and, on Ubuntu
hosts, `kernel.apparmor_restrict_unprivileged_userns=0`). Without that, run the shell elsewhere
or leave the `shell` tool out of `tools`; do not fall back to `sandbox: none`.

A gated GitHub workflow (`.github/workflows/real-provider.yml`, manual `workflow_dispatch`) runs
one tiny real turn against Anthropic and an OpenAI-compatible endpoint when the repository has
`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` secrets; without them it does nothing.
