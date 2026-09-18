# 16. Management web interface

Wave 1 for product design, the design system and the scaffold; pages land as the admin API (15) freezes at week 8. The management interface is a product, not a console.

**Expert profile.** Product designer and frontend engineer: information architecture, design systems, accessibility, TypeScript, testing UIs, comfortable owning the visual quality bar.

**Mission.** An easy to use and beautiful management interface for operators: the whole server and its bridges legible at a glance, every routine task two clicks away, and the hard tasks (moderation, migration, cluster operations) guided. Built on the public admin API from 15 and served by the homeserver itself. See `PLAN.md` sections 4 (D12) and 8.4.

**Owns.** `web/` (the application, the design system, the generated TypeScript API client, Storybook, Playwright tests), embedding of the built assets into the binary (with 15), the product design artifacts (`docs/design/` for information architecture, flows and the design tokens), accessibility conformance, internationalization scaffolding, dark and light themes, responsive layouts down to tablet width.

**Provides.** The interface; usability findings that feed back into 15's API design; the design tokens reused by 07's account-management pages.

**Consumes.** 15's OpenAPI document and event stream, 07's OAuth issuer (admin scope, PKCE in the browser), 11's registry data for the bridges pages, 03's cluster status, 13's migration status.

**Day-one work.** Information architecture and the first flows (add a bridge, find and deal with a user, understand a room, watch federation health, run a migration from Synapse); the design system (tokens, typography, spacing, components, empty and error states, density); the stack decision (recommended: TypeScript, Vite, React, Tailwind, Radix primitives, TanStack Query and Router, generated client from OpenAPI, Storybook, Playwright; static build embedded through `rust-embed` and served at `/admin/`); the scaffold with authentication against a mocked issuer and a mock API server generated from 15's OpenAPI draft.

**Phase 0 deliverables.** Design artifacts reviewed with 11 and 15; scaffold running against the mock API with the dashboard and the bridges list; the design system in Storybook with accessibility checks.

**Phase 1 and 2 deliverables.** Dashboard, users and devices, rooms and moderation, bridges (the marquee: health, backlog, add-bridge wizard producing a `Bridge` resource or a registration and Compose snippet, token rotation, deep links to bridge login), appservices, federation destinations and keys, media and quarantine, reports, cluster and shard map, settings for reloadable configuration, audit log, migration from Synapse with live progress; live updates through 15's event stream; keyboard navigation; i18n; the embedded build shipping in the binary.

**Definition of done.** Playwright end-to-end tests for every flow in the information architecture; axe accessibility checks clean; Storybook covers every component; Lighthouse performance and accessibility scores published; a usability pass with at least three operators recorded in `docs/design/`.

**References.** Element Admin (ESS) and `synapse-admin` as the baseline to exceed; `refs/palpo/web-admin/` as a Rust server's admin UI; the mautrix docs for what bridge operators actually do day to day; WAI-ARIA authoring practices.

**Open questions to settle first.** Serving the app from the homeserver only, or also as a separate deployment in the cluster; how much configuration editing the interface exposes (only reloadable sections, with validation from 13); branding and theming for operators.

**Risks.** Building pages before the API stabilizes; mitigate by developing against the mock server generated from 15's OpenAPI document and by treating API changes as RFCs.
