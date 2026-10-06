import type { BlockedEntry } from '../../api/types';
import { stateTone } from '../../lib/format';
import { Badge } from '../../ui/Badge';
import { Panel } from '../../ui/Panel';
import { RelativeTime } from '../../ui/RelativeTime';
import { Cell, Table, type Column } from '../../ui/Table';
import { Empty, NotAvailable } from '../../ui/Text';
import { CopyButton } from '../issues/CopyButton';
import { IssueCell } from '../issues/IssueCell';
import { LastUpdate } from './LastUpdate';

const COLUMNS: Column[] = [
  { label: 'Issue' },
  { label: 'State' },
  { label: 'Session' },
  { label: 'Blocked' },
  { label: 'Last update' },
  { label: 'Error' },
];

export function BlockedTable({ entries, now }: { entries: BlockedEntry[]; now: number }) {
  return (
    <Panel
      title="Blocked sessions"
      description="Issues paused because the agent requested operator input or approval."
    >
      {entries.length === 0 ? (
        <Empty>No blocked sessions.</Empty>
      ) : (
        <Table caption="Blocked sessions" columns={COLUMNS}>
          {entries.map((entry) => {
            const state = entry.state ?? 'Blocked';
            return (
              <tr key={entry.issue_id}>
                <Cell>
                  <IssueCell identifier={entry.issue_identifier} url={entry.issue_url} />
                </Cell>
                <Cell>
                  <Badge tone={stateTone(state)}>{state}</Badge>
                </Cell>
                <Cell>
                  {entry.session_id === null ? (
                    NotAvailable
                  ) : (
                    <CopyButton value={entry.session_id} />
                  )}
                </Cell>
                <Cell>
                  <RelativeTime iso={entry.blocked_at} now={now} />
                </Cell>
                <Cell>
                  <LastUpdate
                    message={entry.last_message}
                    event={entry.last_event}
                    at={entry.last_event_at}
                    now={now}
                  />
                </Cell>
                <Cell class="text-destructive">{entry.error ?? NotAvailable}</Cell>
              </tr>
            );
          })}
        </Table>
      )}
    </Panel>
  );
}
