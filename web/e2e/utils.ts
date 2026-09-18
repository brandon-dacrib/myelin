import type { Page } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

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
