import { formatRelative, formatUtc } from '../lib/format';
import { NotAvailable } from './Text';

/** Relative time ("12s ago") with the exact UTC timestamp as tooltip. */
export function RelativeTime({ iso, now }: { iso: string | null; now: number }) {
  if (iso === null) return NotAvailable;
  return (
    <time dateTime={iso} title={formatUtc(iso)}>
      {formatRelative(iso, now)}
    </time>
  );
}
