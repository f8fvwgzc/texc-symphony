import { act, fireEvent, render, screen } from '@testing-library/preact';
import { useState } from 'preact/hooks';
import { describe, expect, it, vi } from 'vitest';

import { ApiError, type ApiClient } from '../../api/client';
import { ApiContext } from '../../api/context';
import { makeFakeApi } from '../../test/fakeApi';
import { makeIssueDetail, makeRun } from '../../test/fixtures';
import { renderWithApi } from '../../test/render';
import { IssueDrawer } from './IssueDrawer';

const NOW = Date.parse('2026-02-24T20:15:30Z');

function renderDrawer(
  api: ReturnType<typeof makeFakeApi>,
  props: Partial<Parameters<typeof IssueDrawer>[0]> = {},
) {
  const onClose = vi.fn();
  const view = renderWithApi(
    <IssueDrawer
      identifier="MT-HTTP"
      issueUrl="https://example.org/issues/MT-HTTP"
      revision={1}
      storeEnabled
      now={NOW}
      onClose={onClose}
      {...props}
    />,
    api,
  );
  return { ...view, onClose };
}

describe('IssueDrawer', () => {
  it('loads the issue detail and recent runs', async () => {
    const api = makeFakeApi({
      getIssue: vi.fn<ApiClient['getIssue']>(() => Promise.resolve(makeIssueDetail())),
      listRuns: vi.fn<ApiClient['listRuns']>(() =>
        Promise.resolve({ runs: [makeRun()], next_before_id: null }),
      ),
    });
    renderDrawer(api);

    const dialog = screen.getByRole('dialog');
    expect(dialog).toHaveAttribute('aria-labelledby', 'drawer-title');
    expect(await screen.findByText('/tmp/symphony_workspaces/MT-HTTP')).toBeInTheDocument();
    expect(api.getIssue).toHaveBeenCalledWith(
      'MT-HTTP',
      expect.objectContaining({ signal: expect.any(AbortSignal) }),
    );
    expect(screen.getByText('running')).toHaveAttribute('data-tone', 'active');
    expect(screen.getByText('5m 18s')).toBeInTheDocument();
    expect(screen.getByText('12 (in 4 / out 8)')).toBeInTheDocument();
    expect(
      screen.getByRole('link', { name: 'Open MT-HTTP in the issue tracker' }),
    ).toBeInTheDocument();

    expect(await screen.findByRole('link', { name: 'Run #42' })).toHaveAttribute(
      'href',
      '#/runs/42',
    );
    expect(api.listRuns).toHaveBeenCalledWith({ issue: 'MT-HTTP', limit: 5 }, expect.anything());
    expect(screen.getByRole('link', { name: 'All runs for MT-HTTP' })).toHaveAttribute(
      'href',
      '#/runs?issue=MT-HTTP',
    );
  });

  it('renders retry and blocked sections with the last error', async () => {
    const api = makeFakeApi({
      getIssue: vi.fn<ApiClient['getIssue']>(() =>
        Promise.resolve(
          makeIssueDetail({
            status: 'retrying',
            running: null,
            retry: {
              attempt: 3,
              due_at: '2026-02-24T20:16:30Z',
              error: 'boom',
              worker_host: null,
              workspace_path: null,
            },
            blocked: {
              worker_host: 'dm-dev2',
              workspace_path: '/w',
              session_id: null,
              state: null,
              error: 'needs input',
              blocked_at: '2026-02-24T20:14:30Z',
              last_event: 'turn_input_required',
              last_message: 'turn blocked: waiting for user input',
              last_event_at: null,
            },
            attempts: { restart_count: 2, current_retry_attempt: 3 },
            last_error: 'needs input',
            recent_events: [
              { at: '2026-02-24T20:15:00Z', event: 'notification', message: 'hello' },
            ],
          }),
        ),
      ),
    });
    renderDrawer(api, { storeEnabled: false });
    expect(await screen.findByText('in 1m')).toBeInTheDocument();
    expect(screen.getAllByText('needs input')).toHaveLength(2);
    expect(screen.getByText('turn blocked: waiting for user input')).toBeInTheDocument();
    expect(screen.getByText('hello')).toBeInTheDocument();
    expect(screen.getByText('retrying')).toHaveAttribute('data-tone', 'warning');
    expect(api.listRuns).not.toHaveBeenCalled();
  });

  it('explains when the issue is no longer tracked', async () => {
    const api = makeFakeApi({
      getIssue: vi.fn<ApiClient['getIssue']>(() =>
        Promise.reject(new ApiError(404, 'issue_not_found', 'Issue not found')),
      ),
    });
    renderDrawer(api, { storeEnabled: false });
    expect(
      await screen.findByText('MT-HTTP is not running, retrying or blocked right now.'),
    ).toBeInTheDocument();
  });

  it('shows other errors', async () => {
    const api = makeFakeApi({
      getIssue: vi.fn<ApiClient['getIssue']>(() => Promise.reject(new Error('boom'))),
    });
    renderDrawer(api, { storeEnabled: false });
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not load issue: boom');
  });

  it('closes with the button, Escape (cancel) and backdrop clicks', async () => {
    const api = makeFakeApi({
      getIssue: vi.fn<ApiClient['getIssue']>(() => Promise.resolve(makeIssueDetail())),
    });
    const { onClose } = renderDrawer(api, { storeEnabled: false });
    fireEvent.click(screen.getByRole('button', { name: 'Close issue details' }));
    expect(onClose).toHaveBeenCalledTimes(1);

    const dialog = screen.getByRole('dialog');
    dialog.dispatchEvent(new Event('cancel', { cancelable: true }));
    expect(onClose).toHaveBeenCalledTimes(2);

    fireEvent.click(dialog);
    expect(onClose).toHaveBeenCalledTimes(3);
    fireEvent.click(await screen.findByText('Workspace'));
    expect(onClose).toHaveBeenCalledTimes(3);
  });

  it('re-fetches when the live revision changes, at most every 2 s', async () => {
    vi.useFakeTimers();
    const api = makeFakeApi({
      getIssue: vi.fn<ApiClient['getIssue']>(() => Promise.resolve(makeIssueDetail())),
    });
    let bump = () => undefined as void;
    function Harness() {
      const [revision, setRevision] = useState(1);
      bump = () => setRevision((value) => value + 1);
      return (
        <ApiContext.Provider value={api}>
          <IssueDrawer
            identifier="MT-HTTP"
            issueUrl={null}
            revision={revision}
            storeEnabled={false}
            now={NOW}
            onClose={() => undefined}
          />
        </ApiContext.Provider>
      );
    }
    render(<Harness />);
    await vi.advanceTimersByTimeAsync(0);
    expect(api.getIssue).toHaveBeenCalledTimes(1);

    await act(() => bump());
    await act(() => bump());
    await vi.advanceTimersByTimeAsync(1_000);
    expect(api.getIssue).toHaveBeenCalledTimes(1);
    // + a frame for Preact's after-paint effect scheduling.
    await vi.advanceTimersByTimeAsync(1_100);
    expect(api.getIssue).toHaveBeenCalledTimes(2);
    expect(screen.getByText('Workspace')).toBeInTheDocument();
    vi.useRealTimers();
  });
});
