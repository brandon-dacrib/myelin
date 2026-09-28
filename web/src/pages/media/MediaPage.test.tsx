import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  RouterProvider,
  type AnyRouter,
} from "@tanstack/react-router";
import { server } from "@/mocks/node";
import { findMedia } from "@/mocks/data/media";
import { signIn, signOut, type Scope } from "@/lib/auth";
import { MediaPage } from "./MediaPage";
import { validateMediaSearch } from "./media-search";

function renderMedia(initialPath = "/media") {
  const rootRoute = createRootRoute();
  const router = createRouter({
    routeTree: rootRoute.addChildren([
      createRoute({
        getParentRoute: () => rootRoute,
        path: "/media",
        validateSearch: validateMediaSearch,
        component: MediaPage,
      }),
    ]),
    history: createMemoryHistory({ initialEntries: [initialPath] }),
  }) as unknown as AnyRouter;
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return { router };
}

async function rowOf(name: string) {
  const table = await screen.findByRole("table");
  const button = await within(table).findByRole("button", { name });
  return within(button.closest("tr")!);
}

async function openDetail(name: string) {
  const user = userEvent.setup();
  await user.click(await within(await screen.findByRole("table")).findByRole("button", { name }));
  return { user, sheet: within(await screen.findByRole("dialog", { name })) };
}

beforeEach(async () => {
  await signIn();
  // jsdom has no object URLs; a preview only needs one to exist.
  URL.createObjectURL = vi.fn(() => "blob:preview");
  URL.revokeObjectURL = vi.fn();
});
afterEach(() => signOut());

describe("Media", () => {
  it("lists uploads and cached copies with who, how big and whether they are withheld", async () => {
    renderMedia();
    const vacation = await rowOf("vacation.jpg");
    expect(vacation.getByText("@alice:example.org")).toBeInTheDocument();
    expect(vacation.getByText("2.3 MiB")).toBeInTheDocument();
    expect(vacation.getByText("mxc://example.org/vacationPhotoAbc123")).toBeInTheDocument();

    const exe = await rowOf("definitely-not-malware.exe");
    expect(exe.getByText("Quarantined")).toBeInTheDocument();
    expect(exe.getByLabelText("Quarantined, no preview")).toBeInTheDocument();

    const avatar = await rowOf("avatar.png");
    expect(avatar.getByText("Remote")).toBeInTheDocument();
    expect(avatar.getByText("matrix.org")).toBeInTheDocument();
    expect((await rowOf("team-logo.png")).getByText("Protected")).toBeInTheDocument();
    // Newest first by default.
    const rows = within(screen.getByRole("table")).getAllByRole("row");
    expect(within(rows[1]!).getByText("definitely-not-malware.exe")).toBeInTheDocument();
  });

  it("previews images through the authenticated media API with the operator's token", async () => {
    const seen: string[] = [];
    server.events.on("request:start", ({ request }) => {
      if (request.url.includes("/_matrix/client/v1/media/thumbnail/")) {
        seen.push(`${new URL(request.url).pathname} ${request.headers.get("authorization")}`);
      }
    });
    renderMedia();
    const vacation = await rowOf("vacation.jpg");
    expect(await vacation.findByAltText("Preview of vacation.jpg")).toHaveAttribute(
      "src",
      "blob:preview",
    );
    expect(seen).toContainEqual(
      expect.stringMatching(
        /^\/_matrix\/client\/v1\/media\/thumbnail\/example\.org\/vacationPhotoAbc123 Bearer .+/,
      ),
    );
    // Neither a PDF nor quarantined media is asked for.
    expect(seen.some((s) => s.includes("oldReportJkl012"))).toBe(false);
    expect(seen.some((s) => s.includes("notMalwareDef456"))).toBe(false);
    server.events.removeAllListeners();
  });

  it("filters from the address and searches", async () => {
    renderMedia("/media?status=quarantined");
    await rowOf("definitely-not-malware.exe");
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(2);
  });

  it("searches by name, uploader or type", async () => {
    renderMedia();
    await rowOf("vacation.jpg");
    const user = userEvent.setup();
    await user.type(screen.getByLabelText("Search by file name, uploader, type or ID"), "carol");
    await user.click(screen.getByRole("button", { name: "Search" }));
    await waitFor(() =>
      expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(2),
    );
    expect(await rowOf("q1-report.pdf")).toBeTruthy();
  });

  it("quarantines after saying what that does, and lifts it again", async () => {
    renderMedia();
    const { user, sheet } = await openDetail("vacation.jpg");
    await user.click(sheet.getByRole("button", { name: "Quarantine" }));
    const confirm = within(await screen.findByRole("dialog", { name: "Quarantine vacation.jpg?" }));
    expect(confirm.getByText(/Nobody but an administrator can view or download it/)).toBeVisible();
    await user.click(confirm.getByRole("button", { name: "Quarantine" }));

    await waitFor(() =>
      expect(findMedia("example.org", "vacationPhotoAbc123")?.quarantined).toBe(true),
    );
    const detail = within(await screen.findByRole("dialog", { name: "vacation.jpg" }));
    expect(await detail.findByText("Quarantined")).toBeInTheDocument();
    // Quarantined media can't be protected until the quarantine is lifted.
    expect(detail.getByRole("button", { name: "Protect" })).toBeDisabled();

    await user.click(detail.getByRole("button", { name: "Lift quarantine" }));
    await waitFor(() =>
      expect(findMedia("example.org", "vacationPhotoAbc123")?.quarantined).toBe(false),
    );
  });

  it("won't quarantine protected media, and can unprotect it", async () => {
    renderMedia();
    const { user, sheet } = await openDetail("team-logo.png");
    expect(sheet.getByRole("button", { name: "Quarantine" })).toBeDisabled();
    expect(sheet.getByText(/Protected media can't be quarantined/)).toBeInTheDocument();
    await user.click(sheet.getByRole("button", { name: "Unprotect" }));
    await waitFor(() => expect(findMedia("example.org", "teamLogoGhi789")?.protected).toBe(false));
    expect(await sheet.findByRole("button", { name: "Protect" })).toBeEnabled();
  });

  it("deletes one item only once the operator confirms", async () => {
    renderMedia();
    const { user, sheet } = await openDetail("q1-report.pdf");
    await user.click(sheet.getByRole("button", { name: "Delete" }));
    const confirm = within(await screen.findByRole("dialog", { name: "Delete q1-report.pdf?" }));
    expect(confirm.getByText(/This cannot be undone/)).toBeVisible();
    await user.click(confirm.getByRole("button", { name: "Cancel" }));
    expect(findMedia("example.org", "oldReportJkl012")).toBeDefined();

    await user.click(sheet.getByRole("button", { name: "Delete" }));
    const again = within(await screen.findByRole("dialog", { name: "Delete q1-report.pdf?" }));
    await user.click(again.getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(findMedia("example.org", "oldReportJkl012")).toBeUndefined());
    await waitFor(() =>
      expect(
        within(screen.getByRole("table")).queryByRole("button", { name: "q1-report.pdf" }),
      ).not.toBeInTheDocument(),
    );
  });

  it("bulk-deletes this server's unused uploads and keeps protected ones", async () => {
    let sent: unknown;
    server.events.on("request:start", async ({ request }) => {
      if (request.url.endsWith("/api/v1/media/delete")) sent = await request.clone().json();
    });
    renderMedia();
    await rowOf("q1-report.pdf");
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Delete old media" }));
    const dialog = within(await screen.findByRole("dialog", { name: "Delete old media" }));
    fireEvent.change(dialog.getByLabelText(/Not used since/), { target: { value: "2026-06-01" } });
    expect(dialog.getByText(/unused since June 1, 2026/)).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Delete media" }));

    await waitFor(() => expect(findMedia("example.org", "oldReportJkl012")).toBeUndefined());
    expect(sent).toEqual({ before: "2026-06-01T00:00:00Z" });
    // Protected, and last used before June too: kept.
    expect(findMedia("example.org", "teamLogoGhi789")).toBeDefined();
    expect(findMedia("matrix.org", "avatarMno345")).toBeDefined();
    server.events.removeAllListeners();
  });

  it("purges cached copies from one server", async () => {
    renderMedia();
    await rowOf("avatar.png");
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Purge remote cache" }));
    const dialog = within(await screen.findByRole("dialog", { name: "Purge cached remote media" }));
    fireEvent.change(dialog.getByLabelText(/Not used since/), { target: { value: "2026-09-20" } });
    await user.type(dialog.getByLabelText(/Only from server/), "matrix.org");
    expect(dialog.getByText(/Cached copies from matrix.org unused since/)).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Purge cache" }));
    await waitFor(() => expect(findMedia("matrix.org", "avatarMno345")).toBeUndefined());
    expect(findMedia("remote.example", "memePqr678")).toBeDefined();
  });

  it("lets an operator without moderation:write look but not act", async () => {
    signOut();
    await signIn(["admin:read"] satisfies Scope[]);
    renderMedia();
    expect(await screen.findByRole("button", { name: "Delete old media" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Purge remote cache" })).toBeDisabled();
    const { sheet } = await openDetail("vacation.jpg");
    expect(sheet.getByText("Acting on media needs moderation:write.")).toBeInTheDocument();
    expect(sheet.getByRole("button", { name: "Quarantine" })).toBeDisabled();
    expect(sheet.getByRole("button", { name: "Delete" })).toBeDisabled();
  });

  it("says so when the server has no media repository to read", async () => {
    server.use(
      http.get("*/api/v1/media", () =>
        HttpResponse.json(
          {
            type: "urn:hs:problem:unavailable",
            title: "Unavailable",
            status: 503,
            detail: "the media repository data source is not wired into this server",
          },
          { status: 503 },
        ),
      ),
    );
    renderMedia();
    expect(await screen.findByText(/media repository data source/)).toBeInTheDocument();
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
  });
});
