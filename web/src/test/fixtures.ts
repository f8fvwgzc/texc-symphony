import type {
  IssueDetail,
  RunEvent,
  RunRecord,
  StateError,
  StateSnapshot,
  Totals,
} from '../api/types';

/** The `StateSnapshot` example of docs/api/openapi.yaml (asserted by the Elixir tests). */
export function makeSnapshot(overrides: Partial<StateSnapshot> = {}): StateSnapshot {
  return {
    generated_at: '2026-02-24T20:15:30Z',
    counts: { running: 1, retrying: 1, blocked: 1 },
    running: [
      {
        issue_id: 'issue-http',
        issue_identifier: 'MT-HTTP',
        issue_url: 'https://example.org/issues/MT-HTTP',
        state: 'In Progress',
        worker_host: null,
        workspace_path: null,
        session_id: 'thread-http',
        turn_count: 7,
        last_event: 'notification',
        last_message: 'rendered',
        started_at: '2026-02-24T20:10:12Z',
        last_event_at: null,
        tokens: { input_tokens: 4, output_tokens: 8, total_tokens: 12 },
      },
    ],
    retrying: [
      {
        issue_id: 'issue-retry',
        issue_identifier: 'MT-RETRY',
        issue_url: 'https://example.org/issues/MT-RETRY',
        attempt: 2,
        due_at: '2026-02-24T20:15:32Z',
        error: 'boom',
        worker_host: null,
        workspace_path: null,
      },
    ],
    blocked: [
      {
        issue_id: 'issue-blocked',
        issue_identifier: 'MT-BLOCKED',
        issue_url: 'https://example.org/issues/MT-BLOCKED',
        state: 'In Progress',
        error: 'codex turn requires operator input',
        worker_host: 'dm-dev2',
        workspace_path: '/workspaces/MT-BLOCKED',
        session_id: 'thread-blocked',
        blocked_at: '2026-02-24T20:14:00Z',
        last_event: 'turn_input_required',
        last_message: 'turn blocked: waiting for user input',
        last_event_at: '2026-02-24T20:14:00Z',
      },
    ],
    codex_totals: { input_tokens: 4, output_tokens: 8, total_tokens: 12, seconds_running: 42 },
    rate_limits: { primary: { remaining: 11 } },
    ...overrides,
  };
}

export function makeEmptySnapshot(): StateSnapshot {
  return makeSnapshot({
    counts: { running: 0, retrying: 0, blocked: 0 },
    running: [],
    retrying: [],
    blocked: [],
    rate_limits: null,
  });
}

export const SNAPSHOT_TIMEOUT: StateError = {
  generated_at: '2026-02-24T20:15:30Z',
  error: { code: 'snapshot_timeout', message: 'Snapshot timed out' },
};

export function makeIssueDetail(overrides: Partial<IssueDetail> = {}): IssueDetail {
  return {
    issue_identifier: 'MT-HTTP',
    issue_id: 'issue-http',
    status: 'running',
    workspace: { path: '/tmp/symphony_workspaces/MT-HTTP', host: null },
    attempts: { restart_count: 0, current_retry_attempt: 0 },
    running: {
      worker_host: null,
      workspace_path: null,
      session_id: 'thread-http',
      turn_count: 7,
      state: 'In Progress',
      started_at: '2026-02-24T20:10:12Z',
      last_event: 'notification',
      last_message: 'rendered',
      last_event_at: null,
      tokens: { input_tokens: 4, output_tokens: 8, total_tokens: 12 },
    },
    retry: null,
    blocked: null,
    logs: { codex_session_logs: [] },
    recent_events: [],
    last_error: null,
    tracked: {},
    ...overrides,
  };
}

export function makeRun(overrides: Partial<RunRecord> = {}): RunRecord {
  return {
    id: 42,
    issue_id: 'issue-http',
    issue_identifier: 'MT-HTTP',
    issue_title: 'Render the HTTP dashboard',
    attempt: 0,
    worker_host: null,
    workspace_path: '/tmp/symphony_workspaces/MT-HTTP',
    status: 'succeeded',
    error: null,
    turns: 7,
    started_at: '2026-02-24T20:10:12.004Z',
    finished_at: '2026-02-24T20:31:40.250Z',
    duration_ms: 1_288_246,
    tokens: { input: 18_230, output: 2_207, total: 20_437 },
    ...overrides,
  };
}

export function makeRunEvent(seq: number, overrides: Partial<RunEvent> = {}): RunEvent {
  return {
    run_id: 42,
    seq,
    at: `2026-02-24T20:10:${String(10 + seq).padStart(2, '0')}.000Z`,
    kind: seq === 1 ? 'session_started' : 'notification',
    message: `event ${seq}`,
    payload: null,
    ...overrides,
  };
}

export const TOTALS: Totals = {
  runs_total: 128,
  runs_succeeded: 97,
  runs_failed: 21,
  tokens: { input: 1_830_211, output: 220_407, total: 2_050_618 },
  runtime_ms: 48_210_933,
};
