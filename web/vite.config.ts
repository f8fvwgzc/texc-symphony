import { loadEnv } from 'vite';
import { defineConfig } from 'vitest/config';

/** Where `/api` is proxied in `pnpm dev` / `pnpm preview` (the Rust server's default port). */
const DEFAULT_API_URL = 'http://127.0.0.1:4000';

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), 'SYMPHONY_');
  const apiUrl = env['SYMPHONY_API_URL'] ?? DEFAULT_API_URL;
  const proxy = {
    '/api': { target: apiUrl, changeOrigin: true },
  };

  return {
    base: '/',
    // Preact through the automatic JSX runtime; no Babel preset needed.
    oxc: { jsx: { runtime: 'automatic', importSource: 'preact' } },
    server: { proxy },
    preview: { proxy },
    build: {
      outDir: 'dist',
      emptyOutDir: true,
      target: 'es2022',
      sourcemap: false,
      assetsInlineLimit: 0,
      modulePreload: { polyfill: false },
    },
    test: {
      environment: 'happy-dom',
      setupFiles: ['./src/test/setup.ts'],
      include: ['src/**/*.test.{ts,tsx}'],
      restoreMocks: true,
      coverage: {
        provider: 'v8',
        include: ['src/**/*.{ts,tsx}'],
        exclude: ['src/**/*.test.{ts,tsx}', 'src/test/**', 'src/main.tsx'],
        reporter: ['text', 'html'],
      },
    },
  };
});
