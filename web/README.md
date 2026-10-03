# Symphony web dashboard

This is the browser dashboard for a running Symphony orchestrator. It replaces the Phoenix
LiveView page. `pnpm build` writes a static bundle to `web/dist/`, which the `symphony-server`
crate embeds and serves at `/`. All data comes from the JSON/SSE API described in
[`docs/api/openapi.yaml`](../docs/api/openapi.yaml), with a human guide in
[`docs/api/README.md`](../docs/api/README.md).

## What it shows

- **Overview** (`#/`)
  - Counters: running, retrying, blocked, token totals, and live Codex runtime. All-time run
    totals are added when history is enabled.
  - Running sessions: issue, state, session id, runtime / turns, last event, and tokens.
  - Blocked sessions and the retry/backoff queue, with relative due times.
  - The latest rate limits as bucket meters plus credits, with the raw JSON available.
- **Issue drawer** (`#/issues/<identifier>`): live detail from `GET /api/v1/{identifier}`.
  It covers the workspace, attempts, the running, retry or blocked state, and recent events, plus
  the last 5 recorded runs of that issue. It closes with Escape, the Close button, or a click on
  the backdrop.
- **Run history** (`#/runs?status=&issue=&before=`): `GET /api/v1/runs`, with issue and status
  filters and keyset pagination (newer/older).
- **Run detail** (`#/runs/<id>`): the run summary and an events timeline, using `after_seq`
  paging. A run that is still `running` is refreshed every 5 s.
- **Header**
  - A connection badge: `Live` (SSE), `Polling` (fallback), `Offline` (with a retry countdown),
    or `Connecting`.
  - **Refresh now**, which calls `POST /api/v1/refresh` and then re-reads the state.
  - A theme switch that cycles Auto, Light and Dark. The choice is stored in `localStorage`, and
    the page still works when storage is blocked.

Routing uses the URL hash, so the server only has to serve `/` and the files in the bundle; it
needs no SPA fallback route.

## Live data

`src/live/liveSource.ts` is framework-agnostic and fully unit-tested. `src/live/useLiveState.ts`
wraps it in a hook.

1. Open an `EventSource` on `/api/v1/events`. Each `snapshot` event replaces the state, and its
   `id` (the orchestrator generation) is shown in the footer.
2. On an error, close the stream and reconnect with exponential backoff: 1 s, 2 s, 4 s … up to
   30 s, with ±20 % jitter. A stream that stays silent for 45 s, with no snapshot and no
   heartbeat, counts as an error.
3. After 3 consecutive failures, or when `EventSource` is not available, poll
   `GET /api/v1/state` every 5 s. Failed polls back off up to 30 s. While polling, SSE is
   re-probed every 60 s, and the dashboard goes back to `Live` on the first snapshot.

The last good state stays on screen while offline. Live runtime columns tick every second on the
client: `codex_totals.seconds_running` plus `now - started_at` for each running session.

## Stack and why

| Concern | Choice                                                                                                                                                                                                                                                                                                                                                                                    |
| ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| UI      | **Preact 11** (about 6 kB gzip for core plus hooks; React 19 + react-dom is about 60 kB). The UI is a handful of tables and a drawer, so React's extra features don't justify the size. JSX goes through Vite's built-in oxc transform (`jsxImportSource: preact`), so there is no Babel or preset plugin. The trade-off is that dev edits do a full reload instead of a hot module swap. |
| Build   | Vite 8 (Rolldown), `base: '/'`, output in `dist/`                                                                                                                                                                                                                                                                                                                                         |
| Types   | TypeScript **7.0.2** (the native compiler; its binary is still `tsc`). Config is strict, with `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes` and `verbatimModuleSyntax`.                                                                                                                                                                                                        |
| Lint    | **oxlint** 1.86 with `--type-aware` through `oxlint-tsgolint` 7.0.2003, which is built on the TS 7 checker. ESLint was not an option: `typescript-eslint` still caps TypeScript below 6.1, and ESLint 10 needs Node 22.13 or newer.                                                                                                                                                       |
| Format  | Prettier 3                                                                                                                                                                                                                                                                                                                                                                                |
| Tests   | Vitest 5, happy-dom, @testing-library/preact and jest-dom. jsdom 30 needs Node 22.22 or newer, so happy-dom is used.                                                                                                                                                                                                                                                                      |
| Styles  | Plain CSS with custom properties (`src/styles/app.css`). It has light and dark palettes (dark via `prefers-color-scheme` or `data-theme`), responsive breakpoints at 860 px and 560 px, and honours `prefers-reduced-motion`.                                                                                                                                                             |

## Requirements

- Node ≥ 22.12
- pnpm 10.18 (pinned with `packageManager`; with Corepack, run `corepack enable`)

## Scripts

```sh
pnpm install
pnpm dev            # http://localhost:5173, /api proxied to SYMPHONY_API_URL (default http://127.0.0.1:4000)
pnpm build          # -> dist/
pnpm preview        # serve dist/ with the same /api proxy
pnpm typecheck      # tsc --noEmit (TypeScript 7)
pnpm lint           # oxlint --type-aware --deny-warnings
pnpm test           # vitest run
pnpm test:watch
pnpm coverage       # v8 coverage -> coverage/
pnpm format         # prettier --write .
pnpm check          # typecheck + lint + test
```

To point the dev server at another orchestrator:

```sh
SYMPHONY_API_URL=http://10.0.0.5:4100 pnpm dev
```

You can also put `SYMPHONY_API_URL=...` in `web/.env.local`. Start the orchestrator with an HTTP
port (`--port 4000` or `server.port` in WORKFLOW.md) so the proxy has something to reach.

## Layout

```
src/
  api/        types.ts (mirrors openapi.yaml), client.ts (typed fetch client), context.ts
  live/       liveSource.ts (SSE -> polling state machine), useLiveState.ts
  lib/        format.ts (LiveView/terminal formatter ports), router.ts (hash routes), theme.ts
  hooks/      useAsync, useNow, useThrottled
  components/ tables, metrics, rate limits, issue drawer, connection badge, refresh/theme buttons
  pages/      OverviewPage, RunsPage, RunDetailPage
  styles/     app.css
  test/       setup, fixtures (openapi examples), FakeEventSource, fake API client
```

Keep `src/api/types.ts` in sync with `docs/api/openapi.yaml`. Every interface there is named after
the schema it mirrors.

## Accessibility notes

- There is a skip link, and the navigation uses landmarks with `aria-current`.
- Tables have captions and `scope="col"` headers.
- The issue drawer is a native modal `<dialog>`, which provides the focus trap, Escape and focus
  restore. It is labelled by its heading.
- The connection status and refresh results are `<output>` elements (polite live regions). The
  status badge has a screen-reader description that includes the retry countdown.
- Every interactive control has a visible `:focus-visible` ring, and colour is never the only
  signal (badges carry text).
