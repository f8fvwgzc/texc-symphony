import type {
  ErrorBody,
  Health,
  IssueDetail,
  ListRunEventsQuery,
  ListRunsQuery,
  RefreshAccepted,
  RunEventList,
  RunList,
  RunRecord,
  StatePayload,
  Totals,
} from './types';

/** Path of the SSE stream (see `streamEvents` in the OpenAPI document). */
export const EVENTS_PATH = '/api/v1/events';

/** A non-2xx response (with the server's `{error: {code, message}}` envelope when present). */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

/** The request never produced an HTTP response (offline, DNS, CORS, aborted...). */
export class NetworkError extends Error {
  constructor(message: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = 'NetworkError';
  }
}

export interface ApiClientOptions {
  /** Prefix for every path; `''` = same origin (the embedded dashboard and the Vite proxy). */
  baseUrl?: string;
  /** Injected for tests; defaults to the global `fetch`. */
  fetch?: typeof fetch;
}

export interface RequestOptions {
  signal?: AbortSignal;
}

/** Typed wrapper over every JSON endpoint of `docs/api/openapi.yaml`. */
export interface ApiClient {
  getState(options?: RequestOptions): Promise<StatePayload>;
  requestRefresh(options?: RequestOptions): Promise<RefreshAccepted>;
  getIssue(identifier: string, options?: RequestOptions): Promise<IssueDetail>;
  getHealth(options?: RequestOptions): Promise<Health>;
  listRuns(query?: ListRunsQuery, options?: RequestOptions): Promise<RunList>;
  getRun(id: number, options?: RequestOptions): Promise<RunRecord>;
  listRunEvents(
    id: number,
    query?: ListRunEventsQuery,
    options?: RequestOptions,
  ): Promise<RunEventList>;
  getTotals(options?: RequestOptions): Promise<Totals>;
  /** Absolute or same-origin URL for an `EventSource`. */
  eventsUrl(): string;
}

type QueryValue = string | number | undefined;

/** Builds `?a=1&b=x`, skipping `undefined` values; returns `''` when nothing is set. */
export function buildQuery(params: Record<string, QueryValue>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== '') search.set(key, String(value));
  }
  const text = search.toString();
  return text === '' ? '' : `?${text}`;
}

function isErrorBody(value: unknown): value is { error: ErrorBody } {
  if (typeof value !== 'object' || value === null || !('error' in value)) return false;
  const error: unknown = value.error;
  return (
    typeof error === 'object' &&
    error !== null &&
    'code' in error &&
    typeof error.code === 'string' &&
    'message' in error &&
    typeof error.message === 'string'
  );
}

async function readJson(response: Response): Promise<unknown> {
  const text = await response.text();
  if (text === '') return null;
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

export function createApiClient(options: ApiClientOptions = {}): ApiClient {
  const baseUrl = (options.baseUrl ?? '').replace(/\/+$/, '');
  const doFetch = options.fetch ?? ((input, init) => globalThis.fetch(input, init));

  async function request<T>(
    method: 'GET' | 'POST',
    path: string,
    opts?: RequestOptions,
  ): Promise<T> {
    let response: Response;
    try {
      const init: RequestInit = { method, headers: { accept: 'application/json' } };
      if (opts?.signal) init.signal = opts.signal;
      response = await doFetch(`${baseUrl}${path}`, init);
    } catch (cause) {
      if (cause instanceof DOMException && cause.name === 'AbortError') throw cause;
      throw new NetworkError(`Request to ${path} failed`, { cause });
    }

    const body = await readJson(response);
    if (!response.ok) {
      if (isErrorBody(body))
        throw new ApiError(response.status, body.error.code, body.error.message);
      throw new ApiError(
        response.status,
        'request_failed',
        response.statusText === '' ? `HTTP ${response.status}` : response.statusText,
      );
    }
    if (body === undefined) {
      throw new ApiError(response.status, 'invalid_response', `Invalid JSON from ${path}`);
    }
    // The server is trusted to follow docs/api/openapi.yaml; no runtime schema validation.
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion
    return body as T;
  }

  return {
    getState: (opts) => request<StatePayload>('GET', '/api/v1/state', opts),
    requestRefresh: (opts) => request<RefreshAccepted>('POST', '/api/v1/refresh', opts),
    getIssue: (identifier, opts) =>
      request<IssueDetail>('GET', `/api/v1/${encodeURIComponent(identifier)}`, opts),
    getHealth: (opts) => request<Health>('GET', '/api/v1/health', opts),
    listRuns: (query = {}, opts) =>
      request<RunList>(
        'GET',
        `/api/v1/runs${buildQuery({
          limit: query.limit,
          before_id: query.before_id,
          issue: query.issue,
          status: query.status,
        })}`,
        opts,
      ),
    getRun: (id, opts) => request<RunRecord>('GET', `/api/v1/runs/${id}`, opts),
    listRunEvents: (id, query = {}, opts) =>
      request<RunEventList>(
        'GET',
        `/api/v1/runs/${id}/events${buildQuery({ after_seq: query.after_seq, limit: query.limit })}`,
        opts,
      ),
    getTotals: (opts) => request<Totals>('GET', '/api/v1/totals', opts),
    eventsUrl: () => `${baseUrl}${EVENTS_PATH}`,
  };
}

/** True when `error` is the 503 returned by history endpoints while persistence is off. */
export function isStoreDisabled(error: unknown): boolean {
  return error instanceof ApiError && error.code === 'store_disabled';
}

/** Human-readable text for any thrown value. */
export function describeError(error: unknown): string {
  if (error instanceof ApiError) return `${error.code}: ${error.message}`;
  if (error instanceof Error) return error.message;
  return String(error);
}
