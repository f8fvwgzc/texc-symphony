import type { StateSnapshot, Totals } from '../api/types';
import {
  formatCompact,
  formatDurationMs,
  formatInt,
  formatPercent,
  formatRuntimeSeconds,
  totalRuntimeSeconds,
} from '../lib/format';

function Metric(props: {
  label: string;
  value: string;
  detail: string;
  tone?: 'danger' | 'warning';
}) {
  return (
    <article class={props.tone === undefined ? 'card metric' : `card metric metric-${props.tone}`}>
      <p class="metric-label">{props.label}</p>
      <p class="metric-value numeric">{props.value}</p>
      <p class="metric-detail">{props.detail}</p>
    </article>
  );
}

/** Overview counters: live counts, token totals and runtime (plus all-time totals when stored). */
export function MetricGrid({
  snapshot,
  totals,
  now,
}: {
  snapshot: StateSnapshot;
  totals: Totals | null;
  now: number;
}) {
  const { counts, codex_totals: codex } = snapshot;
  return (
    <section class="metric-grid" aria-label="Summary">
      <Metric label="Running" value={String(counts.running)} detail="Active issue sessions." />
      <Metric
        label="Retrying"
        value={String(counts.retrying)}
        detail="Waiting for the next retry window."
        {...(counts.retrying > 0 ? { tone: 'warning' as const } : {})}
      />
      <Metric
        label="Blocked"
        value={String(counts.blocked)}
        detail="Paused for operator input or approval."
        {...(counts.blocked > 0 ? { tone: 'danger' as const } : {})}
      />
      <Metric
        label="Total tokens"
        value={formatInt(codex.total_tokens)}
        detail={`In ${formatInt(codex.input_tokens)} / Out ${formatInt(codex.output_tokens)}`}
      />
      <Metric
        label="Runtime"
        value={formatRuntimeSeconds(totalRuntimeSeconds(snapshot, now))}
        detail="Codex runtime across completed and active sessions."
      />
      {totals !== null && (
        <Metric
          label="All-time runs"
          value={formatInt(totals.runs_total)}
          detail={`${formatPercent(totals.runs_succeeded, totals.runs_total)} succeeded · ${formatInt(
            totals.runs_failed,
          )} failed · ${formatCompact(totals.tokens.total)} tokens · ${formatDurationMs(
            totals.runtime_ms,
          )}`}
        />
      )}
    </section>
  );
}
