import { vi } from 'vitest';

import type { ApiClient } from '../api/client';
import { createTokenStore, memoryStorage } from '../api/token';
import { makeSnapshot, TOTALS } from './fixtures';

/** An `ApiClient` whose methods are `vi.fn()` with sensible defaults. */
export function makeFakeApi(overrides: Partial<ApiClient> = {}) {
  let tickets = 0;
  const api = {
    getState: vi.fn<ApiClient['getState']>(() => Promise.resolve(makeSnapshot())),
    requestRefresh: vi.fn<ApiClient['requestRefresh']>(() =>
      Promise.resolve({
        queued: true as const,
        coalesced: false,
        requested_at: '2026-02-24T20:15:30.123456Z',
        operations: ['poll', 'reconcile'],
      }),
    ),
    getIssue: vi.fn<ApiClient['getIssue']>(() => Promise.reject(new Error('not stubbed'))),
    getHealth: vi.fn<ApiClient['getHealth']>(() =>
      Promise.resolve({
        status: 'ok' as const,
        version: '0.1.0',
        uptime_seconds: 5,
        store: 'sqlite' as const,
        auth_required: false,
      }),
    ),
    listRuns: vi.fn<ApiClient['listRuns']>(() =>
      Promise.resolve({ runs: [], next_before_id: null }),
    ),
    getRun: vi.fn<ApiClient['getRun']>(() => Promise.reject(new Error('not stubbed'))),
    listRunEvents: vi.fn<ApiClient['listRunEvents']>(() => Promise.resolve({ events: [] })),
    getTotals: vi.fn<ApiClient['getTotals']>(() => Promise.resolve(TOTALS)),
    createStreamTicket: vi.fn<ApiClient['createStreamTicket']>(() => {
      tickets += 1;
      return Promise.resolve({
        ticket: `ticket-${tickets}`,
        expires_at: '2026-02-24T20:16:30Z',
        expires_in_ms: 60_000,
      });
    }),
    eventsUrl: vi.fn<ApiClient['eventsUrl']>((ticket) =>
      ticket === undefined ? '/api/v1/events' : `/api/v1/events?ticket=${ticket}`,
    ),
    auth: createTokenStore(memoryStorage(), memoryStorage()),
    ...overrides,
  };
  return api satisfies ApiClient;
}
