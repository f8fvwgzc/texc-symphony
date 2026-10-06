import type { ComponentChildren } from 'preact';

import type { IssueDetail } from '../../api/types';
import {
  formatInt,
  formatRuntimeSeconds,
  runtimeSecondsFromStartedAt,
  stateTone,
} from '../../lib/format';
import { Badge } from '../../ui/Badge';
import { Field, Fields } from '../../ui/Fields';
import { RelativeTime } from '../../ui/RelativeTime';
import { NotAvailable } from '../../ui/Text';
import { CopyButton } from './CopyButton';

/** A titled block inside the drawer. */
export function DrawerSection({ title, children }: { title: string; children: ComponentChildren }) {
  return (
    <section class="space-y-3 border-t pt-4" aria-label={title}>
      <h3 class="text-sm font-semibold">{title}</h3>
      {children}
    </section>
  );
}

function LastEvent({ event, message }: { event: string | null; message: string | null }) {
  return (
    <>
      <span class="font-mono text-xs">{event ?? 'n/a'}</span>
      {message !== null && <div>{message}</div>}
    </>
  );
}

/** Everything `GET /api/v1/{identifier}` says about one issue. */
export function IssueBody({ detail, now }: { detail: IssueDetail; now: number }) {
  const { running, retry, blocked } = detail;
  return (
    <>
      <Fields>
        <Field label="Workspace">
          <span class="font-mono text-xs break-all">{detail.workspace.path}</span>
        </Field>
        <Field label="Worker host">{detail.workspace.host ?? 'local'}</Field>
        <Field label="Retry attempt">{detail.attempts.current_retry_attempt}</Field>
        <Field label="Restarts">{detail.attempts.restart_count}</Field>
        {detail.last_error !== null && (
          <Field label="Last error">
            <span class="text-destructive">{detail.last_error}</span>
          </Field>
        )}
      </Fields>

      {running !== null && (
        <DrawerSection title="Running session">
          <Fields>
            <Field label="State">
              <Badge tone={stateTone(running.state)}>{running.state ?? 'unknown'}</Badge>
            </Field>
            <Field label="Session">
              {running.session_id === null ? (
                NotAvailable
              ) : (
                <CopyButton value={running.session_id} />
              )}
            </Field>
            <Field label="Started">
              <RelativeTime iso={running.started_at} now={now} />
            </Field>
            <Field label="Runtime">
              {formatRuntimeSeconds(runtimeSecondsFromStartedAt(running.started_at, now))}
            </Field>
            <Field label="Turns">{running.turn_count}</Field>
            <Field label="Tokens">
              {formatInt(running.tokens.total_tokens)} (in {formatInt(running.tokens.input_tokens)}
              {' / '}out {formatInt(running.tokens.output_tokens)})
            </Field>
            <Field label="Last event">
              <LastEvent event={running.last_event} message={running.last_message} />
            </Field>
          </Fields>
        </DrawerSection>
      )}

      {retry !== null && (
        <DrawerSection title="Retry">
          <Fields>
            <Field label="Attempt">{retry.attempt ?? 'n/a'}</Field>
            <Field label="Due">
              <RelativeTime iso={retry.due_at} now={now} />
            </Field>
            <Field label="Error">{retry.error ?? NotAvailable}</Field>
          </Fields>
        </DrawerSection>
      )}

      {blocked !== null && (
        <DrawerSection title="Blocked">
          <Fields>
            <Field label="State">
              <Badge tone="danger">{blocked.state ?? 'Blocked'}</Badge>
            </Field>
            <Field label="Since">
              <RelativeTime iso={blocked.blocked_at} now={now} />
            </Field>
            <Field label="Session">
              {blocked.session_id === null ? (
                NotAvailable
              ) : (
                <CopyButton value={blocked.session_id} />
              )}
            </Field>
            <Field label="Error">{blocked.error ?? NotAvailable}</Field>
            <Field label="Last event">
              <LastEvent event={blocked.last_event} message={blocked.last_message} />
            </Field>
          </Fields>
        </DrawerSection>
      )}

      {detail.recent_events.length > 0 && (
        <DrawerSection title="Recent events">
          <ol class="space-y-2">
            {detail.recent_events.map((event) => (
              <li key={`${event.at}-${event.event ?? ''}`}>
                <span class="text-muted-foreground text-xs">
                  <RelativeTime iso={event.at} now={now} />
                </span>{' '}
                <span class="font-mono text-xs">{event.event ?? 'n/a'}</span>
                {event.message !== null && <div>{event.message}</div>}
              </li>
            ))}
          </ol>
        </DrawerSection>
      )}
    </>
  );
}
