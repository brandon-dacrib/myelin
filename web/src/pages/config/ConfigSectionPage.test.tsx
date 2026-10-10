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
import { configRevisions, configValues } from "@/mocks/data/config";
import { signIn, signOut } from "@/lib/auth";
import type { JsonValue } from "@/api/config-schema";

const pristineValues = structuredClone(configValues);
const pristineRevisions = structuredClone(configRevisions);

function renderSection(section: string) {
  const rootRoute = createRootRoute();
  const indexRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/configuration",
    component: () => <p>Configuration index</p>,
  });
  const sectionRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/configuration/$section",
    component: ConfigSectionPage,
  });
  const router = createRouter({
    routeTree: rootRoute.addChildren([indexRoute, sectionRoute]),
    history: createMemoryHistory({ initialEntries: [`/configuration/${section}`] }),
  }) as unknown as AnyRouter;
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
      <Toaster />
    </QueryClientProvider>,
  );
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

describe("ConfigSectionPage", () => {
  it("builds the form from the schema, not from a hardcoded field list", async () => {
    renderSection("rate_limits");

    // A field the page never names: it exists because the schema describes it.
    expect(await screen.findByText("Third party ID validation")).toBeInTheDocument();
    expect(screen.getByText("rate_limits.login.burst_count")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Rate limits" })).toBeInTheDocument();
  });

  it("says a reloadable section applies straight away", async () => {
    renderSection("rate_limits");
    expect(await screen.findByText("Reloadable")).toBeInTheDocument();
  });

  it("warns that a non-reloadable section waits for a restart", async () => {
    // Every administered `cluster` setting is read once, at startup.
    renderSection("cluster");
    expect(
      await screen.findByText("Changes here take effect at the next restart"),
    ).toBeInTheDocument();
  });

  it("says which settings of a mixed section apply at once", async () => {
    // `auth`: registration, the directory, token lifetimes and the password policy are hot; the
    // session secret and upstream providers wait for a restart.
    renderSection("auth");
    const legend = within(await screen.findByRole("region", { name: /^Most changes here/ }));
    expect(legend.getByText("Applies on save")).toBeInTheDocument();
    expect(legend.getByText("Needs a restart")).toBeInTheDocument();
    // Each class is explained once, at the top.
    expect(legend.getByText(/keeps using the old one until it next starts/)).toBeInTheDocument();
    // And each setting says which it is.
    const registration = document.getElementById("setting-enable_registration")!;
    expect(within(registration).getByText("Applies on save")).toBeInTheDocument();
    const providers = document.getElementById("setting-oidc_providers")!;
    expect(within(providers).getByText("Needs a restart")).toBeInTheDocument();
  });

  it("shows a setting the environment pins as read-only, with the reason", async () => {
    renderSection("server");

    expect(await screen.findByText("Pinned by environment")).toBeInTheDocument();
    expect(
      screen.getByText(/refuses to change it. Change it where the environment is set/),
    ).toBeInTheDocument();
    // Its value is shown, but there is no box to type a new one into.
    expect(screen.getByText("example.org")).toBeInTheDocument();
    expect(screen.queryByLabelText("Server name")).not.toBeInTheDocument();
  });

  it("explains why the bootstrap-only section cannot be edited here", async () => {
    renderSection("storage");

    expect(
      await screen.findByText("This section cannot be stored in the database"),
    ).toBeInTheDocument();
    expect(screen.getByText(/read before there is a database to read it from/)).toBeInTheDocument();
    expect(screen.getByText("/var/lib/myelin/data")).toBeInTheDocument();
  });

  it("renders a secret as set-and-hidden, with a way to replace but not to reveal", async () => {
    renderSection("auth");

    // `auth` has three secrets; every one of them reads the same way.
    expect((await screen.findAllByText("Set, hidden")).length).toBeGreaterThan(0);
    expect(
      screen.getByRole("button", { name: "Replace Registration shared secret" }),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /reveal|show/i })).not.toBeInTheDocument();
  });

  it("marks a setting the database changed, and offers to reset it", async () => {
    renderSection("federation");
    await screen.findByText("Client timeout");

    const row = document.getElementById("setting-client_timeout")!;
    expect(within(row).getByText("Changed from default")).toBeInTheDocument();
    // "From database" said no more than "Changed from default" does; only a value from the
    // bootstrap file or the environment, which the operator did not set here, gets a badge.
    expect(within(row).queryByText("From database")).not.toBeInTheDocument();
  });

  it("reviews the merge patch before saving, then saves it", async () => {
    const user = userEvent.setup();
    renderSection("federation");

    const timeout = await screen.findByLabelText("Client timeout");
    await user.clear(timeout);
    await user.type(timeout, "90s");
    await user.tab();

    await user.click(await screen.findByRole("button", { name: "Review and save" }));

    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText("45s")).toBeInTheDocument();
    expect(dialog.getByText("90s")).toBeInTheDocument();
    await user.click(dialog.getByText("Show the JSON Merge Patch this sends"));
    expect(dialog.getByText(/"client_timeout": "90s"/)).toBeInTheDocument();

    await user.click(dialog.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(configValues.federation.client_timeout).toBe("90s"));
    expect(await screen.findByText("Federation saved")).toBeInTheDocument();
    // The toast names the setting, and that it is only read at startup.
    expect(
      screen.getByText("Stored, and waiting for the next restart: Client timeout."),
    ).toBeInTheDocument();
  });

  it("says after saving a hot change that the running server has it now", async () => {
    const user = userEvent.setup();
    renderSection("rate_limits");

    const [burst] = await screen.findAllByLabelText(/^Burst count/);
    await user.clear(burst);
    await user.type(burst, "4");
    await user.tab();

    await user.click(await screen.findByRole("button", { name: "Review and save" }));
    const dialog = within(await screen.findByRole("dialog"));
    expect(
      dialog.getByText("Saving applies this to the running server straight away."),
    ).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Save changes" }));

    expect(await screen.findByText("Rate limits saved")).toBeInTheDocument();
    expect(
      screen.getByText(/^Applied to the running server: (Message|Login) · Burst count\.$/),
    ).toBeInTheDocument();
  });

  it("names the settings of a mixed section that apply at once", async () => {
    renderSection("federation");
    // Seven of the federation section's fourteen settings are hot (the destination sweep's
    // retention joined them on 2026-10-10), so the headline leads with the ones that apply on save.
    const legend = within(
      await screen.findByRole("region", {
        name: "Most changes here apply on save; some wait for a restart",
      }),
    );
    // Seven apply on save, seven need a restart.
    expect(legend.getAllByText("7 settings")).toHaveLength(2);
    const forget = document.getElementById("setting-forget_unused_destinations_after")!;
    expect(within(forget).getByText("Applies on save")).toBeInTheDocument();
    const allowlist = document.getElementById("setting-domain_allowlist")!;
    expect(within(allowlist).getByText("Applies on save")).toBeInTheDocument();
    // The bound on each server's waiting to-device and device-list updates is read per update.
    const durable = document.getElementById("setting-max_queued_durable_edus_per_destination")!;
    expect(within(durable).getByText("Applies on save")).toBeInTheDocument();
    const timeout = document.getElementById("setting-client_timeout")!;
    expect(within(timeout).getByText("Needs a restart")).toBeInTheDocument();
  });

  it("checks a change against the server without saving it", async () => {
    const user = userEvent.setup();
    renderSection("federation");

    const timeout = await screen.findByLabelText("Client timeout");
    await user.clear(timeout);
    await user.type(timeout, "forever");
    await user.tab();

    await user.click(await screen.findByRole("button", { name: "Review and save" }));
    const dialog = within(await screen.findByRole("dialog"));
    await user.click(dialog.getByRole("button", { name: "Check without saving" }));

    expect(await dialog.findByText("The server would reject this:")).toBeInTheDocument();
    expect(dialog.getByText(/expected a duration such as/)).toBeInTheDocument();
    // Nothing was written.
    expect(configValues.federation.client_timeout).toBe("45s");
  });

  it("lands a rejected save on the field it is about", async () => {
    const user = userEvent.setup();
    renderSection("rate_limits");

    // Eight rate-limit buckets each have a "Per second"; the first is `message`.
    // The label carries a required marker, hence the prefix match.
    const [perSecond] = await screen.findAllByLabelText(/^Per second/);
    await user.clear(perSecond);
    await user.type(perSecond, "0");
    await user.tab();

    await user.click(await screen.findByRole("button", { name: "Review and save" }));
    const dialog = within(await screen.findByRole("dialog"));
    await user.click(dialog.getByRole("button", { name: "Save changes" }));

    // The summary at the top of the page names the setting…
    expect(await screen.findByText("The server rejected 1 setting")).toBeInTheDocument();
    // …and the message is attached to the field itself, as an alert.
    const alerts = await screen.findAllByRole("alert");
    expect(alerts.some((el) => /must be greater than zero/.test(el.textContent ?? ""))).toBe(true);
    expect(configValues.rate_limits.message).toEqual({ per_second: 0.5, burst_count: 25 });
  });

  it("offers to re-read when someone else has changed the section", async () => {
    const user = userEvent.setup();
    renderSection("appservices");

    const threshold = await screen.findByLabelText("Tracking failure threshold");
    await user.clear(threshold);
    await user.type(threshold, "20");
    await user.tab();

    // Another operator saves first.
    configRevisions.appservices += 1;

    await user.click(await screen.findByRole("button", { name: "Review and save" }));
    const dialog = within(await screen.findByRole("dialog"));
    await user.click(dialog.getByRole("button", { name: "Save changes" }));

    expect(await screen.findByText("Someone else changed this section")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Re-read the server's copy, keep my edits" }),
    ).toBeInTheDocument();
    expect(configValues.appservices.tracking_failure_threshold).toBe(50);
  });

  it("edits no section's settings as text (decision 0010)", async () => {
    for (const section of Object.keys(pristineValues)) {
      const { container, unmount } = renderSection(section);
      await screen.findByRole("heading", { level: 1 });
      await waitFor(() => expect(container.querySelector("[id^='setting-']")).not.toBeNull());
      expect(container.querySelector("textarea")).toBeNull();
      expect(screen.queryByLabelText(/as JSON/)).not.toBeInTheDocument();
      unmount();
    }
  });

  it("adds a thumbnail size through its own form, and saves the whole list", async () => {
    const user = userEvent.setup();
    renderSection("media");

    await user.click(await screen.findByRole("button", { name: "Add thumbnail size" }));
    const added = within(screen.getByRole("group", { name: /^Thumbnail size 6/ }));
    await user.type(added.getByLabelText(/^Width/), "1024");
    await user.type(added.getByLabelText(/^Height/), "768");
    await user.tab();

    await user.click(await screen.findByRole("button", { name: "Review and save" }));
    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText("5 entries")).toBeInTheDocument();
    expect(dialog.getByText("6 entries")).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(configValues.media.thumbnail_sizes).toHaveLength(6));
    expect((configValues.media.thumbnail_sizes as JsonValue[])[5]).toEqual({
      width: 1024,
      height: 768,
      method: "crop",
    });
  });

  it("lands an error about one list entry on the list, saying which entry", async () => {
    const user = userEvent.setup();
    renderSection("media");
    const before = (configValues.media.thumbnail_sizes as JsonValue[]).length;

    await user.click(await screen.findByRole("button", { name: "Add thumbnail size" }));
    const added = within(
      screen.getByRole("group", { name: new RegExp(`^Thumbnail size ${before + 1}`) }),
    );
    await user.type(added.getByLabelText(/^Width/), "64");

    await user.click(await screen.findByRole("button", { name: "Review and save" }));
    const dialog = within(await screen.findByRole("dialog"));
    await user.click(dialog.getByRole("button", { name: "Save changes" }));

    expect(await screen.findByText("The server rejected 1 setting")).toBeInTheDocument();
    const row = document.getElementById("setting-thumbnail_sizes")!;
    expect(within(row).getByRole("alert")).toHaveTextContent(
      `Entry ${before + 1}, height: must be a whole number of pixels, at least 1`,
    );
    expect(configValues.media.thumbnail_sizes).toHaveLength(before);
  });

  it("shows listeners as set at install, with no way to edit them", async () => {
    renderSection("listeners");
    expect(await screen.findByText("This section cannot be stored in the database")).toBeVisible();
    const row = await waitFor(() => {
      const el = document.getElementById("setting-listeners");
      if (!el) throw new Error("no row yet");
      return el;
    });
    expect(within(row).getByText("Per replica (file or environment)")).toBeInTheDocument();
    expect(within(row).queryByRole("button", { name: "Add listener" })).not.toBeInTheDocument();
  });

  it("marks a bootstrap setting inside an administered section", async () => {
    renderSection("server");
    const row = await waitFor(() => {
      const el = document.getElementById("setting-signing_key_path");
      if (!el) throw new Error("no row yet");
      return el;
    });
    expect(within(row).getByText("Per replica (file or environment)")).toBeInTheDocument();
    expect(row.textContent).toContain(
      "the bootstrap file, an HS__ environment variable or the Helm values",
    );
    expect(within(row).queryByRole("textbox")).not.toBeInTheDocument();
  });

  it("shows a structured setting it may not change as names and values, not JSON", async () => {
    await signIn(["admin:read"]);
    renderSection("listeners");

    const row = await waitFor(() => {
      const el = document.getElementById("setting-listeners");
      if (!el) throw new Error("no row yet");
      return el;
    });
    expect(within(row).getByText("Bind addresses")).toBeInTheDocument();
    expect(within(row).getByText("8008")).toBeInTheDocument();
    expect(row.textContent).not.toContain("{");
    expect(within(row).queryByRole("button", { name: "Add listener" })).not.toBeInTheDocument();
  });

  it("stages a reset as a removal rather than a null", async () => {
    const user = userEvent.setup();
    renderSection("federation");

    await screen.findByText("Custom CA certificates");
    await user.click(
      screen.getByRole("button", { name: "Reset Custom CA certificates to its default" }),
    );

    expect(await screen.findByText("Will reset to default")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = within(await screen.findByRole("dialog"));
    await user.click(dialog.getByText("Show the JSON Merge Patch this sends"));
    expect(dialog.getByText(/"custom_ca_certificates": null/)).toBeInTheDocument();

    await user.click(dialog.getByRole("button", { name: "Save changes" }));
    await waitFor(() => expect(configValues.federation.custom_ca_certificates).toBeUndefined());
  });
});
