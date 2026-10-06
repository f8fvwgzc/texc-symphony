import type { StateSnapshot, Totals } from '../../api/types';
import {
  formatCompact,
  formatDurationMs,
  formatInt,
  formatPercent,
  formatRuntimeSeconds,
  totalRuntimeSeconds,
} from '../../lib/format';
import { cx } from '../../ui/cx';

function Stat(props: {
  label: string;
  value: string;
  detail: string;
  tone?: 'danger' | 'warning';
}) {
  return (
    <article class="from-primary/5 to-card rounded-xl border bg-linear-to-t p-6 shadow-xs">
      <p class="text-muted-foreground text-sm">{props.label}</p>
      <p
        class={cx(
          'mt-1.5 text-3xl font-semibold tabular-nums',
          props.tone === 'danger' && 'text-destructive',
          props.tone === 'warning' && 'text-warn',
        )}
      >
        {props.value}
      </p>
      <p class="text-muted-foreground mt-3 text-sm">{props.detail}</p>
    </article>
  );
}

/** Overview counters: live counts, token totals and runtime (plus all-time totals when stored). */
export function StatGrid({
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
    <section class="grid gap-4 sm:grid-cols-2 xl:grid-cols-3" aria-label="Summary">
      <Stat label="Running" value={String(counts.running)} detail="Active issue sessions." />
      <Stat
        label="Retrying"
        value={String(counts.retrying)}
        detail="Waiting for the next retry window."
        {...(counts.retrying > 0 ? { tone: 'warning' as const } : {})}
      />
      <Stat
        label="Blocked"
        value={String(counts.blocked)}
        detail="Paused for operator input or approval."
        {...(counts.blocked > 0 ? { tone: 'danger' as const } : {})}
      />
      <Stat
        label="Total tokens"
        value={formatInt(codex.total_tokens)}
        detail={`In ${formatInt(codex.input_tokens)} / Out ${formatInt(codex.output_tokens)}`}
      />
      <Stat
        label="Runtime"
        value={formatRuntimeSeconds(totalRuntimeSeconds(snapshot, now))}
        detail="Agent runtime across completed and active sessions."
      />
      {totals !== null && (
        <Stat
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
