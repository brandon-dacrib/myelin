import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http } from "msw";
import { server } from "@/mocks/node";
import { ServerNoticesPage } from "./ServerNoticesPage";
import { SendNoticeDialog } from "./SendNoticeDialog";
import { renderSettingsRoute } from "./test-utils";
import { serverNotices } from "@/mocks/data/server-notices";
import { signIn, signOut } from "@/lib/auth";

const PATH = "/settings/server-notices";

/** Every body POSTed to `/server-notices`, read on the way past to the mock's handler. */
function captureSends(): unknown[] {
  const bodies: unknown[] = [];
  server.use(
    http.post("*/api/v1/server-notices", async ({ request }) => {
      bodies.push(await request.clone().json());
    }),
  );
  return bodies;
}

async function sendForm() {
  const heading = await screen.findByRole("heading", { name: "Send a server notice" });
  return within(heading.closest("section")!);
}

beforeEach(async () => {
  await signIn();
});

afterEach(() => {
  signOut();
});

describe("ServerNoticesPage", () => {
  it("lists what was sent, newest first", async () => {
    renderSettingsRoute(PATH, ServerNoticesPage);
    const history = within(await screen.findByRole("table", { name: "Sent server notices" }));

    const rows = history.getAllByRole("row").slice(1);
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent("@spammer42:example.org");
    expect(rows[0]).toHaveTextContent(/Your account was suspended/);
    expect(rows[1]).toHaveTextContent("@admin:example.org, @alice:example.org, @bot:example.org");
    expect(rows[1]).toHaveTextContent(/restarts for an upgrade/);
    expect(rows[1]).toHaveTextContent("@server:example.org");
  });

  it("adds recipients as chips, refusing anybody not on this server", async () => {
    const user = userEvent.setup();
    renderSettingsRoute(PATH, ServerNoticesPage);
    const form = await sendForm();
    const input = form.getByLabelText(/^Recipients/);

    await user.type(input, "@carol:elsewhere.net{Enter}");
    expect(
      await form.findByText(
        "@carol:elsewhere.net is not on this server; notices only go to users on example.org.",
      ),
    ).toBeInTheDocument();
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(form.queryByRole("list", { name: "Chosen recipients" })).not.toBeInTheDocument();

    await user.clear(input);
    // A bare username is completed with this server's name; a pasted list adds each one.
    await user.type(input, "alice, @bot:example.org{Enter}");
    const chips = within(form.getByRole("list", { name: "Chosen recipients" }));
    expect(chips.getByText("@alice:example.org")).toBeInTheDocument();
    expect(chips.getByText("@bot:example.org")).toBeInTheDocument();
    expect(input).toHaveValue("");
    expect(input).not.toHaveAttribute("aria-invalid");

    await user.click(chips.getByRole("button", { name: "Remove @bot:example.org" }));
    expect(chips.queryByText("@bot:example.org")).not.toBeInTheDocument();
  });

  it("suggests matching users as the operator types", async () => {
    const user = userEvent.setup();
    renderSettingsRoute(PATH, ServerNoticesPage);
    const form = await sendForm();

    await user.type(form.getByLabelText(/^Recipients/), "spam");
    await user.click(await form.findByRole("button", { name: "Add @spammer42:example.org" }));
    const chips = within(form.getByRole("list", { name: "Chosen recipients" }));
    expect(chips.getByText("@spammer42:example.org")).toBeInTheDocument();
  });

  it("sends an m.text notice and says where it went", async () => {
    const user = userEvent.setup();
    const bodies = captureSends();
    renderSettingsRoute(PATH, ServerNoticesPage);
    const form = await sendForm();

    await user.click(form.getByRole("button", { name: "Send notice" }));
    expect(await form.findByText("Add at least one recipient.")).toBeInTheDocument();

    await user.type(form.getByLabelText(/^Recipients/), "@alice:example.org{Enter}admin{Enter}");
    await user.click(form.getByRole("button", { name: "Send to 2 users" }));
    expect(await form.findByText("Write the message to send.")).toBeInTheDocument();
    expect(bodies).toHaveLength(0);

    await user.type(form.getByLabelText(/^Message/), "  Maintenance tonight at 22:00.  ");
    await user.click(form.getByRole("button", { name: "Send to 2 users" }));

    expect(await form.findByText("Notice sent to 2 users.")).toBeInTheDocument();
    expect(bodies[0]).toEqual({
      recipients: ["@alice:example.org", "@admin:example.org"],
      content: { msgtype: "m.text", body: "Maintenance tonight at 22:00." },
      type: "m.room.message",
    });
    expect(form.getByText("!notices-alice:example.org")).toBeInTheDocument();
    expect(form.getByText("!notices-admin:example.org")).toBeInTheDocument();
    expect(form.getByText(`$${serverNotices[0]!.id}-alice`)).toBeInTheDocument();

    // It joins the history, and the form comes back empty for the next one.
    const history = within(screen.getByRole("table", { name: "Sent server notices" }));
    expect(await history.findByText("Maintenance tonight at 22:00.")).toBeInTheDocument();
    await user.click(form.getByRole("button", { name: "Send another" }));
    expect(form.getByLabelText(/^Message/)).toHaveValue("");
  });

  it("shows the server's refusal when a recipient does not exist, having sent nothing", async () => {
    const user = userEvent.setup();
    renderSettingsRoute(PATH, ServerNoticesPage);
    const form = await sendForm();

    await user.type(form.getByLabelText(/^Recipients/), "@nobody:example.org{Enter}");
    await user.type(form.getByLabelText(/^Message/), "Hello");
    await user.click(form.getByRole("button", { name: "Send notice" }));

    expect(await form.findByText("@nobody:example.org does not exist")).toBeInTheDocument();
    expect(serverNotices).toHaveLength(2);
  });

  it("lets a read-only operator see the history but not send", async () => {
    signOut();
    await signIn(["moderation:read"]);
    renderSettingsRoute(PATH, ServerNoticesPage);

    expect(await screen.findByRole("table", { name: "Sent server notices" })).toBeInTheDocument();
    expect(screen.queryByLabelText(/^Message/)).not.toBeInTheDocument();
    expect(screen.getByText(/moderation:write/)).toBeInTheDocument();
  });
});

describe("SendNoticeDialog", () => {
  it("sends to the one user it was opened for", async () => {
    const user = userEvent.setup();
    const bodies = captureSends();
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={client}>
        <SendNoticeDialog userId="@alice:example.org" open onOpenChange={() => {}} />
      </QueryClientProvider>,
    );
    const dialog = within(await screen.findByRole("dialog", { name: "Send a server notice" }));

    expect(dialog.getByText("@alice:example.org")).toBeInTheDocument();
    expect(dialog.queryByLabelText(/^Recipients/)).not.toBeInTheDocument();
    await user.type(dialog.getByLabelText(/^Message/), "Please check your email.");
    await user.click(dialog.getByRole("button", { name: "Send notice" }));

    expect(await dialog.findByText("Notice sent to 1 user.")).toBeInTheDocument();
    expect(bodies[0]).toMatchObject({
      recipients: ["@alice:example.org"],
      content: { msgtype: "m.text", body: "Please check your email." },
    });
    expect(dialog.getByRole("button", { name: "Done" })).toBeInTheDocument();
  });
});
