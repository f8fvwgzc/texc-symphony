import { useEffect, useState } from 'preact/hooks';

import { isRunStatus, type RunStatus } from '../api/types';

/**
 * Hash routes (the Rust server only serves `/` and bundle files, so no SPA fallback is needed):
 *   #/                      overview
 *   #/issues/<identifier>   overview with the issue drawer open
 *   #/runs?status=&issue=&before=   run history
 *   #/runs/<id>             one run + its events timeline
 */
export interface RunsQuery {
  status?: RunStatus;
  issue?: string;
  before?: number;
}

export type Route =
  | { name: 'overview'; issue?: string }
  | { name: 'runs'; query: RunsQuery }
  | { name: 'run'; id: number }
  | { name: 'not-found'; path: string };

function positiveInt(text: string | null | undefined): number | undefined {
  if (text == null || !/^\d+$/.test(text)) return undefined;
  const value = Number.parseInt(text, 10);
  return value > 0 ? value : undefined;
}

export function parseRoute(hash: string): Route {
  const raw = hash.replace(/^#/, '');
  const [pathPart = '', queryPart = ''] = raw.split('?', 2);
  const path = pathPart === '' ? '/' : pathPart;
  const segments = path.split('/').filter((segment) => segment !== '');
  const params = new URLSearchParams(queryPart);

  if (segments.length === 0) return { name: 'overview' };
  if (segments[0] === 'issues' && segments.length === 2 && segments[1] !== undefined) {
    return { name: 'overview', issue: decodeURIComponent(segments[1]) };
  }
  if (segments[0] === 'runs' && segments.length === 1) {
    const query: RunsQuery = {};
    const status = params.get('status');
    if (isRunStatus(status)) query.status = status;
    const issue = params.get('issue');
    if (issue !== null && issue !== '') query.issue = issue;
    const before = positiveInt(params.get('before'));
    if (before !== undefined) query.before = before;
    return { name: 'runs', query };
  }
  if (segments[0] === 'runs' && segments.length === 2) {
    const id = positiveInt(segments[1]);
    if (id !== undefined) return { name: 'run', id };
  }
  return { name: 'not-found', path };
}

export function formatRoute(route: Route): string {
  if (route.name === 'overview') {
    return route.issue === undefined ? '#/' : `#/issues/${encodeURIComponent(route.issue)}`;
  }
  if (route.name === 'runs') {
    const params = new URLSearchParams();
    if (route.query.status !== undefined) params.set('status', route.query.status);
    if (route.query.issue !== undefined) params.set('issue', route.query.issue);
    if (route.query.before !== undefined) params.set('before', String(route.query.before));
    const text = params.toString();
    return text === '' ? '#/runs' : `#/runs?${text}`;
  }
  if (route.name === 'run') return `#/runs/${route.id}`;
  return `#${route.path}`;
}

export function navigate(route: Route): void {
  const next = formatRoute(route);
  if (window.location.hash !== next) window.location.hash = next;
}

export function useHashRoute(): Route {
  const [route, setRoute] = useState(() => parseRoute(window.location.hash));
  useEffect(() => {
    const onChange = () => setRoute(parseRoute(window.location.hash));
    window.addEventListener('hashchange', onChange);
    return () => window.removeEventListener('hashchange', onChange);
  }, []);
  return route;
}
