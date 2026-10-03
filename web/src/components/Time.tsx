import { formatRelative, formatUtc } from '../lib/format';

/** Relative time ("12s ago") with the exact UTC timestamp as tooltip. */
export function RelativeTime({ iso, now }: { iso: string | null; now: number }) {
  if (iso === null) return <span class="muted">n/a</span>;
  return (
    <time dateTime={iso} title={formatUtc(iso)}>
      {formatRelative(iso, now)}
    </time>
  );
}
