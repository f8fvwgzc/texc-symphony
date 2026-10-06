import type { ComponentChildren } from 'preact';

import { cx } from './cx';

export interface Column {
  label: string;
  /** Right-aligned numeric column. */
  numeric?: boolean;
}

/** Bordered, horizontally scrollable data table; `caption` is its accessible name. */
export function Table(props: {
  caption: string;
  columns: Column[];
  busy?: boolean;
  children: ComponentChildren;
}) {
  return (
    <div class="overflow-x-auto rounded-lg border" aria-busy={props.busy}>
      <table class="w-full border-collapse text-left">
        <caption class="sr-only">{props.caption}</caption>
        <thead class="bg-muted">
          <tr class="border-b">
            {props.columns.map((column) => (
              <th
                key={column.label}
                scope="col"
                class={cx(
                  'h-10 px-4 text-sm font-medium whitespace-nowrap',
                  column.numeric === true && 'text-right',
                )}
              >
                {column.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody class="[&_tr]:hover:bg-muted/50 divide-y [&_tr]:transition-colors">
          {props.children}
        </tbody>
      </table>
    </div>
  );
}

export function Cell(props: { numeric?: boolean; class?: string; children: ComponentChildren }) {
  return (
    <td
      class={cx(
        'px-4 py-3 align-top',
        props.numeric === true && 'text-right tabular-nums',
        props.class,
      )}
    >
      {props.children}
    </td>
  );
}
