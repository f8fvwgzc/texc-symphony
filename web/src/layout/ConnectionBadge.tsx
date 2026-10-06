import type { ConnectionStatus } from '../live/liveSource';
import { cx } from '../ui/cx';

const LABELS: Record<ConnectionStatus, string> = {
  connecting: 'Connecting',
  live: 'Live',
  polling: 'Polling',
  offline: 'Offline',
};

const DESCRIPTIONS: Record<ConnectionStatus, string> = {
  connecting: 'Connecting to the event stream.',
  live: 'Receiving live updates over Server-Sent Events.',
  polling: 'Event stream unavailable; refreshing every few seconds.',
  offline: 'Cannot reach the Symphony server.',
};

const DOT_CLASS: Record<ConnectionStatus, string> = {
  connecting: 'bg-muted-foreground animate-pulse',
  live: 'bg-ok',
  polling: 'bg-warn',
  offline: 'bg-destructive',
};

/** How the dashboard is currently receiving state. The status is also exposed as `data-status`. */
export function ConnectionBadge(props: {
  status: ConnectionStatus;
  lastError: string | null;
  nextRetryAt: number | null;
  now: number;
}) {
  const retryIn =
    props.nextRetryAt === null
      ? null
      : Math.max(Math.ceil((props.nextRetryAt - props.now) / 1000), 0);
  const detail = [
    DESCRIPTIONS[props.status],
    props.lastError === null ? null : `Last error: ${props.lastError}.`,
    retryIn === null ? null : `Retrying in ${retryIn}s.`,
  ]
    .filter((part) => part !== null)
    .join(' ');

  return (
    <output
      data-status={props.status}
      class="text-muted-foreground inline-flex h-8 items-center gap-2 rounded-md border px-2.5 text-xs font-medium"
      aria-live="polite"
      title={detail}
    >
      <span class={cx('size-2 rounded-full', DOT_CLASS[props.status])} aria-hidden="true" />
      {LABELS[props.status]}
      {props.status === 'offline' && retryIn !== null && <span> · retry {retryIn}s</span>}
      <span class="sr-only"> {detail}</span>
    </output>
  );
}
