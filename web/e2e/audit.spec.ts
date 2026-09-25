import { readFile } from "node:fs/promises";
import { test, expect } from "@playwright/test";
import {
  signInAsOperator,
  signInReadOnly,
  expectNoAxeViolations,
  installDomNestingGuard,
} from "./utils";

test("audit filters, pagination, detail links and browser history", async ({ page }) => {
  const guard = installDomNestingGuard(page);
  await signInAsOperator(page);
  await page.getByRole("link", { name: "Audit log", exact: true }).click();
  await expect(page.getByRole("link", { name: "Paused bridge", exact: true })).toBeVisible();
  await expectNoAxeViolations(page, "audit log");
  await page.getByRole("button", { name: "Next", exact: true }).click();
  await expect(page).toHaveURL(/cursor=/);
  await expect(page.getByRole("link", { name: "Created the first administrator" })).toBeVisible();
  await page.getByRole("button", { name: "Previous", exact: true }).click();
  await expect(page.getByRole("link", { name: "Paused bridge", exact: true })).toBeVisible();

  await page.getByLabel("Action", { exact: true }).fill("appservices.pause");
  await page.getByRole("button", { name: "Apply filters" }).click();
  await expect(page).toHaveURL(/action=appservices.pause/);
  await expect(page).not.toHaveURL(/cursor=/);
  await expect(page.getByRole("link", { name: "Suspended user", exact: true })).toHaveCount(0);
  await page.getByRole("link", { name: "Paused bridge", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Paused bridge" })).toBeVisible();
  await expect(page.getByText("/paused", { exact: true })).toBeVisible();
  await expect(page.getByText("false", { exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "discord", exact: true })).toHaveAttribute(
    "href",
    /\/bridges\/discord$/,
  );
  await expectNoAxeViolations(page, "audit entry");
  await page.getByRole("link", { name: "Back to audit log" }).click();
  await expect(page.getByLabel("Action", { exact: true })).toHaveValue("appservices.pause");
  await page.getByRole("button", { name: "Clear filters" }).click();
  await expect(page.getByLabel("Action", { exact: true })).toHaveValue("");
  await page.goBack();
  await expect(page.getByLabel("Action", { exact: true })).toHaveValue("appservices.pause");
  guard.assertClean();
});

test("read-only operator can inspect failures and export the applied date range", async ({
  page,
}) => {
  await signInReadOnly(page);
  await page.goto("/admin/audit?outcome=failure&recorded_after=2000-01-01T00%3A00%3A00Z");
  await expect(page.getByRole("link", { name: "Created user", exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "Paused bridge", exact: true })).toHaveCount(0);
  await page.getByRole("link", { name: "Created user", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("@alice:example.org already exists");
  await expect(page.getByText("Failed · HTTP 409")).toBeVisible();
  await page.getByRole("link", { name: "Back to audit log" }).click();
  const downloadPromise = page.waitForEvent("download");
  const requestPromise = page.waitForRequest((request) =>
    request.url().includes("/audit-log/export"),
  );
  await page.getByRole("button", { name: "Export NDJSON" }).click();
  const request = await requestPromise;
  expect(request.headers().authorization).toMatch(/^Bearer /);
  expect(new URL(request.url()).searchParams.get("outcome")).toBeNull();
  expect(new URL(request.url()).searchParams.get("recorded_after")).toBe(
    "2000-01-01T00:00:00.000Z",
  );
  const download = await downloadPromise;
  const entries = (await readFile((await download.path())!, "utf8"))
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line));
  expect(entries.some((entry) => entry.outcome.status === 409)).toBe(true);
  expect(entries.some((entry) => entry.outcome.status === 200)).toBe(true);

  await page.getByLabel("Before (UTC)").fill("1999-01-01T00:00");
  await page.getByRole("button", { name: "Apply filters" }).click();
  await expect(page.getByRole("alert")).toContainText("must be after its start");
  await page.getByRole("button", { name: "Clear filters" }).click();
  await page.getByLabel("Actor ID").fill("nobody");
  await page.getByRole("button", { name: "Apply filters" }).click();
  await expect(page.getByText("No matching audit entries", { exact: true })).toBeVisible();
});

test("phone layout, replay links, missing entries and export errors", async ({ page }) => {
  const guard = installDomNestingGuard(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await signInAsOperator(page);
  await page.goto("/admin/audit");
  await expect(page.getByRole("link", { name: "Paused bridge", exact: true })).toBeVisible();
  await expectNoAxeViolations(page, "audit log on phone");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(
    true,
  );
  await page.goto("/admin/audit/audit-7");
  await expect(page.getByText("Replayed request", { exact: true })).toBeVisible();
  await page.getByRole("link", { name: "audit-8", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Reset password" })).toBeVisible();
  await expect(page.getByText("Replayed request", { exact: true })).toHaveCount(0);
  await expectNoAxeViolations(page, "audit entry on phone");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(
    true,
  );
  await page.goto("/admin/audit/missing-entry");
  await expect(page.getByText("Audit entry not found", { exact: true }).first()).toBeVisible();
  await page.getByRole("link", { name: "Back to audit log" }).click();
  await page.evaluate(() => window.__hsAdminMock!.setForceProblem("/api/v1/audit-log/export", 503));
  await page.getByRole("button", { name: "Export NDJSON" }).click();
  await expect(page.getByText("isn't connected to a data source on this server yet")).toBeVisible();
  guard.assertClean();
});
