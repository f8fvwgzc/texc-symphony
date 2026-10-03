import type { BlockedEntry } from '../api/types';
import { stateTone } from '../lib/format';
import { CopyButton } from './CopyButton';
import { IssueCell } from './IssueCell';
import { LastUpdate } from './LastUpdate';
import { Section } from './Section';
import { Badge } from './StateBadge';
import { RelativeTime } from './Time';

export function BlockedTable({ entries, now }: { entries: BlockedEntry[]; now: number }) {
  return (
    <Section
      title="Blocked sessions"
      description="Issues paused because Codex requested operator input or approval."
    >
      {entries.length === 0 ? (
        <p class="empty">No blocked sessions.</p>
      ) : (
        <div class="table-wrap">
          <table class="table table-blocked">
            <caption class="visually-hidden">Blocked sessions</caption>
            <thead>
              <tr>
                <th scope="col">Issue</th>
                <th scope="col">State</th>
                <th scope="col">Session</th>
                <th scope="col">Blocked</th>
                <th scope="col">Last update</th>
                <th scope="col">Error</th>
              </tr>
            </thead>
            <tbody>
              {entries.map((entry) => {
                const state = entry.state ?? 'Blocked';
                return (
                  <tr key={entry.issue_id}>
                    <td>
                      <IssueCell identifier={entry.issue_identifier} url={entry.issue_url} />
                    </td>
                    <td>
                      <Badge tone={stateTone(state)}>{state}</Badge>
                    </td>
                    <td>
                      {entry.session_id === null ? (
                        <span class="muted">n/a</span>
                      ) : (
                        <CopyButton value={entry.session_id} />
                      )}
                    </td>
                    <td>
                      <RelativeTime iso={entry.blocked_at} now={now} />
                    </td>
                    <td>
                      <LastUpdate
                        message={entry.last_message}
                        event={entry.last_event}
                        at={entry.last_event_at}
                        now={now}
                      />
                    </td>
                    <td class="error-text">{entry.error ?? <span class="muted">n/a</span>}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </Section>
  );
}
