import { describe, expect, it } from 'vitest';

import { formatRoute, parseRoute, type Route } from './router';

describe('router', () => {
  it('parses the supported hash routes', () => {
    expect(parseRoute('')).toEqual({ name: 'overview' });
    expect(parseRoute('#/')).toEqual({ name: 'overview' });
    expect(parseRoute('#/issues/MT%2F1')).toEqual({ name: 'overview', issue: 'MT/1' });
    expect(parseRoute('#/runs')).toEqual({ name: 'runs', query: {} });
    expect(parseRoute('#/runs?status=failed&issue=MT-1&before=40')).toEqual({
      name: 'runs',
      query: { status: 'failed', issue: 'MT-1', before: 40 },
    });
    expect(parseRoute('#/runs/42')).toEqual({ name: 'run', id: 42 });
  });

  it('drops invalid query values and unknown paths', () => {
    expect(parseRoute('#/runs?status=bogus&before=-3')).toEqual({ name: 'runs', query: {} });
    expect(parseRoute('#/runs/abc')).toEqual({ name: 'not-found', path: '/runs/abc' });
    expect(parseRoute('#/nope')).toEqual({ name: 'not-found', path: '/nope' });
  });

  it('round-trips through formatRoute', () => {
    const routes: Route[] = [
      { name: 'overview' },
      { name: 'overview', issue: 'MT 1' },
      { name: 'runs', query: {} },
      { name: 'runs', query: { status: 'blocked', issue: 'MT-9', before: 7 } },
      { name: 'run', id: 3 },
    ];
    for (const route of routes) expect(parseRoute(formatRoute(route))).toEqual(route);
  });
});
