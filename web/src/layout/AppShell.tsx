import type { ComponentChildren } from 'preact';

import { cx } from '../ui/cx';
import { Icon, type IconName } from '../ui/Icon';

export interface NavItem {
  label: string;
  href: string;
  icon: IconName;
  current: boolean;
}

/**
 * Page frame: a sticky sidebar (brand, navigation, footer) beside a sticky header and the content.
 * Below the `md` breakpoint the sidebar becomes a bar above the header.
 */
export function AppShell(props: {
  title: string;
  nav: NavItem[];
  actions: ComponentChildren;
  footer: ComponentChildren;
  children: ComponentChildren;
}) {
  return (
    <div class="md:grid md:min-h-screen md:grid-cols-[15rem_1fr]">
      <a
        class="bg-primary text-primary-foreground sr-only rounded-md px-3 py-2 focus:not-sr-only focus:absolute focus:top-2 focus:left-2 focus:z-30"
        href="#main"
      >
        Skip to content
      </a>

      <aside class="bg-sidebar flex gap-4 border-b p-3 md:sticky md:top-0 md:h-screen md:flex-col md:border-r md:border-b-0">
        <div class="flex items-center gap-2 px-2 py-1">
          <img src="/favicon.png" alt="" width={20} height={20} class="rounded" />
          <span class="font-semibold">Symphony</span>
        </div>
        <nav class="flex gap-1 md:flex-col" aria-label="Primary">
          {props.nav.map((item) => (
            <a
              key={item.href}
              href={item.href}
              aria-current={item.current ? 'page' : undefined}
              class={cx(
                'hover:bg-muted flex h-8 items-center gap-2 rounded-md px-2 no-underline',
                item.current ? 'bg-muted' : 'text-muted-foreground font-normal',
              )}
            >
              <Icon name={item.icon} />
              {item.label}
            </a>
          ))}
        </nav>
        <div class="text-muted-foreground mt-auto hidden px-2 text-xs leading-relaxed md:block">
          {props.footer}
        </div>
      </aside>

      <div class="min-w-0">
        <header class="bg-background/80 sticky top-0 z-10 flex flex-wrap items-center gap-2 border-b px-4 py-2.5 backdrop-blur lg:px-6">
          <h1 class="mr-auto text-base font-medium">{props.title}</h1>
          {props.actions}
        </header>
        <main id="main" class="space-y-4 p-4 outline-none lg:p-6" tabIndex={-1}>
          {props.children}
        </main>
      </div>
    </div>
  );
}
