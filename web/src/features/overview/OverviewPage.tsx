import { isStateError, type StatePayload, type Totals } from '../../api/types';
import { BlockedTable } from './BlockedTable';
import { RateLimits } from './RateLimits';
import { RetryTable } from './RetryTable';
import { RunningTable } from './RunningTable';
import { StatGrid } from './StatGrid';

/** Live overview: counters, running / blocked / retrying tables and rate limits. */
export function OverviewPage({
  state,
  totals,
  now,
}: {
  state: StatePayload | null;
  totals: Totals | null;
  now: number;
}) {
  if (state === null) {
    return (
      <p class="text-muted-foreground" aria-busy="true">
        Waiting for the first snapshot…
      </p>
    );
  }
  if (isStateError(state)) {
    return (
      <section
        class="border-destructive/40 bg-destructive/10 text-destructive rounded-xl border p-5"
        role="alert"
      >
        <h2 class="text-base font-semibold">Snapshot unavailable</h2>
        <p class="mt-1">
          <strong>{state.error.code}:</strong> {state.error.message}
        </p>
      </section>
    );
  }
  return (
    <>
      <StatGrid snapshot={state} totals={totals} now={now} />
      <RunningTable entries={state.running} now={now} />
      <BlockedTable entries={state.blocked} now={now} />
      <RetryTable entries={state.retrying} now={now} />
      <RateLimits value={state.rate_limits} />
    </>
  );
}
