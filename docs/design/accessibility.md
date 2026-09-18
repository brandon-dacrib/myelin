# Accessibility commitments

Track 16. Date: 2026-09-17. Conformance target: WCAG 2.2 level AA for every page, and level AAA for contrast on body text where the tokens allow. These commitments are tested, not aspirational: axe runs in Storybook for every component story and in Playwright for every flow, and a manual screen-reader pass is part of every release.

## Commitments

1. **Keyboard.** Everything the interface does can be done with a keyboard: navigation, tables (arrow keys, Enter, Space, Escape), dialogs (focus trapped, Escape closes, focus returns), menus, wizards, the command palette. Tab order follows reading order. No keyboard traps. Shortcuts are single-key or `g` chords, never override browser or assistive-technology keys, and can be disabled in the operator menu.
2. **Focus.** A visible focus ring (2 px, `--color-focus`, 2 px offset) on every focusable element, in both themes, with at least 3:1 contrast against adjacent colours. Focus is never removed by a `:focus { outline: none }` without a visible replacement. After route changes focus moves to the page heading; after dialog close it returns to the invoking control; after a row action to the row.
3. **Semantics.** Landmarks (`banner`, `navigation`, `main`, `complementary`, `contentinfo`), one `h1` per page, heading levels without skips, tables with `<th scope>` and a caption (visually hidden where the page title already says it), lists as lists, buttons as `<button>`, links as `<a>`. Radix primitives supply the ARIA patterns for dialog, menu, tabs, tooltip, popover, select, switch, checkbox, radio group; we do not hand-roll widgets that Radix provides.
4. **Names and descriptions.** Every control has an accessible name; icon-only buttons have `aria-label` and a tooltip; form fields have visible labels, hints through `aria-describedby`, and errors announced through `aria-describedby` plus `aria-invalid`. Status pills carry text, never only colour or an icon.
5. **Colour and contrast.** Text at least 4.5:1 (7:1 for body text in the default themes), UI components and graphics at least 3:1, in light and dark. Status is always colour plus icon plus text. The curated operator accents are pre-validated for contrast against both themes.
6. **Live regions.** Toasts, the live-stream indicator and live-updating counts announce through `aria-live="polite"`; errors that block a task use `role="alert"`. Live tables do not announce every row change; they announce a summary ("3 bridges updated") at most every 10 s.
7. **Motion.** `prefers-reduced-motion` honoured everywhere; no auto-playing animation; no flashing.
8. **Zoom and reflow.** Usable at 200 % zoom and at 320 px equivalent widths without loss of content or two-dimensional scrolling (tables switch to card rows). Text resizes with the browser setting; no `px` font sizes on text.
9. **Target size.** At least 24 by 24 CSS px for every target with 8 px spacing (WCAG 2.5.8); 40 px on tablet layouts.
10. **Time.** No time limits on any task; the session refresh is silent; long operations show progress and can be left and returned to.
11. **Consistent help and authentication.** Help and shortcuts are in the same place on every page; sign-in is through the issuer with no cognitive-function tests of our own (WCAG 3.3.8).
12. **Language.** `lang` set on `<html>`; text direction ready for RTL (logical CSS properties only: `margin-inline`, `inset-inline-start`); all copy from the catalogue.
13. **Drag.** Nothing requires dragging (WCAG 2.5.7); reordering has a menu alternative.

## How it is tested

- **Storybook**: `@storybook/addon-a11y` on every story with the `wcag2a`, `wcag2aa`, `wcag21aa`, `wcag22aa` rule tags; violations fail the story's test.
- **Playwright**: `@axe-core/playwright` in every end-to-end flow at every step, same tags, zero violations allowed; `e2e/a11y.spec.ts` additionally walks every route in both themes.
- **Lint**: `eslint-plugin-jsx-a11y` strict on every component.
- **Manual pass per release**: VoiceOver on Safari (macOS) and NVDA on Firefox (Windows) through the five flows in `flows.md`; keyboard-only run of the same; 200 % zoom run. Findings are recorded in `docs/design/usability/` with the release.
- **Lighthouse**: accessibility score published per release alongside performance.

## Known exceptions

None yet. Exceptions must be listed here with the reason and the planned fix.
