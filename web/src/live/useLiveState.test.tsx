import { act, render, screen } from '@testing-library/preact';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ApiClient } from '../api/client';
import { makeFakeApi } from '../test/fakeApi';
import { FakeEventSource } from '../test/fakeEventSource';
import { makeSnapshot } from '../test/fixtures';
import { defaultEventSourceFactory, useLiveState, type UseLiveStateOptions } from './useLiveState';

function Probe({ api, options }: { api: ApiClient; options: UseLiveStateOptions }) {
  const live = useLiveState(api, options);
  return (
    <p>
      {live.status}:{live.revision}:{live.generation ?? '-'}
    </p>
  );
}

beforeEach(() => FakeEventSource.reset());
afterEach(() => vi.unstubAllGlobals());

describe('useLiveState', () => {
  it('subscribes on mount and closes the stream on unmount', async () => {
    const api = makeFakeApi();
    const view = render(
      <Probe api={api} options={{ createEventSource: FakeEventSource.factory }} />,
    );
    expect(screen.getByText('connecting:0:-')).toBeInTheDocument();
    await act(() => FakeEventSource.latest().snapshot(makeSnapshot(), 9));
    expect(screen.getByText('live:1:9')).toBeInTheDocument();
    // Preact 11 runs passive-effect cleanups after paint; act() flushes them.
    await act(() => {
      view.unmount();
    });
    expect(FakeEventSource.latest().closed).toBe(true);
  });

  it('polls when EventSource is forced off', async () => {
    const api = makeFakeApi();
    render(<Probe api={api} options={{ createEventSource: null }} />);
    expect(await screen.findByText('polling:1:-')).toBeInTheDocument();
    expect(api.getState).toHaveBeenCalledTimes(1);
  });

  it('detects EventSource support', () => {
    vi.stubGlobal('EventSource', undefined);
    expect(defaultEventSourceFactory()).toBeNull();
    vi.stubGlobal('EventSource', FakeEventSource);
    const factory = defaultEventSourceFactory();
    expect(factory?.('/x')).toBeInstanceOf(FakeEventSource);
  });
});
