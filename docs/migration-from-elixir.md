# Migration from Elixir

Per-crate notes on parity decisions and new capabilities in the Rust port.

## symphony-store

- **New capability.** Elixir kept all run state in memory, so it was lost on restart. The Rust port
  adds a SQLite store (`crates/symphony-store`, with bundled SQLite and WAL) that records each agent
  run along with:
  - its status: `running`, `succeeded`, `failed`, `cancelled` or `blocked`;
  - the error;
  - the number of turns;
  - cumulative token usage;
  - the duration;
  - a per-run event log.
- **Live state is unaffected.** `/api/v1/state` parity still comes from the orchestrator's
  in-memory snapshot, and the store is purely additive history. `TotalsRecord` counts all retained
  runs, unlike the Elixir `codex_totals`, which only counts the current process lifetime.
- **Restart recovery.** At startup, runs left `running` by a crashed process are closed as
  `cancelled` with error `"interrupted by restart"`.
- **Configuration.** `SYMPHONY_DB_PATH` (default `./data/symphony.db`; `off` disables it, and
  `Store::disabled()` is then a no-op) and `SYMPHONY_DB_RETENTION_DAYS` (default 30; `0` keeps
  history forever). The CLI will wire these.
- **Failure handling.** Persistence failures never crash the orchestrator. Operations return
  `StoreError` values, and fire-and-forget writes log their failures with `tracing::warn!`.

## symphony-core

- **YAML parser.** `serde_yaml_ng` 0.10 (maintained `serde_yaml` fork; `serde_yaml` is archived and
  `serde_yml` is disputed). It uses YAML 1.2 core scalars like yamerl: `yes`/`on` stay strings and
  `0x1F`/`0o17` are integers. The known differences from yamerl:
  - **Duplicate keys** are a `workflow_parse_error`, where yamerl silently keeps the first value.
  - **`010`** is the string `"010"`, where yamerl reads the integer 10.
  - **Integers that do not fit in 64 bits** are a parse error.
  - **`.inf`/`.nan`** are kept as their YAML text.
  - **Comment-only front matter** decodes to `{}`, as yamerl does.
- **Config casting** is hand-written Ecto parity:
  - Numeric strings are accepted for integers (`"+5"` too, `" 5"` is not).
  - `null` behaves as an absent key, including inside nested maps.
  - Unknown keys are ignored.
  - `""` is kept.
  - Errors read `"dotted.path message"` and are joined with `", "`.

  Errors are emitted in a deterministic order: sections in schema order, then fields in declaration
  order. Elixir's order depends on map iteration and is unspecified. Ports of the tests only check
  for substrings.
- **Rejected or tightened values (deliberate deviations):**
  - **`null` inside a string list** (`required_labels: [null]`, `active_states: [null]`) is now
    `"<path> is invalid"`. Elixir crashed with a `FunctionClauseError` for labels and accepted `nil`
    for states.
  - **`server.port`** must be at most 65535 (new message `"server.port must be less than or equal to
    65535"`).
  - **Counts** (`max_concurrent_agents`, `max_turns`, per-host and per-state limits) must fit in a
    `u32`.
- **Per-state concurrency map.** Elixir parity: one bad entry rejects the whole config, with no
  `"1"` coercion. This differs from SPEC §5.3.5, which says bad entries are ignored. The test
  harness is the oracle.
- **`codex.stall_timeout_ms`** still rejects negative values, and `0` is valid (it disables stall
  detection).
- **`$VAR` rules** are kept separate per flavour, exactly as in Elixir:
  - **Linear and `workspace.root`:** no trimming; invalid references stay literal; an empty env
    value gives `nil`, with no fallback.
  - **GitHub, GitLab, Jira and Asana** (`config::resolve_*`): values are trimmed; an invalid
    reference gives `nil`; an empty env value gives `nil`; an unset variable falls back to the
    default env var.
- **Adapter validation lives in core.** The per-kind validation and settings resolution (`resolve_linear`, `resolve_github`,
  `resolve_gitlab`, `resolve_jira`, `resolve_asana`, `secret_environment_names`) are pure functions in core, so the
  `WorkflowStore` can refuse a bad workflow at boot and keep last-known-good on reload without depending on
  the tracker crate. Adapters should reuse the resolved structs; their `Debug` impls redact secrets, as does
  `TrackerSettings`' `Debug`. URL checks require a non-empty host (Elixir's `URI.parse("https://")` has host `""` and
  passed).
- **`Issue.native_ref`/labels are typed.** Labels are `Vec<String>`, so the Elixir test that stuffed
  NaiveDateTime/structs into labels is N/A. Nested `native_ref` maps render fine in templates.
- **Templates use `liquid` 0.26** (`with_stdlib`, strict variables) and match Solid on:
  - arrays concatenating;
  - nil rendering empty;
  - `{% if missing %}` being falsy without raising;
  - `{{ issue.foo }}` raising.

  Liquid rejects unknown filters at *parse* time, so such errors are reclassified as
  `TemplateRender`, matching Solid's render-time `UndefinedFilterError`. Typed errors:
  - `workflow_unavailable: …`;
  - `template_parse_error: <msg> template="<template>"` (same prefix and shape as Elixir);
  - `template_render_error: …` (Elixir raised `Solid.RenderError` unwrapped).
- **Continuation prompt.** `prompt::continuation_prompt` and `build_turn_prompt` reproduce the
  agent runner's fixed turn 2+ guidance byte for byte.
- **Workflow line splitting.** Lines split on CRLF, LF, CR, VT and FF. Elixir's non-unicode `\R`
  also matched a raw `0x85` byte, which splits and corrupts characters such as `ą`. That
  latent bug is fixed.
- **Workspace keys are byte-oriented.** Each byte of a multi-byte UTF-8 character becomes one `_`
  (`"éa/ą"` → `__a___--81ff463176b0cdfa`). This matches Elixir's non-unicode regex, and §B.10.1
  of the blueprint is wrong on this point. Workspaces created by the Elixir build are therefore
  found again.
- **PathSafety.** `canonicalize` follows Elixir: lexical `..` first, a missing tail is kept, and
  errors are `path_canonicalize_failed`. It adds a symlink-loop guard: after 40 hops it fails with
  reason `eloop` (latent bug §12.2 #10, fixed). Posix reasons are mapped from errno (`enoent`,
  `enametoolong`, `enotdir`, …).
- **Sandbox policy roots.** `Settings::resolve_runtime_turn_sandbox_policy(ws, remote, base_dir)`
  takes an optional base directory for relative local roots. `WorkflowStore::codex_runtime_settings`
  passes the workflow directory. This fixes §12.2 #12: Elixir used the CWD there, but the workflow
  dir in `local_workspace_root`. The bare function with `base_dir = None` keeps the CWD behaviour.
  The Elixir error `{:unsafe_turn_sandbox_policy, {:invalid_workspace_root, 123}}` cannot be
  produced, because the workspace argument is `Option<&str>`.
- **WorkflowStore:**
  - **API.** It is synchronous: a `Mutex` serialises reloads and `ArcSwap` holds the last-good
    snapshot. It can be used from sync and async code. `spawn_poller(CancellationToken)` adds the
    1 s tokio interval.
  - **Change detection.** The stamp is `(mtime secs, size, std SipHash of the content)`, taken
    from the same read that is parsed. Elixir re-read the file after parsing.
  - **Failure reporting.** A stat or read failure on the unchanged path reports
    `WorkflowFileUnreadable` (Elixir's bare posix atom).
  - **Error logging.** The "Failed to reload workflow … keeping last known good configuration"
    error is logged when the reason changes, and repeated identical failures at most every 30 s.
    Elixir logged on every poll and every call.
  - **Environment.** The env is injected (`EnvSource`; `ProcessEnv`/`MapEnv`), so tests never
    mutate the process environment.
  - **Default path.** The default `WORKFLOW.md` path uses the CWD captured at construction.
- **Errors.** `ConfigError`/`TrackerConfigError` `Display` keep the snake_case tags.
  `ConfigError::user_message()` reproduces `Config.format_config_error/1`, including
  `Missing WORKFLOW.md at <p>: :enoent` and `{:unsupported_tracker_kind, "x"}`.

## symphony-codex

- **Wire parity.** Newline-delimited JSON with no `"jsonrpc"` field. Request ids are fixed: 1 for
  `initialize`, 2 for `thread/start` and 3 for every `turn/start`. Responses match only on exact
  integer ids. Method names, approval decisions, the MCP auto-answer shape and tool-result
  normalization are byte-compatible.
  - `thread/start` always sends `dynamicTools`, even when the list is empty.
  - `turn/start` sends no `model`, `effort` or `summary` fields.
  - `turn/completed` always counts as success (C.13 #4). `turn/failed` and `turn/cancelled` end the
    turn only when `params` is present.
- **Launch.** The local launch is `bash -lc "[unset S… && ]exec <codex.command>"`, as in Elixir.
  It uses bash rather than `sh`, so the login profile is sourced and `unset` strips any secrets the
  profile re-exports. Secret names are the union of `Settings::secret_environment_names()` and
  `DynamicToolHandler::secret_environment_names()`, with invalid names dropped.
- **SSH is a hook.** The codex crate builds the remote command
  `cd '<ws>' && [unset … && ]exec <cmd>` (exported as `launch::remote_launch_command` and
  `shell_escape`). The runtime's `RemoteLauncher` turns it into the `ssh` process. A remote
  `worker_host` without a launcher fails with `remote_launcher_missing`.
- **Dynamic tools.** Codex does not depend on trackers. The runtime implements
  `DynamicToolHandler`, which provides `tool_specs`, `execute(tool, args, issue)` and optional
  extra secret names, as a session-start snapshot. `NoDynamicTools` reproduces the
  adapter-without-tools reply. A result with no boolean `success` used to be `inspect/1` output;
  it is now compact JSON.
- **Events.** Events are typed: `CodexEvent { timestamp, codex_app_server_pid, worker_host, usage,
  token_usage, rate_limits, data: CodexEventData }`, and `CodexEventKind` serializes to the Elixir
  snake_case names.
  - Events are delivered to an `EventSink`, which wraps an unbounded mpsc sender so the read loop
    never blocks.
  - `CodexEvent::to_json()` gives the flat Elixir message map.
  - Token usage and rate limits are extracted per event, using the orchestrator's rules
    (`tokens::extract_token_usage` and `extract_rate_limits`). `TokenAccumulator` keeps the
    high-water-mark delta logic: the `turn/completed` `usage` fallback and snake_case-only
    `limit_id` detection are kept for parity (C.13 #9).
- **Errors.** Errors are typed as `CodexError`. `tag()` returns the Elixir reason atom
  (`port_exit`, `turn_input_required`, `invalid_workspace_cwd`, …), and `Display` gives
  `"<tag>: <detail>"`. The new tags are:
  - `remote_launcher_missing`;
  - `port_spawn_failed`, where Elixir crashed in `Port.open`;
  - `invalid_turn_payload` (see below).

  A sandbox-policy canonicalization failure keeps core's `path_canonicalize_failed`.
- **Kept as Elixir:** `unsupported_tool_call` only for missing or blank tool names (C.13 #7); the
  hard-coded `"Approve this Session"` decision text (C.13 #14); and a missing codex binary showing
  as `port_exit: 127` (C.13 #10).

### Improvements

- **Blocked state is reachable (C.13 #8).** When a turn ends with `turn_input_required` or
  `approval_required`, the client no longer emits `turn_ended_with_error` afterwards. The blocker
  event therefore stays the session's last event, and the orchestrator's
  `input_required_blocker?(last_codex_event)` check works on worker exit.
  - The blocker is also explicit: `CodexError::blocker()`, `CodexEvent::blocker()` and
    `CodexEventKind::blocker()` return a `Blocker` (`InputRequired` or `ApprovalRequired`).
  - `Blocker::message()` returns the orchestrator's `blocker_error` texts.
  - The runner and orchestrator should use the returned error to block instead of retrying.
  - Every other failure still emits `turn_ended_with_error`.
- **No silently dropped server messages (C.13 #13).** While the client waits for a response, any
  message with a string `method` is buffered (up to 1024) and replayed at the start of the next
  turn loop. An approval or tool call that arrives before the `turn/start` result is therefore
  answered, not lost to a turn timeout. Stray non-method JSON is still ignored.
- **Process-group cleanup (C.13 #11, §12.2).** The child is spawned with `process_group(0)` and
  `kill_on_drop(true)`. `stop()` shuts it down in this order:
  1. close stdin;
  2. wait up to `stop_grace` (2 s by default);
  3. `SIGKILL` the whole process group, even after a clean exit, to catch leftover grandchildren;
  4. reap the child;
  5. drain the reader tasks, with a bounded wait.

  Dropping a session also kills the group. Elixir only closed the port, which could orphan children.
- **Bounded waits.**
  - Waiting for the exit after stdout EOF uses the same deadline as the read.
  - Shutdown, reaping and reader draining are all time-bounded.
  - A dynamic tool call is bounded by `turn_timeout_ms`. Elixir's turn clock did not run during a
    tool call. On timeout the call gets a failure reply, `Dynamic tool call timed out after Nms.`,
    and the turn continues.
  - stdout lines go through a bounded channel, so a server that floods output gets backpressure
    instead of unbounded buffering.
- **Startup.** The sandbox policy is resolved before the process is spawned; Elixir opened the port
  first. Payload errors are explicit:
  - `thread/start` without `thread.id` gives `invalid_thread_payload` (C.13 #5).
  - `turn/start` without `turn.id` gives `invalid_turn_payload`, plus a `startup_failed` event.
    Elixir passed the raw map through, and it crashed later.

  Numeric ids are stringified. Non-object JSON lines no longer crash the response wait (C.13 #6).
- **stderr stays separate (C.13 #1).** It is logged line by line with the Elixir rules
  (`Codex <label> output: …`, a warning when an error keyword appears, truncated to 1000
  characters). It never yields `malformed`, and the last 50 lines are available through
  `AppServerSession::stderr_tail()`.
- **Secrets on remote launches (C.13 #12).** Secret env vars are also removed from the local `ssh`
  process environment, so `SendEnv` cannot forward them. Elixir relied only on the remote `unset`.
- **Structured tracing.** The client uses `codex_session` and `codex_turn` spans with the issue
  id, identifier and worker host.
