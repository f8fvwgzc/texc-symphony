import type { RunList } from '../../api/types';
import { formatDurationMs, formatInt, runStatusTone } from '../../lib/format';
import { formatRoute } from '../../lib/router';
import { Badge } from '../../ui/Badge';
import { Empty } from '../../ui/Text';
import { DrawerSection } from './IssueBody';

/** The last few recorded runs of one issue, with a link to its full history. */
export function RecentRuns({ runs, identifier }: { runs: RunList; identifier: string }) {
  return (
    <DrawerSection title="Recent runs">
      {runs.runs.length === 0 ? (
        <Empty>No recorded runs yet.</Empty>
      ) : (
        <ul class="space-y-2">
          {runs.runs.map((run) => (
            <li key={run.id}>
              <a href={formatRoute({ name: 'run', id: run.id })}>Run #{run.id}</a>{' '}
              <Badge tone={runStatusTone(run.status)}>{run.status}</Badge>{' '}
              <span class="text-muted-foreground text-xs">
                attempt {run.attempt} · {formatDurationMs(run.duration_ms)} ·{' '}
                {formatInt(run.tokens.total)} tokens
              </span>
            </li>
          ))}
        </ul>
      )}
      <a class="text-xs" href={formatRoute({ name: 'runs', query: { issue: identifier } })}>
        All runs for {identifier}
      </a>
    </DrawerSection>
  );
}
