import type { RunningEntry } from '../api/types';
import { formatInt, formatRuntimeAndTurns, stateTone } from '../lib/format';
import { CopyButton } from './CopyButton';
import { IssueCell } from './IssueCell';
import { LastUpdate } from './LastUpdate';
import { Section } from './Section';
import { Badge } from './StateBadge';

export function RunningTable({ entries, now }: { entries: RunningEntry[]; now: number }) {
  return (
    <Section
      title="Running sessions"
      description="Active issues, last known agent activity, and token usage."
    >
      {entries.length === 0 ? (
        <p class="empty">No active sessions.</p>
      ) : (
        <div class="table-wrap">
          <table class="table table-running">
            <caption class="visually-hidden">Running sessions</caption>
            <thead>
              <tr>
                <th scope="col">Issue</th>
                <th scope="col">State</th>
                <th scope="col">Session</th>
                <th scope="col">Runtime / turns</th>
                <th scope="col">Last event</th>
                <th scope="col" class="num">
                  Tokens
                </th>
              </tr>
            </thead>
            <tbody>
              {entries.map((entry) => (
                <tr key={entry.issue_id}>
                  <td>
                    <IssueCell identifier={entry.issue_identifier} url={entry.issue_url} />
                  </td>
                  <td>
                    <Badge tone={stateTone(entry.state)}>{entry.state ?? 'unknown'}</Badge>
                  </td>
                  <td>
                    {entry.session_id === null ? (
                      <span class="muted">n/a</span>
                    ) : (
                      <CopyButton value={entry.session_id} />
                    )}
                    {entry.worker_host !== null && (
                      <div class="muted small mono" title="Worker host">
                        {entry.worker_host}
                      </div>
                    )}
                  </td>
                  <td class="numeric">
                    {formatRuntimeAndTurns(entry.started_at, entry.turn_count, now)}
                  </td>
                  <td>
                    <LastUpdate
                      message={entry.last_message}
                      event={entry.last_event}
                      at={entry.last_event_at}
                      now={now}
                    />
                  </td>
                  <td class="num numeric">
                    <div class="stack">
                      <span>{formatInt(entry.tokens.total_tokens)}</span>
                      <span class="muted small">
                        In {formatInt(entry.tokens.input_tokens)} / Out{' '}
                        {formatInt(entry.tokens.output_tokens)}
                      </span>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Section>
  );
}
