import type { ComponentChildren } from 'preact';
import { useEffect, useRef } from 'preact/hooks';

import { ApiError, describeError } from '../api/client';
import { useApi } from '../api/context';
import type { IssueDetail, RunList } from '../api/types';
import { useAsync } from '../hooks/useAsync';
import { useThrottled } from '../hooks/useThrottled';
import {
  formatDurationMs,
  formatInt,
  formatRuntimeSeconds,
  runStatusTone,
  runtimeSecondsFromStartedAt,
  stateTone,
} from '../lib/format';
import { formatRoute } from '../lib/router';
import { CopyButton } from './CopyButton';
import { IssueId } from './IssueId';
import { Badge } from './StateBadge';
import { RelativeTime } from './Time';

function Field({ label, children }: { label: string; children: ComponentChildren }) {
  return (
    <>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </>
  );
}

const na = <span class="muted">n/a</span>;

function IssueBody({ detail, now }: { detail: IssueDetail; now: number }) {
  const { running, retry, blocked } = detail;
  return (
    <>
      <dl class="fields">
        <Field label="Workspace">
          <span class="mono break">{detail.workspace.path}</span>
        </Field>
        <Field label="Worker host">{detail.workspace.host ?? 'local'}</Field>
        <Field label="Retry attempt">{detail.attempts.current_retry_attempt}</Field>
        <Field label="Restarts">{detail.attempts.restart_count}</Field>
        {detail.last_error !== null && (
          <Field label="Last error">
            <span class="danger">{detail.last_error}</span>
          </Field>
        )}
      </dl>

      {running !== null && (
        <section class="drawer-section" aria-label="Running session">
          <h3>Running session</h3>
          <dl class="fields">
            <Field label="State">
              <Badge tone={stateTone(running.state)}>{running.state ?? 'unknown'}</Badge>
            </Field>
            <Field label="Session">
              {running.session_id === null ? na : <CopyButton value={running.session_id} />}
            </Field>
            <Field label="Started">
              <RelativeTime iso={running.started_at} now={now} />
            </Field>
            <Field label="Runtime">
              {formatRuntimeSeconds(runtimeSecondsFromStartedAt(running.started_at, now))}
            </Field>
            <Field label="Turns">{running.turn_count}</Field>
            <Field label="Tokens">
              {formatInt(running.tokens.total_tokens)} (in {formatInt(running.tokens.input_tokens)}
              {' / '}out {formatInt(running.tokens.output_tokens)})
            </Field>
            <Field label="Last event">
              <span class="mono">{running.last_event ?? 'n/a'}</span>
              {running.last_message !== null && <div>{running.last_message}</div>}
            </Field>
          </dl>
        </section>
      )}

      {retry !== null && (
        <section class="drawer-section" aria-label="Retry">
          <h3>Retry</h3>
          <dl class="fields">
            <Field label="Attempt">{retry.attempt ?? 'n/a'}</Field>
            <Field label="Due">
              <RelativeTime iso={retry.due_at} now={now} />
            </Field>
            <Field label="Error">{retry.error ?? na}</Field>
          </dl>
        </section>
      )}

      {blocked !== null && (
        <section class="drawer-section" aria-label="Blocked">
          <h3>Blocked</h3>
          <dl class="fields">
            <Field label="State">
              <Badge tone="danger">{blocked.state ?? 'Blocked'}</Badge>
            </Field>
            <Field label="Since">
              <RelativeTime iso={blocked.blocked_at} now={now} />
            </Field>
            <Field label="Session">
              {blocked.session_id === null ? na : <CopyButton value={blocked.session_id} />}
            </Field>
            <Field label="Error">{blocked.error ?? na}</Field>
            <Field label="Last event">
              <span class="mono">{blocked.last_event ?? 'n/a'}</span>
              {blocked.last_message !== null && <div>{blocked.last_message}</div>}
            </Field>
          </dl>
        </section>
      )}

      {detail.recent_events.length > 0 && (
        <section class="drawer-section" aria-label="Recent events">
          <h3>Recent events</h3>
          <ol class="timeline">
            {detail.recent_events.map((event) => (
              <li key={`${event.at}-${event.event ?? ''}`}>
                <RelativeTime iso={event.at} now={now} />{' '}
                <span class="mono">{event.event ?? 'n/a'}</span>
                {event.message !== null && <div>{event.message}</div>}
              </li>
            ))}
          </ol>
        </section>
      )}
    </>
  );
}

function RecentRuns({ runs, identifier }: { runs: RunList; identifier: string }) {
  return (
    <section class="drawer-section" aria-label="Recent runs">
      <h3>Recent runs</h3>
      {runs.runs.length === 0 ? (
        <p class="empty">No recorded runs yet.</p>
      ) : (
        <ul class="run-list">
          {runs.runs.map((run) => (
            <li key={run.id}>
              <a href={formatRoute({ name: 'run', id: run.id })}>Run #{run.id}</a>{' '}
              <Badge tone={runStatusTone(run.status)}>{run.status}</Badge>{' '}
              <span class="muted small">
                attempt {run.attempt} · {formatDurationMs(run.duration_ms)} ·{' '}
                {formatInt(run.tokens.total)} tokens
              </span>
            </li>
          ))}
        </ul>
      )}
      <a class="small" href={formatRoute({ name: 'runs', query: { issue: identifier } })}>
        All runs for {identifier}
      </a>
    </section>
  );
}

/**
 * Side panel with `GET /api/v1/{identifier}`. Re-fetched (throttled to every 2 s) whenever
 * the live state changes. Escape, the close button or a click on the backdrop close it.
 */
export function IssueDrawer(props: {
  identifier: string;
  issueUrl: string | null;
  revision: number;
  storeEnabled: boolean;
  now: number;
  onClose: () => void;
}) {
  const api = useApi();
  const dialogRef = useRef<HTMLDialogElement>(null);
  const revision = useThrottled(props.revision, 2_000);

  const detail = useAsync(
    (signal) => api.getIssue(props.identifier, { signal }),
    [api, props.identifier, revision],
  );
  const runs = useAsync(
    (signal) =>
      props.storeEnabled
        ? api.listRuns({ issue: props.identifier, limit: 5 }, { signal })
        : Promise.resolve(null),
    [api, props.identifier, props.storeEnabled],
  );

  const { onClose } = props;
  useEffect(() => {
    const dialog = dialogRef.current;
    if (dialog === null) return undefined;
    // Native modal dialog: focus trap, Escape (`cancel`) and focus restore come for free.
    if (!dialog.open) {
      if (typeof dialog.showModal === 'function') dialog.showModal();
      else dialog.setAttribute('open', '');
    }
    const onCancel = (event: Event) => {
      event.preventDefault();
      onClose();
    };
    // Pointer convenience: a click whose target is the <dialog> itself hit the backdrop.
    // (Keyboard users have Escape and the Close button.)
    const onBackdropClick = (event: MouseEvent) => {
      if (event.target === dialog) onClose();
    };
    dialog.addEventListener('cancel', onCancel);
    dialog.addEventListener('click', onBackdropClick);
    return () => {
      dialog.removeEventListener('cancel', onCancel);
      dialog.removeEventListener('click', onBackdropClick);
      if (dialog.open && typeof dialog.close === 'function') dialog.close();
    };
  }, [onClose]);

  const notFound = detail.error instanceof ApiError && detail.error.code === 'issue_not_found';

  return (
    <dialog class="drawer" aria-labelledby="drawer-title" ref={dialogRef}>
      <div class="drawer-panel">
        <header class="drawer-header">
          <div>
            <p class="eyebrow">Issue</p>
            <h2 id="drawer-title" class="drawer-title">
              <IssueId identifier={props.identifier} url={props.issueUrl} />
            </h2>
            {detail.data !== undefined && !notFound && (
              <Badge
                tone={
                  detail.data.status === 'running'
                    ? 'active'
                    : detail.data.status === 'blocked'
                      ? 'danger'
                      : 'warning'
                }
              >
                {detail.data.status}
              </Badge>
            )}
          </div>
          <button
            type="button"
            class="button button-ghost"
            onClick={onClose}
            aria-label="Close issue details"
          >
            Close
          </button>
        </header>

        <div class="drawer-body">
          {notFound ? (
            <p class="empty">{props.identifier} is not running, retrying or blocked right now.</p>
          ) : detail.error !== undefined ? (
            <p class="danger" role="alert">
              Could not load issue: {describeError(detail.error)}
            </p>
          ) : detail.data === undefined ? (
            <p class="muted">Loading…</p>
          ) : (
            <IssueBody detail={detail.data} now={props.now} />
          )}

          {props.storeEnabled && runs.data != null && (
            <RecentRuns runs={runs.data} identifier={props.identifier} />
          )}

          <p class="small">
            <a href={`/api/v1/${encodeURIComponent(props.identifier)}`}>Raw JSON</a>
          </p>
        </div>
      </div>
    </dialog>
  );
}
