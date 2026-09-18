import { test, expect } from "@playwright/test";
import { signInAsOperator, signInReadOnly, expectNoAxeViolations } from "./utils";

// flows.md flow 1: add a bridge. Covers the happy paths (self-managed and
// Kubernetes), the namespace-conflict branch, and the forbidden branch, with
// an axe pass at every step (accessibility.md).

test.describe("Add a bridge", () => {
  test("happy path: self-managed", async ({ page }) => {
    await signInAsOperator(page);
    await expectNoAxeViolations(page, "overview");

    await page.getByRole("link", { name: "Bridges" }).first().click();
    await expect(page.getByRole("heading", { name: "Bridges" })).toBeVisible();
    await expectNoAxeViolations(page, "bridges list");

    await page.getByRole("button", { name: "Add bridge" }).click();
    await expect(page).toHaveURL(/\/bridges\/new/);
    await expectNoAxeViolations(page, "wizard: kind");

    await page.getByRole("radio", { name: /Zulip/ }).click();
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
    await expectNoAxeViolations(page, "wizard: options");
    await page.getByRole("button", { name: "Continue" }).click();

    await expect(page.getByRole("heading", { name: "Review" })).toBeVisible();
    await expectNoAxeViolations(page, "wizard: review");
    await expect(page.getByText("registration.yaml (preview)")).toBeVisible();

    await page.getByRole("button", { name: "Create bridge" }).click();

    await expect(page.getByRole("heading", { name: /Bridge Zulip created/ })).toBeVisible();
    await expect(page.getByText("registration.yaml", { exact: true })).toBeVisible();
    await expect(page.getByText("docker-compose.yaml")).toBeVisible();
    await expectNoAxeViolations(page, "wizard: created");

    await page.getByRole("button", { name: "Open bridge" }).click();
    await expect(page).toHaveURL(/\/bridges\/zulip$/);
    await expect(page.getByRole("heading", { name: "Zulip" })).toBeVisible();
    await expect(page.getByText("Unknown")).toBeVisible();
    await expectNoAxeViolations(page, "bridge detail");
  });

  test("happy path: kubernetes", async ({ page }) => {
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

    await page.getByRole("radio", { name: /IRC \(mautrix\)/ }).click();
    await page.getByRole("button", { name: "Continue" }).click();
    await page.getByRole("button", { name: "Continue" }).click(); // identity

    await expect(page.getByRole("heading", { name: "Deployment" })).toBeVisible();
    await expect(page.getByRole("radio", { name: "Kubernetes" })).toBeVisible();
    await page.getByRole("radio", { name: "Kubernetes" }).click();
    await expect(page.getByLabel("Namespace")).toBeVisible();
    await expectNoAxeViolations(page, "wizard: deployment (kubernetes)");

    await page.getByRole("button", { name: "Continue" }).click();
    await page.getByRole("button", { name: "Continue" }).click(); // options -> review

    await expect(page.getByRole("heading", { name: "Review" })).toBeVisible();
    await expect(page.getByText(/Kubernetes \(bridges\)/)).toBeVisible();
    await page.getByRole("button", { name: "Create bridge" }).click();

    await expect(page.getByRole("heading", { name: /created/ })).toBeVisible();
    // Kubernetes deployments do not get a Compose snippet.
    await expect(page.getByText("docker-compose.yaml")).toHaveCount(0);
  });

  test("namespace conflict is shown on the Identity step with a link", async ({ page }) => {
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
  });

  test("forbidden without bridges:write", async ({ page }) => {
    await signInReadOnly(page);

    // The Bridges list itself is readable...
    await page.getByRole("link", { name: "Bridges" }).first().click();
    await expect(page.getByRole("button", { name: "Add bridge" })).toBeDisabled();

    // ...but the wizard route itself refuses outright when linked to directly.
    await page.goto("/admin/bridges/new");
    await expect(page.getByText("bridges:write")).toBeVisible();
    await expect(page.getByText("Ask an administrator to grant it.")).toBeVisible();
    await expectNoAxeViolations(page, "wizard: forbidden");
  });
});
