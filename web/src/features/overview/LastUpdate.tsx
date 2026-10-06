import { RelativeTime } from '../../ui/RelativeTime';

/** "Last event" cell: humanized message, then event name and time. */
export function LastUpdate(props: {
  message: string | null;
  event: string | null;
  at: string | null;
  now: number;
}) {
  const text = props.message ?? props.event ?? 'n/a';
  return (
    <div class="flex max-w-md flex-col gap-0.5">
      <span class="line-clamp-2" title={text}>
        {text}
      </span>
      <span class="text-muted-foreground text-xs">
        <span class="font-mono">{props.event ?? 'n/a'}</span>
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
