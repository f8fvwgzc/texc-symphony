import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { StatePayload } from '../api/types';
import { FakeEventSource } from '../test/fakeEventSource';
import { makeEmptySnapshot, makeSnapshot, SNAPSHOT_TIMEOUT } from '../test/fixtures';
import {
  backoffDelay,
  createLiveSource,
  parseStatePayload,
  type LiveSnapshot,
  type LiveSourceOptions,
} from './liveSource';

function setup(overrides: Partial<LiveSourceOptions> = {}) {
  const fetchState = vi.fn<() => Promise<StatePayload>>(() => Promise.resolve(makeEmptySnapshot()));
  const source = createLiveSource({
    url: '/api/v1/events',
    fetchState,
    createEventSource: FakeEventSource.factory,
    random: () => 0.5, // jitter factor exactly 1.0
    ...overrides,
  });
  const updates: LiveSnapshot[] = [];
  source.subscribe((snapshot) => updates.push(snapshot));
  return { source, fetchState, updates };
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date('2026-02-24T20:15:30Z'));
  FakeEventSource.reset();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('backoffDelay', () => {
  it('doubles from the base and caps at the maximum', () => {
    const fixed = () => 0.5;
    expect([1, 2, 3, 4, 5, 6].map((n) => backoffDelay(n, 1000, 10_000, fixed))).toEqual([
      1000, 2000, 4000, 8000, 10_000, 10_000,
    ]);
  });

  it('applies ±20 % jitter', () => {
    expect(backoffDelay(1, 1000, 10_000, () => 0)).toBe(800);
    expect(backoffDelay(1, 1000, 10_000, () => 0.999_999)).toBe(1200);
  });
});

describe('parseStatePayload', () => {
  it('accepts snapshots and in-band errors', () => {
    expect(parseStatePayload(JSON.stringify(makeSnapshot()))).toEqual(makeSnapshot());
    expect(parseStatePayload(JSON.stringify(SNAPSHOT_TIMEOUT))).toEqual(SNAPSHOT_TIMEOUT);
  });

  it('rejects junk', () => {
    expect(parseStatePayload('not json')).toBeNull();
    expect(parseStatePayload('null')).toBeNull();
    expect(parseStatePayload('{"generated_at": 1}')).toBeNull();
    expect(parseStatePayload('{"generated_at": "x"}')).toBeNull();
  });
});

describe('createLiveSource over SSE', () => {
  it('goes live on the first snapshot and tracks the generation', () => {
    const { source, fetchState } = setup();
    source.start();
    expect(source.getSnapshot().status).toBe('connecting');
    const es = FakeEventSource.latest();
    expect(es.url).toBe('/api/v1/events');

    es.open();
    es.snapshot(makeSnapshot(), 7);
    const snapshot = source.getSnapshot();
    expect(snapshot.status).toBe('live');
    expect(snapshot.generation).toBe(7);
    expect(snapshot.revision).toBe(1);
    expect(snapshot.state).toEqual(makeSnapshot());
    expect(snapshot.updatedAt).toBe(Date.now());

    es.snapshot(SNAPSHOT_TIMEOUT, 8);
    expect(source.getSnapshot()).toMatchObject({
      generation: 8,
      revision: 2,
      state: SNAPSHOT_TIMEOUT,
    });
    expect(fetchState).not.toHaveBeenCalled();
  });

  it('ignores malformed frames', () => {
    const { source, updates } = setup();
    source.start();
    FakeEventSource.latest().emit('snapshot', '{oops', '3');
    expect(updates).toHaveLength(0);
    expect(source.getSnapshot().state).toBeNull();
  });

  it('keeps the previous generation when a frame has no id', () => {
    const { source } = setup();
    source.start();
    const es = FakeEventSource.latest();
    es.snapshot(makeSnapshot(), 4);
    es.emit('snapshot', JSON.stringify(makeSnapshot()), '');
    expect(source.getSnapshot()).toMatchObject({ generation: 4, revision: 2 });
  });

  it('reconnects with exponential backoff', () => {
    const { source } = setup();
    source.start();
    const first = FakeEventSource.latest();
    first.snapshot(makeSnapshot(), 1);

    first.fail();
    expect(first.closed).toBe(true);
    expect(source.getSnapshot()).toMatchObject({
      status: 'offline',
      lastError: 'event stream error',
      nextRetryAt: Date.now() + 1000,
    });
    // Stale data is kept while offline.
    expect(source.getSnapshot().state).toEqual(makeSnapshot());

    vi.advanceTimersByTime(999);
    expect(FakeEventSource.instances).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(FakeEventSource.instances).toHaveLength(2);

    FakeEventSource.latest().fail();
    expect(source.getSnapshot().nextRetryAt).toBe(Date.now() + 2000);
    vi.advanceTimersByTime(2000);
    expect(FakeEventSource.instances).toHaveLength(3);

    // A snapshot resets the failure count: the next delay is the base again.
    FakeEventSource.latest().snapshot(makeSnapshot(), 2);
    expect(source.getSnapshot()).toMatchObject({
      status: 'live',
      lastError: null,
      nextRetryAt: null,
    });
    FakeEventSource.latest().fail();
    expect(source.getSnapshot().nextRetryAt).toBe(Date.now() + 1000);
  });

  it('stays "connecting" while it has never received data', () => {
    const { source } = setup();
    source.start();
    FakeEventSource.latest().fail();
    expect(source.getSnapshot().status).toBe('connecting');
  });

  it('ignores events from a replaced stream', () => {
    const { source } = setup();
    source.start();
    const first = FakeEventSource.latest();
    first.fail();
    vi.advanceTimersByTime(1000);
    first.snapshot(makeSnapshot(), 99);
    first.fail();
    expect(source.getSnapshot().state).toBeNull();
    expect(FakeEventSource.instances).toHaveLength(2);
  });

  it('treats a silent stream as an error, heartbeats keep it alive', () => {
    const { source } = setup({ staleAfterMs: 45_000 });
    source.start();
    const es = FakeEventSource.latest();
    es.open();
    es.snapshot(makeSnapshot(), 1);
    vi.advanceTimersByTime(30_000);
    es.heartbeat(1);
    vi.advanceTimersByTime(30_000);
    expect(es.closed).toBe(false);
    vi.advanceTimersByTime(15_000);
    expect(es.closed).toBe(true);
    expect(source.getSnapshot()).toMatchObject({
      status: 'offline',
      lastError: 'event stream went silent',
    });
  });

  it('reports a throwing EventSource constructor as a failure', () => {
    const { source } = setup({
      createEventSource: () => {
        throw new Error('blocked by CSP');
      },
    });
    source.start();
    expect(source.getSnapshot().lastError).toBe('blocked by CSP');
  });
});

describe('createLiveSource fallback to polling', () => {
  it('polls when EventSource is unsupported', async () => {
    const { source, fetchState } = setup({ createEventSource: null });
    source.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchState).toHaveBeenCalledTimes(1);
    expect(source.getSnapshot()).toMatchObject({
      status: 'polling',
      generation: null,
      revision: 1,
    });

    await vi.advanceTimersByTimeAsync(5000);
    expect(fetchState).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(5000);
    expect(fetchState).toHaveBeenCalledTimes(3);
  });

  it('switches to polling after repeated SSE failures', async () => {
    const { source, fetchState } = setup({ maxSseFailures: 3 });
    source.start();
    FakeEventSource.latest().fail(); // 1 -> retry in 1 s
    vi.advanceTimersByTime(1000);
    FakeEventSource.latest().fail(); // 2 -> retry in 2 s
    vi.advanceTimersByTime(2000);
    expect(fetchState).not.toHaveBeenCalled();
    FakeEventSource.latest().fail(); // 3 -> polling
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchState).toHaveBeenCalledTimes(1);
    expect(source.getSnapshot().status).toBe('polling');
    expect(FakeEventSource.instances).toHaveLength(3);
  });

  it('backs off failed polls and recovers', async () => {
    const { source, fetchState } = setup({ createEventSource: null });
    fetchState
      .mockRejectedValueOnce(new Error('down'))
      .mockRejectedValueOnce(new Error('still down'));
    source.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(source.getSnapshot()).toMatchObject({
      status: 'offline',
      lastError: 'down',
      nextRetryAt: Date.now() + 5000,
    });
    await vi.advanceTimersByTimeAsync(5000);
    expect(source.getSnapshot()).toMatchObject({
      lastError: 'still down',
      nextRetryAt: Date.now() + 10_000,
    });
    await vi.advanceTimersByTimeAsync(10_000);
    expect(fetchState).toHaveBeenCalledTimes(3);
    expect(source.getSnapshot()).toMatchObject({ status: 'polling', lastError: null });
  });

  it('re-probes SSE while polling and goes live again', async () => {
    const { source, fetchState } = setup({ maxSseFailures: 1, sseProbeIntervalMs: 60_000 });
    source.start();
    FakeEventSource.latest().fail();
    await vi.advanceTimersByTimeAsync(0);
    expect(source.getSnapshot().status).toBe('polling');

    // First probe fails: polling continues and another probe is scheduled.
    await vi.advanceTimersByTimeAsync(60_000);
    expect(FakeEventSource.instances).toHaveLength(2);
    FakeEventSource.latest().fail();
    expect(source.getSnapshot().status).toBe('polling');

    await vi.advanceTimersByTimeAsync(60_000);
    expect(FakeEventSource.instances).toHaveLength(3);
    FakeEventSource.latest().snapshot(makeSnapshot(), 12);
    expect(source.getSnapshot()).toMatchObject({ status: 'live', generation: 12 });

    const calls = fetchState.mock.calls.length;
    await vi.advanceTimersByTimeAsync(20_000);
    expect(fetchState).toHaveBeenCalledTimes(calls);
  });

  it('discards a poll that resolves after SSE took over', async () => {
    let resolvePoll: (payload: StatePayload) => void = () => undefined;
    const { source, fetchState } = setup({ maxSseFailures: 1, sseProbeIntervalMs: 1000 });
    source.start();
    fetchState.mockImplementation(
      () =>
        new Promise<StatePayload>((resolve) => {
          resolvePoll = resolve;
        }),
    );
    FakeEventSource.latest().fail();
    await vi.advanceTimersByTimeAsync(1000);
    FakeEventSource.latest().snapshot(makeSnapshot(), 5);
    resolvePoll(makeEmptySnapshot());
    await vi.advanceTimersByTimeAsync(0);
    expect(source.getSnapshot()).toMatchObject({ status: 'live', state: makeSnapshot() });
  });
});

describe('createLiveSource lifecycle', () => {
  it('stop() closes the stream and cancels timers', async () => {
    const { source, fetchState, updates } = setup({ createEventSource: null });
    source.start();
    await vi.advanceTimersByTimeAsync(0);
    source.stop();
    const count = updates.length;
    await vi.advanceTimersByTimeAsync(60_000);
    expect(fetchState).toHaveBeenCalledTimes(1);
    expect(updates).toHaveLength(count);

    const live = setup();
    live.source.start();
    const es = FakeEventSource.latest();
    live.source.stop();
    expect(es.closed).toBe(true);
    es.fail();
    vi.advanceTimersByTime(10_000);
    expect(FakeEventSource.instances).toHaveLength(1);
  });

  it('start() is idempotent', () => {
    const { source } = setup();
    source.start();
    source.start();
    expect(FakeEventSource.instances).toHaveLength(1);
  });

  it('refresh() fetches once and keeps the connection status', async () => {
    const { source, fetchState } = setup();
    source.start();
    FakeEventSource.latest().snapshot(makeEmptySnapshot(), 1);
    fetchState.mockResolvedValueOnce(makeSnapshot());
    await source.refresh();
    expect(source.getSnapshot()).toMatchObject({
      status: 'live',
      state: makeSnapshot(),
      revision: 2,
    });

    fetchState.mockRejectedValueOnce(new Error('nope'));
    await source.refresh();
    expect(source.getSnapshot().lastError).toBe('nope');
  });

  it('refresh() does not overwrite a newer SSE payload', async () => {
    const { source, fetchState } = setup();
    source.start();
    let resolve: (payload: StatePayload) => void = () => undefined;
    fetchState.mockImplementationOnce(
      () =>
        new Promise<StatePayload>((done) => {
          resolve = done;
        }),
    );
    const pending = source.refresh();
    FakeEventSource.latest().snapshot(makeSnapshot(), 2);
    resolve(makeEmptySnapshot());
    await pending;
    expect(source.getSnapshot().state).toEqual(makeSnapshot());
  });

  it('unsubscribe stops notifications', () => {
    const { source } = setup();
    const listener = vi.fn();
    const unsubscribe = source.subscribe(listener);
    source.start();
    unsubscribe();
    FakeEventSource.latest().snapshot(makeSnapshot(), 1);
    expect(listener).not.toHaveBeenCalled();
  });
});
