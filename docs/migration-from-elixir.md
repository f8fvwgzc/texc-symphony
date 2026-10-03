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

- **Deterministic stderr attribution (found by stress testing).** stderr is a separate pipe in Rust, so the
  stream label switches to the turn label *before* `turn/start` is written. Server output produced while handling
  the turn is therefore always logged as `turn stream output`. With the label set only after the response, a
  loaded machine could log it as `response stream output` (Elixir merged stderr into stdout and never had
  this race).

## symphony-trackers

- **Trait shape.** `Tracker` is an object-safe `async_trait`. Every read takes the *current*
  `TrackerSettings` as an argument, because Elixir re-read the live config on every call. Tool
  execution uses the snapshot captured by `ToolBinding::bind`, matching `Tracker.bind_agent_tools/0`.
  `build_tracker` / `tracker_for_kind` dispatch on the six exact kind strings.
- **Adapter validation and secret names reuse core.** The adapters call `config::resolve_*`,
  `validate_tracker` and `secret_environment_names`; they do not duplicate those rules. `$VAR` and
  default env vars are still resolved on every call, through an injected `EnvSource`.
- **Writes.** Per SPEC §11.5 there is still no generic comment or state CRUD. Mutations go through
  each adapter's agent tool, backed by the public raw passthroughs `LinearTracker::graphql` and
  `{GitHub,GitLab,Jira,Asana}Tracker::request`. These return the status and body without status
  mapping.
- **Errors.** `TrackerError` keeps the Elixir reason tags in `Display`/`tag()`, for example
  `github_api_status: 503` or `jira_missing_next_page_token`.
  - `inspect()` renders the Elixir `inspect/1` form used in tool `"reason"` fields, such as
    `:timeout` or `{:github_api_status, 503}`.
  - `category()` maps each error to the SPEC §11.4 categories, with 429 mapped to
    `tracker_rate_limited`.
  - `user_message()` keeps the orchestrator's two special-cased Linear messages.
- **Test seams.** Elixir injected `request_fun`/client closures. Rust injects a `Transport` instead:
  production uses `ReqwestTransport`, and the tests use `wiremock`. The redirect test routes logical
  origins (`https://gitlab.test`, `https://sink.test`) to mock servers via
  `ReqwestTransport::with_origin_override`, so the redirect/credential logic runs unmodified.
  Elixir tests that needed non-JSON terms (a PID body, a non-integer status, atom-keyed maps) are
  N/A; a text body test replaces the PID case.
- **Tool output.** The output is pretty JSON with sorted keys (`serde_json` without
  `preserve_order`), byte-compatible with `Jason.encode!(pretty: true)` for these payloads. The
  generic "unsupported" response (Memory) stays compact JSON. A non-object Linear body renders like
  Elixir `inspect`: strings are quoted and `null` is `nil`.
- **Query parameters.** Scalars are stringified (`10` becomes `"10"`, `true` becomes `"true"`,
  `null` becomes `""`). Nested arrays/objects in `params`/`query` are rejected with the tool's
  `invalid_params`/`invalid_query` message, where Elixir crashed in `URI.encode_query`. An empty map
  never adds a trailing `?`. Inline `?query` in a tool path keeps working, and params are appended
  with `&`.
- **URL normalization.** Tool paths are parsed with the WHATWG `url` crate, which resolves `..`
  segments before sending. Elixir sent them raw and the server resolved them. Either way the path
  stays on the configured host, under the same credential.
- **Content types.** `application/json` and any `*/*+json` are decoded as JSON. Other bodies become
  JSON strings, and an empty body is `""`. Invalid JSON under a JSON type is a transport error
  (`invalid_json`), as in Req.

### Improvements

- **Pagination safety cap.** Elixir had no cap, and a provider that repeated a cursor looped
  forever.
  - A read stops after `MAX_PAGES` = 1,000 pages with `<provider>_pagination_limit_exceeded`
    (category `tracker_pagination`).
  - Cursor-based providers (Linear `endCursor`, Jira `nextPageToken`, Asana `offset`) fail fast
    with `<provider>_pagination_repeated_cursor` when a cursor repeats within one read.
- **Token scrubbing.** Elixir had none.
  - Every `TransportError` message is scrubbed before it leaves `HttpClient`. That covers the exact
    credential values, any `Bearer …`/`Basic …` token, and URL user-info.
  - reqwest errors drop their URL, and credential headers are marked sensitive.
  - `Debug` for requests redacts header values.
  - The Linear non-200 body log is scrubbed with the API key.
  - Tool `"reason"` strings and logs therefore never carry a token. A test drives a transport whose
    errors echo the `Authorization` header.
- **Retry-After cap.** Req's `safe_transient` retry policy is reproduced: `GET`/`HEAD` only; status
  408/429/500/502/503/504 or timeout/refused/closed; 3 retries at 1 s, 2 s and 4 s, or
  `Retry-After`. A server-supplied `Retry-After` is now capped at 60 s
  (`RetryPolicy::max_delay`), so one bad header cannot stall a poll for an hour. POST (Linear, Jira
  search/bulkfetch) and all non-GET tool calls are still never retried.
- **Redirects.** Redirects are followed manually: at most 10 hops, then `too_many_redirects`.
  - On a scheme, host or port change, `Authorization`, `Private-Token`, `Cookie` and
    `Proxy-Authorization` are dropped. They stay dropped for the rest of the chain, including a
    bounce back to the origin.
  - 301/302/303 turn a non-HEAD request into a body-less GET and drop `Content-Type`; 307/308 keep
    the method and body.
  - This is tested for same-origin, cross-origin and A→B→A chains.
- **Linear: missing `pageInfo`.** A page with `data.issues.nodes` but an incomplete `pageInfo` now
  ends pagination and returns the accumulated pages plus that page. Elixir returned only that page
  and dropped the earlier ones (D1.5.4).
- **Linear: missing API key in `graphql/3`.** It now returns `missing_linear_api_token`, so
  `linear_graphql` shows the dedicated auth message. Elixir wrapped it as
  `{:linear_api_request, :missing_linear_api_token}` and showed the generic transport message
  (D1.9.1).
- **GitLab: unassigned issues.** `assignee_id` only inspects `assignees[0]`/`assignee` objects, so
  an unassigned issue gives `None`. Elixir fell through to the issue map and reported the issue's
  own global id as the assignee (D1.7.4).
- **Jira: ADF `attrs`.** Only *string* `attrs.text`/`shortName`/`url` values are used, and
  non-strings fall through to the next attribute. Elixir's `||` could return a number and crash the
  string concatenation.
- **Asana: `permalink_url`.** A non-string `permalink_url` normalizes to `None` instead of leaking a
  non-string into `Issue.url`.
- **Structured tracing.** Each HTTP exchange runs in a `tracker_http` debug span with the method and
  the query-less URL. Retry attempts log Req-style warnings:
  `retry: got response with status 503, will retry in 1000ms, 3 attempts left`.

## symphony-server

- **LiveView becomes a Vite SPA plus SSE.** Phoenix, LiveView, the `/live` socket, the session
  cookie, CSRF, `secret_key_base`, `Plug.MethodOverride` and the vendored Phoenix JS are gone.
  - `GET /` serves the `web/` dashboard (a Vite bundle that `build.rs` embeds from `web/dist` at
    compile time).
  - The dashboard reads the JSON API and subscribes to `GET /api/v1/events` (Server-Sent Events).
  - The old `/dashboard.css` and `/vendor/...` routes are dropped and now answer the JSON 404.
  - If `web/dist` is missing at build time, a small placeholder page is embedded instead
    (`src/placeholder.html`, which tells you to run `pnpm --dir web build`). Cargo prints a
    warning, but the Rust build never fails. `SYMPHONY_WEB_DIST` overrides the bundle path, and
    `symphony_server::WEB_UI_EMBEDDED` tells the binary which page it embedded.
  - Caching: `index.html` and the other unhashed files are `no-cache`, and hashed `/assets/*` are
    `public, max-age=31536000, immutable`. Every file has a strong ETag, and `If-None-Match`
    answers 304.
  - There is no SPA fallback, because the dashboard uses hash routing. Every other non-API path
    keeps the Elixir JSON `404 not_found`.
- **No dependency on the runtime.** The server defines its own view model (`view::*`, which is
  also the JSON contract) and the `ControlPlane` trait. The binary adapts the orchestrator to
  that trait, using `state`, `refresh`, `workspace_root` and `changes` (a `watch::Receiver<u64>`
  generation counter that replaces the PubSub topic). `issue` has a default implementation built
  on `presenter::issue_view`.
- **Kept as Elixir.** These keep their Elixir behavior:
  - Routes, the 404/405 envelopes, and `GET /api/v1/state` always returning 200, with
    `snapshot_timeout` / `snapshot_unavailable` reported in the body.
  - Issue lookup: running, then retrying, then blocked; any snapshot failure gives
    `issue_not_found`.
  - `restart_count = max(attempt - 1, 0)`, `recent_events` taken from `running || blocked` and
    dropped when there is no timestamp, and the workspace fallback
    `Path.join(workspace.root, workspace_key(id))`.
  - `logs.codex_session_logs` is always `[]`, `tracked` is always `{}`, and `due_at` is
    recomputed per request (E.9 #5).
  - `codex_totals.seconds_running` counts ended sessions only, and is passed through (E.9 #1).
  - Timestamps are second-truncated with a `Z` suffix, except `requested_at`, which has
    microseconds.
  - A single trailing slash is ignored (`NormalizePathLayer`), and `HEAD` works on every `GET`
    route.
  - The bind host must be an IP literal or a name that resolves, preferring IPv4. A bad host
    or a busy port is a startup error.
  - Lists are sorted by `issue_id` (E.9 #14).
- **Serialization details.** `seconds_running` serializes whole values as integers (`42`) and
  others as floats (`42.5`), as Jason did for integer and float terms. JSON key order is the
  struct order; clients must not depend on it.
- **Reserved identifiers.** Literal routes win, so `state`, `refresh`, `health`, `events`, `runs`
  and `totals` cannot be looked up as issues. An identifier that is not valid UTF-8 after
  percent-decoding answers `404 issue_not_found`.
- **New endpoints.** These follow `docs/api/openapi.yaml`:
  - `health`, which never touches the orchestrator;
  - `events` (SSE);
  - `runs`, `runs/{id}`, `runs/{id}/events` and `totals`, backed by `symphony-store`. They answer
    `503 store_disabled` when persistence is off. Bad values answer `400 invalid_parameter`, and
    the message names the parameter. An unknown run answers `404 run_not_found`.
  - `/api/openapi.json`. `build.rs` converts the YAML contract at compile time, keeping key order.

### Improvements

- **Debounced, shared snapshots for live updates (E.9 #12).** Elixir broadcast on every Codex
  event, and each LiveView took a full snapshot per message.
  - One SSE hub task debounces generation changes on both edges (200 ms by default), so a burst
    yields at most about 5 snapshots per second.
  - The hub takes **one** snapshot per window for all clients, and none while no client is
    connected.
  - Each client is pull-based and reads a `watch` channel that holds only the latest frame. A
    slow client skips intermediate snapshots and always gets the newest one, with constant memory
    per client.
  - The hub subscribes to changes before the first client takes its initial snapshot, so no
    change can fall between them. A test caught this race.
- **SSE protocol.**
  - The stream opens with `retry: 3000`.
  - Each `snapshot` event's `id` is the generation.
  - A `heartbeat` (`{at, generation}`, no id) is sent every 15 s.
  - A client that reconnects with `Last-Event-ID` gets the current snapshot first.
  - Headers are `Cache-Control: no-cache` and `X-Accel-Buffering: no`. SSE is never compressed or
    subject to the request timeout.
  - `max_sse_clients` (default 64) bounds streams; extra clients get `503 request_failed`.
  - Streams end as soon as shutdown starts, so graceful shutdown never hangs on open dashboards.
    `shutdown_grace` (10 s) bounds how long other connections may take to finish.
- **Timeouts are enforced by the server, not the implementor.**
  - Snapshot: 15 s, reported as `snapshot_timeout`.
  - Issue lookup: 15 s, answered as a 404.
  - Refresh: 5 s, answered as `503 orchestrator_unavailable`. Elixir crashed into a 500 here
    (E.9 #13).
  - Every non-streaming request: 30 s, answered as `503 request_failed`.
- **Failures keep the JSON envelope.**
  - A handler panic is caught and answers `500 request_failed`, matching Phoenix
    `render_errors`, instead of dropping the connection.
  - Store errors map to `request_failed` (500, or 503 when the store thread is gone) and are
    logged.
- **Hardening.**
  - Security headers go on every response, not just `GET /`: `nosniff`, `SAMEORIGIN`,
    `strict-origin-when-cross-origin`, `x-permitted-cross-domain-policies: none`, and a strict
    CSP (`script-src 'self'`).
  - Every response carries an `x-request-id`, and requests are traced with tower-http.
  - Responses are compressed with gzip or brotli when the client asks.
  - CORS is off by default. `cors_allowed_origins` opts in, and `*` allows any origin.

## symphony-runtime

- **Actor instead of GenServer.** The orchestrator is one tokio task that owns all scheduling state
  and handles one event at a time from a biased `select!`: commands (snapshot, refresh), timer
  events (tick, poll cycle, retry), worker runtime info, per-run Codex event streams, and worker
  completions from a `JoinSet` (the monitor `DOWN`). Tracker calls are still awaited inline, so a
  snapshot waits behind a slow tracker call, bounded by the client's 15 s timeout.
- **Run ids replace monitor refs.** Each dispatch gets a `run_id`. Codex events arrive on a stream
  keyed by that run id, so events from a killed run cannot reach a newer run of the same issue.
  Elixir keyed updates by issue id. When a worker completes, its remaining runtime-info messages and
  Codex events are drained before the exit is handled, which keeps Elixir's "messages before `DOWN`"
  ordering.
- **Timers.** Tick and retry timers still carry tokens and are checked on fire (G1). Superseded or
  removed timers are also aborted. The 20 ms poll-cycle start has no token, as in Elixir.
- **Time.** Durations use tokio's monotonic clock: backoff, poll countdown, runtime seconds and stall
  detection. Tests therefore run on the paused clock. Displayed timestamps (`started_at`,
  `blocked_at`, `due_at`) use `Utc::now()`. Stall detection measures monotonic time since the last
  Codex activity, where Elixir used wall-clock `last_codex_timestamp || started_at`.
- **Error strings are kept** (§12.2 #15): `agent exited: <reason>`, `retry poll failed: <reason>`,
  `retry dispatch refresh failed: <reason>`, `stalled for <n>ms without codex activity` and
  `no available orchestrator slots`. Reasons are rendered with their `Display` tag form, such as
  `workspace_prepare_failed: worker_host=worker-a status=75 ...`, not Elixir `inspect`.
- **Blocked state is typed.** A run is blocked when the entry's last event is
  `turn_input_required`/`approval_required`, when its last message is an MCP elicitation, or (new)
  when the worker fails with `CodexError::blocker()`. This resolves §12.1 risk #2 together with the
  codex crate. The test-only `completion` field is gone; its test is ported as "normal exit after
  an input-required event".
- **Worker seam.** The orchestrator spawns `WorkerFactory` futures. Production uses
  `CodexWorkerFactory`/`AgentRunner`, and tests use `FnWorkerFactory`. A worker reports through
  `WorkerContext { events: EventSink, reporter, cancel, cancel_grace, ... }`. A spawn can no longer
  fail, so the Elixir spawn-failure path (G6) is gone. `schedule_issue_retry` still claims the issue
  defensively.
- **Snapshot.** `Snapshot` is a typed struct with Elixir field names. The differences:
  - Lists are sorted: running and blocked by identifier, retrying by `due_in_ms`.
  - `polling.checking` replaces `checking?`.
  - A blocked row's `session_id` is `Option`, and presenters render `None` as `"n/a"`.
  - New fields: `generation`, `max_concurrent_agents`, `workspace_root`, `tracker {kind,
    project_slug}`, retry `due_at`, and running `retry_attempt`.
  - `Snapshot::issue(identifier)` implements the E.2.4 lookup, including the
    `<workspace.root>/<workspace_key>` fallback.
- **Handle.** `RuntimeHandle` survives orchestrator restarts. `snapshot()` returns `Timeout` or
  `Unavailable`. `request_refresh()` returns `Unavailable` or a new `Timeout`, where Elixir crashed
  the request with a 500. `subscribe()` returns a `watch::Receiver<u64>` generation counter that
  replaces the PubSub topic and is bumped on every state change. The first command channel is
  installed before `Runtime::start` returns, so early requests queue instead of failing.
- **SSH configuration is explicit.** `SshConfig::from_env()` reads `SYMPHONY_SSH_CONFIG` once, and
  the executable can be injected. Tests point it at a fake `ssh` instead of rewriting `PATH`, which
  edition 2024 makes `unsafe`. As a result there is no `ssh_not_found` test that hides `PATH`; a
  missing explicit executable reports `ssh_spawn_failed`.
- **Normalized SSH hosts.** `worker.ssh_hosts` is trimmed and deduplicated once
  (`workspace::worker_hosts`). The scheduler's per-host counts and the runner's host now agree
  (B.14 "messy config").
- **Humanized messages.** `humanize::humanize_codex_message` ports E.5.9 for the API `last_message`
  and the dashboard EVENT column. It looks up string keys only, and compact JSON replaces
  `inspect/1`. Session-level events now keep their details in `last_codex_message`, so they render
  as `session started (<id>)` and `turn ended with error: <reason>`. Elixir stored `nil` and
  rendered `... error: nil`.
- **Missing identifiers.** `remove_issue_workspaces(None, _)` does nothing. Elixir derived the key
  `"issue"` and removed `<root>/issue`.
- **Startup cleanup** still finishes before the first tick (§12.2 #22), but it is concurrent; see
  the improvements below.

### Improvements

- **`after_run` on cancellation (G4, D4).** Reconciliation, stall handling and shutdown cancel a
  worker cooperatively. The runner then drops the Codex session (killing its process group) and
  runs `after_run`, bounded by `min(hooks.timeout_ms, worker_cancel_grace)`. The orchestrator waits
  `worker_cancel_grace` (10 s by default) plus 500 ms, then aborts the worker. It always waits for
  the worker to be gone before `before_remove` and `rm -rf` (G3). An aborted or panicking worker
  still skips `after_run`.
- **Process groups for hooks and remote commands (G5).** Local hooks (`sh -lc`) and every `ssh`
  invocation run in their own process group, and the whole group gets `SIGKILL` on timeout or when
  the waiting future is dropped. Elixir left `sh` and its children running. Output is merged
  stdout and stderr, as in Elixir, and drained for at most 250 ms after exit, so a background child
  that holds the pipe cannot hang a hook.
- **Hook timeout names and real timeout tests (G12, §12.2 #11).** Remote hook timeouts report the
  hook name; prepare and remove scripts still report `remote_command`. Real timeout tests replace
  the stale Elixir one, for local and remote hooks, and include a check that a timed-out hook's
  background child dies.
- **Hooks no longer receive tracker secrets (D10).** The tracker secret env vars
  (`Settings::secret_environment_names`) are removed from hook processes and from the local `ssh`
  of remote hooks, matching what Codex already got. `RunnerOptions::strip_hook_secrets = false`
  restores the Elixir behaviour for workflows whose hooks need the token, for example a `git clone`
  with `$GITHUB_TOKEN`.
- **Remote workspace containment (§12.1 #10).** Remote paths ending in `/.` or `/..` (identifiers
  `.` or `..`) are rejected locally. The prepare script also resolves `pwd -P` of the workspace and
  of the root on the worker. If the workspace is not strictly inside the root it exits 3 with a
  `__SYMPHONY_WORKSPACE_ESCAPE__` line, which becomes `workspace_outside_root`, before
  `after_create` can run.
- **Stall retries keep their host (G8).** The stall retry carries `worker_host` and
  `workspace_path`, so it prefers the same machine and the recorded workspace.
- **No stuck claims (G7).** If a retry dispatch finds no SSH capacity after revalidation, it is
  requeued with `no available orchestrator slots` instead of being dropped with the claim held.
- **Bounded, concurrent startup cleanup (G9, §12.2 #22).** Terminal-issue workspaces are removed
  concurrently across (issue × host) pairs, capped by `startup_cleanup_concurrency` (8 by default).
  Each SSH command is bounded by `hooks.timeout_ms`. Removing an issue's workspaces on every
  configured host is concurrent as well.
- **Tracker read timeout.** Every orchestrator and runner tracker read is bounded (120 s by default,
  `TrackerClient::with_timeout`) and fails with `tracker_timeout: <ms>ms` instead of wedging the
  actor.
- **One-for-all restart without overlap.** If the orchestrator panics, the supervisor aborts every
  worker of that incarnation **and awaits** them before a fresh orchestrator starts. Workers' Codex
  and hook process groups die on drop. More than 3 restarts within 5 s end the runtime with
  `RuntimeError::RestartBudgetExceeded`. A panicking worker is treated like a failed run
  (`agent exited: worker panicked: ...`) and retried.
- **Run history (new).** Each dispatch records a `symphony-store` run.
  - Status mapping: normal exit → `succeeded`, error → `failed`, blocker → `blocked`, and
    reconciliation, stall or shutdown → `cancelled` with the reason.
  - Recorded data: runtime info, turns, cumulative tokens (written only when they change) and Codex
    events.
  - Streaming deltas and token-count notifications are throttled to one per method per second per
    run.
  - Writes are fire-and-forget on a per-run task, so store errors never affect scheduling. A run
    whose recorder disappears (orchestrator crash) is closed as `cancelled`.
- **Structured tracing.** Each worker runs inside a `worker` span (issue id and identifier, run id,
  attempt, worker host), and the runner adds an `agent_run` span. The Elixir log texts are kept.

## symphony (binary)

- **One `main`.** `SymphonyElixir.CLI` (escript), the Burrito `__BURRITO=1` path and
  `Application.start/2` become `symphony_cli::app::main` (`crates/symphony/src/app.rs`); `main.rs`
  only calls it. `cli::evaluate` is pure (CWD, environment and the `File.regular?/1` check are
  injected), so the `cli_test.exs` cases run as unit tests without touching process state.
- **CLI parity.** The Elixir check order is kept: parse/usage, then the acknowledgement banner
  (byte-identical red box; `This Symphony implementation is a low key engineering preview.` is the
  first line the release smoke test greps), then `--logs-root` (trimmed, last wins, empty is a
  usage error), then `--port` (last wins), then the workflow file (`Workflow file not found:
  <expanded path>`), then boot. Parsing still happens before the acknowledgement check. Bad
  arguments print the exact Elixir line `Usage: symphony [--logs-root <path>] [--port <port>]
  [path-to-WORKFLOW.md]` followed by a second line `Run \`symphony --help\` for all options.`
  (the Rust CLI has more options than that line lists) and exit 1 instead of clap's 2.
- **Superset of the CLI.** `--host`, `--db-path`/`--no-db`, `--help`, `--version` (crate version
  plus the build-time `SYMPHONY_VERSION_SUFFIX`, e.g. `0.1.0-nightly`), environment fallbacks
  (`SYMPHONY_WORKFLOW`, `SYMPHONY_HOST`, `SYMPHONY_PORT`, `SYMPHONY_LOGS_ROOT`, `SYMPHONY_DB_PATH`,
  `SYMPHONY_DB_RETENTION_DAYS`, `SYMPHONY_LOG_FORMAT`; a flag always wins, the variable wins over
  `WORKFLOW.md`), and the `workspace before-remove` subcommand. A workflow file literally named
  `workspace` must be passed as `./workspace`.
- **Startup order.** The workflow is loaded and validated before logging is configured (Elixir
  configured the log handler first), because whether stdout carries logs depends on
  `observability.dashboard_enabled`. Load errors still go to stderr with
  `Failed to start Symphony with workflow <path>: <reason>`, where `<reason>` is
  `ConfigError::user_message()` instead of an Elixir `inspect` term. A bad HTTP host or a busy port
  aborts startup with the same prefix (Elixir: the `HttpServer` child failed to start).
- **Exit codes.** 0 after SIGINT/SIGTERM (the BEAM had its own handling); 1 for usage, banner,
  missing/invalid workflow, startup failures, a runtime that exceeded its restart budget, or a
  second signal during shutdown.
- **Log file.** `<logs_root>/log/symphony.log`, 10 MiB × 5, written by a small size-rotating
  writer (`rotating.rs`; `tracing-appender` only rotates by time). Files are `symphony.log`
  (active) and `symphony.log.1`..`.5`, not disk_log's `.1..5`/`.idx`/`.siz`. Lines are `tracing`'s
  single-line text format; the `key=value` context stays in the message text, so the `debug`
  skill's grep patterns still match.
- **Terminal dashboard.** `dashboard::format` reproduces the E.5 frames byte for byte; the Elixir
  golden fixtures are copied verbatim to `crates/symphony/tests/fixtures/status_dashboard_snapshots/`
  and checked by `tests/status_dashboard_snapshots.rs` (`UPDATE_SNAPSHOTS=1` rewrites them, as in
  Elixir). `dashboard::Scheduler` ports the render coalescing (one frame per
  `render_interval_ms`, latest wins, identical frames skipped, re-render at least every second);
  `dashboard::tps` ports the 5 s rolling throughput, its once-per-second throttle and the
  (still unrendered) 10-minute sparkline with the Elixir test vectors. Without a TTY the dashboard
  falls back to `COLUMNS`, 120 when unset and 115 when invalid, as in Elixir.
  `observability.refresh_ms`/`render_interval_ms` are re-read on every tick and
  `dashboard_enabled: false` stops a running dashboard (it cannot be switched on live), as in
  Elixir.
- **HTTP adapter.** `control::RuntimeControlPlane` implements `symphony_server::ControlPlane` over
  `RuntimeHandle`: snapshot rows map onto the API views with `humanize_codex_message` for
  `last_message`, `due_at` is recomputed per request from `due_in_ms` (Elixir parity, may jitter by
  one second), `SnapshotError::{Timeout, Unavailable}` map to `snapshot_timeout`/
  `snapshot_unavailable`, and both refresh failures map to `503 orchestrator_unavailable`.
- **`workspace before-remove`** ports `mix workspace.before_remove` with the same messages, the
  hard-coded `openai/symphony` default repository and the same no-op rules (no branch, no `gh`,
  `gh auth status` failing, `gh pr list` failing). Commands are resolved on `PATH` at call time
  through a `CommandRunner`, so tests inject fakes instead of mutating the global `PATH`.
- **Not ported:** `mix specs.check` (Rust signatures are always typed; `missing_docs` and
  `clippy -D warnings` replace the policy), and the Phoenix `secret_key_base`.

### Improvements

- **Logs reach `docker logs` and journald.** Elixir removed the console handler whenever the file
  handler was installed, so a container or a systemd unit showed nothing. Logs now also go to
  stdout whenever stdout is not a terminal or the terminal dashboard is disabled, as text or, with
  `SYMPHONY_LOG_FORMAT=json`, as JSON lines.
- **TTY check for the terminal dashboard** (E.9 #6). It only runs when stdout is a terminal, so
  redirected output never fills with clear-screen escape sequences; the `app_status=offline` frame
  is likewise only written when the dashboard was running.
- **Live status colours** (E.9 #8). Elixir compared string literals with atom events, so nearly
  every live row was blue. The colour key is now the event name, or the `codex/event/*` method of a
  wrapper notification, so `turn_completed` (magenta), `codex/event/task_started` (green) and
  `codex/event/token_count` (yellow) light up as intended. The golden fixtures are unchanged.
- **One line per retry entry** (E.9 #9). Elixir joined retry rows with `", "` and split them again,
  so an error text containing `", "` broke into unprefixed lines.
- **Render fingerprint includes the frame context.** A change to `agent.max_concurrent_agents`,
  the project slug or the bound URL redraws immediately instead of within the 1 s periodic
  re-render.
- **Graceful, bounded shutdown.** SIGINT/SIGTERM stop the dashboard, cancel agent runs (each gets
  the runtime's 10 s grace for `after_run` before its process group is killed), drain the HTTP
  server (10 s), flush the run-history store (bounded by 10 s) and exit 0. A second signal forces
  exit 1 immediately; the tokio runtime is shut down with a 5 s bound so a stuck blocking task
  cannot hang the exit.
- **Run history lifecycle.** At startup the store closes runs left `running` by a crashed process
  (`cancelled`, "interrupted by restart") and prunes by `SYMPHONY_DB_RETENTION_DAYS` (newest 100
  runs always kept), then prunes every 24 h. An unopenable database aborts startup with a hint to
  use `--no-db` instead of silently running without history.
- **Memory tracker from `WORKFLOW.md`.** With `tracker.kind: memory`, `tracker.provider.issues`
  seeds the in-memory tracker (applied synchronously before the first poll, re-applied when the
  reloaded workflow changes it; `dispatchable` defaults to `true`). Elixir could only fill it from
  application env in tests; this makes demos and the binary's end-to-end test possible without a
  real tracker.
- **`before-remove` reads PR numbers from stdout only.** Elixir merged stderr into stdout, so a
  `gh` warning could be taken for a PR number. Failure output still includes both streams. A
  process killed by a signal reports `exit signal` instead of crashing the task.
- **Invalid environment values are named** (`Invalid SYMPHONY_PORT="abc": expected a port number
  between 0 and 65535`) instead of falling back silently.

## xtask

- **`pr-body-check`** ports `mix pr_body.check` (`cargo run -p xtask -- pr-body-check --file F`).
  Messages are byte-compatible (`Missing required heading: ...`, `Required headings are out of
  order.`, the placeholder, empty-section, bullet and checkbox errors, `ERROR: ` prefixes on
  stderr, `PR body format invalid. Read \`<template>\` and follow it precisely.`, `PR body format
  OK`), and so are the matching rules: substring heading search, sections only when a heading is
  followed by exactly `"\n\n"`, the next section starting at the first `"\n" + <any other
  heading>`, CRLF headings keeping their `\r`. Invalid options print the Elixir
  `Invalid option(s): [{"--wat", nil}]`; positional arguments are ignored.
- The template is still looked up relative to the working directory
  (`.github/pull_request_template.md`, then `../.github/pull_request_template.md`); new:
  `--template <path>` overrides it and `$PR_BODY_FILE` stands in for `--file`. An unreadable body
  reports the OS error text instead of an Elixir atom (`Unable to read missing.md: No such file or
  directory (os error 2)`).
- All 15 `pr_body_check_test.exs` cases are ported as unit tests (run against temporary
  directories passed as the working directory, so they run in parallel), plus a test that the
  repository's own template accepts a filled-in body.
