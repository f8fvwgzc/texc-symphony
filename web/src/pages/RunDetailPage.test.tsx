import { fireEvent, screen } from '@testing-library/preact';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { ApiError, type ApiClient } from '../api/client';
import { makeFakeApi } from '../test/fakeApi';
import { makeRun, makeRunEvent } from '../test/fixtures';
import { renderWithApi } from '../test/render';
import { EVENTS_PAGE_SIZE, RUN_POLL_MS, RunDetailPage } from './RunDetailPage';

afterEach(() => {
  vi.useRealTimers();
});

describe('RunDetailPage', () => {
  it('shows the run summary and its events timeline', async () => {
    const api = makeFakeApi({
      getRun: vi.fn<ApiClient['getRun']>(() =>
        Promise.resolve(makeRun({ error: 'flaky test', worker_host: 'dm-dev2' })),
      ),
      listRunEvents: vi.fn<ApiClient['listRunEvents']>(() =>
        Promise.resolve({
          events: [makeRunEvent(1, { payload: { session_id: 'thread-http' } }), makeRunEvent(2)],
        }),
      ),
    });
    renderWithApi(<RunDetailPage id={42} />, api);

    expect(
      await screen.findByText('Render the HTTP dashboard', { exact: false }),
    ).toBeInTheDocument();
    expect(screen.getByText('succeeded')).toHaveClass('badge-active');
    expect(screen.getByText('21m 28s')).toBeInTheDocument();
    expect(screen.getByText('20,437 (in 18,230 / out 2,207)')).toBeInTheDocument();
    expect(screen.getByText('flaky test')).toHaveClass('danger');
    expect(screen.getByText('dm-dev2')).toBeInTheDocument();

    const timeline = await screen.findByRole('list', { name: 'Run events' });
    expect(timeline.children).toHaveLength(2);
    expect(screen.getByText('session_started')).toBeInTheDocument();
    expect(screen.getByText('event 1')).toBeInTheDocument();
    expect(screen.getByText('20:10:11.000')).toBeInTheDocument();
    expect(screen.getByText(/"session_id": "thread-http"/)).toBeInTheDocument();
    expect(api.listRunEvents).toHaveBeenCalledWith(42, { after_seq: 0, limit: EVENTS_PAGE_SIZE });
  });

  it('loads more events with after_seq', async () => {
    const fullPage = Array.from({ length: EVENTS_PAGE_SIZE }, (_, index) =>
      makeRunEvent(index + 1),
    );
    const listRunEvents = vi
      .fn<ReturnType<typeof makeFakeApi>['listRunEvents']>()
      .mockResolvedValueOnce({ events: fullPage })
      .mockResolvedValueOnce({
        events: [makeRunEvent(EVENTS_PAGE_SIZE + 1, { message: 'the last one' })],
      });
    const api = makeFakeApi({
      getRun: vi.fn<ApiClient['getRun']>(() => Promise.resolve(makeRun())),
      listRunEvents,
    });
    renderWithApi(<RunDetailPage id={42} />, api);

    fireEvent.click(await screen.findByRole('button', { name: 'Load more events' }));
    expect(await screen.findByText('the last one')).toBeInTheDocument();
    expect(listRunEvents).toHaveBeenLastCalledWith(42, {
      after_seq: EVENTS_PAGE_SIZE,
      limit: EVENTS_PAGE_SIZE,
    });
    expect(screen.queryByRole('button', { name: 'Load more events' })).toBeNull();
  });

  it('polls a running run for new events', async () => {
    vi.useFakeTimers();
    const listRunEvents = vi
      .fn<ReturnType<typeof makeFakeApi>['listRunEvents']>()
      .mockResolvedValueOnce({ events: [makeRunEvent(1)] })
      .mockResolvedValue({ events: [makeRunEvent(2, { message: 'fresh' })] });
    const getRun = vi.fn(() =>
      Promise.resolve(makeRun({ status: 'running', finished_at: null, duration_ms: null })),
    );
    renderWithApi(<RunDetailPage id={42} />, makeFakeApi({ getRun, listRunEvents }));
    await vi.advanceTimersByTimeAsync(100);
    expect(screen.getByText('still running')).toBeInTheDocument();
    expect(screen.getByText('Live: new events appear every few seconds.')).toBeInTheDocument();

    await vi.advanceTimersByTimeAsync(RUN_POLL_MS + 100);
    expect(listRunEvents).toHaveBeenLastCalledWith(42, { after_seq: 1, limit: EVENTS_PAGE_SIZE });
    expect(screen.getByText('fresh')).toBeInTheDocument();
    expect(getRun.mock.calls.length).toBeGreaterThanOrEqual(2);
  });

  it('handles missing runs and disabled history', async () => {
    const missing = makeFakeApi({
      getRun: vi.fn<ApiClient['getRun']>(() =>
        Promise.reject(new ApiError(404, 'run_not_found', 'Run not found')),
      ),
      listRunEvents: vi.fn<ApiClient['listRunEvents']>(() =>
        Promise.reject(new ApiError(404, 'run_not_found', 'Run not found')),
      ),
    });
    const view = renderWithApi(<RunDetailPage id={7} />, missing);
    expect(
      await screen.findByText('Could not load run: run_not_found: Run not found'),
    ).toBeInTheDocument();
    expect(
      await screen.findByText('Could not load events: run_not_found: Run not found'),
    ).toBeInTheDocument();
    view.unmount();

    const disabled = makeFakeApi({
      getRun: vi.fn<ApiClient['getRun']>(() =>
        Promise.reject(new ApiError(503, 'store_disabled', 'disabled')),
      ),
    });
    renderWithApi(<RunDetailPage id={7} />, disabled);
    expect(await screen.findByText(/Run history is disabled/)).toBeInTheDocument();
  });

  it('says when a run has no events', async () => {
    renderWithApi(
      <RunDetailPage id={42} />,
      makeFakeApi({ getRun: vi.fn<ApiClient['getRun']>(() => Promise.resolve(makeRun())) }),
    );
    expect(await screen.findByText('No events recorded for this run.')).toBeInTheDocument();
  });
});
