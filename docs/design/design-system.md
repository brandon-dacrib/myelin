# Design system

Track 16. Date: 2026-09-18. Status: draft for review with 11 and 15, tokens implemented as code. Companion documents: `information-architecture.md`, `flows.md`, `states-density-responsiveness.md`, `accessibility.md`, `baselines.md`. Tokens live at `web/src/styles/tokens.css`; components at `web/src/components/ui/`, each with a Storybook story.

## 1. Intent

An instrument panel, not a marketing site or a spreadsheet (`information-architecture.md` principle 6). The interface should feel considered and quiet: one accent used sparingly for interactive emphasis, colour spent almost entirely on status, generous but not wasteful whitespace, a restrained type scale, and motion that explains cause rather than decorates. The bar to clear is `baselines.md`'s "generic template look" — synapse-admin's Material defaults and Element Admin's blankness are both failure modes we design away from, not starting points.

Two operator-visible personalisation knobs exist (Settings → Appearance, `information-architecture.md` §9.3): a curated accent set and light/dark theme. Nothing else is themeable — no arbitrary CSS, so contrast stays guaranteed by construction.

## 2. Colour

Colour is a neutral ramp plus a small semantic set. The neutral ramp is used for surfaces, borders and text; colour is reserved for the accent (interactive emphasis, selection, links) and status (success, warning, danger, info). Every pairing listed here is checked at 4.5:1 for UI text and 3:1 for large text/icons/borders in both themes; body text targets 7:1 (AAA) as `accessibility.md` commits to.

### 2.1 Neutral ramp

A single desaturated slate ramp (`--gray-*`, 0 = lightest, 950 = darkest) is the base for backgrounds, surfaces, borders and text in both themes; dark mode is not a naive invert, it is a second set of role assignments over the same ramp so elevation and hierarchy read correctly.

| Role token | Light | Dark |
|---|---|---|
| `--color-canvas` (page background) | `gray-50` | `gray-950` |
| `--color-surface` (cards, panels, table) | `gray-0` (white) | `gray-900` |
| `--color-surface-raised` (dialogs, popovers, menus) | `gray-0` + elevation shadow | `gray-850` + elevation shadow |
| `--color-surface-sunken` (inputs, code blocks) | `gray-100` | `gray-950` |
| `--color-border` | `gray-200` | `gray-800` |
| `--color-border-strong` | `gray-300` | `gray-700` |
| `--color-text` | `gray-900` | `gray-50` |
| `--color-text-muted` | `gray-600` | `gray-400` |
| `--color-text-faint` | `gray-500` | `gray-500` |
| `--color-focus` | `accent-600` | `accent-400` |

### 2.2 Accent (curated set, one active at a time)

Default accent is `indigo`. Settings → Appearance offers `indigo`, `teal`, `violet`, `amber-accent` and `slate` (a near-neutral accent for operators who want the calmest possible panel); each is pre-validated at the same contrast steps as indigo below, so switching accent never breaks contrast.

| Token | Light | Dark | Use |
|---|---|---|---|
| `--color-accent` | `indigo-600` | `indigo-400` | Primary buttons, active nav, selection, links |
| `--color-accent-hover` | `indigo-700` | `indigo-300` | Hover/pressed |
| `--color-accent-muted` | `indigo-50` | `indigo-950` | Subtle backgrounds (selected row, active tab) |
| `--color-accent-text-on` | `white` | `gray-950` | Text/icon on a solid accent fill |

### 2.3 Status (colour + icon + text, never colour alone, per `accessibility.md` §5)

| Status | Token | Light | Dark | Icon (lucide) |
|---|---|---|---|---|
| Success / healthy / running | `--color-success` | `green-700` (`#166534`) | `green-400` | `circle-check` |
| Warning / degraded / backing off | `--color-warning` | `amber-800` (`#92400e`) | `amber-400` | `triangle-alert` |
| Danger / failing / error | `--color-danger` | `red-700` (`#b91c1c`) | `red-400` | `circle-x` |
| Info / waiting / neutral-in-progress | `--color-info` | `blue-700` (`#1d4ed8`) | `blue-400` | `info` |
| Muted / paused / disabled | `--color-muted-status` | `gray-600` (`= --color-text-muted`) | `gray-400` (`= --color-text-muted`) | `circle-pause` |

Light-theme values are darker than a naive 600-step (`green-600`, `amber-600`, `red-600`, `blue-600`) because these render as 12px badge text on a tinted wash background; `scripts/check-contrast.mjs` verifies every pairing here reaches 4.5:1 and is the check that caught the original, too-light values. `--color-muted-status` deliberately reuses `--color-text-muted`'s per-theme values rather than one flat grey, because a flat grey that cleared 4.5:1 in light mode failed it in dark mode.

Each has a matching `-bg` token (a 50/950-step wash for pill and banner backgrounds) and `-border`. Badges and status pills always render icon + text; colour is the third channel, not the only one.

### 2.4 Theme selection

`data-theme="light" | "dark"` on `<html>`, default following `prefers-color-scheme`, overridable per operator and persisted (`localStorage`, read before first paint via a blocking inline script to avoid a flash). Tokens are defined once under `:root` (light values) and re-defined under `:root[data-theme="dark"]` and `@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { … } }`, so the app, Storybook and any future SSR shell resolve the same way.

## 3. Typography

System font stack — no web font download, no FOUT, consistent with "calm instrument panel" and with running offline/embedded in the binary:

- Sans (UI and body): `-apple-system, "Segoe UI", Inter, Roboto, Helvetica, Arial, sans-serif` (`--font-sans`).
- Monospace (identifiers, tokens, code, raw JSON, per `states-density-responsiveness.md` "identifiers in monospace at 13 px"): `ui-monospace, "SF Mono", "Cascadia Code", "Roboto Mono", Consolas, monospace` (`--font-mono`).

Scale (rem, 1 rem = 16 px base; line-height paired for readability, not decoration):

| Token | Size | Line height | Weight | Use |
|---|---|---|---|---|
| `--text-xs` | 0.75rem / 12px | 1rem | 400/500 | Captions, table meta, tooltips |
| `--text-sm` | 0.8125rem / 13px | 1.25rem | 400/500 | Compact-density table rows, identifiers |
| `--text-base` | 0.875rem / 14px | 1.375rem | 400 | Body text, comfortable-density table rows, form inputs |
| `--text-md` | 1rem / 16px | 1.5rem | 400/500 | Card titles, dialog body |
| `--text-lg` | 1.125rem / 18px | 1.625rem | 600 | Section headings, dialog titles |
| `--text-xl` | 1.375rem / 22px | 1.75rem | 600 | Page title |
| `--text-2xl` | 1.75rem / 28px | 2.125rem | 700 | Tile numbers, empty-state headline |
| `--text-3xl` | 2.25rem / 36px | 2.5rem | 700 | Rare: auth screen, big number callouts |

Body text defaults to 14px, not the web-default 16px, because this is a dense operator tool read up close on desktop monitors — deliberate departure from marketing-site norms, still AAA-checked against `--color-text`/`--color-canvas`. No `px` font sizes ship on text (accessibility §8); the scale is in `rem` so browser zoom and OS text-size settings work.

## 4. Spacing and grid

4 px base unit, the scale from `states-density-responsiveness.md` §3: `--space-1` (4) `--space-2` (8) `--space-3` (12) `--space-4` (16) `--space-6` (24) `--space-8` (32) `--space-12` (48) `--space-16` (64). Page gutter `--space-6` (24px) desktop, `--space-4` (16px) tablet. Content max width `--content-max: 90rem` (1440px).

## 5. Radii

A small set, used consistently by role rather than by component, so the panel reads as one system:

| Token | Value | Use |
|---|---|---|
| `--radius-xs` | 4px | Badges, pills, inline code |
| `--radius-sm` | 6px | Inputs, buttons, table row hover |
| `--radius-md` | 8px | Cards, menus, popovers |
| `--radius-lg` | 12px | Dialogs, sheets |
| `--radius-full` | 9999px | Avatars, status dots |

## 6. Elevation

Elevation communicates stacking order (what is above what), not decoration. Dark mode leans on a lighter surface colour plus a hairline border more than on shadow, since shadows read poorly on dark backgrounds; light mode leans on shadow plus a faint border.

| Token | Light | Dark | Use |
|---|---|---|---|
| `--elevation-0` | none | none | Page canvas, flat cards |
| `--elevation-1` | `0 1px 2px rgb(15 23 42 / 0.06), 0 1px 1px rgb(15 23 42 / 0.04)` | `0 1px 2px rgb(0 0 0 / 0.4)` + `1px solid --color-border` | Resting card that needs separation, table |
| `--elevation-2` | `0 2px 8px rgb(15 23 42 / 0.10)` | `0 2px 8px rgb(0 0 0 / 0.5)` + border | Popover, dropdown menu, toast |
| `--elevation-3` | `0 8px 24px rgb(15 23 42 / 0.14)` | `0 8px 24px rgb(0 0 0 / 0.6)` + border | Dialog, sheet |
| `--elevation-4` | `0 16px 40px rgb(15 23 42 / 0.18)` | `0 16px 40px rgb(0 0 0 / 0.7)` + border | Command palette |

## 7. Motion

Durations and easing are fixed tokens (`states-density-responsiveness.md` §5), never ad hoc per component:

- `--motion-fast: 120ms` hover/pressed. `--motion-reveal: 180ms` menus, tooltips, tab content. `--motion-modal: 240ms` dialogs, drawers.
- `--ease-out: cubic-bezier(0.2, 0, 0, 1)`, `--ease-in: cubic-bezier(0.4, 0, 1, 1)`.
- `prefers-reduced-motion: reduce` collapses every transition to an 80ms opacity fade and removes transforms; implemented once in `tokens.css` via a media query that overrides the duration tokens, so components never special-case it individually.

## 8. Density

Two density modes, tokenised so `DataTable` and list rows read them rather than each page hand-rolling padding (`states-density-responsiveness.md` §3):

| Token | Comfortable (default) | Compact |
|---|---|---|
| `--row-height` | 44px | 36px |
| `--row-text` | `--text-base` (14px) | `--text-sm` (13px) |

Only tables/lists change with density; forms, dialogs and headers stay at comfortable spacing always.

## 9. Iconography

`lucide-react` exclusively — one geometric style, 1.5px stroke, sized `16` (inline with text), `20` (buttons, nav) or `24` (empty/error state illustrations, tiles). Icons are never the sole carrier of meaning (paired with text or an accessible name).

## 10. Component inventory (Phase 0)

Primitives, each in `web/src/components/ui/<name>/` with `<Name>.tsx`, `<Name>.stories.tsx`, and co-located unit tests where behaviour (not just rendering) exists:

`Button`, `Input` (+ `Textarea`), `Select`, `Dialog`, `Sheet`, `Table` (`DataTable`: sorting, cursor pagination, column priority, density-aware), `Badge` (status pill), `Toast` (+ `Toaster`), `EmptyState`, `ErrorState` (and `ForbiddenState` as a variant), `Skeleton`. Built on Radix primitives (`radix-ui` package) for dialog, select, tabs, tooltip, switch, checkbox, toast, popover, menu — per `accessibility.md` §3, we do not hand-roll what Radix already gets right.

## 11. Implementation notes

- Tailwind 4 is configured CSS-first: `web/src/styles/tokens.css` defines the custom properties (light values under `:root`, dark overrides under `:root[data-theme="dark"]` and the `prefers-color-scheme` media query), and `web/src/styles/index.css` maps them into an `@theme` block so utilities like `bg-canvas`, `text-muted`, `rounded-md` resolve to the same tokens raw CSS uses. There is exactly one source of truth per token.
- Component variants use `class-variance-authority`; conditional class merging uses `tailwind-merge` via a `cn()` helper (`web/src/lib/cn.ts`).
- No component reaches for a raw hex value, `px` spacing literal, or ad hoc shadow; if a value is needed twice it becomes a token.
