import type { JsonValue } from '../api/types';
import { prettyJson, summarizeRateLimits } from '../lib/format';
import { Section } from './Section';

/** Latest upstream rate-limit snapshot: buckets with meters, credits, and the raw JSON. */
export function RateLimits({ value }: { value: JsonValue }) {
  const summary = summarizeRateLimits(value);
  return (
    <Section title="Rate limits" description="Latest upstream rate-limit snapshot, when available.">
      {value === null ? (
        <p class="empty">No rate-limit data reported yet.</p>
      ) : (
        <>
          {summary !== null && (
            <ul class="limits" aria-label="Rate limit buckets">
              <li class="limit">
                <span class="limit-label">Limit</span>
                <span class="limit-value mono">{summary.name}</span>
              </li>
              {summary.buckets.map((bucket) => (
                <li class="limit" key={bucket.label}>
                  <span class="limit-label">{bucket.label}</span>
                  <span class="limit-value mono">{bucket.text}</span>
                  {bucket.ratio !== null && (
                    <meter
                      class="limit-meter"
                      min={0}
                      max={1}
                      low={0.2}
                      high={0.5}
                      optimum={1}
                      value={bucket.ratio}
                      aria-label={`${bucket.label} remaining`}
                    />
                  )}
                </li>
              ))}
              <li class="limit">
                <span class="limit-label">Credits</span>
                <span class="limit-value mono">{summary.credits.replace(/^credits /, '')}</span>
              </li>
            </ul>
          )}
          <details class="raw" open={summary === null}>
            <summary>Raw JSON</summary>
            <pre class="code-panel">{prettyJson(value)}</pre>
          </details>
        </>
      )}
    </Section>
  );
}
