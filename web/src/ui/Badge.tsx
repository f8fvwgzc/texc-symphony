import type { Tone } from '../lib/format';
import { cx } from './cx';

const DOT_CLASS: Record<Tone, string> = {
  active: 'bg-ok',
  warning: 'bg-warn',
  danger: 'bg-destructive',
  neutral: 'bg-muted-foreground',
};

/** Outlined status pill with a coloured dot. The tone is also exposed as `data-tone`. */
export function Badge({ tone, mono, children }: { tone: Tone; mono?: boolean; children: string }) {
  return (
    <span
      data-tone={tone}
      class={cx(
        'text-muted-foreground inline-flex items-center gap-1.5 rounded-md border px-2 py-0.5 text-xs font-medium whitespace-nowrap',
        mono === true && 'font-mono',
      )}
    >
      <span class={cx('size-1.5 rounded-full', DOT_CLASS[tone])} aria-hidden="true" />
      {children}
    </span>
  );
}
