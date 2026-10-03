import type { RetryEntry } from '../api/types';
import { IssueCell } from './IssueCell';
import { Section } from './Section';
import { RelativeTime } from './Time';

export function RetryTable({ entries, now }: { entries: RetryEntry[]; now: number }) {
  const sorted = entries.toSorted((a, b) => (a.due_at ?? '').localeCompare(b.due_at ?? ''));
  return (
    <Section title="Retry queue" description="Issues backing off until their next retry window.">
      {sorted.length === 0 ? (
        <p class="empty">No issues are currently backing off.</p>
      ) : (
        <div class="table-wrap">
          <table class="table table-retry">
            <caption class="visually-hidden">Retry queue</caption>
            <thead>
              <tr>
                <th scope="col">Issue</th>
                <th scope="col" class="num">
                  Attempt
                </th>
                <th scope="col">Due</th>
                <th scope="col">Error</th>
              </tr>
            </thead>
            <tbody>
              {sorted.map((entry) => (
                <tr key={entry.issue_id}>
                  <td>
                    <IssueCell identifier={entry.issue_identifier} url={entry.issue_url} />
                  </td>
                  <td class="num numeric">{entry.attempt ?? 'n/a'}</td>
                  <td>
                    <RelativeTime iso={entry.due_at} now={now} />
                  </td>
                  <td class="error-text">{entry.error ?? <span class="muted">n/a</span>}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Section>
  );
}
