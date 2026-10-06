import { useCallback } from 'preact/hooks';

import { useApi } from './api/context';
import { isStateError } from './api/types';
import { IssueDrawer } from './features/issues/IssueDrawer';
import { OverviewPage } from './features/overview/OverviewPage';
import { RunDetailPage } from './features/runs/RunDetailPage';
import { RunsPage, runsFilterKey } from './features/runs/RunsPage';
import { useAsync } from './hooks/useAsync';
import { useNow } from './hooks/useNow';
import { useThrottled } from './hooks/useThrottled';
import { AppShell } from './layout/AppShell';
import { ConnectionBadge } from './layout/ConnectionBadge';
import { RefreshButton } from './layout/RefreshButton';
import { ThemeToggle } from './layout/ThemeToggle';
import { formatUtc } from './lib/format';
import { formatRoute, navigate, useHashRoute } from './lib/router';
import { useLiveState, type UseLiveStateOptions } from './live/useLiveState';
import { Panel } from './ui/Panel';

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
    <>
      <AppShell
        title={onHistory ? 'Run history' : 'Overview'}
        nav={[
          {
            label: 'Overview',
            href: formatRoute({ name: 'overview' }),
            icon: 'dashboard',
            current: !onHistory,
          },
          {
            label: 'Run history',
            href: formatRoute({ name: 'runs', query: {} }),
            icon: 'history',
            current: onHistory,
          },
        ]}
        actions={
          <>
            <ConnectionBadge
              status={live.status}
              lastError={live.lastError}
              nextRetryAt={live.nextRetryAt}
              now={now}
            />
            <RefreshButton onRefreshed={live.refresh} />
            <ThemeToggle />
          </>
        }
        footer={
          <>
            {health.data !== undefined && (
              <p>
                symphony {health.data.version} · history {storeEnabled ? 'on' : 'off'}
              </p>
            )}
            {live.state !== null && <p>Snapshot {formatUtc(live.state.generated_at)}</p>}
            {live.generation !== null && <p>generation {live.generation}</p>}
          </>
        }
      >
        {route.name === 'overview' && (
          <OverviewPage state={live.state} totals={totals.data ?? null} now={now} />
        )}
        {route.name === 'runs' && <RunsPage key={runsFilterKey(route.query)} query={route.query} />}
        {route.name === 'run' && <RunDetailPage key={route.id} id={route.id} />}
        {route.name === 'not-found' && (
          <Panel title="Page not found">
            <a href={formatRoute({ name: 'overview' })}>Back to the overview</a>
          </Panel>
        )}
      </AppShell>

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
    </>
  );
}
