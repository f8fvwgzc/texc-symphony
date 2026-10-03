import { render } from '@testing-library/preact';
import type { ComponentChildren } from 'preact';

import type { ApiClient } from '../api/client';
import { ApiContext } from '../api/context';

/** Renders `ui` with the given API client in context (kept across `rerender`). */
export function renderWithApi(ui: ComponentChildren, api: ApiClient) {
  return render(<>{ui}</>, {
    wrapper: ({ children }: { children: ComponentChildren }) => (
      <ApiContext.Provider value={api}>{children}</ApiContext.Provider>
    ),
  });
}
