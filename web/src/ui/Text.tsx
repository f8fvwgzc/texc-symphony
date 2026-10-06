import type { ComponentChildren } from 'preact';

import type { JsonValue } from '../api/types';
import { prettyJson } from '../lib/format';

/** Placeholder line for an empty list. */
export function Empty({ children }: { children: ComponentChildren }) {
  return <p class="text-muted-foreground py-2">{children}</p>;
}

/** Inline error message, announced to screen readers. */
export function ErrorText({ children }: { children: ComponentChildren }) {
  return (
    <p class="text-destructive" role="alert">
      {children}
    </p>
  );
}

export const NotAvailable = <span class="text-muted-foreground">n/a</span>;

/** Collapsible pretty-printed JSON. */
export function JsonDetails(props: { label: string; value: JsonValue; open?: boolean }) {
  return (
    <details class="mt-2" open={props.open}>
      <summary class="text-muted-foreground cursor-pointer text-xs">{props.label}</summary>
      <pre class="bg-muted mt-2 max-h-96 overflow-auto rounded-lg p-3 font-mono text-xs">
        {prettyJson(props.value)}
      </pre>
    </details>
  );
}
