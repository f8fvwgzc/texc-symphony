import { useEffect, useState } from 'preact/hooks';

import { describeError, isStoreDisabled } from '../../api/client';
import { useApi } from '../../api/context';
import type { RunRecord } from '../../api/types';
import { useAsync } from '../../hooks/useAsync';
import { formatDurationMs, formatInt, formatUtc, runStatusTone } from '../../lib/format';
import { formatRoute } from '../../lib/router';
import { Badge } from '../../ui/Badge';
import { Button, buttonClass } from '../../ui/Button';
import { Field, Fields } from '../../ui/Fields';
import { Panel } from '../../ui/Panel';
import { Empty, ErrorText } from '../../ui/Text';
import { StoreDisabledNotice } from './StoreDisabledNotice';
import { Timeline } from './Timeline';
import { RUN_POLL_MS, useRunEvents } from './useRunEvents';

function RunSummary({ run }: { run: RunRecord }) {
  return (
    <Fields>
      <Field label="Issue">
        <span class="font-semibold">{run.issue_identifier}</span>
        {run.issue_title !== null && (
          <span class="text-muted-foreground"> — {run.issue_title}</span>
        )}
      </Field>
      <Field label="Status">
        <Badge tone={runStatusTone(run.status)}>{run.status}</Badge>
      </Field>
      <Field label="Attempt">{run.attempt}</Field>
      <Field label="Turns">{run.turns}</Field>
      <Field label="Started">
        <span class="font-mono text-xs">{formatUtc(run.started_at)}</span>
      </Field>
      <Field label="Finished">
        <span class="font-mono text-xs">
          {run.finished_at === null ? 'still running' : formatUtc(run.finished_at)}
        </span>
      </Field>
      <Field label="Duration">{formatDurationMs(run.duration_ms)}</Field>
      <Field label="Tokens">
        <span class="tabular-nums">
          {formatInt(run.tokens.total)} (in {formatInt(run.tokens.input)} / out{' '}
          {formatInt(run.tokens.output)})
        </span>
      </Field>
      <Field label="Worker host">{run.worker_host ?? 'local'}</Field>
      <Field label="Workspace">
        <span class="font-mono text-xs break-all">{run.workspace_path ?? 'n/a'}</span>
      </Field>
      {run.error !== null && (
        <Field label="Error">
          <span class="text-destructive" data-tone="danger">
            {run.error}
          </span>
        </Field>
      )}
    </Fields>
  );
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
    <a class={buttonClass()} href={formatRoute({ name: 'runs', query: {} })}>
      ← All runs
    </a>
  );

  if (isStoreDisabled(run.error)) {
    return (
      <Panel title={`Run #${id}`} actions={back}>
        <StoreDisabledNotice />
      </Panel>
    );
  }

  return (
    <>
      <Panel title={`Run #${id}`} actions={back}>
        {run.error !== undefined && run.data === undefined ? (
          <ErrorText>Could not load run: {describeError(run.error)}</ErrorText>
        ) : run.data === undefined ? (
          <p class="text-muted-foreground">Loading…</p>
        ) : (
          <RunSummary run={run.data} />
        )}
      </Panel>
      <Panel
        title="Events"
        description={live ? 'Live: new events appear every few seconds.' : 'Timeline of this run.'}
      >
        {events.error !== undefined && (
          <ErrorText>Could not load events: {describeError(events.error)}</ErrorText>
        )}
        {events.events.length === 0 && !events.loading && events.error === undefined ? (
          <Empty>No events recorded for this run.</Empty>
        ) : (
          <Timeline events={events.events} />
        )}
        {events.hasMore && (
          <div class="mt-4">
            <Button disabled={events.loading} onClick={() => void events.loadNext()}>
              Load more events
            </Button>
          </div>
        )}
      </Panel>
    </>
  );
}
