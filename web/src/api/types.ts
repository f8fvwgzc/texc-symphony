/**
 * Types for the Symphony observability API.
 *
 * Hand-written from `docs/api/openapi.yaml` (the contract the Rust server implements).
 * Keep the two in sync: every schema name below matches a `components/schemas` entry.
 */

/** UTC RFC 3339 timestamp, e.g. `2026-02-24T20:15:30Z`. */
export type Timestamp = string;

/** `{"error": {"code", "message"}}` — body of every non-2xx response. */
export interface ErrorBody {
  code: string;
  message: string;
}

export interface ErrorEnvelope {
  error: ErrorBody;
}

/** Known machine-readable error codes (the set is open; unknown codes are still strings). */
export type ErrorCode =
  | 'not_found'
  | 'method_not_allowed'
  | 'issue_not_found'
  | 'orchestrator_unavailable'
  | 'run_not_found'
  | 'invalid_parameter'
  | 'store_disabled'
  | 'request_failed';

export interface TokenUsage {
  input_tokens: number | null;
  output_tokens: number | null;
  total_tokens: number | null;
}

export interface RunningEntry {
  issue_id: string;
  issue_identifier: string;
  issue_url: string | null;
  state: string | null;
  worker_host: string | null;
  workspace_path: string | null;
  session_id: string | null;
  turn_count: number;
  last_event: string | null;
  last_message: string | null;
  started_at: Timestamp | null;
  last_event_at: Timestamp | null;
  tokens: TokenUsage;
}

export interface RetryEntry {
  issue_id: string;
  issue_identifier: string;
  issue_url: string | null;
  attempt: number | null;
  due_at: Timestamp | null;
  error: string | null;
  worker_host: string | null;
  workspace_path: string | null;
}

export interface BlockedEntry {
  issue_id: string;
  issue_identifier: string;
  issue_url: string | null;
  state: string | null;
  error: string | null;
  worker_host: string | null;
  workspace_path: string | null;
  session_id: string | null;
  blocked_at: Timestamp | null;
  last_event: string | null;
  last_message: string | null;
  last_event_at: Timestamp | null;
}

/** `seconds_running` counts ended sessions only; add live elapsed time of running rows. */
export interface CodexTotals {
  input_tokens: number;
  output_tokens: number;
  total_tokens: number;
  seconds_running: number;
}

/** Any JSON value: Codex rate limits are passed through verbatim. */
export type JsonValue =
  string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };

export type JsonObject = { [key: string]: JsonValue };

export interface StateSnapshot {
  generated_at: Timestamp;
  counts: { running: number; retrying: number; blocked: number };
  running: RunningEntry[];
  retrying: RetryEntry[];
  blocked: BlockedEntry[];
  codex_totals: CodexTotals;
  rate_limits: JsonValue;
}

export type SnapshotErrorCode = 'snapshot_timeout' | 'snapshot_unavailable';

/** In-band snapshot failure (still HTTP 200). */
export interface StateError {
  generated_at: Timestamp;
  error: { code: SnapshotErrorCode | (string & {}); message: string };
}

/** Body of `GET /api/v1/state` and of every SSE `snapshot` event. */
export type StatePayload = StateSnapshot | StateError;

export function isStateError(payload: StatePayload): payload is StateError {
  return 'error' in payload;
}

export interface IssueRunning {
  worker_host: string | null;
  workspace_path: string | null;
  session_id: string | null;
  turn_count: number;
  state: string | null;
  started_at: Timestamp | null;
  last_event: string | null;
  last_message: string | null;
  last_event_at: Timestamp | null;
  tokens: TokenUsage;
}

export interface IssueRetry {
  attempt: number | null;
  due_at: Timestamp | null;
  error: string | null;
  worker_host: string | null;
  workspace_path: string | null;
}

export interface IssueBlocked {
  worker_host: string | null;
  workspace_path: string | null;
  session_id: string | null;
  state: string | null;
  error: string | null;
  blocked_at: Timestamp | null;
  last_event: string | null;
  last_message: string | null;
  last_event_at: Timestamp | null;
}

export interface RecentEvent {
  at: Timestamp;
  event: string | null;
  message: string | null;
}

export type IssueStatus = 'running' | 'retrying' | 'blocked';

export interface IssueDetail {
  issue_identifier: string;
  issue_id: string;
  status: IssueStatus;
  workspace: { path: string; host: string | null };
  attempts: { restart_count: number; current_retry_attempt: number };
  running: IssueRunning | null;
  retry: IssueRetry | null;
  blocked: IssueBlocked | null;
  logs: { codex_session_logs: JsonValue[] };
  recent_events: RecentEvent[];
  last_error: string | null;
  tracked: JsonObject;
}

export interface RefreshAccepted {
  queued: true;
  coalesced: boolean;
  requested_at: string;
  operations: string[];
}

export type StoreMode = 'sqlite' | 'disabled';

export interface Health {
  status: 'ok';
  version: string;
  uptime_seconds: number;
  store: StoreMode;
}

/** Payload of SSE `heartbeat` events. */
export interface Heartbeat {
  at: Timestamp;
  generation: number;
}

export const RUN_STATUSES = ['running', 'succeeded', 'failed', 'cancelled', 'blocked'] as const;

export type RunStatus = (typeof RUN_STATUSES)[number];

export function isRunStatus(value: string | null | undefined): value is RunStatus {
  return RUN_STATUSES.some((status) => status === value);
}

export interface RunTokens {
  input: number;
  output: number;
  total: number;
}

export interface RunRecord {
  id: number;
  issue_id: string;
  issue_identifier: string;
  issue_title: string | null;
  attempt: number;
  worker_host: string | null;
  workspace_path: string | null;
  status: RunStatus;
  error: string | null;
  turns: number;
  started_at: string;
  finished_at: string | null;
  duration_ms: number | null;
  tokens: RunTokens;
}

export interface RunList {
  runs: RunRecord[];
  next_before_id: number | null;
}

export interface RunEvent {
  run_id: number;
  seq: number;
  at: string;
  kind: string;
  message: string | null;
  payload: JsonValue;
}

export interface RunEventList {
  events: RunEvent[];
}

export interface Totals {
  runs_total: number;
  runs_succeeded: number;
  runs_failed: number;
  tokens: RunTokens;
  runtime_ms: number;
}

/** Query of `GET /api/v1/runs`. */
export interface ListRunsQuery {
  limit?: number;
  before_id?: number;
  issue?: string;
  status?: RunStatus;
}

/** Query of `GET /api/v1/runs/{id}/events`. */
export interface ListRunEventsQuery {
  after_seq?: number;
  limit?: number;
}
