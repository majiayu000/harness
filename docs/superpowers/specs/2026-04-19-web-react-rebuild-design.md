# Web React Rebuild — Design Spec

**Date:** 2026-04-19
**Status:** Approved for planning
**Supersedes:** Static `crates/harness-server/static/{dashboard.html,dashboard.css,dashboard.js,overview.html}` (deleted, not kept as legacy)

## Goal

Replace the two static HTML pages served by `harness-server` (`/` dashboard, `/overview` system overview) with a single React SPA written in TypeScript, styled with Tailwind and shadcn/ui, and bundled with Vite. Visual output must match the current pages pixel-for-pixel; this is a faithful port, not a redesign.

## Non-Goals

- Visual redesign. The existing Ember/Forge/Ink/Bloom/Terminal/Mint/Linen/Porcelain/Pixel/Multica palettes and the off-black+rust developer aesthetic are preserved.
- Adding new features (per-agent tokens, runtime CPU/RAM, WebSocket streaming, etc.). The React rewrite consumes the same `/api/dashboard` and `/api/overview` shapes and degrades the same way.
- Project-scoped dashboard routing (`/?project=...`). Still tracked as a follow-up; `<ProjectsTable>` navigates to plain `/`.
- Migrating `sdk/typescript/` — it is an unrelated NPM-publishable client library and stays untouched.

## Architecture

### Repository layout

```
harness/
├── crates/harness-server/
│   ├── build.rs               # invokes bun build during cargo build
│   ├── src/
│   │   ├── overview.rs        # serves index.html for /overview
│   │   ├── dashboard.rs       # serves index.html for /
│   │   └── http.rs            # adds /assets/* route
│   └── (static/ directory deleted)
├── sdk/typescript/             # unchanged; NPM client library
├── web/                        # NEW — top-level sibling
│   ├── package.json
│   ├── tsconfig.json
│   ├── vite.config.ts
│   ├── tailwind.config.ts
│   ├── postcss.config.js
│   ├── index.html
│   ├── README.md
│   ├── src/
│   │   ├── main.tsx
│   │   ├── App.tsx
│   │   ├── routes/
│   │   │   ├── Dashboard.tsx
│   │   │   └── Overview.tsx
│   │   ├── components/
│   │   │   ├── Sidebar.tsx
│   │   │   ├── TopBar.tsx
│   │   │   ├── Panel.tsx
│   │   │   ├── KpiCard.tsx
│   │   │   ├── Sparkline.tsx
│   │   │   ├── StackedArea.tsx
│   │   │   ├── HeatmapRow.tsx
│   │   │   ├── ProjectsTable.tsx
│   │   │   ├── RuntimeCard.tsx
│   │   │   ├── Feed.tsx
│   │   │   ├── AlertList.tsx
│   │   │   ├── PaletteFab.tsx
│   │   │   ├── PaletteDrawer.tsx
│   │   │   └── TokenPrompt.tsx
│   │   ├── lib/
│   │   │   ├── api.ts
│   │   │   ├── palette.tsx    # PaletteProvider context
│   │   │   ├── format.ts
│   │   │   └── queries.ts     # useDashboard / useOverview React Query hooks
│   │   ├── types/
│   │   │   ├── dashboard.ts
│   │   │   └── overview.ts
│   │   └── styles/
│   │       └── globals.css    # tailwind + palette variables
│   └── dist/                  # vite build output, gitignored
│                              # *.test.ts / *.test.tsx live next to the file they cover
└── Cargo.toml
```

### Build pipeline

- `crates/harness-server/build.rs` detects changes under `../../web/src`, `../../web/package.json`, `../../web/tsconfig.json`, `../../web/tailwind.config.ts`, `../../web/vite.config.ts` via `cargo:rerun-if-changed=` directives.
- On build it shells to `bun install --frozen-lockfile` + `bun run build` (cwd = `../../web`). If `bun` is missing it prints a clear install hint and fails cleanly.
- `bun run build` runs `tsc --noEmit` then `vite build`, producing `web/dist/index.html` plus `web/dist/assets/{index.<hash>.js,index.<hash>.css}`.
- Rust reads the produced files via `include_bytes!`/`include_str!` from `crates/harness-server/src/overview.rs` and `dashboard.rs`. Asset paths use relative `../../../web/dist/...` references.
- The `crates/harness-server/src/assets.rs` module (new) reads the asset filenames from `web/dist/index.html` at build time via a `build.rs`-emitted const so Rust can serve them at `/assets/:filename` with correct `Content-Type` + `Cache-Control: public, max-age=31536000, immutable`.

### Runtime serving

- `GET /` → `dashboard::index` → returns `web/dist/index.html` inlined.
- `GET /overview` → `overview::index` → returns the same `index.html` (SPA; the client router dispatches on `location.pathname`).
- `GET /assets/:filename` → `assets::serve` → matches against the hash-named JS/CSS produced by Vite, else 404.
- Auth middleware already exempts `/` and `/overview`; it must also exempt `/assets/*` since the bundle itself carries no secrets.
- `/api/dashboard` and `/api/overview` unchanged; still require auth when `api_token` is configured. The React client injects a bearer token from `sessionStorage` (same key as today: `harness_token`).

### Development loop

- `bun run dev` starts Vite at `:5173`.
- `vite.config.ts` configures a proxy: `/api/*`, `/health`, `/ws` → `http://localhost:9800`, so the user runs `harness serve --port 9800` in another terminal.
- HMR hot-reloads components and styles.

### Continuous integration

- New workflow `.github/workflows/web-ci.yml`: runs on `web/**` changes. Steps: `bun install --frozen-lockfile` → `bun run typecheck` → `bun run test` → `bun run build`.
- Existing `.github/workflows/ci.yml` must gain a preceding step that builds the web bundle so `cargo test -p harness-server` (which `include_bytes!`s `dist/*`) succeeds. Concretely: the `Test` / `Check` / `Clippy` jobs install bun and run `bun install && bun run build` in `web/` before invoking cargo.
- Path-based filtering: when only `docs/**` or similar changes, skip both the web and the cargo steps.

## Component Model

Shared components live in `web/src/components/`. Each has a `.test.tsx` sibling. No cross-component state — props in, JSX out. The only stateful components are:

- `<App>` — owns the React Query client and `<PaletteProvider>`.
- `<PaletteProvider>` — context for current palette, persisted to `localStorage.harness.palette`. Exposes `usePalette()` hook returning `{ current, setCurrent, palettes }`.
- `<TokenPrompt>` — uncontrolled modal triggered by 401 responses; on save, writes `sessionStorage.harness_token` and retries the failed query.

Routes in `web/src/routes/`:

- `Dashboard.tsx` — owns the four existing tabs (Active / History / Channels / Submit) via a simple `useState<'board'|'history'|'channels'|'submit'>`. Each tab body is itself a component so tabs can be tested independently.
- `Overview.tsx` — composes KPI band, StackedArea, ProjectsTable, RuntimeCard grid, HeatmapRow list, Feed, AlertList.

## Data Flow & Types

- `lib/api.ts` exports `apiFetch(path, init?)`. It injects `Authorization: Bearer ${sessionStorage.harness_token || ''}` when a token exists, and on a 401 it dispatches a module-level event listened to by `<TokenPrompt>` mounted at `<App>`. After the user submits a token, the original request is re-fetched via React Query's `refetch()` / cache invalidation.
- `lib/queries.ts`:
  - `useDashboard()` → `GET /api/dashboard`, `refetchInterval: 5000`, `refetchIntervalInBackground: true`.
  - `useOverview()` → `GET /api/overview`, same interval.
  - Both expose `isLoading`, `isError`, `error`, `data`, and a derived `isConnected` flag that drives the `<Status>` dot.
- `types/dashboard.ts` and `types/overview.ts` are hand-written TypeScript mirrors of the server's JSON shape. They are exported from a single `web/src/types/index.ts` barrel.
- `harness-sdk` is added to `web/package.json` as `"harness-sdk": "file:../sdk/typescript"` so shared domain types (`Event`, `TaskState` if present) are imported rather than duplicated.

## Palette System

- All 10 palettes are expressed as `data-palette="<id>"` selectors in `web/src/styles/globals.css`, each defining the same set of CSS custom properties (`--bg`, `--bg-1..3`, `--line`, `--line-2..3`, `--ink`, `--ink-2..4`, `--rust`, `--rust-deep`, etc.).
- `tailwind.config.ts` registers these as theme colors using Tailwind v4 `@theme` / `hsl(var(--rust))` bindings, so `bg-rust`, `text-ink`, `border-line-2` etc. all track palette changes.
- shadcn/ui is configured in `components.json` with the same CSS-variable color mode; all installed shadcn primitives (`Button`, `Dialog`, `Input`, `Skeleton`) inherit the palette automatically.
- `<PaletteProvider>` sets `document.documentElement.dataset.palette` whenever the user picks a new palette.
- `<PaletteDrawer>` renders the 10-button swatch list; matches the existing FAB position, animation, and swatch preview rendering.

## Error Handling & Edge Cases

- **Initial load (pre-first-fetch):** the relevant panels show shadcn `<Skeleton>` blocks matching each panel's final layout (e.g. six KPI skeletons, a rectangular skeleton for the throughput chart). No flash of empty state.
- **401 Unauthorized:** `apiFetch` throws; React Query surfaces it; the app-level listener on the 401 event opens `<TokenPrompt>`. On submit, the token is stored and all queries are invalidated so they refetch with the new header.
- **Network / 5xx:** the `<Status>` indicator flips to red and shows "connection lost". React Query retries with exponential backoff (defaults) up to 3 times, then keeps polling every 5s.
- **Empty arrays (no projects, no runtimes, no events, no alerts):** each section renders a muted one-line placeholder in `var(--ink-4)` matching the current fallback copy.
- **Null fields (`tokens_24h`, `cpu_pct`, `ram_pct`, `avg_score`):** components render `—` instead of the number. The KPI delta line shows a context string (`"no scans"`, `"per-agent n/a"`) when the numeric source is `null`.
- **localStorage / sessionStorage unavailable:** `usePalette()` and token helpers catch exceptions and fall through to memory-only state. The app remains functional for the session.

## Testing

Test files co-located with sources, discovered by Vitest via `**/*.test.{ts,tsx}`.

| Type | Targets | Expected count |
|---|---|---|
| Unit (vitest) | `lib/format.ts` (fmtInt/fmtScore/fmtPct/relativeAgo), `lib/api.ts` 401 dispatch, `lib/palette.tsx` reducer, `lib/queries.ts` buckets aggregation | ~6 |
| Component (RTL) | `<KpiCard>`, `<ProjectsTable>`, `<Feed>`, `<AlertList>`, `<PaletteDrawer>`, `<TokenPrompt>`, `<StackedArea>`, `<HeatmapRow>`, `<RuntimeCard>`, `<Sidebar>`, `<TopBar>`, `<Panel>` — each asserts props-to-DOM, not integration | ~12 |
| Type | `tsc --noEmit` across the whole `web/` project | 1 CI job |

E2E (Playwright) is explicitly deferred to a later PR.

## Migration & Cutover

- Single PR, single cutover. No dual-run, no feature flag.
- The old `crates/harness-server/static/` directory is deleted in the same PR. `src/dashboard.rs` and `src/overview.rs` are rewritten to serve the React bundle.
- `harness_token` sessionStorage key and `harness.palette` localStorage key are preserved so users with bookmarks don't have to re-enter tokens or re-pick palettes.

## Open Questions (Resolved)

- ~~Nested vs top-level `web/`~~ — resolved: top-level, consistent with `sdk/typescript/`.
- ~~Backward-compatible CSS variables~~ — resolved: no, full Tailwind migration with `data-palette="*"` variants.
- ~~Dashboard scope~~ — resolved: included; no legacy.
- ~~Testing depth~~ — resolved: tsc + vitest + RTL (~18 tests), no Playwright.

## Risks

- **Build-time dependency on bun.** CI jobs that touch Rust must install bun first. Documented in the CI workflow and README.
- **`include_bytes!` couples Rust build to web build success.** If Vite output filenames change unexpectedly, Rust won't compile. Mitigation: `build.rs` writes an explicit manifest const (`ASSET_JS: &str`, `ASSET_CSS: &str`) that Rust code reads, so the coupling is explicit and the failure mode is a clear compile error rather than a 404 at runtime.
- **Tailwind + 10 palettes = large `globals.css`.** Acceptable; it's comparable in size to the current inline `<style>` block in `overview.html` (~400 lines of CSS).
- **shadcn palette + custom palette interaction.** Both operate on CSS variables, but if shadcn expects HSL tokens and our palettes expose hex, a small mapping layer in `globals.css` is required (documented in the palette CSS file header).
