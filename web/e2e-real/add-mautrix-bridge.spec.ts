import { execSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { test, expect } from "@playwright/test";

/**
 * A real mautrix bridge, added through the wizard, started from the files the wizard produced,
 * watched connecting on the Created page, against a real `hs serve` -- the whole of flows.md
 * flow 1 with nothing mocked, and the first time a mautrix bridge has been pointed at this
 * server at all (docs/bridges/mautrix.md).
 *
 * Needs, on top of `playwright.real.config.ts`'s `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN`:
 *
 *   HS_REAL_BRIDGE_DIR=/abs/path/for/the/bridge/files   (created if absent; bind-mounted)
 *   HS_REAL_BRIDGE_RUN=1                                 (it runs `docker run`, so opt in)
 *
 * The server under test has to be reachable from a container as `host.docker.internal:8008`
 * (Docker Desktop and OrbStack both provide that name), and the bridge is published on this
 * host's port 29318 so the server can push to it. The container is removed at the end.
 */
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const bridgeDir = process.env.HS_REAL_BRIDGE_DIR;
const optedIn = process.env.HS_REAL_BRIDGE_RUN === "1";
const CONTAINER = "myelin-e2e-mautrix-whatsapp";
const IMAGE = "dock.mau.dev/mautrix/whatsapp:latest";

test.describe("a real mautrix bridge, added through the wizard", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken || !bridgeDir || !optedIn,
    "HS_REAL_SERVER_URL, HS_REAL_ADMIN_TOKEN, HS_REAL_BRIDGE_DIR and HS_REAL_BRIDGE_RUN=1 are all needed",
  );

  test.afterAll(() => {
    try {
      execSync(`docker rm -f ${CONTAINER}`, { stdio: "ignore" });
    } catch {
      /* not running */
    }
  });

  test("WhatsApp: wizard, files, docker run, first ping, sign-in guide", async ({ page }) => {
    test.setTimeout(240_000);
    await page.goto("/");
    await page.getByRole("tab", { name: "Access token" }).click();
    await page.getByLabel("Access token").fill(adminToken!);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();

    await page.goto("/admin/bridges/new");
    await page.getByRole("radio", { name: /WhatsApp/ }).click();
    await page.getByRole("button", { name: "Continue" }).click(); // -> identity
    await expect(page.getByRole("heading", { name: "Identity" })).toBeVisible();
    await page.getByRole("button", { name: "Continue" }).click(); // -> deployment

    // The server runs on this host and the bridge in Docker: each has to be told where the
    // other is, which is exactly what these two fields are for.
    await page
      .getByLabel(/This server, as the bridge reaches it/)
      .fill("http://host.docker.internal:8008");
    await page.getByLabel(/The bridge, as this server reaches it/).fill("http://127.0.0.1:29318");
    await page.getByRole("button", { name: "Continue" }).click(); // -> options
    await expect(page.getByLabel("Bridge administrator")).toHaveValue(/^@.+:.+/);
    await page.getByRole("button", { name: "Continue" }).click(); // -> review
    await expect(page.getByRole("region", { name: "config.yaml (preview)" })).toContainText(
      "address: http://host.docker.internal:8008",
    );
    await page.getByRole("button", { name: "Create bridge" }).click();

    await expect(page.getByRole("heading", { name: /Bridge WhatsApp created/ })).toBeVisible({
      timeout: 15_000,
    });
    await page.screenshot({ path: "test-results/real-bridge-created-waiting.png", fullPage: true });

    // The files, exactly as the page shows them.
    const config = await page.getByRole("region", { name: "config.yaml" }).textContent();
    const registration = await page
      .getByRole("region", { name: "registration.yaml" })
      .textContent();
    expect(config).toContain("as_token:");
    expect(registration).toContain("io.myelin.bridge_type: mautrix-whatsapp");
    mkdirSync(bridgeDir!, { recursive: true });
    writeFileSync(path.join(bridgeDir!, "config.yaml"), config ?? "");
    writeFileSync(path.join(bridgeDir!, "registration.yaml"), registration ?? "");

    await expect(page.getByRole("status")).toContainText("Waiting for the bridge's first ping");

    execSync(`docker run -d --name ${CONTAINER} -p 29318:29318 -v ${bridgeDir}:/data ${IMAGE}`, {
      stdio: "inherit",
    });

    // The page is polling; the bridge pings this server as it starts, and this turns green.
    await expect(page.getByRole("status")).toContainText("Connected", { timeout: 120_000 });
    await page.screenshot({
      path: "test-results/real-bridge-created-connected.png",
      fullPage: true,
    });

    await page.getByRole("link", { name: "Open bridge" }).click();
    await expect(page.getByRole("heading", { name: "WhatsApp" })).toBeVisible();
    await expect(page.getByText("Healthy").first()).toBeVisible();
    await page.screenshot({ path: "test-results/real-bridge-detail.png", fullPage: true });
    await page.getByRole("tab", { name: "Sign in" }).click();
    await expect(page.getByText(/Linked devices/)).toBeVisible();
    await page.screenshot({ path: "test-results/real-bridge-sign-in.png", fullPage: true });
  });
});
