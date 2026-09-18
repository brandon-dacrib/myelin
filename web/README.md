# Management web interface

Owned by track 16 (`docs/workstreams/16-management-web-interface.md`). A TypeScript
single-page application for operating the hs Matrix homeserver: dashboard, bridges
(the marquee page), and the rest of the information architecture in
`docs/design/information-architecture.md`. Stack decision: `docs/decisions/0003-web-stack.md`.
Design system: `docs/design/design-system.md`. Current status: `docs/status/16-management-web-interface.md`.

## Requirements

Node 26+, npm. No Docker, no running homeserver required for any command below — everything
here runs against MSW mocks (`src/mocks/`), matching track 15's real
`crates/hs-admin/openapi/openapi.yaml` (reconciled 2026-09-18; see
`docs/status/16-management-web-interface.md`). `mocks/openapi.yaml`, this track's own earlier
draft, is now only a fallback used if the real file is ever absent.

## Commands

| Command                                    | What it does                                                                                                                                                                            |
| ------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `npm run dev`                              | Vite dev server against the real admin API at `/api/v1` (needs a running homeserver).                                                                                                   |
| `npm run dev:mock`                         | Vite dev server with MSW mocking the admin API. Sign in with either button on the landing screen; "read-only" demonstrates the permissions model.                                       |
| `npm run build`                            | Regenerates the typed client, typechecks, and builds the production bundle to `dist/` (served by the homeserver at `/admin/`, or standalone with a `config.json` next to `index.html`). |
| `npm run build:mock`                       | Same, but bundles the MSW mock worker and builds to `dist-mock/` — used by `npm run preview:mock` and the Playwright suite.                                                             |
| `npm run preview` / `npm run preview:mock` | Serves the corresponding build locally.                                                                                                                                                 |
| `npm run generate:client`                  | Regenerates `src/api/schema.d.ts` from whichever OpenAPI document is authoritative (see `scripts/generate-client.mjs` and `docs/decisions/0003-web-stack.md`).                          |
| `npm run mock:openapi`                     | Reports which OpenAPI document is authoritative right now and what to do about it (`scripts/check-openapi.mjs`).                                                                        |
| `npm run lint` / `npm run lint:fix`        | ESLint (flat config, `jsx-a11y` strict) + Prettier check/fix.                                                                                                                           |
| `npm run typecheck`                        | `tsc -b`, no emit.                                                                                                                                                                      |
| `npm run test` / `npm run test:watch`      | Vitest unit and component tests (jsdom, Testing Library, MSW).                                                                                                                          |
| `npm run test:e2e` / `npm run test:e2e:ui` | Playwright end-to-end tests (`e2e/`) against `npm run preview:mock`; axe (`@axe-core/playwright`) runs at every step of every flow.                                                     |
| `npm run storybook`                        | Storybook dev server for every primitive component (`src/components/ui/`), with the `a11y` addon running axe on each story.                                                             |
| `npm run build:storybook`                  | Static Storybook build to `storybook-static/`.                                                                                                                                          |
| `npm run check`                            | `lint && typecheck && test && build` — run this (or at least lint+typecheck+test) after every change; do not proceed past a failure.                                                    |

`node scripts/check-contrast.mjs` independently verifies the status-colour token pairs in
`src/styles/tokens.css` meet 4.5:1 in both themes; run it after touching any `--color-success` /
`-warning` / `-danger` / `-info` / `-muted-status` / text-on-fill token.

## Layout

```
src/
  styles/           design tokens (tokens.css) and global CSS
  components/ui/    primitives: Button, Input, Select, Dialog, Sheet, DataTable, Badge,
                     Toast, EmptyState, ErrorState/ForbiddenState, Skeleton — each with a
                     Storybook story, most with a co-located test
  components/shell/  app shell: Sidebar, TopBar, CommandPalette, SignIn, nav model
  components/        shared app-level pieces (CopyableId, CopyBlock, RelativeTime, Sparkline)
  api/               generated schema (schema.d.ts), the typed fetch client, query/mutation
                     hooks per resource (dashboard.ts, bridges.ts, users.ts, rooms.ts,
                     federation.ts — see bridges.ts's doc comment for the reconciliation
                     against the real AppService/BridgeType model)
  lib/               auth (mock issuer client), theme, cn(), query client, small helpers
  mocks/             MSW handlers + fixture data (browser.ts for the app, node.ts for tests)
  pages/             route components: DashboardPage, bridges/ (list, detail, add-bridge
                     wizard), UsersPage/UserDetailPage, RoomsPage/RoomDetailPage,
                     FederationPage/FederationDestinationPage, PlaceholderPage for the
                     remaining information-architecture sections not yet built
  routes.tsx          the route tree (TanStack Router, code-based, every page lazy-loaded
                     via lazyRouteComponent for route-level code splitting)
e2e/                 Playwright specs: add-bridge.spec.ts (flows.md flow 1 end to end),
                     bridges-list.spec.ts (nested-interactive-element regression coverage,
                     desktop and phone viewport), users-rooms-federation.spec.ts
mocks/openapi.yaml   this track's own OpenAPI draft; fallback only, see Requirements above
scripts/             generate-client.mjs, check-openapi.mjs, check-contrast.mjs
```

## What is and is not built yet

Built: the scaffold, the design system and every Phase-0 primitive component, the application
shell (navigation, theme switching including a curated accent set, command palette, responsive
down to tablet width, keyboard navigation), the dashboard, the bridges list/detail/add-bridge
wizard pages (flows.md flow 1 in full, including the Kubernetes and self-managed deployment
paths, the namespace-conflict branch, and the forbidden branch), and the Users, Rooms and
Federation list/detail pages (flows.md flows 2-4: search, understand, and the primary actions
— lock/suspend/deactivate a user, block a room, reset a federation destination's backoff).

Not built (routes exist as `PlaceholderPage` so navigation matches the full information
architecture, but the pages themselves are Phase 1/2 per the brief): Reports, Media, Cluster,
Migration, Audit log, Settings. See `docs/status/16-management-web-interface.md` for what is
next, including narrower gaps on the pages that are built (e.g. no reset-password flow yet).
