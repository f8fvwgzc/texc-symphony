import type { ComponentChildren } from 'preact';

export function Section(props: {
  title: string;
  description?: string;
  actions?: ComponentChildren;
  children: ComponentChildren;
  id?: string;
}) {
  const headingId = `${props.id ?? props.title.toLowerCase().replace(/\W+/g, '-')}-title`;
  return (
    <section class="card section" aria-labelledby={headingId}>
      <div class="section-header">
        <div>
          <h2 class="section-title" id={headingId}>
            {props.title}
          </h2>
          {props.description !== undefined && <p class="section-copy">{props.description}</p>}
        </div>
        {props.actions}
      </div>
      {props.children}
    </section>
  );
}
