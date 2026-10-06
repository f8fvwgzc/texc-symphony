import { fireEvent, render, screen } from '@testing-library/preact';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { ApiError, type ApiClient } from '../api/client';
import { makeFakeApi } from '../test/fakeApi';
import { renderWithApi } from '../test/render';
import { ConnectionBadge } from './ConnectionBadge';
import { CopyButton } from '../features/issues/CopyButton';
import { RateLimits } from '../features/overview/RateLimits';
import { RefreshButton } from './RefreshButton';
import { ThemeToggle } from './ThemeToggle';

const NOW = Date.parse('2026-02-24T20:15:30Z');

afterEach(() => {
  vi.useRealTimers();
  delete document.documentElement.dataset['theme'];
  window.localStorage.clear();
});

describe('ConnectionBadge', () => {
  it.each([
    ['live', 'Live'],
    ['polling', 'Polling'],
    ['connecting', 'Connecting'],
  ] as const)('labels %s', (status, label) => {
    render(<ConnectionBadge status={status} lastError={null} nextRetryAt={null} now={NOW} />);
    const badge = screen.getByRole('status');
    expect(badge).toHaveTextContent(label);
    expect(badge).toHaveAttribute('data-status', status);
  });

  it('shows the retry countdown and last error when offline', () => {
    render(
      <ConnectionBadge
        status="offline"
        lastError="event stream error"
        nextRetryAt={NOW + 4_200}
        now={NOW}
      />,
    );
    const badge = screen.getByRole('status');
    expect(badge).toHaveTextContent('Offline · retry 5s');
    expect(badge).toHaveAttribute(
      'title',
      'Cannot reach the Symphony server. Last error: event stream error. Retrying in 5s.',
    );
  });
});

describe('RateLimits', () => {
  it('renders buckets, a meter and credits', () => {
    render(
      <RateLimits
        value={{
          limit_id: 'priority-tier',
          primary: { remaining: 25, limit: 100, reset_in_seconds: 30 },
          credits: { unlimited: true },
        }}
      />,
    );
    expect(screen.getByText('priority-tier')).toBeInTheDocument();
    expect(screen.getByText('25/100 reset 30s')).toBeInTheDocument();
    expect(screen.getByLabelText('primary remaining')).toHaveAttribute('value', '0.25');
    expect(screen.getByText('unlimited')).toBeInTheDocument();
    expect(screen.getByText('Raw JSON')).toBeInTheDocument();
  });

  it('opens the raw JSON for non-object values', () => {
    const { container } = render(<RateLimits value={[1, 2]} />);
    expect(container.querySelector('details')).toHaveAttribute('open');
    expect(screen.queryByRole('list', { name: 'Rate limit buckets' })).toBeNull();
  });
});

describe('RefreshButton', () => {
  it('queues a refresh and re-reads state', async () => {
    const api = makeFakeApi();
    const onRefreshed = vi.fn();
    renderWithApi(<RefreshButton onRefreshed={onRefreshed} />, api);
    fireEvent.click(screen.getByRole('button', { name: 'Refresh now' }));
    expect(await screen.findByText('Poll and reconcile queued.')).toBeInTheDocument();
    expect(api.requestRefresh).toHaveBeenCalledTimes(1);
    expect(onRefreshed).toHaveBeenCalledTimes(1);
  });

  it('reports coalesced refreshes', async () => {
    const api = makeFakeApi({
      requestRefresh: vi.fn<ApiClient['requestRefresh']>(() =>
        Promise.resolve({
          queued: true as const,
          coalesced: true,
          requested_at: 't',
          operations: [],
        }),
      ),
    });
    renderWithApi(<RefreshButton onRefreshed={() => undefined} />, api);
    fireEvent.click(screen.getByRole('button', { name: 'Refresh now' }));
    expect(await screen.findByText('A poll was already in progress.')).toBeInTheDocument();
  });

  it('shows 503 errors', async () => {
    const api = makeFakeApi({
      requestRefresh: vi.fn<ApiClient['requestRefresh']>(() =>
        Promise.reject(
          new ApiError(503, 'orchestrator_unavailable', 'Orchestrator is unavailable'),
        ),
      ),
    });
    const onRefreshed = vi.fn();
    renderWithApi(<RefreshButton onRefreshed={onRefreshed} />, api);
    fireEvent.click(screen.getByRole('button', { name: 'Refresh now' }));
    expect(
      await screen.findByText(
        'Refresh failed (orchestrator_unavailable: Orchestrator is unavailable)',
      ),
    ).toHaveAttribute('data-tone', 'danger');
    expect(onRefreshed).not.toHaveBeenCalled();
  });
});

describe('CopyButton', () => {
  it('copies the full value and flips its label briefly', async () => {
    vi.useFakeTimers();
    const writeText = vi.fn(() => Promise.resolve());
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    render(<CopyButton value="thread-1234567890" />);
    const button = screen.getByRole('button', { name: 'Copy session ID thread-1234567890' });
    expect(button).toHaveTextContent('thre…567890');
    fireEvent.click(button);
    await vi.advanceTimersByTimeAsync(0);
    expect(writeText).toHaveBeenCalledWith('thread-1234567890');
    expect(button).toHaveTextContent('Copied');
    await vi.advanceTimersByTimeAsync(1_200);
    expect(button).toHaveTextContent('thre…567890');
  });
});

describe('ThemeToggle', () => {
  it('cycles auto -> light -> dark -> auto and remembers the choice', () => {
    render(<ThemeToggle />);
    const button = screen.getByRole('button');
    expect(button).toHaveTextContent('Theme: Auto');
    fireEvent.click(button);
    expect(button).toHaveTextContent('Theme: Light');
    expect(document.documentElement.dataset['theme']).toBe('light');
    expect(window.localStorage.getItem('symphony.theme')).toBe('light');
    fireEvent.click(button);
    expect(document.documentElement.dataset['theme']).toBe('dark');
    fireEvent.click(button);
    expect(document.documentElement.dataset['theme']).toBeUndefined();
    expect(window.localStorage.getItem('symphony.theme')).toBeNull();
  });
});
