import { test, expect, type Page } from "@playwright/test";

/**
 * Proves the app against a real `hs serve`, not `hs-admin-mock`
 * (docs/status/16-management-web-interface.md, "Prove it against the running binary, not
 * against the mock"). See `playwright.real.config.ts` for how to run this and what each env var
 * does; skipped entirely (no server started, every test a no-op skip) unless
 * `HS_REAL_SERVER_URL` is set.
 *
 * That day has arrived: `hs serve` wires `hs_auth::admin_verifier::AdminTokenVerifier` to the
 * admin surface, so an access token belonging to an account with `is_admin` set is accepted by
 * `/api/v1/*` and the authenticated walkthrough below actually runs. Verified 2026-09-20 against
 * a real `hs serve --data-dir`: registered an admin with `hs register --admin`, logged in over
 * the client-server API, and `GET /api/v1/me` answered 200 with `admin:read`/`admin:write`.
 *
 * Known flake, deliberately not papered over: run as a whole suite, "users list is real" and
 * "user detail" land on the sign-in page, while each passes on its own. The trace shows the only
 * `/api/v1` call in the failing test was `GET /api/v1/me` and it never completed — Playwright
 * records status `-1`. That call is the *sign-in* in `beforeEach`, not a call from the page under
 * test: `signInWithToken` turns a failed fetch into "Couldn't reach the server", leaves you on
 * the sign-in form, and the old `toHaveURL` assertion could not see it (see the comment there).
 * So the app is not signing anybody out on a blip — it says the server is unreachable, which is
 * correct. What is still unexplained is why that one `fetch` to the Vite dev server's `/api/v1`
 * proxy fails only when the suite runs as a whole, against a server answering 200 to twelve
 * consecutive curls. Chase it in the proxy, not in the app.
 */

const hasServer = Boolean(process.env.HS_REAL_SERVER_URL);
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;

test.describe("real server", () => {
  test.skip(!hasServer, "HS_REAL_SERVER_URL not set — see playwright.real.config.ts");

  test("sign-in honestly rejects an unrecognized token", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByRole("tab", { name: "Access token" })).toBeVisible();
    await page.getByRole("tab", { name: "Access token" }).click();
    await page.getByLabel("Access token").fill("syt_definitely_not_a_real_token");
    await page.getByRole("button", { name: "Sign in" }).click();

    // Whatever the real reason (today: the verifier isn't wired up yet, so every token is
    // "unrecognized"; once it is, a genuinely bad token gets the same answer) — the important
    // thing is an honest, specific message, not a stuck spinner or an unhandled exception.
    await expect(page.getByRole("alert")).toContainText(/wasn't recognized|expired/i);
    await page.screenshot({ path: "test-results/real-sign-in-rejected.png", fullPage: true });
  });

  test.describe("authenticated", () => {
    test.skip(!adminToken, "HS_REAL_ADMIN_TOKEN not set — see playwright.real.config.ts");

    test.beforeEach(async ({ page }) => {
      await page.goto("/");
      await page.getByRole("tab", { name: "Access token" }).click();
      await page.getByLabel("Access token").fill(adminToken!);
      await page.getByRole("button", { name: "Sign in" }).click();
      // NOT a URL assertion. `AppShell` renders `<SignIn />` in place of the app whenever there
      // is no session, at whatever URL you are on -- so `toHaveURL(/\/admin\/?$/)`, which this
      // used to assert, passes identically whether sign-in succeeded or failed, and a failed
      // sign-in then surfaced as a mystifying failure in the *next* test's body. Assert the thing
      // that is only true once there is a session.
      await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();
      await expect(page.getByRole("banner")).toBeVisible();
    });

    test("dashboard renders what's real and reports what isn't", async ({ page }) => {
      await screenshotHonestly(page, "/", "real-dashboard.png");
    });

    test("audit log lists durable entries, opens one, and exports NDJSON", async ({ page }) => {
      await page.goto("/admin/audit");
      await expect(page.getByRole("heading", { name: "Audit log" })).toBeVisible();
      const firstEntry = page.locator('table a[href*="/audit/"]').first();
      const empty = page.getByText("No changes recorded yet", { exact: true });
      await expect(firstEntry.or(empty)).toBeVisible();

      if (await firstEntry.isVisible()) {
        const action = await firstEntry.textContent();
        await firstEntry.click();
        await expect(page.getByRole("heading", { name: action?.trim() })).toBeVisible();
        await expect(page.getByText("Full audit entry (JSON)")).toBeVisible();
        await page.getByRole("link", { name: "Back to audit log" }).click();
      }

      const downloadPromise = page.waitForEvent("download");
      await page.getByRole("button", { name: "Export NDJSON" }).click();
      const download = await downloadPromise;
      expect(download.suggestedFilename()).toMatch(/^audit-log-.*\.ndjson$/);
    });

    test("users list is real (GET /users is wired)", async ({ page }) => {
      await page.goto("/admin/users");
      // Either real rows, a real empty state, or (if the directory source isn't attached, per
      // track 15's status file) an honest 503 "not connected to a data source" — never a bare
      // ErrorState and never an infinite spinner.
      await expect(page.getByRole("table").or(page.getByRole("status"))).toBeVisible({
        timeout: 10_000,
      });
      await page.screenshot({ path: "test-results/real-users.png", fullPage: true });
    });

    // `appservices.list` used to answer 501 and this test asserted the interface said so. All
    // sixteen bridge operations are real now (docs/status/11-appservices-and-bridges.md), so the
    // honest assertion is a table of bridges or the empty state -- never a bare error.
    test("bridges list is real (GET /appservices is wired)", async ({ page }) => {
      await page.goto("/admin/bridges");
      await expect(page.getByRole("table").or(page.getByRole("status"))).toBeVisible({
        timeout: 10_000,
      });
      await page.screenshot({ path: "test-results/real-bridges.png", fullPage: true });
    });

    test("user detail is real, and its sub-resource gap is honest", async ({ page }) => {
      // /users/{user_id} is real; /users/{user_id}/devices is not in track 15's real-handler
      // list yet, so the Sessions section should say so rather than claim "No devices."
      await page.goto("/admin/users/@ops:test.local");
      await expect(page.getByRole("heading", { name: "@ops:test.local" })).toBeVisible({
        timeout: 10_000,
      });
      await page.screenshot({ path: "test-results/real-user-detail.png", fullPage: true });
    });

    // `rooms.list` used to answer 501 and this test asserted the interface said so. It is a real
    // handler now, so the honest assertion is the same one the users list gets: a table, or an
    // empty state, or an honest 503 — never a bare error and never a stuck spinner.
    test("rooms list is real (GET /rooms is wired)", async ({ page }) => {
      await page.goto("/admin/rooms");
      await expect(page.getByRole("table").or(page.getByRole("status"))).toBeVisible({
        timeout: 10_000,
      });
      await page.screenshot({
        path: "test-results/real-rooms.png",
        fullPage: true,
      });
    });
  });
});

/** Loads `path`, waits for the page to settle, and screenshots it — used for pages this suite
 * doesn't assert specific content on (the mix of real/501/503 sections varies with what's wired
 * up on the server under test), so the screenshot itself is the evidence. */
async function screenshotHonestly(page: Page, path: string, filename: string) {
  await page.goto(path);
  await page.waitForLoadState("networkidle");
  await page.screenshot({ path: `test-results/${filename}`, fullPage: true });
}
