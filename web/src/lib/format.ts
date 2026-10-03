/**
 * Pure formatting helpers. The first group ports the LiveView dashboard helpers
 * (`dashboard_live.ex`) and the terminal rate-limit formatter (`status_dashboard.ex`)
 * so both UIs read the same.
 */
import type { JsonValue, StateSnapshot } from '../api/types';

const NA = 'n/a';

function groupDigits(digits: string): string {
  return digits.replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

/** `1234567` -> `1,234,567`; anything that is not an integer -> `n/a`. */
export function formatInt(value: unknown): string {
  if (typeof value !== 'number' || !Number.isInteger(value)) return NA;
  const sign = value < 0 ? '-' : '';
  return sign + groupDigits(String(Math.abs(value)));
}

/** Compact count: `950`, `12.3k`, `4.5M`. Non-numbers -> `n/a`. */
export function formatCompact(value: unknown): string {
  if (typeof value !== 'number' || !Number.isFinite(value)) return NA;
  const abs = Math.abs(value);
  if (abs < 1000) return String(Math.trunc(value));
  const [divisor, suffix] = abs < 1e6 ? [1e3, 'k'] : abs < 1e9 ? [1e6, 'M'] : [1e9, 'B'];
  const scaled = value / divisor;
  return `${scaled.toFixed(Math.abs(scaled) < 10 ? 1 : 0).replace(/\.0$/, '')}${suffix}`;
}

/** `125` -> `2m 5s` (minutes unbounded, negatives clamp to 0). Matches `format_runtime_seconds`. */
export function formatRuntimeSeconds(seconds: number): string {
  const whole = Number.isFinite(seconds) ? Math.max(Math.trunc(seconds), 0) : 0;
  return `${Math.floor(whole / 60)}m ${whole % 60}s`;
}

/** Whole seconds between `startedAt` (ISO string) and `nowMs`; unparsable/null -> 0. */
export function runtimeSecondsFromStartedAt(
  startedAt: string | null | undefined,
  nowMs: number,
): number {
  if (startedAt == null) return 0;
  const started = Date.parse(startedAt);
  if (Number.isNaN(started)) return 0;
  return Math.floor((nowMs - started) / 1000);
}

/** `seconds_running` of ended sessions plus live elapsed time of each running session. */
export function totalRuntimeSeconds(snapshot: StateSnapshot, nowMs: number): number {
  const ended = Number.isFinite(snapshot.codex_totals.seconds_running)
    ? snapshot.codex_totals.seconds_running
    : 0;
  return snapshot.running.reduce(
    (total, entry) => total + Math.max(runtimeSecondsFromStartedAt(entry.started_at, nowMs), 0),
    ended,
  );
}

/** `"2m 5s / 7"` when `turnCount > 0`, else `"2m 5s"`. */
export function formatRuntimeAndTurns(
  startedAt: string | null,
  turnCount: number | null | undefined,
  nowMs: number,
): string {
  const runtime = formatRuntimeSeconds(runtimeSecondsFromStartedAt(startedAt, nowMs));
  return typeof turnCount === 'number' && Number.isInteger(turnCount) && turnCount > 0
    ? `${runtime} / ${turnCount}`
    : runtime;
}

/** Human duration for run history: `850ms`, `42s`, `21m 28s`, `3h 05m`. */
export function formatDurationMs(ms: number | null | undefined): string {
  if (typeof ms !== 'number' || !Number.isFinite(ms) || ms < 0) return NA;
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const totalSeconds = Math.floor(ms / 1000);
  if (totalSeconds < 60) return `${totalSeconds}s`;
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  if (hours === 0) return `${minutes}m ${seconds}s`;
  return `${hours}h ${String(minutes).padStart(2, '0')}m`;
}

/** `"12s ago"`, `"in 4m"`, `"just now"`; null/unparsable -> `n/a`. */
export function formatRelative(iso: string | null | undefined, nowMs: number): string {
  if (iso == null) return NA;
  const at = Date.parse(iso);
  if (Number.isNaN(at)) return NA;
  const deltaSeconds = Math.round((at - nowMs) / 1000);
  const abs = Math.abs(deltaSeconds);
  if (abs < 2) return 'just now';
  let text: string;
  if (abs < 60) text = `${abs}s`;
  else if (abs < 3600) text = `${Math.floor(abs / 60)}m`;
  else if (abs < 86_400) text = `${Math.floor(abs / 3600)}h ${Math.floor((abs % 3600) / 60)}m`;
  else text = `${Math.floor(abs / 86_400)}d`;
  return deltaSeconds < 0 ? `${text} ago` : `in ${text}`;
}

/** `2026-02-24T20:15:30.123Z` -> `2026-02-24 20:15:30 UTC` (stable, locale-independent). */
export function formatUtc(iso: string | null | undefined): string {
  if (iso == null) return NA;
  const at = Date.parse(iso);
  if (Number.isNaN(at)) return iso;
  return `${new Date(at).toISOString().slice(0, 19).replace('T', ' ')} UTC`;
}

export type Tone = 'active' | 'danger' | 'warning' | 'neutral';

/** Badge colour for a tracker state; same keyword rules as `state_badge_class`. */
export function stateTone(state: string | null | undefined): Tone {
  const normalized = (state ?? '').toLowerCase();
  const has = (words: string[]) => words.some((word) => normalized.includes(word));
  if (has(['progress', 'running', 'active'])) return 'active';
  if (has(['blocked', 'error', 'failed'])) return 'danger';
  if (has(['todo', 'queued', 'pending', 'retry'])) return 'warning';
  return 'neutral';
}

/** Badge colour for a run-history status. */
export function runStatusTone(status: string): Tone {
  switch (status) {
    case 'running':
    case 'succeeded':
      return 'active';
    case 'failed':
    case 'blocked':
      return 'danger';
    case 'cancelled':
      return 'warning';
    default:
      return 'neutral';
  }
}

/** Only `http(s)` URLs with a host are linkable (rejects `javascript:` and friends). */
export function externalIssueUrl(url: string | null | undefined): string | null {
  if (typeof url !== 'string') return null;
  const trimmed = url.trim();
  try {
    const parsed = new URL(trimmed);
    if ((parsed.protocol === 'http:' || parsed.protocol === 'https:') && parsed.hostname !== '') {
      return trimmed;
    }
  } catch {
    return null;
  }
  return null;
}

/** `thread-1234567890` -> `thre…567890` (ids longer than 10 chars). */
export function compactSessionId(id: string | null | undefined): string {
  if (typeof id !== 'string' || id === '') return NA;
  return id.length > 10 ? `${id.slice(0, 4)}…${id.slice(-6)}` : id;
}

/** `0.756` -> `76%`. */
export function formatPercent(part: number, whole: number): string {
  if (!Number.isFinite(part) || !Number.isFinite(whole) || whole <= 0) return NA;
  return `${Math.round((part / whole) * 100)}%`;
}

// ---------------------------------------------------------------------------
// Rate limits (port of `format_rate_limits` in status_dashboard.ex, as structured parts)
// ---------------------------------------------------------------------------

export interface RateLimitBucket {
  label: 'primary' | 'secondary';
  text: string;
  /** `remaining / limit` in [0, 1] when both are known. */
  ratio: number | null;
}

export interface RateLimitSummary {
  name: string;
  buckets: RateLimitBucket[];
  credits: string;
}

type JsonRecord = { [key: string]: JsonValue };

function isRecord(value: JsonValue | undefined): value is JsonRecord {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function firstValue(record: JsonRecord, keys: string[]): JsonValue | undefined {
  for (const key of keys) {
    const value = record[key];
    if (value !== undefined && value !== null && value !== false) return value;
  }
  return undefined;
}

function asText(value: JsonValue): string {
  return typeof value === 'string' ? value : JSON.stringify(value);
}

const RESET_KEYS = [
  'reset_in_seconds',
  'resetInSeconds',
  'reset_at',
  'resetAt',
  'resets_at',
  'resetsAt',
];

function formatBucket(bucket: JsonValue | undefined): { text: string; ratio: number | null } {
  if (bucket === undefined || bucket === null) return { text: NA, ratio: null };
  if (!isRecord(bucket)) return { text: asText(bucket), ratio: null };
  const remaining = bucket['remaining'];
  const limit = bucket['limit'];
  const remainingInt = typeof remaining === 'number' && Number.isInteger(remaining);
  const limitInt = typeof limit === 'number' && Number.isInteger(limit);
  let text: string;
  let ratio: number | null = null;
  if (remainingInt && limitInt) {
    text = `${formatInt(remaining)}/${formatInt(limit)}`;
    ratio = limit > 0 ? Math.min(Math.max(remaining / limit, 0), 1) : null;
  } else if (remainingInt) text = `remaining ${formatInt(remaining)}`;
  else if (limitInt) text = `limit ${formatInt(limit)}`;
  else if (Object.keys(bucket).length === 0) text = NA;
  else text = asText(bucket).slice(0, 40);

  const reset = firstValue(bucket, RESET_KEYS);
  if (reset !== undefined) {
    const resetText =
      typeof reset === 'number' && Number.isInteger(reset) ? `${formatInt(reset)}s` : asText(reset);
    text += ` reset ${resetText}`;
  }
  return { text, ratio };
}

function formatNumber(value: number): string {
  return Number.isInteger(value) ? formatInt(value) : value.toFixed(2);
}

function formatCredits(credits: JsonValue | undefined): string {
  if (credits === undefined || credits === null) return 'credits n/a';
  if (!isRecord(credits)) return `credits ${asText(credits)}`;
  if (credits['unlimited'] === true) return 'credits unlimited';
  const balance = credits['balance'];
  if (credits['has_credits'] === true && typeof balance === 'number') {
    return `credits ${formatNumber(balance)}`;
  }
  if (credits['has_credits'] === true) return 'credits available';
  return 'credits none';
}

/** Structured view of a Codex rate-limit object; `null` when nothing usable was reported. */
export function summarizeRateLimits(value: JsonValue): RateLimitSummary | null {
  if (!isRecord(value)) return null;
  const name = firstValue(value, ['limit_id', 'limit_name']);
  const buckets: RateLimitBucket[] = [];
  for (const label of ['primary', 'secondary'] as const) {
    const { text, ratio } = formatBucket(value[label]);
    buckets.push({ label, text, ratio });
  }
  return {
    name: name === undefined ? 'unknown' : asText(name),
    buckets,
    credits: formatCredits(value['credits']),
  };
}

/** Pretty JSON for raw panels (`n/a` for null). */
export function prettyJson(value: JsonValue | undefined): string {
  if (value === undefined || value === null) return NA;
  return JSON.stringify(value, null, 2);
}
