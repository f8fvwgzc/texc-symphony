import { useCallback, useEffect, useRef, useState } from 'preact/hooks';

import { useApi } from '../../api/context';
import type { RunEvent } from '../../api/types';

export const EVENTS_PAGE_SIZE = 200;
/** While a run is still `running`, new events are fetched this often. */
export const RUN_POLL_MS = 5_000;

/** Incrementally loaded event list for one run (`after_seq` paging + live polling). */
export function useRunEvents(id: number, live: boolean) {
  const api = useApi();
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [error, setError] = useState<unknown>(undefined);
  const [loading, setLoading] = useState(true);
  const lastSeq = useRef(0);
  const inFlight = useRef(false);

  const loadNext = useCallback(async () => {
    if (inFlight.current) return;
    inFlight.current = true;
    setLoading(true);
    try {
      const page = await api.listRunEvents(id, {
        after_seq: lastSeq.current,
        limit: EVENTS_PAGE_SIZE,
      });
      const fresh = page.events.filter((event) => event.seq > lastSeq.current);
      const last = fresh[fresh.length - 1];
      if (last !== undefined) lastSeq.current = last.seq;
      if (fresh.length > 0) setEvents((current) => [...current, ...fresh]);
      setHasMore(page.events.length >= EVENTS_PAGE_SIZE);
      setError(undefined);
    } catch (reason) {
      setError(reason);
    } finally {
      inFlight.current = false;
      setLoading(false);
    }
  }, [api, id]);

  useEffect(() => {
    lastSeq.current = 0;
    setEvents([]);
    void loadNext();
  }, [loadNext]);

  useEffect(() => {
    if (!live) return undefined;
    const timer = setInterval(() => void loadNext(), RUN_POLL_MS);
    return () => clearInterval(timer);
  }, [live, loadNext]);

  return { events, hasMore, error, loading, loadNext };
}
