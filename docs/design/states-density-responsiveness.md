# Empty and error states, density, responsiveness

Track 16. Date: 2026-09-17. These rules apply to every page and are enforced by the shared components (`EmptyState`, `ErrorState`, `ForbiddenState`, `DataTable`, `PageShell`) so that individual pages cannot get them wrong.

## 1. Page states

Every data-bearing view has exactly these states, rendered by the same components in the same place the content would be:

| State | When | Rendering |
|---|---|---|
| **Loading, first time** | No cached data | Skeletons in the shape of the content (table rows, tiles). Never a full-page spinner. Appears only after 150 ms to avoid flashing. |
| **Loading, refresh** | Cached data, refetching | Content stays; a thin progress line under the page header; no layout shift. |
| **Empty, nothing exists** | The resource has no items at all | An `EmptyState`: an outline icon, a one-line title stating the fact ("No bridges yet"), one sentence of orientation ("Bridges connect other networks to this server."), one primary action if the operator can create ("Add bridge"), and a docs link. Never a blank table. |
| **Empty, filtered** | Items exist but none match | A smaller `EmptyState` inside the table: "No bridges match these filters", with "Clear filters". The filter bar stays. |
| **Forbidden** | 403 or missing scope | `ForbiddenState` naming the scope: "Viewing bridges needs the `bridges:read` scope. Ask an administrator to grant it." Sidebar entries for sections the operator cannot read are hidden; actions are disabled with the scope in a tooltip. |
| **Not found** | 404 | A page-level state with the identifier and, when the audit log knows, what happened to it ("Deleted by @alice 3 days ago") and a link to the list. |
| **Error** | 5xx, network, parse | `ErrorState` with a plain-language title ("Couldn't load bridges"), the problem detail's `title` and `detail` if present, the request ID for support, and **Retry**. The previous data, if any, remains visible under a banner instead of being replaced. |
| **Stale** | Live stream lost | Content stays; the top bar indicator says "Reconnecting" then "Stale since 12:04"; pages poll every 30 s. |
| **Partial** | One panel of a page fails | Only that panel shows its `ErrorState`; the rest of the page renders. A page never fails whole because one widget did. |

Problem details (RFC 9457) are the only error format the interface understands. Mapping: `type` chooses the copy (a small dictionary of known types such as `namespace-conflict`, `validation`, `rate-limited`, `not-owner`), `title` is the heading fallback, `detail` the body, `errors[].pointer` maps to form fields, `instance` or the `X-Request-Id` header is the request ID shown in the footer of the state.

Mutation errors are shown where the action was taken: inline under the field for validation, in the dialog for dialog actions, as a toast with Retry for row actions. A toast never carries the only copy of an error the operator will need later; it links to the place the error lives.

## 2. Empty states, by page

| Page | First-run empty | Copy |
|---|---|---|
| Overview, Attention | Nothing wrong | "Nothing needs your attention." with a small checkmark, and a line with the last time something did. |
| Bridges | No registry entries | "No bridges yet. Bridges connect WhatsApp, Signal, Telegram and other networks to this server." Action: Add bridge. |
| Users | Only the operator | "You are the only user so far." Actions: Create user, Invite with registration token. |
| Rooms | None | "No rooms yet. Rooms appear when users create or join them." |
| Reports | None | "No reports. When users report messages, rooms or people, they arrive here." |
| Federation | No destinations | "No federation traffic yet. When your users join rooms on other servers, those servers appear here." |
| Media | Nothing stored | "No media stored yet." with the configured store location. |
| Cluster | Single node | Section replaced by "This host"; the cluster pages are not shown. |
| Migration | No import | The start page (flow 5, step 2), not an empty state. |
| Audit | Nothing yet | "No changes recorded yet. Every action taken here is logged." |

## 3. Density

- **Grid.** 4 px base; spacing tokens 4, 8, 12, 16, 24, 32, 48, 64. Page gutter 24 px on desktop, 16 px on tablet. Content max width 1440 px; tables may extend to the full width of the content area.
- **Two densities**, chosen per operator in the top bar: **Comfortable** (table row 44 px, 14 px text) and **Compact** (row 36 px, 13 px text). Default comfortable; compact is remembered. Only tables and lists change; headers, forms and dialogs do not.
- **Tables** are the workhorse. Sticky header, sticky first column on horizontal overflow, column visibility menu, sortable headers with explicit direction, row selection with a checkbox column that appears on hover or keyboard focus, and row actions in a trailing menu plus the one or two most common as inline icon buttons. Numbers right-aligned and tabular; identifiers in monospace at 13 px; relative times with absolute in the tooltip.
- **Detail pages** use a 2:1 split above 1280 px (content left, facts panel right) and stack below.
- **Tiles** (overview stats) are 4 per row at 1440, 3 at 1280, 2 at tablet.
- **Dialogs** are 480 px wide for confirmations and 640 px for forms; anything larger is a page.
- **No page has more than one primary button visible at a time.**

## 4. Responsiveness

Targets: desktop first, usable on a tablet, must not break on a phone.

| Width | Layout |
|---|---|
| ≥ 1280 px | Full sidebar (240 px), page content, optional facts panel. |
| 1024 to 1279 px | Icon rail (56 px) with labels in tooltips; facts panel stacks below content. |
| 768 to 1023 px (tablet) | Navigation in a drawer from the top bar; tables show priority columns only (each column declares `priority: 1` to `3`) with the rest in an expandable row; page actions collapse into a menu after the primary; wizards keep the step rail as a horizontal stepper. |
| < 768 px (not a target) | Single column, tables become cards with the primary and status fields; nothing overflows horizontally; every action stays reachable. |

Rules: no horizontal page scroll at any width; touch targets at least 24 by 24 px with 8 px spacing (WCAG 2.5.8) and 40 px on tablet; hover-only affordances have a focus and touch equivalent; keyboard shortcuts are never the only way.

## 5. Motion

- Durations: 120 ms for hover and pressed states, 180 ms for reveal (menus, tooltips, tab content), 240 ms for dialogs and drawers. Easing `cubic-bezier(0.2, 0, 0, 1)` out, `cubic-bezier(0.4, 0, 1, 1)` in.
- Motion communicates cause: a toast slides from the action's side of the screen, a drawer from the edge it lives on. No decorative animation, no skeleton shimmer faster than 1.2 s.
- `prefers-reduced-motion` removes transforms and keeps only opacity at 80 ms.

## 6. Copy

- Sentences, not labels, for states and confirmations. Sentence case everywhere, including buttons ("Add bridge"). No exclamation marks.
- Name the object and the consequence: "Suspend @alice:example.org? They can read but not send until you lift it."
- Say what the system will do in the button: "Suspend", not "OK".
- Times: relative under 7 days ("4 min ago"), absolute otherwise, ISO 8601 in the tooltip, operator's locale and time zone.
- Numbers: locale-formatted; compact above 10,000 (12.4k) in tiles, full in tables.
- All copy lives in the message catalogue (`web/src/i18n/`) with English as the source; no hard-coded strings in components.
