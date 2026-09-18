# 16. Management web interface: status

Track brief: `docs/workstreams/16-management-web-interface.md`. Owner directories: `web/`, `docs/design/`.

Last updated: 2026-09-18 (day one, single session: Phase 0 scaffold and design system, the bridges marquee, then a full reconciliation against track 15's real OpenAPI document once it appeared mid-session).

## Done

Design artifacts (survived from the interrupted attempt, read and continued from, not rewritten):

- `docs/design/information-architecture.md`, `flows.md`, `accessibility.md`, `baselines.md`, `states-density-responsiveness.md`.
- `docs/design/design-system.md` (new this session): colour (light/dark via CSS `light-dark()`), typography scale, spacing, radii, elevation, motion, density, iconography, component inventory. Tokens implemented as code at `web/src/styles/tokens.css` (+ `base.css`, `index.css`).

Stack and scaffold:

- `docs/decisions/0003-web-stack.md`: records the stack (Vite, React 19, Tailwind 4 CSS-first, Radix via the `radix-ui` package, TanStack Query 5 + Router 1 code-based, `openapi-fetch` + `openapi-typescript`, MSW 2, Vitest 5, Storybook 10, Playwright 1).
- Full config: `tsconfig*.json`, `vite.config.ts`, `eslint.config.js` (flat, `jsx-a11y` strict), `.prettierrc.json`, `.storybook/{main,preview}.ts(x)`, `playwright.config.ts`.

Design system as code, all in `web/src/components/ui/`, each with a Storybook story and most with a Vitest/Testing-Library test: `Button`, `Input`/`Textarea`/`Field`, `Select`, `Dialog`, `Sheet`, `DataTable` (sorting, cursor pagination, column priority + tablet expansion row + <768px card fallback, density-aware), `Badge`, `Toast`/`Toaster`, `EmptyState`, `ErrorState`/`ForbiddenState`, `Skeleton`.

Application shell (`web/src/components/shell/`): `AppShell`, `Sidebar` (full/rail/drawer per breakpoint, scope-filtered, live error count on Bridges), `TopBar` (search trigger, live/polling indicator, theme cycling, operator menu), `CommandPalette` (⌘K / `/`, nav + bridge jump, arrow-key + Enter), `SignIn` (mock issuer), `g`-chord navigation (`g o/b/u/r/f`), focus-to-main on route change.

Pages: `DashboardPage`, `BridgesListPage`, `BridgeDetailPage`, the add-bridge wizard (`pages/bridges/wizard/`), and `PlaceholderPage` for every other information-architecture route (Users, Rooms, Reports, Federation, Media, Cluster, Migration, Audit, Settings) so navigation matches the full IA even though those pages are not built. **All of these were rewritten mid-session against the real API — see "Reconciliation" below; do not assume the shapes described in `flows.md`'s example paths are current.**

Testing:

- Vitest: 28 tests across 7 files. `npm run test` passes.
- Storybook: every primitive has a story; `@storybook/addon-a11y` runs axe (`wcag2a/2aa/21aa/22aa`) on every story in both themes via a theme-toolbar decorator. `npm run build:storybook` succeeds.
- Playwright: `e2e/add-bridge.spec.ts` covers flows.md flow 1 in full — happy path self-managed, happy path Kubernetes, the namespace-conflict branch, and the forbidden branch — with an `@axe-core/playwright` check at every step. **Run and passing** (Chromium downloaded successfully over the network in this environment: `npx playwright install chromium`; all 4 tests green).
- `npm run build` and `npm run build:mock` both succeed; `dist/` is the production artifact for track 15 to embed. `npm run check` (lint + typecheck + test + build) is clean.

A real accessibility bug was found and fixed via axe coverage, not left for later: an unlayered `button { color: inherit }` in `base.css` was silently beating every Tailwind `text-*` utility applied to a `<button>` (unlayered CSS always outranks `@layer`-wrapped rules regardless of specificity), making the primary "Add bridge" button render dark text on its indigo fill (2.83:1 contrast). Fixed by deleting the duplicate rule. Separately, several status-badge colour pairs (`success`, `warning`, `--color-text-faint` in both themes, `muted-status` in dark) were too light for 4.5:1 at 12-13px; retuned and verified with a new `scripts/check-contrast.mjs`. See `docs/design/design-system.md` §2.3.

## Reconciliation against track 15's real OpenAPI document (2026-09-18)

`crates/hs-admin/openapi/openapi.yaml` appeared partway through this session; its status file explicitly invited track 16 to generate against it ("Track 16: generate your client and mock-check your work against openapi.yaml; it is validated ... and the contract test proves the real router agrees with it"). `docs/decisions/0003-web-stack.md`'s generation-source rule was followed: `scripts/generate-client.mjs` now prefers it whenever it exists (the "own draft, `web/mocks/openapi.yaml`" fallback is unused unless the real file is deleted). This was a substantial rewrite, not a type-level touch-up, because the real resource model differs from this track's own earlier draft in ways that reshape the UI:

- **Resources are `/appservices` and `/bridge-types`**, not `/bridges` and `/bridges/kinds`. "Bridge" was always this track's UI framing over 11's generic appservice registry; the real API makes that explicit. `web/src/api/bridges.ts` and every bridges page were rewritten around `AppService`/`BridgeType`.
- **`AppService` has no `name` or `kind` field**, and nothing links a created appservice back to the bridge-type catalog entry it came from. `deriveDisplayName()` (humanises `id`) and `deriveKindLabel()` (reads `protocols`) in `web/src/api/bridges.ts` are this track's documented, reasonable-decision UI stand-ins. **Feedback for 15/11**: consider adding a display name and/or a `bridge_type` back-reference to `AppService`, or accept that the management UI will keep deriving one.
- **No inline backlog summary** on the list resource (`AppServicePage`), only a separate paginated `GET /appservices/{id}/backlog`. The bridges list can no longer show a backlog column without an N+1 fetch per row, so it doesn't; backlog only shows on the bridge detail page now. **Feedback for 15**: a cheap backlog count/age on the list item (like `health` already gets) would let the list surface backlog again.
- **No `state`/`kind` filter parameter** on `GET /appservices` (only free-text `q`, `limit`, `cursor`, `include_total`). The bridges list's health filter now applies client-side to whatever page is loaded, not server-side across all pages. **Feedback for 15**: a `health` filter param would fix this properly.
- **Replay is asynchronous**: `POST /appservices/{id}/replay` returns `202` + a `Task`, not a synchronous result. The UI shows a toast naming the task id and does not (yet) poll `/tasks/{id}` for completion.
- **The wizard's registration/compose/Kubernetes-resource YAML is rendered by the server** (`POST /bridge-types/{type}/render`, returning `registration`, `registration_yaml`, `compose_yaml`, `bridge_resource_yaml`), not assembled client-side. `pages/bridges/wizard/artifacts.ts` (this track's earlier hand-rolled YAML builder) is deleted. The Review step now calls render on entry/re-entry and shows its result; Create sends the render result's `registration`/`registration_yaml` to `POST /appservices`. The admin API has no "deployment" concept at all — which artifacts to _show_ (Compose vs. the Kubernetes `Bridge` resource) stays a presentational choice this track makes client-side from the render result, which always contains both.
- **Real scopes are `admin:read`, `admin:write`, `bridges:read`, `bridges:write`, `moderation:read`, `moderation:write`** — not the `moderation:*` this track's own earlier draft guessed from an informal reading of the brief. `src/lib/auth.ts` fixed: `admin:write` implies every scope; `bridges:write`/`moderation:write` each imply their own `:read`.
- **The `Problem` (RFC 9457) shape has no structured "which resource conflicts" field.** `instance` identifies the request, not reliably a pre-existing conflicting resource, so the namespace-conflict banner on the Identity step now shows the message only, with no "View" link to the conflicting resource (this track's earlier mock had invented one). **Feedback for 15**: a structured conflict detail (e.g. `errors[]` entries with a resource pointer, or an extension field) would let the UI link to what's conflicting.
- **`GET /appservices/{id}/registration` (which includes tokens) needs `bridges:write`**, not `bridges:read`; the Registration tab now shows a `ForbiddenState` naming that scope for read-only operators, rather than attempting the call.
- **No real login/session sub-resource** on appservices at all. The Logins tab is now honest about this gap (explains it, links to `links.login_url` if the API ever populates it) instead of showing fabricated remote-login data against a schema that doesn't support it.
- **No single `/overview`/dashboard resource.** `DashboardPage` is now composed client-side from `GET /statistics/overview` (counts), `GET /server` (version/uptime), `GET /cluster` (replica count — there is no explicit single-node/cluster boolean; `replica_count <= 1` is this track's documented heuristic, used for the wizard's Kubernetes-card visibility too), `GET /appservices` (bridges strip + unhealthy attention rows), `GET /federation/destinations` (federation strip + failing-over-an-hour attention rows), and `GET /audit-log` (recent 5). The "Activity" sparklines section from the earlier draft is dropped for now: `GET /statistics/timeseries?metric=...` exists but its metric-name vocabulary isn't documented in the schema, so wiring it up needs either a real backend to introspect or an RFC amendment naming the metrics.

New mock fixtures/handlers matching the real shapes: `web/src/mocks/data/{appservices,bridge-types,dashboard}.ts`, `web/src/mocks/handlers.ts` (fully rewritten). `web/src/mocks/browser.ts`'s Playwright test seam (`window.__hsAdminMock`) was renamed `setClusterMode` (was `setOverviewMode`) to match.

**What this means for anyone reading `flows.md`**: the flow narrative (steps, branches, what the operator sees) is still accurate; the API paths it cites (`web/mocks/openapi.yaml`, itself now just a fallback) are not what the app actually calls. `flows.md` was not rewritten in this pass (it is a design document, not code) — treat this section and `web/src/api/bridges.ts`'s doc comment as the current source of truth for the real contract, and update `flows.md`'s path references in a follow-up pass.

## Next

1. Users, Rooms, Reports, Federation pages (flows 2-4 in `flows.md`), now with a real API to build against for most of them (`/users`? not yet inspected in this session — only the resources this session's pages needed were read in full; a fresh pass over the full `openapi.yaml` is worthwhile before starting each page).
2. Real OAuth: swap `src/lib/auth.ts`'s mock issuer client for `oauth4webapi` against 07's issuer (07's brief; not yet started as of this session, per its own status file's dependency chain — check again, since 07 was also active this session).
3. SSE live updates (`GET /events`, referenced in 15's status file) once wired up; `TopBar`'s "Polling every 30s" indicator and each page's `refetchInterval` are the seam to replace.
4. Poll `GET /tasks/{id}` after `POST /appservices/{id}/replay` (currently fire-and-forget with a toast naming the task id).
5. Code-split the route bundle (currently one ~575 kB / 179 kB gzip chunk) via `React.lazy` per route before this grows further with Phase 1/2 pages.
6. i18n scaffolding (`web/src/i18n/`) — not started; all copy is inline English.
7. Wire up `GET /statistics/timeseries` for the dropped Activity sparklines, once the metric-name vocabulary is confirmed (ask 15, or read `hs-admin`'s statistics handler implementation once it exists beyond the mock).
8. Lighthouse scores and a three-operator usability pass (definition of done) are unstarted; need real users/a running instance.
9. Update `flows.md`'s path citations to match the real API (see the reconciliation note above); currently only this status file and `web/src/api/bridges.ts`'s doc comment carry the corrected paths.

## Blockers

None.

## Interfaces provided

- `web/dist/` (production build, base path `/admin/`) for 15 to embed via `rust-embed`; also runs standalone reading an optional `config.json` for API base URL/issuer.
- Usability/API-shape feedback for 15, gathered by actually building against the real contract — collected under "Reconciliation" above (appservice display name/kind reference, list-level backlog and health filter params, async replay, structured conflict details).
- `docs/design/design-system.md` tokens (`web/src/styles/tokens.css`) reusable by 07's account-management pages per the brief's "provides" list, once 07 exists.

## Interfaces needed

- 15: confirmation of the `/statistics/timeseries` metric-name vocabulary; the SSE event stream's exact event shapes once consumed; responses to the feedback items above.
- 07: the real OAuth issuer (authorization code + PKCE, admin scopes) to replace the mock in `src/lib/auth.ts`.
- 11: nothing directly consumed this session (appservice data now comes from 15's `hs-admin` mock, which stands in for 11's registry); will matter once 15's real router is backed by 11's actual data.
- 03: cluster status is now consumed (`GET /cluster`) for the single-node/cluster heuristic; no further ask yet.
- 13: reloadable-configuration schema for the Settings page (not built this session).

## Decisions made

- Stack and its two open points (route style: code-based; OpenAPI source order: prefer track 15's document whenever it exists) — `docs/decisions/0003-web-stack.md`.
- Tailwind v4 tokens use the native CSS `light-dark()` function (compiled to a `--lightningcss-light`/`-dark` toggle by Lightning CSS, driven by the `color-scheme` property under `[data-theme]`) instead of duplicating every token block per theme.
- Body text defaults to 14px (not the web-default 16px): a deliberate density choice for a dense operator tool, AAA-contrast-checked regardless.
- `DataTable` row actions are inline icon buttons only, no trailing overflow menu, to stay within Phase 0's explicit component inventory; Radix `DropdownMenu`/`Tabs`/`Switch` are composed directly at their call sites (operator menu, wizard database-mode select, bridge detail tabs) rather than wrapped as new `ui/` primitives not in `design-system.md`'s inventory.
- `window.__hsAdminMock` (`src/mocks/browser.ts`) is a small, explicit, mock-only test seam letting Playwright force cluster mode via `worker.use()` + a query-cache invalidation; `page.route()` cannot intercept a response MSW's service worker synthesizes without an outgoing network request, and a full page reload discards the SW-side runtime override, so the Kubernetes e2e test also had to route client-side rather than via `page.goto()`.
- The add-bridge wizard tracks progress via a `step` search param (not literal path segments like `/bridges/new/step/2`) so back/forward still work without a deeper route tree — an equivalent implementation of `flows.md`'s "progress is in the URL."
- **Reconciliation decisions** (all documented inline where they live, summarised here): `deriveDisplayName`/`deriveKindLabel` as UI stand-ins for fields the real `AppService` lacks; `replica_count <= 1` as the single-node/cluster heuristic (no explicit boolean exists); which render-result artifact to show driven by the wizard's own `deployment` choice, not sent to or interpreted by the server; dropped the namespace-conflict "View" link (no honest source for it in the real `Problem` shape); dropped the Activity sparklines pending the timeseries metric vocabulary; Logins tab rewritten to state the real gap rather than fabricate data.

## Shared dependencies added

None to the Rust workspace (this track only touches `web/`, `docs/design/`, `docs/status/16-management-web-interface.md`, and dated files under `docs/decisions/`). `web/package.json`'s dependency set was already fully specified by the interrupted attempt's scaffold; nothing was added or removed.

## How to verify

From `web/`:

```
npm run lint         # eslint (jsx-a11y strict) + prettier --check
npm run typecheck    # tsc -b
npm run test         # vitest run (28 tests)
npm run build        # generate:client (from crates/hs-admin/openapi/openapi.yaml) + tsc -b + vite build -> dist/
npm run build:storybook   # storybook build -> storybook-static/
npm run test:e2e     # playwright test (builds+serves dist-mock, runs e2e/add-bridge.spec.ts with axe)
node scripts/check-contrast.mjs   # offline contrast check for the status tokens
```

`npm run check` runs the first four in sequence and is clean. `npm run test:e2e` needs Chromium (`npx playwright install chromium` if not already cached); it downloaded successfully over the network in this session, and all 4 tests pass. `npm run dev:mock` for interactive use; sign in with either button on the landing screen.
