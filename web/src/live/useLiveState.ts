import { useEffect, useMemo, useState } from 'preact/hooks';

import type { ApiClient } from '../api/client';
import {
  createLiveSource,
  type EventSourceFactory,
  type LiveSnapshot,
  type LiveSourceOptions,
} from './liveSource';

export type LiveTuning = Omit<LiveSourceOptions, 'url' | 'fetchState' | 'createEventSource'>;

export interface UseLiveStateOptions extends LiveTuning {
  /** Override SSE support detection (tests pass a fake; `null` forces polling). */
  createEventSource?: EventSourceFactory | null;
}

export interface LiveState extends LiveSnapshot {
  /** Re-fetch `/api/v1/state` immediately. */
  refresh: () => Promise<void>;
}

/** Uses the browser `EventSource` when available, otherwise `null` (poll only). */
export function defaultEventSourceFactory(): EventSourceFactory | null {
  if (typeof globalThis.EventSource !== 'function') return null;
  return (url) => new globalThis.EventSource(url);
}

/**
 * Live orchestrator state: SSE on `/api/v1/events`, falling back to polling `/api/v1/state`.
 * The connection is created on mount and torn down on unmount.
 */
export function useLiveState(client: ApiClient, options: UseLiveStateOptions = {}): LiveState {
  const source = useMemo(
    () =>
      createLiveSource({
        ...options,
        url: client.eventsUrl(),
        fetchState: () => client.getState(),
        createEventSource:
          options.createEventSource === undefined
            ? defaultEventSourceFactory()
            : options.createEventSource,
      }),
    // The source is created once per client; tuning options are read at creation time.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [client],
  );
  const [snapshot, setSnapshot] = useState<LiveSnapshot>(() => source.getSnapshot());

  useEffect(() => {
    const unsubscribe = source.subscribe(setSnapshot);
    source.start();
    setSnapshot(source.getSnapshot());
    return () => {
      unsubscribe();
      source.stop();
    };
  }, [source]);

  return useMemo(() => ({ ...snapshot, refresh: () => source.refresh() }), [snapshot, source]);
}
