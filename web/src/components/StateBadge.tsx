import type { Tone } from '../lib/format';

export function Badge({ tone, children }: { tone: Tone; children: string }) {
  return <span class={`badge badge-${tone}`}>{children}</span>;
}
