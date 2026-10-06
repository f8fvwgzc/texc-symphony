import type { RunningEntry } from '../../api/types';
import { formatInt, formatRuntimeAndTurns, stateTone } from '../../lib/format';
import { Badge } from '../../ui/Badge';
import { Panel } from '../../ui/Panel';
import { Cell, Table, type Column } from '../../ui/Table';
import { Empty, NotAvailable } from '../../ui/Text';
import { CopyButton } from '../issues/CopyButton';
import { IssueCell } from '../issues/IssueCell';
import { LastUpdate } from './LastUpdate';

const COLUMNS: Column[] = [
  { label: 'Issue' },
  { label: 'State' },
  { label: 'Session' },
  { label: 'Runtime / turns' },
  { label: 'Last event' },
  { label: 'Tokens', numeric: true },
];

export function RunningTable({ entries, now }: { entries: RunningEntry[]; now: number }) {
  return (
    <Panel
      title="Running sessions"
      description="Active issues, last known agent activity, and token usage."
    >
      {entries.length === 0 ? (
        <Empty>No active sessions.</Empty>
      ) : (
        <Table caption="Running sessions" columns={COLUMNS}>
          {entries.map((entry) => (
            <tr key={entry.issue_id}>
              <Cell>
                <IssueCell identifier={entry.issue_identifier} url={entry.issue_url} />
              </Cell>
              <Cell>
                <Badge tone={stateTone(entry.state)}>{entry.state ?? 'unknown'}</Badge>
              </Cell>
              <Cell>
                {entry.session_id === null ? NotAvailable : <CopyButton value={entry.session_id} />}
                {entry.worker_host !== null && (
                  <div class="text-muted-foreground mt-1 font-mono text-xs" title="Worker host">
                    {entry.worker_host}
                  </div>
                )}
              </Cell>
              <Cell class="tabular-nums">
                {formatRuntimeAndTurns(entry.started_at, entry.turn_count, now)}
              </Cell>
              <Cell>
                <LastUpdate
                  message={entry.last_message}
                  event={entry.last_event}
                  at={entry.last_event_at}
                  now={now}
                />
              </Cell>
              <Cell numeric>
                <div>{formatInt(entry.tokens.total_tokens)}</div>
                <div class="text-muted-foreground text-xs">
                  In {formatInt(entry.tokens.input_tokens)} / Out{' '}
                  {formatInt(entry.tokens.output_tokens)}
                </div>
              </Cell>
            </tr>
          ))}
        </Table>
      )}
    </Panel>
  );
}
