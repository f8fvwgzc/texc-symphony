import type { JsonValue } from '../../api/types';
import { summarizeRateLimits } from '../../lib/format';
import { Panel } from '../../ui/Panel';
import { Empty, JsonDetails } from '../../ui/Text';

function Limit(props: { label: string; text: string; ratio?: number | null }) {
  return (
    <li class="bg-muted flex flex-col gap-1 rounded-lg p-3">
      <span class="text-muted-foreground text-xs font-medium tracking-wide uppercase">
        {props.label}
      </span>
      <span class="font-mono text-sm">{props.text}</span>
      {props.ratio != null && (
        <meter
          class="h-2 w-full"
          min={0}
          max={1}
          low={0.2}
          high={0.5}
          optimum={1}
          value={props.ratio}
          aria-label={`${props.label} remaining`}
        />
      )}
    </li>
  );
}

/** Latest upstream rate-limit snapshot: buckets with meters, credits, and the raw JSON. */
export function RateLimits({ value }: { value: JsonValue }) {
  const summary = summarizeRateLimits(value);
  return (
    <Panel title="Rate limits" description="Latest upstream rate-limit snapshot, when available.">
      {value === null ? (
        <Empty>No rate-limit data reported yet.</Empty>
      ) : (
        <>
          {summary !== null && (
            <ul class="grid gap-3 sm:grid-cols-2 lg:grid-cols-4" aria-label="Rate limit buckets">
              <Limit label="Limit" text={summary.name} />
              {summary.buckets.map((bucket) => (
                <Limit
                  key={bucket.label}
                  label={bucket.label}
                  text={bucket.text}
                  ratio={bucket.ratio}
                />
              ))}
              <Limit label="Credits" text={summary.credits.replace(/^credits /, '')} />
            </ul>
          )}
          <JsonDetails label="Raw JSON" value={value} open={summary === null} />
        </>
      )}
    </Panel>
  );
}
