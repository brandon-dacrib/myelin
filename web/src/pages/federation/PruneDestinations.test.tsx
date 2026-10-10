import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { federationDestinations } from "@/mocks/data/dashboard";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { Toaster } from "@/components/ui/toast/Toaster";
import { FederationPage } from "../FederationPage";
import { validateFederationSearch } from "./federation-search";

/**
 * The prune panel (decision 0042): the sweep's setting in words with a link to change it, a
 * preview before anything is forgotten, and the report both times.
 */
function open() {
  return renderRoutes(
    [
      {
        path: "/federation",
        component: () => (
          <>
            <FederationPage />
            <Toaster />
          </>
        ),
        validateSearch: validateFederationSearch,
      },
    ],
    "/federation",
    ["/federation/$serverName", "/configuration/$section"],
  );
}

function panel() {
  return within(screen.getByRole("region", { name: "Servers this one shares no room with" }));
}

describe("Pruning destinations", () => {
  beforeEach(async () => {
    await signIn();
  });
  afterEach(() => {
    server.resetHandlers();
    signOut();
  });

  it("says what the hourly sweep does and when, and links to the setting", async () => {
    open();
    await screen.findByRole("table", { name: "Federation destinations" });
    await waitFor(() => expect(panel().getByText("1 week")).toBeInTheDocument());
    expect(
      panel().getByText(/The hourly sweep forgets each one once it has had nothing queued/),
    ).toBeInTheDocument();
    expect(panel().getByRole("link", { name: "Forget unused destinations after" })).toHaveAttribute(
      "href",
      "/configuration/federation#setting-forget_unused_destinations_after",
    );
  });

  it("says when the sweep is off", async () => {
    server.use(
      http.get("/api/v1/config/federation", () =>
        HttpResponse.json({
          name: "federation",
          reloadable: true,
          source: "database",
          last_reloaded_at: null,
          values: { forget_unused_destinations_after: "0s" },
        }),
      ),
    );
    open();
    await screen.findByRole("table", { name: "Federation destinations" });
    await waitFor(() => expect(panel().getByText("off")).toBeInTheDocument());
    expect(panel().getByText(/so only a prune here forgets them/)).toBeInTheDocument();
  });

  it("previews a prune, names both sides and why, then forgets what the preview named", async () => {
    open();
    await screen.findByRole("table", { name: "Federation destinations" });
    expect(await screen.findByText("65 servers")).toBeInTheDocument();
    await userEvent.click(panel().getByRole("button", { name: "Preview prune" }));

    const preview = within(await screen.findByRole("region", { name: "If you prune now" }));
    expect(preview.getByText("8 servers would be forgotten; 57 kept.")).toBeInTheDocument();
    expect(preview.getByText("8: Shares no room, nothing queued")).toBeInTheDocument();
    expect(preview.getByText("57: Shares a room")).toBeInTheDocument();
    await userEvent.click(preview.getByText("Which servers"));
    expect(preview.getByText("srv-07.example.net")).toBeInTheDocument();
    // Nothing has gone yet.
    expect(federationDestinations).toHaveLength(65);

    await userEvent.click(preview.getByRole("button", { name: "Forget 8 servers" }));
    const done = within(await screen.findByRole("region", { name: "Pruned" }));
    expect(done.getByText("8 servers were forgotten; 57 kept.")).toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "If you prune now" })).not.toBeInTheDocument();
    expect(await screen.findByText("Forgot 8 servers")).toBeInTheDocument();
    await waitFor(() => expect(screen.getByText("57 servers")).toBeInTheDocument());
    expect(federationDestinations).toHaveLength(57);
  });

  it("asks the server to forget failing servers too when told to, and says when nothing would go", async () => {
    const bodies: unknown[] = [];
    server.use(
      http.post("/api/v1/federation/destinations/prune", async ({ request }) => {
        bodies.push([new URL(request.url).search, await request.json()]);
        return HttpResponse.json({
          dry_run: true,
          forgotten: { count: 0, by_reason: {}, servers: [] },
          kept: {
            count: 1,
            by_reason: { failing_recently: 1 },
            servers: [
              {
                server_name: "mozilla.org",
                reason: "failing_recently",
                detail: "failing for 2 hours, not yet for 7 days",
              },
            ],
          },
        });
      }),
    );
    open();
    await screen.findByRole("table", { name: "Federation destinations" });
    // Opened from the keyboard, the way jsdom can open a Radix select.
    panel().getByRole("combobox").focus();
    await userEvent.keyboard("{Enter}");
    await userEvent.click(await screen.findByRole("option", { name: "Failing for a week" }));
    await userEvent.click(panel().getByRole("button", { name: "Preview prune" }));

    const preview = within(await screen.findByRole("region", { name: "If you prune now" }));
    expect(preview.getByText(/Nothing would be forgotten/)).toBeInTheDocument();
    expect(preview.getByText("1: Not failing for long enough")).toBeInTheDocument();
    expect(preview.queryByRole("button", { name: /^Forget/ })).not.toBeInTheDocument();
    expect(bodies).toEqual([["?dry_run=true", { failing_for: "7d" }]]);
  });
});
