import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { settleMigration } from "@/mocks/data/migration";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { MigrationPage } from "./MigrationPage";

const ROUTES = [{ path: "/migration", component: MigrationPage }];
const KNOWN = ["/users", "/rooms", "/tasks/$taskId"];

function open() {
  return renderRoutes(ROUTES, "/migration", KNOWN);
}

/** Posts made to `path`, as the server saw them. */
function recordPosts(path: string) {
  const bodies: unknown[] = [];
  server.events.on("request:start", async ({ request }) => {
    if (request.method !== "GET" && new URL(request.url).pathname === `/api/v1${path}`) {
      const text = await request.clone().text();
      bodies.push(text ? JSON.parse(text) : null);
    }
  });
  return bodies;
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => {
  server.events.removeAllListeners();
  signOut();
});

async function pointAtSynapse(user: ReturnType<typeof userEvent.setup>) {
  const form = within(await screen.findByRole("form", { name: "Synapse source" }));
  await user.clear(form.getByLabelText(/^Host/));
  await user.type(form.getByLabelText(/^Host/), "synapse-db.internal");
  await user.type(form.getByLabelText(/^Password/), "s3cret");
  await user.type(form.getByLabelText(/^Media store path/), "/var/lib/synapse/media_store");
  await user.click(form.getByRole("button", { name: "Save source" }));
}

describe("Migration", () => {
  it("says what is copied and what is not before anything starts", async () => {
    open();
    const summary = await screen.findByText("What is copied, and what is not");
    const details = summary.closest("details")!;
    expect(details).toHaveAttribute("open");
    const moves = within(within(details).getByRole("region", { name: "Copied, in this order" }));
    expect(moves.getAllByRole("listitem")).toHaveLength(14);
    expect(moves.getByText("Sessions (access tokens)")).toBeInTheDocument();
    expect(moves.getByText("Other servers' media")).toBeInTheDocument();
    const stays = within(within(details).getByRole("region", { name: "Not copied" }));
    expect(stays.getByText("Thumbnails")).toBeInTheDocument();
    expect(stays.getByText("Presence")).toBeInTheDocument();
    expect(stays.getByText("Receipts in threads")).toBeInTheDocument();
    expect(stays.getByText("Bridges")).toBeInTheDocument();
  });

  it("walks from pointing at Synapse through copying and verifying to cutover", async () => {
    const user = userEvent.setup();
    const patches = recordPosts("/config/migration");
    open();

    // Nothing can start until a source is set.
    expect(await screen.findByRole("button", { name: "Start copying" })).toBeDisabled();
    await pointAtSynapse(user);
    expect(await screen.findByText("Set, hidden")).toBeInTheDocument();
    expect(
      screen.getByText("postgresql://synapse@synapse-db.internal:5432/synapse"),
    ).toBeInTheDocument();
    expect(patches).toEqual([
      {
        synapse: {
          database: {
            host: "synapse-db.internal",
            port: 5432,
            database: "synapse",
            user: "synapse",
            password: "s3cret",
          },
          media_store_path: "/var/lib/synapse/media_store",
          batch_size: 500,
        },
      },
    ]);

    await user.click(screen.getByRole("button", { name: "Start copying" }));
    expect(await screen.findByText("Copying", { selector: "span" })).toBeInTheDocument();
    expect(screen.getByRole("table", { name: "What has been copied" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Pause" })).toBeEnabled();
    // The source can no longer be changed under a running copy.
    expect(screen.queryByRole("button", { name: "Change source" })).not.toBeInTheDocument();

    settleMigration();
    expect(
      await screen.findByText("Ready for cutover", { selector: "span" }, { timeout: 5_000 }),
    ).toBeInTheDocument();
    const copied = screen.getByRole("table", { name: "What has been copied" });
    expect(within(copied).getByText("Accounts")).toBeInTheDocument();
    expect(within(copied).getByText("1,240")).toBeInTheDocument();
    // The streams added on 2026-10-01 are named in words, each with what it keeps working.
    for (const label of [
      "Device encryption keys",
      "Cross-signing keys",
      "Key backups",
      "Notification rules (push rules)",
      "Phones to notify (pushers)",
      "Sync filters",
      "Read receipts",
    ]) {
      expect(within(copied).getByText(label)).toBeInTheDocument();
    }
    expect(within(copied).getByText(/encrypted history stays readable/)).toBeInTheDocument();
    expect(within(copied).queryByText("e2e_keys")).not.toBeInTheDocument();

    // Verify: a task, then the findings.
    await user.click(screen.getByRole("button", { name: "Verify" }));
    settleMigration();
    expect(
      await screen.findByText("Everything matches", {}, { timeout: 5_000 }),
    ).toBeInTheDocument();
    expect(screen.getByRole("table", { name: "Verification" })).toBeInTheDocument();

    // Cut over: only once both checklist items are ticked, and after a confirmation.
    const cutover = screen.getByRole("button", { name: "Cut over" });
    expect(cutover).toBeDisabled();
    await user.click(screen.getByRole("checkbox", { name: /Synapse is stopped/ }));
    expect(cutover).toBeDisabled();
    await user.click(screen.getByRole("checkbox", { name: /will reach this server/ }));
    expect(cutover).toBeEnabled();
    await user.click(cutover);
    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText(/keep Synapse stopped/)).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Cut over now" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    settleMigration();
    expect(
      await screen.findByText("Completed", { selector: "span" }, { timeout: 5_000 }),
    ).toBeInTheDocument();
    expect(screen.getByText(/This server is the one in service/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Abort migration" })).not.toBeInTheDocument();

    // The log tells the story.
    expect(await screen.findByText(/cut over by @admin:example.org/)).toBeInTheDocument();
  }, 30_000);

  it("pauses, resumes, and aborts after saying Synapse is untouched", async () => {
    const user = userEvent.setup();
    open();
    await pointAtSynapse(user);
    await user.click(await screen.findByRole("button", { name: "Start copying" }));
    await user.click(await screen.findByRole("button", { name: "Pause" }));
    expect(await screen.findByText("Paused", { selector: "span" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Resume" }));
    expect(await screen.findByText("Copying", { selector: "span" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Abort migration" }));
    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText(/Synapse was only ever read/)).toBeInTheDocument();
    expect(dialog.getByText(/What was already copied stays here/)).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Abort migration" }));
    expect(await screen.findByText("Aborted", { selector: "span" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Start again" })).toBeEnabled();
  });

  it("says in the server's own words why a start was refused", async () => {
    const user = userEvent.setup();
    server.use(
      http.post("/api/v1/migration/start", () =>
        HttpResponse.json(
          {
            type: "https://myelin.dev/problems/validation-failed",
            title: "Validation failed",
            status: 400,
            detail:
              "could not connect to postgresql://synapse@synapse-db.internal:5432/synapse: connection refused",
          },
          { status: 400, headers: { "Content-Type": "application/problem+json" } },
        ),
      ),
    );
    open();
    await pointAtSynapse(user);
    await user.click(await screen.findByRole("button", { name: "Start copying" }));
    expect(await screen.findByText(/connection refused/)).toBeInTheDocument();
    expect(screen.getByText("Not started", { selector: "span" })).toBeInTheDocument();
  });

  it("keeps the stored password unless a new one is typed", async () => {
    const user = userEvent.setup();
    const patches = recordPosts("/config/migration");
    open();
    await pointAtSynapse(user);
    await user.click(await screen.findByRole("button", { name: "Change source" }));
    const form = within(await screen.findByRole("form", { name: "Synapse source" }));
    expect(form.getByText(/A password is stored/)).toBeInTheDocument();
    await user.clear(form.getByLabelText(/^Database/));
    await user.type(form.getByLabelText(/^Database/), "synapse_main");
    await user.click(form.getByRole("button", { name: "Save source" }));
    await screen.findByText("postgresql://synapse@synapse-db.internal:5432/synapse_main");
    const second = patches[1] as { synapse: { database: Record<string, unknown> } };
    expect(second.synapse.database).not.toHaveProperty("password");
  });
});
