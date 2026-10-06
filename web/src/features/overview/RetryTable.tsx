import type { RetryEntry } from '../../api/types';
import { Panel } from '../../ui/Panel';
import { RelativeTime } from '../../ui/RelativeTime';
import { Cell, Table, type Column } from '../../ui/Table';
import { Empty, NotAvailable } from '../../ui/Text';
import { IssueCell } from '../issues/IssueCell';

const COLUMNS: Column[] = [
  { label: 'Issue' },
  { label: 'Attempt', numeric: true },
  { label: 'Due' },
  { label: 'Error' },
];

export function RetryTable({ entries, now }: { entries: RetryEntry[]; now: number }) {
  const sorted = entries.toSorted((a, b) => (a.due_at ?? '').localeCompare(b.due_at ?? ''));
  return (
    <Panel title="Retry queue" description="Issues backing off until their next retry window.">
      {sorted.length === 0 ? (
        <Empty>No issues are currently backing off.</Empty>
      ) : (
        <Table caption="Retry queue" columns={COLUMNS}>
          {sorted.map((entry) => (
            <tr key={entry.issue_id}>
              <Cell>
                <IssueCell identifier={entry.issue_identifier} url={entry.issue_url} />
              </Cell>
              <Cell numeric>{entry.attempt ?? 'n/a'}</Cell>
              <Cell>
                <RelativeTime iso={entry.due_at} now={now} />
              </Cell>
              <Cell class="text-destructive">{entry.error ?? NotAvailable}</Cell>
            </tr>
          ))}
        </Table>
      )}
    </Panel>
  );
}
