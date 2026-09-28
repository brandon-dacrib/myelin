import { afterEach, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { useParams } from "@tanstack/react-router";
import { server } from "@/mocks/node";
import { findUser } from "@/mocks/data/users";
import { mediaItems } from "@/mocks/data/media";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { useUser } from "@/api/users";
import { ModerationCard } from "./ModerationCard";

const ALICE = "@alice:example.org";
const SPAMMER = "@spammer42:example.org";

/** The card as the user page mounts it: fed by the user query, so a change shows at once. */
function Harness() {
  const { userId } = useParams({ strict: false }) as { userId: string };
  const { data } = useUser(userId);
  return data ? <ModerationCard user={data} /> : <p>loading</p>;
}

function renderCard(userId: string) {
  return renderRoutes(
    [{ path: "/users/$userId", component: Harness }],
    `/users/${encodeURIComponent(userId)}`,
    ["/tasks/$taskId"],
  );
}

afterEach(() => {
  server.events.removeAllListeners();
  signOut();
});

describe("ModerationCard", () => {
  it("suspends with a reason after a confirmation that names the user, then unsuspends", async () => {
    await signIn();
    const user = userEvent.setup();
    let body: unknown = null;
    server.events.on("request:start", async ({ request }) => {
      if (request.url.endsWith("/suspend")) body = await request.clone().json();
    });
    renderCard(ALICE);

    await user.click(await screen.findByRole("button", { name: "Suspend" }));
    const dialog = within(await screen.findByRole("dialog", { name: `Suspend ${ALICE}?` }));
    await user.type(dialog.getByLabelText("Reason"), "flooding #general");
    await user.click(dialog.getByRole("button", { name: "Suspend" }));

    expect(await screen.findByText("Suspended")).toBeInTheDocument();
    expect(body).toMatchObject({ reason: "flooding #general" });
    expect(findUser(ALICE)?.suspended).toBe(true);

    await user.click(screen.getByRole("button", { name: "Unsuspend" }));
    expect(await screen.findByText("Not suspended")).toBeInTheDocument();
    expect(findUser(ALICE)?.suspended).toBe(false);
  });

  it("shadow-bans and lifts it", async () => {
    await signIn();
    const user = userEvent.setup();
    renderCard(ALICE);

    await user.click(await screen.findByRole("button", { name: "Shadow-ban" }));
    const dialog = within(await screen.findByRole("dialog", { name: `Shadow-ban ${ALICE}?` }));
    await user.click(dialog.getByRole("button", { name: "Shadow-ban" }));
    expect(await screen.findByText("Shadow-banned")).toBeInTheDocument();
    expect(findUser(ALICE)?.shadow_banned).toBe(true);

    await user.click(screen.getByRole("button", { name: "Lift shadow-ban" }));
    expect(await screen.findByRole("button", { name: "Shadow-ban" })).toBeInTheDocument();
    expect(findUser(ALICE)?.shadow_banned).toBe(false);
  });

  it("keeps the suspend dialog open and says why when the server refuses", async () => {
    await signIn();
    server.use(
      http.post("*/api/v1/users/:user_id/suspend", () =>
        HttpResponse.json(
          {
            type: "urn:hs:problem:conflict",
            title: "Conflict",
            status: 409,
            detail: "an administrator cannot be suspended",
          },
          { status: 409 },
        ),
      ),
    );
    const user = userEvent.setup();
    renderCard(ALICE);
    await user.click(await screen.findByRole("button", { name: "Suspend" }));
    const dialog = within(await screen.findByRole("dialog", { name: `Suspend ${ALICE}?` }));
    await user.click(dialog.getByRole("button", { name: "Suspend" }));
    expect(await dialog.findByText(/an administrator cannot be suspended/)).toBeInTheDocument();
  });

  it("starts a redaction and follows its task to the result, with a link to the task", async () => {
    await signIn();
    let body: unknown = null;
    server.events.on("request:start", async ({ request }) => {
      if (request.url.endsWith("/redact-events")) body = await request.clone().json();
    });
    // The task finishes on its first poll, so the test does not wait out the mock's clock.
    server.use(
      http.get("*/api/v1/tasks/:id", ({ params }) =>
        HttpResponse.json({
          id: String(params.id),
          action: "user.redact_events",
          status: "succeeded",
          created_at: new Date().toISOString(),
          progress: { current: 5, total: 5, unit: "events" },
          result: {
            total: 5,
            redacted: 4,
            failed_count: 1,
            failed: [{ event_id: "$abc", room_id: "!general:example.org", error: "not allowed" }],
          },
        }),
      ),
    );
    const user = userEvent.setup();
    renderCard(SPAMMER);

    await user.click(await screen.findByRole("button", { name: "Redact messages…" }));
    const dialog = within(
      await screen.findByRole("dialog", { name: `Redact messages sent by ${SPAMMER}?` }),
    );
    await user.type(dialog.getByLabelText("Only the most recent"), "0");
    await user.click(dialog.getByRole("button", { name: "Redact messages" }));
    expect(await dialog.findByText(/A whole number, 1 or more/)).toBeInTheDocument();

    await user.clear(dialog.getByLabelText("Only the most recent"));
    await user.type(dialog.getByLabelText("Only the most recent"), "5");
    await user.type(dialog.getByLabelText("Reason"), "spam");
    await user.click(dialog.getByRole("button", { name: "Redact messages" }));

    const followed = within(await screen.findByRole("region", { name: "Redacting messages" }));
    expect(body).toMatchObject({ limit: 5, reason: "spam" });
    expect(followed.getByRole("link", { name: "Open in Tasks" })).toHaveAttribute(
      "href",
      expect.stringMatching(/^\/tasks\/task_/),
    );
    expect(
      await followed.findByText(/Redacted 4 of 5 events/, {}, { timeout: 4000 }),
    ).toBeVisible();
    expect(followed.getByText("1 could not be redacted")).toBeInTheDocument();
  });

  it("shows a running redaction's progress", async () => {
    await signIn();
    const user = userEvent.setup();
    renderCard(SPAMMER);
    await user.click(await screen.findByRole("button", { name: "Redact messages…" }));
    const dialog = within(await screen.findByRole("dialog"));
    await user.click(dialog.getByRole("button", { name: "Redact messages" }));
    const followed = within(await screen.findByRole("region", { name: "Redacting messages" }));
    expect(followed.getByRole("progressbar")).toHaveAccessibleName(/of 12 events/);
  });

  it("deletes all of a user's media after naming them, and reports what went", async () => {
    await signIn();
    const user = userEvent.setup();
    renderCard(ALICE);
    await user.click(await screen.findByRole("button", { name: "Delete all media…" }));
    const dialog = within(
      await screen.findByRole("dialog", { name: `Delete all media uploaded by ${ALICE}?` }),
    );
    expect(await dialog.findByText(/They have uploaded 1 file/)).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Delete all media" }));

    const followed = within(await screen.findByRole("region", { name: "Deleting media" }));
    expect(await followed.findByText(/Deleted 1 file \(2\.3 MiB\)/)).toBeInTheDocument();
    expect(mediaItems.some((m) => m.uploader === ALICE)).toBe(false);
  });

  it("disables what the session's scopes do not allow, and says which scope", async () => {
    await signIn(["admin:read"]);
    renderCard(ALICE);
    const suspend = await screen.findByRole("button", { name: "Suspend" });
    expect(suspend).toBeDisabled();
    expect(suspend).toHaveAttribute("title", "Needs moderation:write");
    expect(screen.getByRole("button", { name: "Shadow-ban" })).toHaveAttribute(
      "title",
      "Needs moderation:write",
    );
    expect(screen.getByRole("button", { name: "Sign in as user…" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Redact messages…" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Delete all media…" })).toBeDisabled();
    expect(await screen.findByText("Changing it needs admin:write.")).toBeInTheDocument();
  });

  it("will not sign in as a deactivated user", async () => {
    await signIn();
    findUser(ALICE)!.deactivated = true;
    renderCard(ALICE);
    const button = await screen.findByRole("button", { name: "Sign in as user…" });
    expect(button).toBeDisabled();
    expect(button).toHaveAttribute("title", "A deactivated account cannot be signed in as");
  });
});
