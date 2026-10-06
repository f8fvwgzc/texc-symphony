import type { ComponentChildren } from 'preact';

import { cx } from './cx';

type Variant = 'solid' | 'outline';

const BASE =
  'inline-flex h-8 items-center justify-center gap-1.5 rounded-md px-3 text-sm font-medium ' +
  'whitespace-nowrap shadow-xs transition-colors disabled:pointer-events-none disabled:opacity-50';

const VARIANT_CLASS: Record<Variant, string> = {
  solid: 'bg-primary text-primary-foreground hover:bg-primary/90',
  outline: 'border bg-background hover:bg-muted',
};

/** Classes of a button, for links that should look like one. */
export function buttonClass(variant: Variant = 'outline'): string {
  return cx(BASE, VARIANT_CLASS[variant], 'no-underline');
}

export function Button(props: {
  variant?: Variant;
  type?: 'button' | 'submit';
  disabled?: boolean;
  onClick?: () => void;
  'aria-label'?: string;
  children: ComponentChildren;
}) {
  return (
    <button
      type={props.type ?? 'button'}
      class={cx(BASE, VARIANT_CLASS[props.variant ?? 'outline'])}
      disabled={props.disabled}
      onClick={props.onClick}
      aria-label={props['aria-label']}
    >
      {props.children}
    </button>
  );
}
