import { isStateError, type StatePayload, type Totals } from '../api/types';
import { BlockedTable } from '../components/BlockedTable';
import { MetricGrid } from '../components/Metrics';
import { RateLimits } from '../components/RateLimits';
import { RetryTable } from '../components/RetryTable';
import { RunningTable } from '../components/RunningTable';

/** Live overview: counters, rate limits, running / blocked / retrying tables. */
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
      <p class="card section muted" aria-busy="true">
        Waiting for the first snapshot…
      </p>
    );
  }
  if (isStateError(state)) {
    return (
      <section class="card notice-card" role="alert">
        <h2 class="notice-title">Snapshot unavailable</h2>
        <p>
          <strong>{state.error.code}:</strong> {state.error.message}
        </p>
      </section>
    );
  }
  return (
    <>
      <MetricGrid snapshot={state} totals={totals} now={now} />
      <RunningTable entries={state.running} now={now} />
      <BlockedTable entries={state.blocked} now={now} />
      <RetryTable entries={state.retrying} now={now} />
      <RateLimits value={state.rate_limits} />
    </>
  );
}
