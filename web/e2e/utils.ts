import type { Page, ConsoleMessage } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

/**
 * React reports invalid DOM nesting (e.g. a `<button>` inside a `<button>`,
 * or an `<a>` inside a `<button>`) via `console.error`, and it does so
 * regardless of whether the offending markup is currently visible — it
 * validates the actual DOM tree, not what's on screen. That is exactly why
 * axe (which only audits what's perceivable) missed the bridges-list nested
 * -button defect: the violation lived in the <768px card fallback, which is
 * `display:none` at every viewport Playwright's default (1280x720) tests
 * ran axe at. This guard catches that class of bug at any viewport;
 * `expectNoAxeViolations` is still run at a phone-width viewport
 * separately, as defence in depth, since axe also flags nested-interactive
 * controls once they are actually visible.
 *
 * Install once per test, right after the page is created, then call
 * `assertClean()` after each interaction you want covered (or once at the
 * end of the test).
 */
export function installDomNestingGuard(page: Page): { assertClean: () => void } {
  const warnings: string[] = [];
  const onConsole = (msg: ConsoleMessage) => {
    if (msg.type() !== "error") return;
    const text = msg.text();
    if (/cannot (be a descendant of|contain a nested)/i.test(text)) {
      warnings.push(text);
    }
  };
  page.on("console", onConsole);
  return {
    assertClean() {
      if (warnings.length > 0) {
        throw new Error(
          `Invalid DOM nesting reported by React (e.g. nested interactive elements):\n${warnings.join("\n---\n")}`,
        );
      }
    },
  };
}

/** Signs in through the mock issuer (docs/decisions/0003-web-stack.md). */
export async function signInAsOperator(page: Page): Promise<void> {
  await page.goto("/admin/");
  await page.getByRole("button", { name: "Sign in as operator" }).click();
  await page.getByRole("heading", { name: "Overview" }).waitFor();
}

/** Signs in with a reduced scope set, to exercise the permissions model. */
export async function signInReadOnly(page: Page): Promise<void> {
  await page.goto("/admin/");
  await page.getByRole("button", { name: "Sign in read-only (demo)" }).click();
  await page.getByRole("heading", { name: "Overview" }).waitFor();
}

/**
 * Runs axe against the current page at the WCAG levels accessibility.md
 * commits to, and fails the test on any violation.
 */
export async function expectNoAxeViolations(page: Page, label: string): Promise<void> {
  const results = await new AxeBuilder({ page })
    .withTags(["wcag2a", "wcag2aa", "wcag21aa", "wcag22aa"])
    .analyze();
  if (results.violations.length > 0) {
    const summary = results.violations
      .map((v) => `${v.id} (${v.impact}): ${v.nodes.length} node(s) — ${v.help}`)
      .join("\n");
    throw new Error(`axe violations on ${label}:\n${summary}`);
  }
}
