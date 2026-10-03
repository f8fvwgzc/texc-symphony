import { describe, expect, it } from 'vitest';

import { makeSnapshot } from '../test/fixtures';
import {
  compactSessionId,
  externalIssueUrl,
  formatCompact,
  formatDurationMs,
  formatInt,
  formatPercent,
  formatRelative,
  formatRuntimeAndTurns,
  formatRuntimeSeconds,
  formatUtc,
  prettyJson,
  runStatusTone,
  runtimeSecondsFromStartedAt,
  stateTone,
  summarizeRateLimits,
  totalRuntimeSeconds,
} from './format';

const NOW = Date.parse('2026-02-24T20:15:30Z');

describe('formatInt', () => {
  it('groups thousands', () => {
    expect(formatInt(0)).toBe('0');
    expect(formatInt(999)).toBe('999');
    expect(formatInt(1000)).toBe('1,000');
    expect(formatInt(1234567)).toBe('1,234,567');
    expect(formatInt(-1234)).toBe('-1,234');
  });

  it('renders n/a for non-integers', () => {
    expect(formatInt(null)).toBe('n/a');
    expect(formatInt(undefined)).toBe('n/a');
    expect(formatInt(1.5)).toBe('n/a');
    expect(formatInt('12')).toBe('n/a');
  });
});

describe('formatCompact', () => {
  it('abbreviates large numbers', () => {
    expect(formatCompact(950)).toBe('950');
    expect(formatCompact(12_345)).toBe('12k');
    expect(formatCompact(1_500)).toBe('1.5k');
    expect(formatCompact(2_050_618)).toBe('2.1M');
    expect(formatCompact(3_000_000_000)).toBe('3B');
    expect(formatCompact(Number.NaN)).toBe('n/a');
  });
});

describe('runtime helpers (LiveView parity)', () => {
  it('formats whole minutes and seconds, clamping negatives', () => {
    expect(formatRuntimeSeconds(0)).toBe('0m 0s');
    expect(formatRuntimeSeconds(75)).toBe('1m 15s');
    expect(formatRuntimeSeconds(4321)).toBe('72m 1s');
    expect(formatRuntimeSeconds(42.9)).toBe('0m 42s');
    expect(formatRuntimeSeconds(-5)).toBe('0m 0s');
    expect(formatRuntimeSeconds(Number.NaN)).toBe('0m 0s');
  });

  it('computes elapsed seconds from an ISO timestamp', () => {
    expect(runtimeSecondsFromStartedAt('2026-02-24T20:10:12Z', NOW)).toBe(318);
    expect(runtimeSecondsFromStartedAt('garbage', NOW)).toBe(0);
    expect(runtimeSecondsFromStartedAt(null, NOW)).toBe(0);
  });

  it('appends the turn count only when positive', () => {
    expect(formatRuntimeAndTurns('2026-02-24T20:10:12Z', 7, NOW)).toBe('5m 18s / 7');
    expect(formatRuntimeAndTurns('2026-02-24T20:10:12Z', 0, NOW)).toBe('5m 18s');
    expect(formatRuntimeAndTurns(null, null, NOW)).toBe('0m 0s');
  });

  it('adds live running time to ended-session runtime', () => {
    expect(totalRuntimeSeconds(makeSnapshot(), NOW)).toBe(42 + 318);
  });
});

describe('formatDurationMs', () => {
  it('picks a readable unit', () => {
    expect(formatDurationMs(850)).toBe('850ms');
    expect(formatDurationMs(42_000)).toBe('42s');
    expect(formatDurationMs(1_288_246)).toBe('21m 28s');
    expect(formatDurationMs(3 * 3_600_000 + 5 * 60_000)).toBe('3h 05m');
    expect(formatDurationMs(null)).toBe('n/a');
    expect(formatDurationMs(-1)).toBe('n/a');
  });
});

describe('formatRelative / formatUtc', () => {
  it('describes past and future times', () => {
    expect(formatRelative('2026-02-24T20:15:30Z', NOW)).toBe('just now');
    expect(formatRelative('2026-02-24T20:15:00Z', NOW)).toBe('30s ago');
    expect(formatRelative('2026-02-24T20:15:32Z', NOW)).toBe('in 2s');
    expect(formatRelative('2026-02-24T20:05:30Z', NOW)).toBe('10m ago');
    expect(formatRelative('2026-02-24T17:00:30Z', NOW)).toBe('3h 15m ago');
    expect(formatRelative('2026-02-20T20:15:30Z', NOW)).toBe('4d ago');
    expect(formatRelative(null, NOW)).toBe('n/a');
    expect(formatRelative('nope', NOW)).toBe('n/a');
  });

  it('prints UTC without locale dependence', () => {
    expect(formatUtc('2026-02-24T20:15:30.123Z')).toBe('2026-02-24 20:15:30 UTC');
    expect(formatUtc(null)).toBe('n/a');
    expect(formatUtc('not a date')).toBe('not a date');
  });
});

describe('tones', () => {
  it('maps tracker states like state_badge_class', () => {
    expect(stateTone('In Progress')).toBe('active');
    expect(stateTone('Running')).toBe('active');
    expect(stateTone('Blocked')).toBe('danger');
    expect(stateTone('Failed')).toBe('danger');
    expect(stateTone('Todo')).toBe('warning');
    expect(stateTone('Queued for retry')).toBe('warning');
    expect(stateTone('Done')).toBe('neutral');
    expect(stateTone(null)).toBe('neutral');
  });

  it('maps run statuses', () => {
    expect(runStatusTone('running')).toBe('active');
    expect(runStatusTone('succeeded')).toBe('active');
    expect(runStatusTone('failed')).toBe('danger');
    expect(runStatusTone('blocked')).toBe('danger');
    expect(runStatusTone('cancelled')).toBe('warning');
    expect(runStatusTone('other')).toBe('neutral');
  });
});

describe('externalIssueUrl', () => {
  it('accepts http(s) URLs with a host', () => {
    expect(externalIssueUrl(' https://linear.app/x/issue/MT-1 ')).toBe(
      'https://linear.app/x/issue/MT-1',
    );
    expect(externalIssueUrl('http://example.org')).toBe('http://example.org');
  });

  it('rejects other schemes and junk', () => {
    expect(externalIssueUrl('javascript:alert(1)')).toBeNull();
    expect(externalIssueUrl('ftp://example.org')).toBeNull();
    expect(externalIssueUrl('/relative')).toBeNull();
    expect(externalIssueUrl(null)).toBeNull();
  });
});

describe('misc', () => {
  it('compacts long session ids', () => {
    expect(compactSessionId('thread-1234567890')).toBe('thre…567890');
    expect(compactSessionId('short')).toBe('short');
    expect(compactSessionId(null)).toBe('n/a');
  });

  it('formats percentages', () => {
    expect(formatPercent(97, 128)).toBe('76%');
    expect(formatPercent(1, 0)).toBe('n/a');
  });

  it('pretty prints JSON', () => {
    expect(prettyJson(null)).toBe('n/a');
    expect(prettyJson({ a: 1 })).toBe('{\n  "a": 1\n}');
  });
});

describe('summarizeRateLimits (terminal format_rate_limits parity)', () => {
  it('matches the credits_unlimited golden fixture', () => {
    const summary = summarizeRateLimits({
      limit_id: 'priority-tier',
      primary: { remaining: 100, limit: 100, reset_in_seconds: 1 },
      secondary: { remaining: 500, limit: 500, resetInSeconds: 1 },
      credits: { unlimited: true },
    });
    expect(summary).toEqual({
      name: 'priority-tier',
      buckets: [
        { label: 'primary', text: '100/100 reset 1s', ratio: 1 },
        { label: 'secondary', text: '500/500 reset 1s', ratio: 1 },
      ],
      credits: 'credits unlimited',
    });
  });

  it('handles partial buckets and credit balances', () => {
    const summary = summarizeRateLimits({
      limit_name: 'tier',
      primary: { remaining: 11 },
      secondary: { limit: 2000, resets_at: '2026-02-24T21:00:00Z' },
      credits: { has_credits: true, balance: 9876.5 },
    });
    expect(summary?.name).toBe('tier');
    expect(summary?.buckets[0]?.text).toBe('remaining 11');
    expect(summary?.buckets[1]?.text).toBe('limit 2,000 reset 2026-02-24T21:00:00Z');
    expect(summary?.credits).toBe('credits 9876.50');
  });

  it('falls back for missing data', () => {
    const summary = summarizeRateLimits({ primary: {}, credits: { has_credits: false } });
    expect(summary?.name).toBe('unknown');
    expect(summary?.buckets.map((bucket) => bucket.text)).toEqual(['n/a', 'n/a']);
    expect(summary?.credits).toBe('credits none');
    expect(summarizeRateLimits({ credits: { has_credits: true } })?.credits).toBe(
      'credits available',
    );
    expect(summarizeRateLimits({ credits: { has_credits: true, balance: 1200 } })?.credits).toBe(
      'credits 1,200',
    );
    expect(summarizeRateLimits({})?.credits).toBe('credits n/a');
    expect(summarizeRateLimits(null)).toBeNull();
    expect(summarizeRateLimits('weird')).toBeNull();
  });

  it('computes the remaining ratio', () => {
    const summary = summarizeRateLimits({ primary: { remaining: 25, limit: 100 } });
    expect(summary?.buckets[0]?.ratio).toBe(0.25);
  });
});
