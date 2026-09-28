import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  RouterProvider,
  type AnyRouter,
} from "@tanstack/react-router";
import { ConfigSectionPage } from "./ConfigSectionPage";
import { Toaster } from "@/components/ui/toast/Toaster";
import { configHistory, configRevisions, configValues } from "@/mocks/data/config";
import { signIn, signOut } from "@/lib/auth";

const pristineValues = structuredClone(configValues);
const pristineRevisions = structuredClone(configRevisions);

function renderSection(section: string, search = "") {
  const rootRoute = createRootRoute();
  const sectionRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/configuration/$section",
    component: ConfigSectionPage,
  });
  const history = createMemoryHistory({ initialEntries: [`/configuration/${section}${search}`] });
  const router = createRouter({
    routeTree: rootRoute.addChildren([sectionRoute]),
    history,
  }) as unknown as AnyRouter;
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
      <Toaster />
    </QueryClientProvider>,
  );
  return { history };
}

async function historySection() {
  const heading = await screen.findByRole("heading", { name: "Change history" });
  return heading.closest("section") as HTMLElement;
}

/** The history entry for one revision (its `<li>`). */
async function entry(revision: number) {
  const section = await historySection();
  const label = await within(section).findByText(`revision ${revision}`);
  return label.closest("li") as HTMLElement;
}

beforeEach(async () => {
  for (const [name, values] of Object.entries(pristineValues)) {
    configValues[name] = structuredClone(values);
  }
  for (const [name, revision] of Object.entries(pristineRevisions)) {
    configRevisions[name] = revision;
  }
  await signIn(["admin:read", "admin:write"]);
});

afterEach(() => {
  signOut();
});

describe("ConfigHistory", () => {
  it("shows which setting each change touched, before and after, by whom", async () => {
    renderSection("rate_limits");
    const latest = await entry(7);
    expect(within(latest).getByText("@admin:example.org")).toBeInTheDocument();
    const line = within(latest).getByTitle("rate_limits.message.burst_count");
    expect(line).toHaveTextContent(/Message · Burst count:\s*20\s*to\s*25/);
    expect(within(await entry(6)).getByTitle("rate_limits.login.per_second")).toHaveTextContent(
      /Login · Per second:\s*0\.1\s*to\s*0\.17/,
    );
  });

  it("says when a change predates prior values, and offers no revert for it", async () => {
    renderSection("rate_limits");
    const legacy = await entry(5);
    expect(within(legacy).getByText("Earlier values not recorded")).toBeInTheDocument();
    expect(within(legacy).queryByRole("button", { name: /Revert/ })).not.toBeInTheDocument();
    expect(within(legacy).getByTitle("rate_limits.enabled")).toHaveTextContent(/not recorded/);
  });

  it("never shows a secret, only that it changed", async () => {
    renderSection("auth");
    const rotated = await entry(4);
    expect(within(rotated).getByTitle("auth.registration_shared_secret")).toHaveTextContent(
      /set, hidden\s*to\s*set, hidden/,
    );
  });

  it("reverts a change after saying what will change, and records the revert", async () => {
    const user = userEvent.setup();
    renderSection("rate_limits");
    await user.click(within(await entry(7)).getByRole("button", { name: "Revert revision 7" }));
    const dialog = await screen.findByRole("dialog", { name: "Revert revision 7?" });
    expect(within(dialog).getByTitle("rate_limits.message.burst_count")).toHaveTextContent(
      /25\s*to\s*20/,
    );
    expect(within(dialog).getByTitle("rate_limits.login.burst_count")).toHaveTextContent(
      /5\s*to\s*3/,
    );
    await user.click(within(dialog).getByRole("button", { name: "Revert" }));

    expect(await screen.findByText("Revision 7 reverted")).toBeInTheDocument();
    await waitFor(() =>
      expect(configValues.rate_limits.message).toMatchObject({ burst_count: 20 }),
    );
    const revert = await entry(8);
    expect(within(revert).getByText("Reverts revision 7")).toBeInTheDocument();
  });

  it("names the later change a revert would undo, and goes ahead only when asked", async () => {
    configHistory.push({
      revision: 8,
      section: "rate_limits",
      patch: { login: { per_second: 1 } },
      actor: "@other:example.org",
      at: new Date().toISOString(),
      before: { "/login/per_second": 0.17 },
      reverts: null,
    });
    configRevisions.rate_limits = 8;
    const user = userEvent.setup();
    renderSection("rate_limits");
    await user.click(within(await entry(6)).getByRole("button", { name: "Revert revision 6" }));
    const dialog = await screen.findByRole("dialog", { name: "Revert revision 6?" });
    await user.click(within(dialog).getByRole("button", { name: "Revert" }));

    const warning = await within(dialog).findByRole("alert");
    expect(warning).toHaveTextContent("Reverting undoes them too");
    expect(warning).toHaveTextContent(/Login · Per second — changed again in revision 8 by @other/);
    await user.click(within(dialog).getByRole("button", { name: "Revert anyway" }));
    expect(await screen.findByText("Revision 6 reverted")).toBeInTheDocument();
    await waitFor(() => expect(configValues.rate_limits.login).toMatchObject({ per_second: 0.1 }));
  });

  it("offers no revert to a read-only token", async () => {
    await signIn(["admin:read"]);
    renderSection("rate_limits");
    await entry(7);
    expect(screen.queryByRole("button", { name: /Revert revision/ })).not.toBeInTheDocument();
  });

  it("pages through older changes, keeping the page in the URL", async () => {
    for (let revision = 8; revision <= 19; revision += 1) {
      configHistory.push({
        revision,
        section: "federation",
        patch: { client_timeout: `${revision}s` },
        actor: "@admin:example.org",
        at: new Date().toISOString(),
        before: { "/client_timeout": `${revision - 1}s` },
        reverts: null,
      });
    }
    const user = userEvent.setup();
    const { history } = renderSection("federation");
    await entry(19);
    expect(screen.queryByText("revision 3")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Older changes" }));
    await entry(3);
    expect(history.location.search).toContain("history=r10");
    expect(screen.queryByText("revision 19")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Newer changes" }));
    await entry(19);
  });

  it("opens on the page of history the URL names", async () => {
    for (let revision = 8; revision <= 19; revision += 1) {
      configHistory.push({
        revision,
        section: "federation",
        patch: { client_timeout: `${revision}s` },
        actor: "@admin:example.org",
        at: new Date().toISOString(),
        before: { "/client_timeout": `${revision - 1}s` },
        reverts: null,
      });
    }
    renderSection("federation", "?history=r10");
    await entry(9);
    expect(screen.queryByText("revision 19")).not.toBeInTheDocument();
  });
});
