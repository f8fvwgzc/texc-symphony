import { fireEvent, screen, within } from '@testing-library/preact';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { ApiError, type ApiClient } from '../api/client';
import { makeFakeApi } from '../test/fakeApi';
import { makeRun } from '../test/fixtures';
import { at } from '../test/dom';
import { renderWithApi } from '../test/render';
import { RUNS_PAGE_SIZE, RunsPage } from './RunsPage';

afterEach(() => {
  window.location.hash = '';
});

describe('RunsPage', () => {
  it('lists runs with filters applied to the request', async () => {
    const api = makeFakeApi({
      listRuns: vi.fn<ApiClient['listRuns']>(() =>
        Promise.resolve({
          runs: [
            makeRun(),
            makeRun({ id: 41, status: 'failed', error: 'tests failed', duration_ms: 42_000 }),
            makeRun({ id: 40, status: 'running', finished_at: null, duration_ms: null }),
          ],
          next_before_id: 40,
        }),
      ),
    });
    renderWithApi(<RunsPage query={{ status: 'failed', issue: 'MT-HTTP' }} />, api);

    const table = await screen.findByRole('table', { name: 'Runs' });
    expect(api.listRuns).toHaveBeenCalledWith(
      { limit: RUNS_PAGE_SIZE, issue: 'MT-HTTP', status: 'failed' },
      expect.anything(),
    );
    const rows = within(table).getAllByRole('row');
    expect(rows).toHaveLength(4);
    const first = within(at(rows, 1));
    expect(first.getByRole('link', { name: '#42' })).toHaveAttribute('href', '#/runs/42');
    expect(first.getByText('Render the HTTP dashboard')).toBeInTheDocument();
    expect(first.getByText('succeeded')).toHaveClass('badge-active');
    expect(first.getByText('21m 28s')).toBeInTheDocument();
    expect(first.getByText('20,437')).toBeInTheDocument();
    expect(first.getByText('2026-02-24 20:10:12 UTC')).toBeInTheDocument();
    expect(within(at(rows, 2)).getByText('tests failed')).toBeInTheDocument();
    expect(within(at(rows, 3)).getAllByText('running')).toHaveLength(2);
  });

  it('pages older and back to newer', async () => {
    const api = makeFakeApi({
      listRuns: vi.fn<ApiClient['listRuns']>(() =>
        Promise.resolve({ runs: [makeRun()], next_before_id: 42 }),
      ),
    });
    const { rerender } = renderWithApi(<RunsPage query={{}} />, api);
    const newer = await screen.findByRole('button', { name: '← Newer' });
    expect(newer).toBeDisabled();
    fireEvent.click(screen.getByRole('button', { name: 'Older →' }));
    expect(window.location.hash).toBe('#/runs?before=42');

    rerender(<RunsPage query={{ before: 42 }} />);
    await screen.findByRole('table', { name: 'Runs' });
    expect(api.listRuns).toHaveBeenLastCalledWith(
      { limit: RUNS_PAGE_SIZE, before_id: 42 },
      expect.anything(),
    );
    fireEvent.click(screen.getByRole('button', { name: '← Newer' }));
    expect(window.location.hash).toBe('#/runs');
  });

  it('disables "Older" on the last page', async () => {
    const api = makeFakeApi({
      listRuns: vi.fn<ApiClient['listRuns']>(() =>
        Promise.resolve({ runs: [makeRun()], next_before_id: null }),
      ),
    });
    renderWithApi(<RunsPage query={{}} />, api);
    expect(await screen.findByRole('button', { name: 'Older →' })).toBeDisabled();
  });

  it('submits filters through the URL hash', async () => {
    const api = makeFakeApi();
    renderWithApi(<RunsPage query={{}} />, api);
    expect(await screen.findByText('No runs match these filters.')).toBeInTheDocument();
    fireEvent.input(screen.getByLabelText('Issue'), { target: { value: ' MT-9 ' } });
    fireEvent.change(screen.getByLabelText('Status'), { target: { value: 'blocked' } });
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
    expect(window.location.hash).toBe('#/runs?status=blocked&issue=MT-9');
  });

  it('explains when history is disabled', async () => {
    const api = makeFakeApi({
      listRuns: vi.fn<ApiClient['listRuns']>(() =>
        Promise.reject(new ApiError(503, 'store_disabled', 'Run history store is disabled')),
      ),
    });
    renderWithApi(<RunsPage query={{}} />, api);
    expect(await screen.findByText(/Run history is disabled on this server/)).toBeInTheDocument();
  });

  it('offers a retry on errors', async () => {
    const listRuns = vi
      .fn<ReturnType<typeof makeFakeApi>['listRuns']>()
      .mockRejectedValueOnce(new Error('offline'))
      .mockResolvedValueOnce({ runs: [makeRun()], next_before_id: null });
    renderWithApi(<RunsPage query={{}} />, makeFakeApi({ listRuns }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not load runs: offline');
    fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    expect(await screen.findByRole('table', { name: 'Runs' })).toBeInTheDocument();
  });
});
