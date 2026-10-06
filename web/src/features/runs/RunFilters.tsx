import { useState } from 'preact/hooks';

import { isRunStatus, RUN_STATUSES } from '../../api/types';
import { formatRoute, navigate, type RunsQuery } from '../../lib/router';
import { Button, buttonClass } from '../../ui/Button';

const INPUT_CLASS = 'bg-background h-8 rounded-md border px-2.5 text-sm shadow-xs';

/** Issue and status filters of the run history; applying them navigates to the filtered route. */
export function RunFilters({ query }: { query: RunsQuery }) {
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
    <form class="flex flex-wrap items-end gap-3" onSubmit={onSubmit} aria-label="Filter runs">
      <label class="flex flex-col gap-1">
        <span class="text-muted-foreground text-xs">Issue</span>
        <input
          type="search"
          class={INPUT_CLASS}
          value={issue}
          placeholder="e.g. MT-123"
          onInput={(event) => setIssue(event.currentTarget.value)}
        />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-muted-foreground text-xs">Status</span>
        <select
          class={INPUT_CLASS}
          value={status}
          onChange={(event) => setStatus(event.currentTarget.value)}
        >
          <option value="">All</option>
          {RUN_STATUSES.map((value) => (
            <option key={value} value={value}>
              {value}
            </option>
          ))}
        </select>
      </label>
      <Button type="submit" variant="solid">
        Apply
      </Button>
      {(query.issue !== undefined || query.status !== undefined) && (
        <a class={buttonClass()} href={formatRoute({ name: 'runs', query: {} })}>
          Clear
        </a>
      )}
    </form>
  );
}
