/**
 * Framework-agnostic live connection to the orchestrator state.
 *
 * Strategy:
 *  1. Open an `EventSource` on `/api/v1/events`. Every `snapshot` event replaces the state.
 *  2. On an SSE error the stream is closed and reopened with exponential backoff + jitter
 *     (`reconnectBaseMs` doubling up to `reconnectMaxMs`). A stream that stays silent for
 *     `staleAfterMs` (no snapshot and no heartbeat) counts as an error.
 *  3. After `maxSseFailures` consecutive failures — or when `EventSource` is unsupported —
 *     fall back to polling `GET /api/v1/state` every `pollIntervalMs`. Failed polls back off
 *     exponentially too. While polling, SSE is re-probed every `sseProbeIntervalMs` and the
 *     source switches back to live as soon as a probe delivers a snapshot.
 *
 * Status: `connecting` (nothing received yet) -> `live` (SSE) | `polling` (fallback) |
 * `offline` (last attempt failed; retrying at `nextRetryAt`).
 */
import type { StatePayload } from '../api/types';

export type ConnectionStatus = 'connecting' | 'live' | 'polling' | 'offline';

/** The subset of `EventSource` this module uses (lets tests inject a fake). */
export interface EventSourceLike {
  /** `open`, `error` and named SSE events (`snapshot`, `heartbeat`). */
  addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void;
  close(): void;
}

export type EventSourceFactory = (url: string) => EventSourceLike;

export interface LiveSnapshot {
  status: ConnectionStatus;
  /** Last payload received (kept while offline so the UI can show stale data). */
  state: StatePayload | null;
  /** SSE `id:` of the last snapshot (orchestrator generation); null when polling. */
  generation: number | null;
  /** Increments on every applied payload (SSE or poll); handy as an effect dependency. */
  revision: number;
  /** `Date.now()` when the last payload arrived. */
  updatedAt: number | null;
  /** Description of the last connection failure (null once healthy again). */
  lastError: string | null;
  /** `Date.now()`-based time of the next reconnect / poll attempt while offline. */
  nextRetryAt: number | null;
}

export interface LiveSourceOptions {
  url: string;
  fetchState: () => Promise<StatePayload>;
  /** `null` = SSE unsupported (poll only). */
  createEventSource: EventSourceFactory | null;
  pollIntervalMs?: number;
  maxSseFailures?: number;
  reconnectBaseMs?: number;
  reconnectMaxMs?: number;
  sseProbeIntervalMs?: number;
  staleAfterMs?: number;
  /** `[0, 1)`; used for ±20 % jitter. Inject `() => 0.5` for deterministic tests. */
  random?: () => number;
  now?: () => number;
}

export interface LiveSource {
  start(): void;
  stop(): void;
  /** Fetch `/api/v1/state` once now (e.g. after `POST /refresh`) regardless of mode. */
  refresh(): Promise<void>;
  getSnapshot(): LiveSnapshot;
  subscribe(listener: (snapshot: LiveSnapshot) => void): () => void;
}

export const LIVE_DEFAULTS = {
  pollIntervalMs: 5_000,
  maxSseFailures: 3,
  reconnectBaseMs: 1_000,
  reconnectMaxMs: 30_000,
  sseProbeIntervalMs: 60_000,
  staleAfterMs: 45_000,
} as const;

/** `base * 2^(attempt-1)` capped at `max`, then ±20 % jitter. `attempt` starts at 1. */
export function backoffDelay(
  attempt: number,
  baseMs: number,
  maxMs: number,
  random: () => number = Math.random,
): number {
  const exponential = Math.min(baseMs * 2 ** Math.max(attempt - 1, 0), maxMs);
  const jitter = 0.8 + 0.4 * random();
  return Math.round(exponential * jitter);
}

function clear(timer: ReturnType<typeof setTimeout> | undefined): undefined {
  if (timer !== undefined) clearTimeout(timer);
  return undefined;
}

/** Minimal structural check of an SSE `snapshot` payload. */
export function parseStatePayload(data: string): StatePayload | null {
  let value: unknown;
  try {
    value = JSON.parse(data);
  } catch {
    return null;
  }
  if (typeof value !== 'object' || value === null || !('generated_at' in value)) return null;
  if (typeof value.generated_at !== 'string') return null;
  if (!('error' in value) && !('counts' in value && 'running' in value)) return null;
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- shape checked above
  return value as StatePayload;
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function createLiveSource(options: LiveSourceOptions): LiveSource {
  const cfg = {
    url: options.url,
    pollIntervalMs: options.pollIntervalMs ?? LIVE_DEFAULTS.pollIntervalMs,
    maxSseFailures: options.maxSseFailures ?? LIVE_DEFAULTS.maxSseFailures,
    reconnectBaseMs: options.reconnectBaseMs ?? LIVE_DEFAULTS.reconnectBaseMs,
    reconnectMaxMs: options.reconnectMaxMs ?? LIVE_DEFAULTS.reconnectMaxMs,
    sseProbeIntervalMs: options.sseProbeIntervalMs ?? LIVE_DEFAULTS.sseProbeIntervalMs,
    staleAfterMs: options.staleAfterMs ?? LIVE_DEFAULTS.staleAfterMs,
  };
  const random = options.random ?? Math.random;
  const now = options.now ?? Date.now;

  let snapshot: LiveSnapshot = {
    status: 'connecting',
    state: null,
    generation: null,
    revision: 0,
    updatedAt: null,
    lastError: null,
    nextRetryAt: null,
  };
  const listeners = new Set<(snapshot: LiveSnapshot) => void>();

  let mode: 'idle' | 'sse' | 'poll' | 'stopped' = 'idle';
  // Read through a function so TypeScript does not narrow `mode` across `await`.
  const stopped = () => mode === 'stopped';
  let source: EventSourceLike | null = null;
  let sseFailures = 0;
  let pollFailures = 0;
  // Bumped whenever an in-flight poll result must be ignored (mode change / stop).
  let pollEpoch = 0;
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  let pollTimer: ReturnType<typeof setTimeout> | undefined;
  let probeTimer: ReturnType<typeof setTimeout> | undefined;
  let staleTimer: ReturnType<typeof setTimeout> | undefined;

  function update(patch: Partial<LiveSnapshot>): void {
    snapshot = { ...snapshot, ...patch };
    for (const listener of listeners) listener(snapshot);
  }

  function applyPayload(state: StatePayload, status: ConnectionStatus, generation: number | null) {
    update({
      status,
      state,
      generation,
      revision: snapshot.revision + 1,
      updatedAt: now(),
      lastError: null,
      nextRetryAt: null,
    });
  }

  function closeSource(): void {
    staleTimer = clear(staleTimer);
    if (source) {
      // Listeners of a replaced source are ignored via the `source !== es` guards.
      source.close();
      source = null;
    }
  }

  function armStaleTimer(): void {
    staleTimer = clear(staleTimer);
    staleTimer = setTimeout(() => onSseError('event stream went silent'), cfg.staleAfterMs);
  }

  // ---- SSE ---------------------------------------------------------------

  function openSse(): void {
    const factory = options.createEventSource;
    if (factory === null || mode === 'stopped') return;
    closeSource();
    let es: EventSourceLike;
    try {
      es = factory(cfg.url);
    } catch (error) {
      onSseError(describe(error));
      return;
    }
    source = es;
    es.addEventListener('open', () => {
      if (source === es) armStaleTimer();
    });
    es.addEventListener('error', () => {
      if (source === es) onSseError('event stream error');
    });
    es.addEventListener('snapshot', (event) => {
      if (source !== es) return;
      const payload = parseStatePayload(event.data);
      if (payload === null) return; // Malformed frame: ignore, the next one replaces it.
      armStaleTimer();
      sseFailures = 0;
      if (mode === 'poll') {
        // A probe succeeded: leave polling mode.
        pollEpoch += 1;
        pollTimer = clear(pollTimer);
        probeTimer = clear(probeTimer);
        pollFailures = 0;
      }
      mode = 'sse';
      const id = Number.parseInt(event.lastEventId, 10);
      applyPayload(payload, 'live', Number.isNaN(id) ? snapshot.generation : id);
    });
    es.addEventListener('heartbeat', () => {
      if (source === es) armStaleTimer();
    });
    armStaleTimer();
  }

  function onSseError(reason: string): void {
    if (mode === 'stopped') return;
    closeSource();
    sseFailures += 1;

    if (mode === 'poll') {
      // A failed probe: keep polling, try again later.
      scheduleProbe();
      return;
    }
    if (sseFailures >= cfg.maxSseFailures) {
      update({ lastError: reason });
      startPolling();
      return;
    }
    const delay = backoffDelay(sseFailures, cfg.reconnectBaseMs, cfg.reconnectMaxMs, random);
    update({
      status: snapshot.state === null ? 'connecting' : 'offline',
      lastError: reason,
      nextRetryAt: now() + delay,
    });
    reconnectTimer = clear(reconnectTimer);
    reconnectTimer = setTimeout(openSse, delay);
  }

  function scheduleProbe(): void {
    probeTimer = clear(probeTimer);
    if (options.createEventSource === null || mode !== 'poll') return;
    probeTimer = setTimeout(openSse, cfg.sseProbeIntervalMs);
  }

  // ---- Polling -----------------------------------------------------------

  function startPolling(): void {
    mode = 'poll';
    pollFailures = 0;
    reconnectTimer = clear(reconnectTimer);
    void poll();
    scheduleProbe();
  }

  async function poll(): Promise<void> {
    pollTimer = clear(pollTimer);
    const epoch = pollEpoch;
    try {
      const payload = await options.fetchState();
      if (epoch !== pollEpoch || mode !== 'poll') return;
      pollFailures = 0;
      applyPayload(payload, 'polling', null);
      pollTimer = setTimeout(() => void poll(), cfg.pollIntervalMs);
    } catch (error) {
      if (epoch !== pollEpoch || mode !== 'poll') return;
      pollFailures += 1;
      const delay = backoffDelay(
        pollFailures,
        cfg.pollIntervalMs,
        Math.max(cfg.reconnectMaxMs, cfg.pollIntervalMs),
        random,
      );
      update({ status: 'offline', lastError: describe(error), nextRetryAt: now() + delay });
      pollTimer = setTimeout(() => void poll(), delay);
    }
  }

  // ---- Public API ----------------------------------------------------------

  return {
    start() {
      if (mode !== 'idle') return;
      if (options.createEventSource === null) {
        startPolling();
      } else {
        mode = 'sse';
        openSse();
      }
    },
    stop() {
      mode = 'stopped';
      pollEpoch += 1;
      closeSource();
      reconnectTimer = clear(reconnectTimer);
      pollTimer = clear(pollTimer);
      probeTimer = clear(probeTimer);
      listeners.clear();
    },
    async refresh() {
      if (mode === 'stopped') return;
      const revision = snapshot.revision;
      try {
        const payload = await options.fetchState();
        // Do not overwrite a newer payload that arrived meanwhile; keep the connection status.
        if (stopped() || snapshot.revision !== revision) return;
        update({ state: payload, revision: revision + 1, updatedAt: now() });
      } catch (error) {
        if (!stopped()) update({ lastError: describe(error) });
      }
    },
    getSnapshot: () => snapshot,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}
