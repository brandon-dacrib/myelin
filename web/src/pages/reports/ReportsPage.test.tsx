import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { ReportsPage } from "./ReportsPage";
import { ReportDetailPage } from "./ReportDetailPage";
import { validateReportsSearch } from "./reports-search";

const ROUTES = [
  { path: "/reports", component: ReportsPage, validateSearch: validateReportsSearch },
  {
    path: "/reports/$reportId",
    component: ReportDetailPage,
    validateSearch: validateReportsSearch,
  },
];
const KNOWN = ["/users/$userId", "/rooms/$roomId"];

function open(path: string) {
  return renderRoutes(ROUTES, path, KNOWN);
}

async function table() {
  return within(await screen.findByRole("table", { name: "Reports" }));
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("Reports queue", () => {
  it("opens on the open reports, newest first", async () => {
    open("/reports");
    const rows = (await table()).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(4);
    expect(within(rows[0]).getByText("Scam link, posted in three rooms")).toBeInTheDocument();
    expect(
      within(rows[1]).getByText("Sends me unsolicited invites every hour"),
    ).toBeInTheDocument();
    expect(within(rows[3]).getByText("No reason given")).toBeInTheDocument();
    expect(screen.queryByText("Abusive message")).not.toBeInTheDocument();
  });

  it("asks the server for exactly what the URL says", async () => {
    let asked: URLSearchParams | undefined;
    server.use(
      http.get("/api/v1/reports", ({ request }) => {
        asked = new URL(request.url).searchParams;
        return HttpResponse.json({ items: [], next_cursor: null, prev_cursor: null });
      }),
    );
    open("/reports?status=all&kind=user&sort=score");
    expect(await screen.findByText("No matching reports")).toBeInTheDocument();
    expect(asked?.get("status")).toBeNull();
    expect(asked?.get("kind")).toBe("user");
    expect(asked?.get("sort")).toBe("score");
  });

  it("says plainly when nothing is waiting", async () => {
    server.use(
      http.get("/api/v1/reports", () =>
        HttpResponse.json({ items: [], next_cursor: null, prev_cursor: null }),
      ),
    );
    open("/reports");
    expect(await screen.findByText("No open reports")).toBeInTheDocument();
  });

  it("shows closed reports when asked", async () => {
    open("/reports?status=all");
    const t = await table();
    expect(t.getByText("Abusive message")).toBeInTheDocument();
    expect(t.getByText("Dismissed")).toBeInTheDocument();
  });

  it("does not show the queue without moderation:read", async () => {
    await signIn(["bridges:read"]);
    open("/reports");
    expect(await screen.findByText(/This needs the/)).toHaveTextContent("moderation:read");
  });

  it("says a missing handler is missing, not that there are no reports", async () => {
    server.use(
      http.get("/api/v1/reports", () =>
        HttpResponse.json(
          { type: "urn:hs:problem:not-implemented", title: "Not implemented", status: 501 },
          { status: 501 },
        ),
      ),
    );
    open("/reports");
    expect(await screen.findByText("Reports isn't implemented on this server yet")).toBeVisible();
    expect(screen.queryByText("No open reports")).not.toBeInTheDocument();
  });
});

describe("A report", () => {
  it("shows the reported message, who is involved, and resolves with a note", async () => {
    const user = userEvent.setup();
    let body: unknown;
    server.use(
      http.post("/api/v1/reports/:id/resolve", async ({ request }) => {
        body = await request.clone().json();
        return undefined;
      }),
    );
    open("/reports/01J9ZQ0000000000000000R008");

    expect(
      await screen.findByText(/Free crypto!!! Claim yours at https:\/\/totally-legit/),
    ).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "@alice:example.org" })).toHaveAttribute(
      "href",
      "/users/%40alice%3Aexample.org",
    );
    expect(screen.getByText("-100 (very offensive)")).toBeInTheDocument();

    await user.click(screen.getByRole("radio", { name: /Redacted the content/ }));
    await user.type(screen.getByLabelText("Note"), "Redacted; warned them in DM.");
    await user.click(screen.getByRole("button", { name: "Resolve report" }));

    expect(await screen.findByRole("heading", { name: "Decision" })).toBeInTheDocument();
    expect(body).toEqual({ resolution: "redacted", note: "Redacted; warned them in DM." });
    expect(screen.getByText("Resolved")).toBeInTheDocument();
    expect(screen.getByText("Redacted the content")).toBeInTheDocument();
    expect(screen.getByText("Redacted; warned them in DM.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Resolve report" })).not.toBeInTheDocument();
  });

  it("dismisses with no action", async () => {
    const user = userEvent.setup();
    open("/reports/01J9ZQ0000000000000000R006");
    await user.click(await screen.findByRole("radio", { name: /No action/ }));
    await user.click(screen.getByRole("button", { name: "Dismiss report" }));
    expect(await screen.findByRole("heading", { name: "Decision" })).toBeInTheDocument();
    expect(screen.getByText("Dismissed")).toBeInTheDocument();
  });

  it("asks what was done before saving, and for a note with 'something else'", async () => {
    const user = userEvent.setup();
    open("/reports/01J9ZQ0000000000000000R007");
    await user.click(await screen.findByRole("button", { name: "Resolve report" }));
    expect(screen.getByRole("alert")).toHaveTextContent("Choose what was done");
    await user.click(screen.getByRole("radio", { name: /Something else/ }));
    await user.click(screen.getByRole("button", { name: "Resolve report" }));
    expect(screen.getByRole("alert")).toHaveTextContent("Say in the note what was done.");
  });

  it("shows the server's refusal when someone else closed it first", async () => {
    const user = userEvent.setup();
    server.use(
      http.post("/api/v1/reports/:id/resolve", () =>
        HttpResponse.json(
          {
            type: "urn:hs:problem:conflict",
            title: "Conflict",
            status: 409,
            detail: "this report is already closed",
          },
          { status: 409 },
        ),
      ),
    );
    open("/reports/01J9ZQ0000000000000000R007");
    await user.click(await screen.findByRole("radio", { name: /Warned the user/ }));
    await user.click(screen.getByRole("button", { name: "Resolve report" }));
    expect(await screen.findByText("this report is already closed")).toBeInTheDocument();
  });

  it("shows a redacted message as redacted", async () => {
    open("/reports/01J9ZQ0000000000000000R005");
    expect(await screen.findByText("Its content has been removed.")).toBeInTheDocument();
  });

  it("shows the decision on a closed report, and no form", async () => {
    open("/reports/01J9ZQ0000000000000000R003");
    expect(await screen.findByText("Retaliation for a valid report.")).toBeInTheDocument();
    expect(screen.getByText("No action (dismiss)")).toBeInTheDocument();
    expect(screen.queryByRole("radio")).not.toBeInTheDocument();
  });

  it("lets a read-only moderator look but not decide", async () => {
    await signIn(["moderation:read"]);
    open("/reports/01J9ZQ0000000000000000R007");
    expect(await screen.findByText(/Deciding a report needs the/)).toBeInTheDocument();
    expect(screen.queryByRole("radio")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Delete report" })).not.toBeInTheDocument();
  });

  it("deletes a report after saying what that means, and goes back to the queue", async () => {
    const user = userEvent.setup();
    const { router } = open("/reports/01J9ZQ0000000000000000R007");
    await user.click(await screen.findByRole("button", { name: "Delete report" }));
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveTextContent("removed for good");
    await user.click(within(dialog).getByRole("button", { name: "Delete report" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/reports"));
    const t = await table();
    expect(t.queryByText("Sends me unsolicited invites every hour")).not.toBeInTheDocument();
  });

  it("says when a report does not exist", async () => {
    open("/reports/nope");
    expect(await screen.findByText(/not found/i)).toBeInTheDocument();
  });
});
