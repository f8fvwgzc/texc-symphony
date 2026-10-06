import type { RunEvent } from '../../api/types';
import { formatUtc } from '../../lib/format';
import { Badge } from '../../ui/Badge';
import { JsonDetails } from '../../ui/Text';

/** `HH:MM:SS.mmm` (UTC) of an event timestamp. */
function eventTime(iso: string): string {
  const at = Date.parse(iso);
  return Number.isNaN(at) ? iso : new Date(at).toISOString().slice(11, 23);
}

/** The events of one run, oldest first, each with its collapsible payload. */
export function Timeline({ events }: { events: RunEvent[] }) {
  return (
    <ol class="divide-border divide-y" aria-label="Run events">
      {events.map((event) => (
        <li key={event.seq} class="py-3 first:pt-0 last:pb-0">
          <div class="flex flex-wrap items-center gap-2">
            <time
              class="text-muted-foreground font-mono text-xs"
              dateTime={event.at}
              title={formatUtc(event.at)}
            >
              {eventTime(event.at)}
            </time>
            <Badge tone="neutral" mono>
              {event.kind}
            </Badge>
            <span class="text-muted-foreground text-xs">#{event.seq}</span>
          </div>
          {event.message !== null && <p class="mt-1 break-words">{event.message}</p>}
          {event.payload !== null && <JsonDetails label="Payload" value={event.payload} />}
        </li>
      ))}
    </ol>
  );
}
