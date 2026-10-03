import { useCallback, useEffect, useRef, useState } from 'preact/hooks';

import { describeError, isStoreDisabled } from '../api/client';
import { useApi } from '../api/context';
import type { RunEvent, RunRecord } from '../api/types';
import { Section } from '../components/Section';
import { Badge } from '../components/StateBadge';
import { useAsync } from '../hooks/useAsync';
import { formatDurationMs, formatInt, formatUtc, prettyJson, runStatusTone } from '../lib/format';
import { formatRoute } from '../lib/router';
import { StoreDisabledNotice } from './RunsPage';

export const EVENTS_PAGE_SIZE = 200;
/** While a run is still `running`, new events are fetched this often. */
export const RUN_POLL_MS = 5_000;

function eventTime(iso: string): string {
  const at = Date.parse(iso);
  return Number.isNaN(at) ? iso : new Date(at).toISOString().slice(11, 23);
}

function RunSummary({ run }: { run: RunRecord }) {
  return (
    <dl class="fields fields-wide">
      <dt>Issue</dt>
      <dd>
        <span class="issue-id">{run.issue_identifier}</span>
        {run.issue_title !== null && <span class="muted"> — {run.issue_title}</span>}
      </dd>
      <dt>Status</dt>
      <dd>
        <Badge tone={runStatusTone(run.status)}>{run.status}</Badge>
      </dd>
      <dt>Attempt</dt>
      <dd>{run.attempt}</dd>
      <dt>Turns</dt>
      <dd>{run.turns}</dd>
      <dt>Started</dt>
      <dd class="mono">{formatUtc(run.started_at)}</dd>
      <dt>Finished</dt>
      <dd class="mono">
        {run.finished_at === null ? 'still running' : formatUtc(run.finished_at)}
      </dd>
      <dt>Duration</dt>
      <dd>{formatDurationMs(run.duration_ms)}</dd>
      <dt>Tokens</dt>
      <dd class="numeric">
        {formatInt(run.tokens.total)} (in {formatInt(run.tokens.input)} / out{' '}
        {formatInt(run.tokens.output)})
      </dd>
      <dt>Worker host</dt>
      <dd>{run.worker_host ?? 'local'}</dd>
      <dt>Workspace</dt>
      <dd class="mono break">{run.workspace_path ?? 'n/a'}</dd>
      {run.error !== null && (
        <>
          <dt>Error</dt>
          <dd class="danger">{run.error}</dd>
        </>
      )}
    </dl>
  );
}

export function Timeline({ events }: { events: RunEvent[] }) {
  return (
    <ol class="timeline" aria-label="Run events">
      {events.map((event) => (
        <li key={event.seq} class="timeline-item">
          <div class="timeline-head">
            <time class="mono small" dateTime={event.at} title={formatUtc(event.at)}>
              {eventTime(event.at)}
            </time>
            <span class="badge badge-neutral mono">{event.kind}</span>
            <span class="muted small">#{event.seq}</span>
          </div>
          {event.message !== null && <p class="timeline-message">{event.message}</p>}
          {event.payload !== null && (
            <details class="raw">
              <summary>Payload</summary>
              <pre class="code-panel">{prettyJson(event.payload)}</pre>
            </details>
          )}
        </li>
      ))}
    </ol>
  );
}

/** Incrementally loaded event list for one run (`after_seq` paging + live polling). */
function useRunEvents(id: number, live: boolean) {
  const api = useApi();
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [error, setError] = useState<unknown>(undefined);
  const [loading, setLoading] = useState(true);
  const lastSeq = useRef(0);
  const inFlight = useRef(false);

  const loadNext = useCallback(async () => {
    if (inFlight.current) return;
    inFlight.current = true;
    setLoading(true);
    try {
      const page = await api.listRunEvents(id, {
        after_seq: lastSeq.current,
        limit: EVENTS_PAGE_SIZE,
      });
      const fresh = page.events.filter((event) => event.seq > lastSeq.current);
      const last = fresh[fresh.length - 1];
      if (last !== undefined) lastSeq.current = last.seq;
      if (fresh.length > 0) setEvents((current) => [...current, ...fresh]);
      setHasMore(page.events.length >= EVENTS_PAGE_SIZE);
      setError(undefined);
    } catch (reason) {
      setError(reason);
    } finally {
      inFlight.current = false;
      setLoading(false);
    }
  }, [api, id]);

  useEffect(() => {
    lastSeq.current = 0;
    setEvents([]);
    void loadNext();
  }, [loadNext]);

  useEffect(() => {
    if (!live) return undefined;
    const timer = setInterval(() => void loadNext(), RUN_POLL_MS);
    return () => clearInterval(timer);
  }, [live, loadNext]);

  return { events, hasMore, error, loading, loadNext };
}

/** One run: summary + events timeline. */
export function RunDetailPage({ id }: { id: number }) {
  const api = useApi();
  const [pollTick, setPollTick] = useState(0);
  const run = useAsync((signal) => api.getRun(id, { signal }), [api, id, pollTick]);
  const live = run.data?.status === 'running';
  const events = useRunEvents(id, live);

  useEffect(() => {
    if (!live) return undefined;
    const timer = setInterval(() => setPollTick((tick) => tick + 1), RUN_POLL_MS);
    return () => clearInterval(timer);
  }, [live]);

  const back = (
    <a class="button button-ghost" href={formatRoute({ name: 'runs', query: {} })}>
      ← All runs
    </a>
  );

  if (isStoreDisabled(run.error)) {
    return (
      <Section title={`Run #${id}`} actions={back}>
        <StoreDisabledNotice />
      </Section>
    );
  }

  return (
    <>
      <Section title={`Run #${id}`} actions={back}>
        {run.error !== undefined && run.data === undefined ? (
          <p class="danger" role="alert">
            Could not load run: {describeError(run.error)}
          </p>
        ) : run.data === undefined ? (
          <p class="muted">Loading…</p>
        ) : (
          <RunSummary run={run.data} />
        )}
      </Section>
      <Section
        title="Events"
        description={live ? 'Live: new events appear every few seconds.' : 'Timeline of this run.'}
      >
        {events.error !== undefined && (
          <p class="danger" role="alert">
            Could not load events: {describeError(events.error)}
          </p>
        )}
        {events.events.length === 0 && !events.loading && events.error === undefined ? (
          <p class="empty">No events recorded for this run.</p>
        ) : (
          <Timeline events={events.events} />
        )}
        {events.hasMore && (
          <button
            type="button"
            class="button button-ghost"
            disabled={events.loading}
            onClick={() => void events.loadNext()}
          >
            Load more events
          </button>
        )}
      </Section>
    </>
  );
}
