import type { ComponentChildren } from 'preact';

/** A titled card: the building block of every page. */
export function Panel(props: {
  title: string;
  description?: string;
  actions?: ComponentChildren;
  children: ComponentChildren;
}) {
  const headingId = `${props.title.toLowerCase().replace(/\W+/g, '-')}-title`;
  return (
    <section class="bg-card rounded-xl border shadow-xs" aria-labelledby={headingId}>
      <header class="flex flex-wrap items-end justify-between gap-3 px-6 pt-6">
        <div>
          <h2 class="leading-none font-semibold" id={headingId}>
            {props.title}
          </h2>
          {props.description !== undefined && (
            <p class="text-muted-foreground mt-1.5 text-sm">{props.description}</p>
          )}
        </div>
        {props.actions}
      </header>
      <div class="p-6">{props.children}</div>
    </section>
  );
}
