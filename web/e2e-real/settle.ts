import type { Page } from "@playwright/test";

/**
 * Waits for a page to settle before a screenshot. Not `waitForLoadState("networkidle")`: every
 * signed-in page holds the admin event stream open (`src/api/events.ts`, `GET /api/v1/events`),
 * so the network is never idle. Instead: loaded, no loading skeletons left (best effort, a
 * running task's bar pulses too), and a moment for the last paint.
 */
export async function settle(page: Page): Promise<void> {
  await page.waitForLoadState("load");
  await page
    .locator(".animate-pulse")
    .first()
    .waitFor({ state: "detached", timeout: 5_000 })
    .catch(() => {});
  await page.waitForTimeout(300);
}
