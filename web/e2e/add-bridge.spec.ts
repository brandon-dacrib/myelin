import { test, expect } from "@playwright/test";
import {
  signInAsOperator,
  signInReadOnly,
  expectNoAxeViolations,
  installDomNestingGuard,
} from "./utils";

// flows.md flow 1: add a bridge. Covers the happy paths (self-managed and
// Kubernetes), the namespace-conflict branch, and the forbidden branch, with
// an axe pass at every step (accessibility.md) and a React DOM-nesting guard
// (see installDomNestingGuard's doc comment for why axe alone missed the
// bridges-list nested-button defect).

test.describe("Add a bridge", () => {
  test("happy path: self-managed", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await expectNoAxeViolations(page, "overview");

    await page.getByRole("link", { name: "Bridges" }).first().click();
    await expect(page.getByRole("heading", { name: "Bridges" })).toBeVisible();
    await expectNoAxeViolations(page, "bridges list");

    await page.getByRole("button", { name: "Add bridge" }).click();
    await expect(page).toHaveURL(/\/bridges\/new/);
    await expectNoAxeViolations(page, "wizard: kind");

    // The catalogue is grouped; a search narrows it. Bluesky is not among the seeded bridges.
    await page.getByRole("heading", { name: "Messaging" }).waitFor();
    await page.getByLabel("Search bridges").fill("blue");
    await expect(page.getByRole("radio", { name: /WhatsApp/ })).toHaveCount(0);
    await page.getByRole("radio", { name: /Bluesky/ }).click();
    await expect(page.getByRole("link", { name: "Documentation" })).toBeVisible();
    await page.getByRole("button", { name: "Continue" }).click();

    await expect(page.getByRole("heading", { name: "Identity" })).toBeVisible();
    await expectNoAxeViolations(page, "wizard: identity");
    await page.getByRole("button", { name: "Continue" }).click();

    await expect(page.getByRole("heading", { name: "Deployment" })).toBeVisible();
    await expectNoAxeViolations(page, "wizard: deployment");
    // Single-node mode: only Self-managed is offered.
    await expect(page.getByRole("radio", { name: "Kubernetes" })).toHaveCount(0);
    await expect(page.getByRole("radio", { name: "Self-managed" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await page.getByRole("button", { name: "Continue" }).click();

    await expect(page.getByRole("heading", { name: "Options" })).toBeVisible();
    // The operator adding the bridge is prefilled as its administrator.
    await expect(page.getByLabel("Bridge administrator")).toHaveValue("@ops:example.org");
    await expectNoAxeViolations(page, "wizard: options");
    await page.getByRole("button", { name: "Continue" }).click();

    await expect(page.getByRole("heading", { name: "Review" })).toBeVisible();
    await expectNoAxeViolations(page, "wizard: review");
    await expect(page.getByText("registration.yaml (preview)")).toBeVisible();
    // A mautrix bridge's own config is rendered too, pointed at this server.
    const configPreview = page.getByRole("region", { name: "config.yaml (preview)" });
    await expect(configPreview).toContainText("address: http://myelin:8008");
    await expect(configPreview).toContainText('"@ops:example.org": admin');

    await page.getByRole("button", { name: "Create bridge" }).click();

    await expect(page.getByRole("heading", { name: /Bridge Bluesky created/ })).toBeVisible();
    await expect(page.getByText("config.yaml", { exact: true })).toBeVisible();
    await expect(page.getByText("registration.yaml", { exact: true })).toBeVisible();
    await expect(page.getByText("docker-compose.yaml")).toBeVisible();
    // The runbook: how to start it, that the page is watching for its first ping, and how to
    // sign in, with the bot named for this server.
    await expect(page.getByText("docker compose up -d bluesky")).toBeVisible();
    await expect(page.getByRole("status")).toContainText("Waiting for the bridge's first ping");
    await expect(page.getByRole("heading", { name: "Sign in" })).toBeVisible();
    await expect(page.getByText("@blueskybot:example.org").first()).toBeVisible();
    await expect(page.getByText(/app password/)).toBeVisible();
    await expectNoAxeViolations(page, "wizard: created");

    await page.getByRole("link", { name: "Open bridge" }).click();
    await expect(page).toHaveURL(/\/bridges\/bluesky$/);
    await expect(page.getByRole("heading", { name: "Bluesky" })).toBeVisible();
    await expect(page.getByText("Unknown").first()).toBeVisible();
    await expectNoAxeViolations(page, "bridge detail");

    // The detail page knows what it is and how to sign in to it.
    await page.getByRole("tab", { name: "Sign in" }).click();
    await expect(page.getByText(/app password/)).toBeVisible();
    await expect(page.getByRole("link", { name: /Bluesky documentation/ })).toBeVisible();
    await expectNoAxeViolations(page, "bridge detail: sign in");
    domGuard.assertClean();
  });

  test("happy path: kubernetes", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    // Kubernetes is only offered in cluster mode (flows.md flow 1 step 4);
    // override the mock overview response for this test to exercise it (see
    // the doc comment on window.__hsAdminMock in src/mocks/browser.ts for
    // why this goes through the worker rather than page.route()). The
    // override lives in the page's JS, not the service worker, so getting
    // there has to stay client-side routing (no page.goto/full reload).
    await page.evaluate(() => window.__hsAdminMock?.setClusterMode("cluster", 3));
    await page.getByRole("link", { name: "Bridges" }).first().click();
    await expect(page.getByRole("heading", { name: "Bridges" })).toBeVisible();
    await page.getByRole("button", { name: "Add bridge" }).click();
    await expect(page).toHaveURL(/\/bridges\/new/);

    await page.getByRole("radio", { name: /LinkedIn/ }).click();
    await page.getByRole("button", { name: "Continue" }).click();
    await page.getByRole("button", { name: "Continue" }).click(); // identity

    await expect(page.getByRole("heading", { name: "Deployment" })).toBeVisible();
    await expect(page.getByRole("radio", { name: "Kubernetes" })).toBeVisible();
    await page.getByRole("radio", { name: "Kubernetes" }).click();
    await expect(page.getByLabel("Namespace")).toBeVisible();
    // The server's address follows the deployment until the operator types one.
    await expect(page.getByLabel(/This server, as the bridge reaches it/)).toHaveValue(
      "http://myelin.bridges.svc:8008",
    );
    await expectNoAxeViolations(page, "wizard: deployment (kubernetes)");

    await page.getByRole("button", { name: "Continue" }).click();
    await page.getByRole("button", { name: "Continue" }).click(); // options -> review

    await expect(page.getByRole("heading", { name: "Review" })).toBeVisible();
    await expect(page.getByText(/Kubernetes \(bridges\)/)).toBeVisible();
    await page.getByRole("button", { name: "Create bridge" }).click();

    await expect(page.getByRole("heading", { name: /created/ })).toBeVisible();
    // Kubernetes deployments do not get a Compose snippet; they get the resource and its apply line.
    await expect(page.getByText("docker-compose.yaml")).toHaveCount(0);
    await expect(page.getByText("Bridge resource (Kubernetes)")).toBeVisible();
    await expect(page.getByText("kubectl apply -f linkedin-bridge.yaml")).toBeVisible();
    domGuard.assertClean();
  });

  test("namespace conflict is shown on the Identity step with a link", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/bridges/new");

    // WhatsApp's default id and user namespace collide with the seeded fixture.
    await page.getByRole("radio", { name: /WhatsApp/ }).click();
    await page.getByRole("button", { name: "Continue" }).click(); // -> identity
    await page.getByRole("button", { name: "Continue" }).click(); // -> deployment
    await page.getByRole("button", { name: "Continue" }).click(); // -> options
    await page.getByRole("button", { name: "Continue" }).click(); // -> review
    await page.getByRole("button", { name: "Create bridge" }).click();

    await expect(page.getByRole("heading", { name: "Identity" })).toBeVisible();
    const conflict = page.getByRole("alert");
    await expect(conflict).toContainText("already registered");
    await expectNoAxeViolations(page, "wizard: namespace conflict");
    domGuard.assertClean();
  });

  test("forbidden without bridges:write", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInReadOnly(page);

    // The Bridges list itself is readable...
    await page.getByRole("link", { name: "Bridges" }).first().click();
    await expect(page.getByRole("button", { name: "Add bridge" })).toBeDisabled();

    // ...but the wizard route itself refuses outright when linked to directly.
    await page.goto("/admin/bridges/new");
    await expect(page.getByText("bridges:write")).toBeVisible();
    await expect(page.getByText("Ask an administrator to grant it.")).toBeVisible();
    await expectNoAxeViolations(page, "wizard: forbidden");
    domGuard.assertClean();
  });
});
