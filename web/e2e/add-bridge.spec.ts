import { test, expect, type Page } from "@playwright/test";
import {
  signInAsOperator,
  signInReadOnly,
  expectNoAxeViolations,
  installDomNestingGuard,
} from "./utils";

// Registering a bridge you run yourself (flows.md flow 1, as it was before RFC 0017): the
// render-and-register path, now reached from Bridges > Registrations. Bridges this server runs
// are offered instead (offer-bridge.spec.ts). Covers the happy path, the namespace-conflict
// branch and the forbidden branch, with an axe pass at every step (accessibility.md) and a
// React DOM-nesting guard (see installDomNestingGuard's doc comment).

async function openRegistrations(page: Page) {
  await page.getByRole("link", { name: "Bridges" }).first().click();
  await expect(page.getByRole("heading", { name: "Bridges" })).toBeVisible();
  await page.getByRole("link", { name: "Registrations" }).click();
  await expect(page).toHaveURL(/\/bridges\/registrations$/);
}

test.describe("Register a bridge you run yourself", () => {
  test("happy path", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await expectNoAxeViolations(page, "overview");

    await openRegistrations(page);
    await expectNoAxeViolations(page, "registrations list");

    await page.getByRole("button", { name: "Register a bridge you run yourself" }).click();
    await expect(page).toHaveURL(/\/bridges\/registrations\/new/);
    await expect(
      page.getByRole("heading", { name: "Register a bridge you run yourself" }),
    ).toBeVisible();
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

    // Self-managed only: no Kubernetes choice (a bridge this server runs is an offering).
    await expect(page.getByRole("heading", { name: "Addresses" })).toBeVisible();
    await expect(page.getByRole("radio", { name: "Kubernetes" })).toHaveCount(0);
    await expect(page.getByLabel(/This server, as the bridge reaches it/)).toHaveValue(
      "http://myelin:8008",
    );
    await expectNoAxeViolations(page, "wizard: addresses");
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

    await page.getByRole("button", { name: "Register bridge" }).click();

    await expect(page.getByRole("heading", { name: /Bridge Bluesky created/ })).toBeVisible();
    await expect(page.getByText("config.yaml", { exact: true })).toBeVisible();
    await expect(page.getByText("registration.yaml", { exact: true })).toBeVisible();
    await expect(page.getByText("docker-compose.yaml")).toBeVisible();
    // Nothing about a Bridge resource or an operator: this bridge is run by hand.
    await expect(page.getByText(/kubectl|operator does the rest/)).toHaveCount(0);
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
    // And who has signed in: a shared mautrix bridge is asked about a person the operator names.
    await expect(page.getByRole("heading", { name: "Who has signed in" })).toBeVisible();
    await page.getByLabel("Matrix user").fill("@bob:example.org");
    await page.getByRole("button", { name: "Check" }).click();
    await expect(page.getByText("@bob:example.org is not signed in.")).toBeVisible();
    await expectNoAxeViolations(page, "bridge detail: sign in");
    domGuard.assertClean();
  });

  test("namespace conflict is shown on the Identity step", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/bridges/registrations/new");

    // WhatsApp's default id and user namespace collide with the seeded fixture.
    await page.getByRole("radio", { name: /WhatsApp/ }).click();
    await page.getByRole("button", { name: "Continue" }).click(); // -> identity
    await page.getByRole("button", { name: "Continue" }).click(); // -> addresses
    await page.getByRole("button", { name: "Continue" }).click(); // -> options
    await page.getByRole("button", { name: "Continue" }).click(); // -> review
    await page.getByRole("button", { name: "Register bridge" }).click();

    await expect(page.getByRole("heading", { name: "Identity" })).toBeVisible();
    const conflict = page.getByRole("alert");
    await expect(conflict).toContainText("already registered");
    await expectNoAxeViolations(page, "wizard: namespace conflict");
    domGuard.assertClean();
  });

  test("forbidden without bridges:write", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInReadOnly(page);

    // The lists themselves are readable...
    await page.getByRole("link", { name: "Bridges" }).first().click();
    await expect(page.getByRole("button", { name: "Offer a bridge" })).toBeDisabled();
    await page.getByRole("link", { name: "Registrations" }).click();
    await expect(
      page.getByRole("button", { name: "Register a bridge you run yourself" }),
    ).toBeDisabled();

    // ...but the wizards refuse outright when linked to directly.
    for (const path of ["/admin/bridges/registrations/new", "/admin/bridges/new"]) {
      await page.goto(path);
      await expect(page.getByText("bridges:write")).toBeVisible();
      await expect(page.getByText("Ask an administrator to grant it.")).toBeVisible();
    }
    await expectNoAxeViolations(page, "wizard: forbidden");
    domGuard.assertClean();
  });
});
