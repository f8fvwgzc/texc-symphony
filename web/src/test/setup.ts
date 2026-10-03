import '@testing-library/jest-dom/vitest';
import { act, cleanup } from '@testing-library/preact';
import { afterEach } from 'vitest';

// Vitest runs without globals, so Testing Library cannot register its auto-cleanup.
// act() also flushes Preact 11's after-paint effect cleanups (closing live connections).
afterEach(async () => {
  await act(() => cleanup());
});
