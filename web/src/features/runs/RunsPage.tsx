import { useState } from 'preact/hooks';

import { describeError, isStoreDisabled } from '../../api/client';
import { useApi } from '../../api/context';
import type { RunRecord } from '../../api/types';
import { useAsync } from '../../hooks/useAsync';
import { formatDurationMs, formatInt, formatUtc, runStatusTone } from '../../lib/format';
import { formatRoute, navigate, type RunsQuery } from '../../lib/router';
import { Badge } from '../../ui/Badge';
import { Button } from '../../ui/Button';
import { Icon } from '../../ui/Icon';
import { Panel } from '../../ui/Panel';
import { Cell, Table, type Column } from '../../ui/Table';
import { Empty, ErrorText } from '../../ui/Text';
import { RunFilters } from './RunFilters';
import { StoreDisabledNotice } from './StoreDisabledNotice';

export const RUNS_PAGE_SIZE = 25;

const COLUMNS: Column[] = [
  { label: 'Run' },
  { label: 'Issue' },
  { label: 'Status' },
  { label: 'Attempt', numeric: true },
  { label: 'Turns', numeric: true },
  { label: 'Started' },
  { label: 'Duration', numeric: true },
  { label: 'Tokens', numeric: true },
];

function RunRow({ run }: { run: RunRecord }) {
  return (
    <tr>
      <Cell>
        <a href={formatRoute({ name: 'run', id: run.id })}>#{run.id}</a>
      </Cell>
      <Cell>
        <div class="font-semibold">{run.issue_identifier}</div>
        {run.issue_title !== null && (
          <div class="text-muted-foreground line-clamp-1 max-w-sm text-xs" title={run.issue_title}>
            {run.issue_title}
          </div>
        )}
      </Cell>
      <Cell>
        <Badge tone={runStatusTone(run.status)}>{run.status}</Badge>
        {run.error !== null && (
          <div class="text-muted-foreground mt-1 line-clamp-2 max-w-xs text-xs" title={run.error}>
            {run.error}
          </div>
        )}
      </Cell>
      <Cell numeric>{run.attempt}</Cell>
      <Cell numeric>{run.turns}</Cell>
      <Cell class="font-mono text-xs whitespace-nowrap">{formatUtc(run.started_at)}</Cell>
      <Cell numeric>
        {run.status === 'running' ? 'running' : formatDurationMs(run.duration_ms)}
      </Cell>
      <Cell numeric>{formatInt(run.tokens.total)}</Cell>
    </tr>
  );
}

/** Remount key: the page (and its cursor stack) resets when the filters change. */
export function runsFilterKey(query: RunsQuery): string {
  return `${query.issue ?? ''}|${query.status ?? ''}`;
}

/** Run history (`GET /api/v1/runs`) with filters and keyset pagination. */
export function RunsPage({ query }: { query: RunsQuery }) {
  const api = useApi();
  // Cursors of the newer pages we came from, so "Newer" can step back.
  const [cursorStack, setCursorStack] = useState<(number | undefined)[]>([]);

  const page = useAsync(
    (signal) => {
      const params: Parameters<typeof api.listRuns>[0] = { limit: RUNS_PAGE_SIZE };
      if (query.before !== undefined) params.before_id = query.before;
      if (query.issue !== undefined) params.issue = query.issue;
      if (query.status !== undefined) params.status = query.status;
      return api.listRuns(params, { signal });
    },
    [api, query.before, query.issue, query.status],
  );

  const withBefore = (before: number | undefined): RunsQuery => {
    const next: RunsQuery = { ...query };
    if (before === undefined) delete next.before;
    else next.before = before;
    return next;
  };

  const goOlder = (nextBefore: number) => {
    setCursorStack((stack) => [...stack, query.before]);
    navigate({ name: 'runs', query: withBefore(nextBefore) });
  };
  const goNewer = () => {
    const previous = cursorStack[cursorStack.length - 1];
    setCursorStack((stack) => stack.slice(0, -1));
    navigate({ name: 'runs', query: withBefore(cursorStack.length > 0 ? previous : undefined) });
  };

  let body;
  if (isStoreDisabled(page.error)) {
    body = <StoreDisabledNotice />;
  } else if (page.error !== undefined) {
    body = (
      <div class="flex flex-wrap items-center gap-3">
        <ErrorText>Could not load runs: {describeError(page.error)}</ErrorText>
        <Button onClick={page.reload}>Retry</Button>
      </div>
    );
  } else if (page.data === undefined) {
    body = <p class="text-muted-foreground">Loading…</p>;
  } else if (page.data.runs.length === 0) {
    body = <Empty>No runs match these filters.</Empty>;
  } else {
    const nextBefore = page.data.next_before_id;
    body = (
      <>
        <Table caption="Runs" columns={COLUMNS} busy={page.loading}>
          {page.data.runs.map((run) => (
            <RunRow key={run.id} run={run} />
          ))}
        </Table>
        <nav class="mt-4 flex items-center gap-2" aria-label="Run history pages">
          <p class="text-muted-foreground mr-auto">
            {page.data.runs.length} runs on this page, newest first
          </p>
          <Button disabled={query.before === undefined} onClick={goNewer}>
            <Icon name="chevronLeft" />
            Newer
          </Button>
          <Button
            disabled={nextBefore === null}
            onClick={() => nextBefore !== null && goOlder(nextBefore)}
          >
            Older
            <Icon name="chevronRight" />
          </Button>
        </nav>
      </>
    );
  }

  return (
    <Panel
      title="Run history"
      description="Every agent run recorded by this server, newest first."
      actions={<RunFilters query={query} />}
    >
      {body}
    </Panel>
  );
}
