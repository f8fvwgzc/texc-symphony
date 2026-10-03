import { useCallback } from 'preact/hooks';

import { useApi } from './api/context';
import { isStateError } from './api/types';
import { ConnectionBadge } from './components/ConnectionBadge';
import { IssueDrawer } from './components/IssueDrawer';
import { RefreshButton } from './components/RefreshButton';
import { ThemeToggle } from './components/ThemeToggle';
import { useAsync } from './hooks/useAsync';
import { useNow } from './hooks/useNow';
import { useThrottled } from './hooks/useThrottled';
import { formatUtc } from './lib/format';
import { formatRoute, navigate, useHashRoute } from './lib/router';
import { useLiveState, type UseLiveStateOptions } from './live/useLiveState';
import { OverviewPage } from './pages/OverviewPage';
import { RunDetailPage } from './pages/RunDetailPage';
import { RunsPage, runsFilterKey } from './pages/RunsPage';

export function App({ liveOptions }: { liveOptions?: UseLiveStateOptions }) {
  const api = useApi();
  const route = useHashRoute();
  const live = useLiveState(api, liveOptions);
  const now = useNow(1_000);

  const health = useAsync((signal) => api.getHealth({ signal }), [api]);
  const storeEnabled = health.data?.store === 'sqlite';
  // All-time totals change slowly; re-read them at most every 10 s of live updates.
  const totalsKey = useThrottled(live.revision, 10_000);
  const totals = useAsync(
    (signal) => (storeEnabled ? api.getTotals({ signal }) : Promise.resolve(null)),
    [api, storeEnabled, totalsKey],
  );

  const drawerIssue = route.name === 'overview' ? route.issue : undefined;
  const closeDrawer = useCallback(() => navigate({ name: 'overview' }), []);
  const issueUrl =
    drawerIssue === undefined || live.state === null || isStateError(live.state)
      ? null
      : ([...live.state.running, ...live.state.retrying, ...live.state.blocked].find(
          (entry) => entry.issue_identifier === drawerIssue,
        )?.issue_url ?? null);

  const onHistory = route.name === 'runs' || route.name === 'run';

  return (
    <div class="app">
      <a class="skip-link" href="#main">
        Skip to content
      </a>
      <header class="topbar">
        <div class="brand">
          <img src="/favicon.png" alt="" width={28} height={28} />
          <div>
            <p class="eyebrow">Symphony</p>
            <p class="brand-title">Operations</p>
          </div>
        </div>
        <nav class="nav" aria-label="Primary">
          <a href={formatRoute({ name: 'overview' })} aria-current={onHistory ? undefined : 'page'}>
            Overview
          </a>
          <a
            href={formatRoute({ name: 'runs', query: {} })}
            aria-current={onHistory ? 'page' : undefined}
          >
            Run history
          </a>
        </nav>
        <div class="topbar-actions">
          <ConnectionBadge
            status={live.status}
            lastError={live.lastError}
            nextRetryAt={live.nextRetryAt}
            now={now}
          />
          <RefreshButton onRefreshed={live.refresh} />
          <ThemeToggle />
        </div>
      </header>

      <main id="main" class="content" tabIndex={-1}>
        {route.name === 'overview' && (
          <OverviewPage state={live.state} totals={totals.data ?? null} now={now} />
        )}
        {route.name === 'runs' && <RunsPage key={runsFilterKey(route.query)} query={route.query} />}
        {route.name === 'run' && <RunDetailPage key={route.id} id={route.id} />}
        {route.name === 'not-found' && (
          <section class="card section">
            <h2 class="section-title">Page not found</h2>
            <p>
              <a href={formatRoute({ name: 'overview' })}>Back to the overview</a>
            </p>
          </section>
        )}
      </main>

      <footer class="footer muted small">
        {live.state !== null && <span>Snapshot {formatUtc(live.state.generated_at)}</span>}
        {live.generation !== null && <span> · generation {live.generation}</span>}
        {health.data !== undefined && (
          <span>
            {' '}
            · symphony {health.data.version} · history {storeEnabled ? 'on' : 'off'}
          </span>
        )}
      </footer>

      {drawerIssue !== undefined && (
        <IssueDrawer
          key={drawerIssue}
          identifier={drawerIssue}
          issueUrl={issueUrl}
          revision={live.revision}
          storeEnabled={storeEnabled}
          now={now}
          onClose={closeDrawer}
        />
      )}
    </div>
  );
}
