import type { ComponentChildren } from 'preact';

/** Label / value list (`<dl>`), two columns on wide screens. */
export function Fields({ children }: { children: ComponentChildren }) {
  return <dl class="grid grid-cols-[8rem_1fr] gap-x-4 gap-y-2">{children}</dl>;
}

export function Field({ label, children }: { label: string; children: ComponentChildren }) {
  return (
    <>
      <dt class="text-muted-foreground">{label}</dt>
      <dd class="min-w-0">{children}</dd>
    </>
  );
}
