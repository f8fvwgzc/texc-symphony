import type { ConnectionStatus } from '../live/liveSource';

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
    <output class={`connection connection-${props.status}`} aria-live="polite" title={detail}>
      <span class="connection-dot" aria-hidden="true" />
      {LABELS[props.status]}
      {props.status === 'offline' && retryIn !== null && (
        <span class="connection-retry"> · retry {retryIn}s</span>
      )}
      <span class="visually-hidden"> {detail}</span>
    </output>
  );
}
