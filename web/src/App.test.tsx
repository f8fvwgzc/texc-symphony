import { act, fireEvent, screen, within } from '@testing-library/preact';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ApiClient } from './api/client';

import { App } from './App';
import { makeFakeApi } from './test/fakeApi';
import { FakeEventSource } from './test/fakeEventSource';
import { makeEmptySnapshot, makeIssueDetail, makeSnapshot } from './test/fixtures';
import { renderWithApi } from './test/render';

beforeEach(() => {
  FakeEventSource.reset();
  window.location.hash = '';
});

afterEach(() => {
  vi.useRealTimers();
  window.location.hash = '';
});

function renderApp(api = makeFakeApi()) {
  const view = renderWithApi(
    <App liveOptions={{ createEventSource: FakeEventSource.factory, random: () => 0.5 }} />,
    api,
  );
  return { ...view, api };
}

describe('App', () => {
  it('connects over SSE and renders live snapshots', async () => {
    const { api } = renderApp();
    expect(screen.getAllByRole('status')[0]).toHaveTextContent('Connecting');
    expect(FakeEventSource.latest().url).toBe('/api/v1/events');

    await act(() => FakeEventSource.latest().snapshot(makeSnapshot(), 3));
    expect(await screen.findByRole('table', { name: 'Running sessions' })).toBeInTheDocument();
    expect(screen.getAllByRole('status')[0]).toHaveTextContent('Live');
    expect(screen.getByText(/generation 3/)).toBeInTheDocument();
    expect(await screen.findByText(/symphony 0.1.0 · history on/)).toBeInTheDocument();
    expect(await screen.findByText('All-time runs')).toBeInTheDocument();
    expect(api.getState).not.toHaveBeenCalled();

    await act(() => FakeEventSource.latest().snapshot(makeEmptySnapshot(), 4));
    expect(await screen.findByText('No active sessions.')).toBeInTheDocument();
  });

  it('falls back to polling when SSE keeps failing', async () => {
    const { api } = renderApp();
    vi.useFakeTimers();
    await act(() => FakeEventSource.latest().fail());
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    await act(() => FakeEventSource.latest().fail());
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_000);
    });
    await act(() => FakeEventSource.latest().fail());
    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(api.getState).toHaveBeenCalledTimes(1);
    expect(screen.getAllByRole('status')[0]).toHaveTextContent('Polling');
    expect(screen.getByRole('table', { name: 'Running sessions' })).toBeInTheDocument();
  });

  it('refresh button POSTs and re-reads the state', async () => {
    const { api } = renderApp();
    await act(() => FakeEventSource.latest().snapshot(makeEmptySnapshot(), 1));
    fireEvent.click(screen.getByRole('button', { name: 'Refresh now' }));
    expect(await screen.findByText('Poll and reconcile queued.')).toBeInTheDocument();
    expect(api.requestRefresh).toHaveBeenCalledTimes(1);
    expect(api.getState).toHaveBeenCalledTimes(1);
    expect(await screen.findByRole('table', { name: 'Running sessions' })).toBeInTheDocument();
  });

  it('opens the issue drawer from the hash route', async () => {
    const api = makeFakeApi({
      getIssue: vi.fn<ApiClient['getIssue']>(() => Promise.resolve(makeIssueDetail())),
    });
    renderApp(api);
    await act(() => FakeEventSource.latest().snapshot(makeSnapshot(), 1));
    const running = await screen.findByRole('table', { name: 'Running sessions' });
    fireEvent.click(within(running).getByRole('link', { name: 'Details for MT-HTTP' }));
    await act(() => {
      window.location.hash = '#/issues/MT-HTTP';
      window.dispatchEvent(new HashChangeEvent('hashchange'));
    });
    const dialog = await screen.findByRole('dialog');
    expect(
      within(dialog).getByRole('link', { name: 'Open MT-HTTP in the issue tracker' }),
    ).toHaveAttribute('href', 'https://example.org/issues/MT-HTTP');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Close issue details' }));
    expect(window.location.hash).toBe('#/');
  });

  it('navigates to run history', async () => {
    renderApp();
    await act(() => {
      window.location.hash = '#/runs';
      window.dispatchEvent(new HashChangeEvent('hashchange'));
    });
    expect(await screen.findByRole('heading', { name: 'Run history' })).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Run history' })).toHaveAttribute(
      'aria-current',
      'page',
    );
  });

  it('renders a not-found page for unknown routes', async () => {
    window.location.hash = '#/nope';
    renderApp();
    expect(await screen.findByRole('heading', { name: 'Page not found' })).toBeInTheDocument();
  });
});
