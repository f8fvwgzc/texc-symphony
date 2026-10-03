import { useState } from 'preact/hooks';

import { describeError, isStoreDisabled } from '../api/client';
import { useApi } from '../api/context';
import { isRunStatus, RUN_STATUSES } from '../api/types';
import { Section } from '../components/Section';
import { Badge } from '../components/StateBadge';
import { useAsync } from '../hooks/useAsync';
import { formatDurationMs, formatInt, formatUtc, runStatusTone } from '../lib/format';
import { formatRoute, navigate, type RunsQuery } from '../lib/router';

export const RUNS_PAGE_SIZE = 25;

export function StoreDisabledNotice() {
  return (
    <p class="empty">
      Run history is disabled on this server (persistence is off), so only live state is available.
    </p>
  );
}

function Filters({ query }: { query: RunsQuery }) {
  const [issue, setIssue] = useState(query.issue ?? '');
  const [status, setStatus] = useState<string>(query.status ?? '');

  const onSubmit = (event: Event) => {
    event.preventDefault();
    const next: RunsQuery = {};
    const trimmed = issue.trim();
    if (trimmed !== '') next.issue = trimmed;
    if (isRunStatus(status)) next.status = status;
    navigate({ name: 'runs', query: next });
  };

  return (
    <form class="filters" onSubmit={onSubmit} aria-label="Filter runs">
      <label>
        <span>Issue</span>
        <input
          type="search"
          value={issue}
          placeholder="e.g. MT-123"
          onInput={(event) => setIssue(event.currentTarget.value)}
        />
      </label>
      <label>
        <span>Status</span>
        <select value={status} onChange={(event) => setStatus(event.currentTarget.value)}>
          <option value="">All</option>
          {RUN_STATUSES.map((value) => (
            <option key={value} value={value}>
              {value}
            </option>
          ))}
        </select>
      </label>
      <button type="submit" class="button">
        Apply
      </button>
      {(query.issue !== undefined || query.status !== undefined) && (
        <a class="button button-ghost" href={formatRoute({ name: 'runs', query: {} })}>
          Clear
        </a>
      )}
    </form>
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
      <p class="danger" role="alert">
        Could not load runs: {describeError(page.error)}{' '}
        <button type="button" class="button button-ghost" onClick={page.reload}>
          Retry
        </button>
      </p>
    );
  } else if (page.data === undefined) {
    body = <p class="muted">Loading…</p>;
  } else if (page.data.runs.length === 0) {
    body = <p class="empty">No runs match these filters.</p>;
  } else {
    const nextBefore = page.data.next_before_id;
    body = (
      <>
        <div class="table-wrap" aria-busy={page.loading}>
          <table class="table table-runs">
            <caption class="visually-hidden">Runs</caption>
            <thead>
              <tr>
                <th scope="col">Run</th>
                <th scope="col">Issue</th>
                <th scope="col">Status</th>
                <th scope="col" class="num">
                  Attempt
                </th>
                <th scope="col" class="num">
                  Turns
                </th>
                <th scope="col">Started</th>
                <th scope="col" class="num">
                  Duration
                </th>
                <th scope="col" class="num">
                  Tokens
                </th>
              </tr>
            </thead>
            <tbody>
              {page.data.runs.map((run) => (
                <tr key={run.id}>
                  <td>
                    <a href={formatRoute({ name: 'run', id: run.id })}>#{run.id}</a>
                  </td>
                  <td>
                    <div class="stack">
                      <span class="issue-id">{run.issue_identifier}</span>
                      {run.issue_title !== null && (
                        <span class="muted small event-text" title={run.issue_title}>
                          {run.issue_title}
                        </span>
                      )}
                    </div>
                  </td>
                  <td>
                    <Badge tone={runStatusTone(run.status)}>{run.status}</Badge>
                    {run.error !== null && (
                      <div class="muted small event-text" title={run.error}>
                        {run.error}
                      </div>
                    )}
                  </td>
                  <td class="num numeric">{run.attempt}</td>
                  <td class="num numeric">{run.turns}</td>
                  <td class="mono small">{formatUtc(run.started_at)}</td>
                  <td class="num numeric">
                    {run.status === 'running' ? 'running' : formatDurationMs(run.duration_ms)}
                  </td>
                  <td class="num numeric">{formatInt(run.tokens.total)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <nav class="pager" aria-label="Run history pages">
          <button
            type="button"
            class="button button-ghost"
            disabled={query.before === undefined}
            onClick={goNewer}
          >
            ← Newer
          </button>
          <button
            type="button"
            class="button button-ghost"
            disabled={nextBefore === null}
            onClick={() => nextBefore !== null && goOlder(nextBefore)}
          >
            Older →
          </button>
        </nav>
      </>
    );
  }

  return (
    <Section
      title="Run history"
      description="Every agent run recorded by this server, newest first."
      actions={<Filters query={query} />}
    >
      {body}
    </Section>
  );
}
