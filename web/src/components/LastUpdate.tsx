import { RelativeTime } from './Time';

/** "Codex update" cell: humanized message, then event name and time. */
export function LastUpdate(props: {
  message: string | null;
  event: string | null;
  at: string | null;
  now: number;
}) {
  const text = props.message ?? props.event ?? 'n/a';
  return (
    <div class="stack">
      <span class="event-text" title={text}>
        {text}
      </span>
      <span class="muted event-meta">
        <span class="mono">{props.event ?? 'n/a'}</span>
        {props.at !== null && (
          <>
            {' · '}
            <RelativeTime iso={props.at} now={props.now} />
          </>
        )}
      </span>
    </div>
  );
}
