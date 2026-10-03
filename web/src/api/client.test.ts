import { describe, expect, it, vi } from 'vitest';

import { makeSnapshot } from '../test/fixtures';
import {
  ApiError,
  buildQuery,
  createApiClient,
  describeError,
  isStoreDisabled,
  NetworkError,
} from './client';

function jsonResponse(status: number, body: unknown, statusText = ''): Response {
  return new Response(body === undefined ? '' : JSON.stringify(body), {
    status,
    statusText,
    headers: { 'content-type': 'application/json' },
  });
}

function clientWith(...responses: (Response | Error)[]) {
  const fetchMock = vi.fn<typeof fetch>();
  for (const response of responses) {
    if (response instanceof Error) fetchMock.mockRejectedValueOnce(response);
    else fetchMock.mockResolvedValueOnce(response);
  }
  return { client: createApiClient({ baseUrl: 'http://host:4000/', fetch: fetchMock }), fetchMock };
}

function urlOf(input: RequestInfo | URL | undefined): string {
  if (input === undefined) return '';
  if (typeof input === 'string') return input;
  return input instanceof URL ? input.href : input.url;
}

function calledUrl(fetchMock: ReturnType<typeof vi.fn<typeof fetch>>, index = 0): string {
  return urlOf(fetchMock.mock.calls[index]?.[0]);
}

describe('buildQuery', () => {
  it('skips undefined and empty values', () => {
    expect(buildQuery({})).toBe('');
    expect(buildQuery({ a: undefined, b: '' })).toBe('');
    expect(buildQuery({ limit: 5, issue: 'MT 1' })).toBe('?limit=5&issue=MT+1');
  });
});

describe('createApiClient', () => {
  it('GETs the state (including in-band snapshot errors)', async () => {
    const snapshot = makeSnapshot();
    const { client, fetchMock } = clientWith(
      jsonResponse(200, snapshot),
      jsonResponse(200, {
        generated_at: 'x',
        error: { code: 'snapshot_timeout', message: 'Snapshot timed out' },
      }),
    );
    await expect(client.getState()).resolves.toEqual(snapshot);
    expect(calledUrl(fetchMock)).toBe('http://host:4000/api/v1/state');
    expect(fetchMock.mock.calls[0]?.[1]?.method).toBe('GET');
    await expect(client.getState()).resolves.toMatchObject({ error: { code: 'snapshot_timeout' } });
  });

  it('POSTs refresh and surfaces 503 as ApiError', async () => {
    const { client, fetchMock } = clientWith(
      jsonResponse(202, { queued: true, coalesced: true, requested_at: 't', operations: ['poll'] }),
      jsonResponse(503, {
        error: { code: 'orchestrator_unavailable', message: 'Orchestrator is unavailable' },
      }),
    );
    await expect(client.requestRefresh()).resolves.toMatchObject({ coalesced: true });
    expect(fetchMock.mock.calls[0]?.[1]?.method).toBe('POST');
    const error = await client.requestRefresh().catch((reason: unknown) => reason);
    expect(error).toBeInstanceOf(ApiError);
    expect(error).toMatchObject({ status: 503, code: 'orchestrator_unavailable' });
    expect(describeError(error)).toBe('orchestrator_unavailable: Orchestrator is unavailable');
  });

  it('URL-encodes issue identifiers and maps 404', async () => {
    const { client, fetchMock } = clientWith(
      jsonResponse(404, { error: { code: 'issue_not_found', message: 'Issue not found' } }),
    );
    await expect(client.getIssue('MT/1 x')).rejects.toMatchObject({
      code: 'issue_not_found',
      status: 404,
    });
    expect(calledUrl(fetchMock)).toBe('http://host:4000/api/v1/MT%2F1%20x');
  });

  it('builds run history URLs', async () => {
    const { client, fetchMock } = clientWith(
      jsonResponse(200, { runs: [], next_before_id: null }),
      jsonResponse(200, { events: [] }),
      jsonResponse(200, { id: 3 }),
      jsonResponse(200, { runs_total: 0 }),
      jsonResponse(200, { status: 'ok' }),
    );
    await client.listRuns({ limit: 10, before_id: 40, issue: 'MT-1', status: 'failed' });
    await client.listRunEvents(3, { after_seq: 5, limit: 100 });
    await client.getRun(3);
    await client.getTotals();
    await client.getHealth();
    expect(fetchMock.mock.calls.map((call) => urlOf(call[0]))).toEqual([
      'http://host:4000/api/v1/runs?limit=10&before_id=40&issue=MT-1&status=failed',
      'http://host:4000/api/v1/runs/3/events?after_seq=5&limit=100',
      'http://host:4000/api/v1/runs/3',
      'http://host:4000/api/v1/totals',
      'http://host:4000/api/v1/health',
    ]);
  });

  it('flags store_disabled', async () => {
    const { client } = clientWith(
      jsonResponse(503, {
        error: { code: 'store_disabled', message: 'Run history store is disabled' },
      }),
    );
    const error = await client.listRuns().catch((reason: unknown) => reason);
    expect(isStoreDisabled(error)).toBe(true);
    expect(isStoreDisabled(new Error('x'))).toBe(false);
  });

  it('falls back to request_failed for non-envelope errors', async () => {
    const { client } = clientWith(
      new Response('<html>oops</html>', { status: 502, statusText: 'Bad Gateway' }),
      new Response('', { status: 500 }),
    );
    await expect(client.getHealth()).rejects.toMatchObject({
      code: 'request_failed',
      message: 'Bad Gateway',
    });
    await expect(client.getHealth()).rejects.toMatchObject({
      code: 'request_failed',
      message: 'HTTP 500',
    });
  });

  it('rejects invalid JSON on success', async () => {
    const { client } = clientWith(new Response('not json', { status: 200 }));
    await expect(client.getState()).rejects.toMatchObject({ code: 'invalid_response' });
  });

  it('wraps transport failures in NetworkError but lets aborts through', async () => {
    const abort = new DOMException('aborted', 'AbortError');
    const { client } = clientWith(new TypeError('Failed to fetch'), abort);
    const error = await client.getState().catch((reason: unknown) => reason);
    expect(error).toBeInstanceOf(NetworkError);
    expect(describeError(error)).toBe('Request to /api/v1/state failed');
    await expect(client.getState()).rejects.toBe(abort);
  });

  it('passes the abort signal and exposes the events URL', async () => {
    const { client, fetchMock } = clientWith(jsonResponse(200, makeSnapshot()));
    const controller = new AbortController();
    await client.getState({ signal: controller.signal });
    expect(fetchMock.mock.calls[0]?.[1]?.signal).toBe(controller.signal);
    expect(client.eventsUrl()).toBe('http://host:4000/api/v1/events');
    expect(createApiClient().eventsUrl()).toBe('/api/v1/events');
  });

  it('describes arbitrary errors', () => {
    expect(describeError('plain')).toBe('plain');
    expect(describeError(new Error('boom'))).toBe('boom');
  });
});
