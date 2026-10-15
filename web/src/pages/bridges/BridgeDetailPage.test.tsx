import { afterEach, beforeEach, describe, expect, it } from "vitest";
import userEvent from "@testing-library/user-event";
import { render, screen, waitFor, within } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { signIn, signOut } from "@/lib/auth";
import { server } from "@/mocks/node";
import { Toaster } from "@/components/ui/toast/Toaster";
import { setVolumeSize } from "@/mocks/data/bridge-offerings";
import { renderBridgesRoute } from "./test-utils";
import { BridgeDetailPage } from "./BridgeDetailPage";

function renderBridge(id: string) {
  return renderBridgesRoute("/bridges/$bridgeId", BridgeDetailPage, `/bridges/${id}`);
}

/** The bridge's page on its Transactions tab, with toasts rendered. */
async function openTransactions(id: string) {
  render(<Toaster />);
  renderBridge(id);
  await userEvent.click(await screen.findByRole("tab", { name: "Transactions" }));
}

/** What the page sent to `POST /appservices/{id}/{action}`: its body and Idempotency-Key. */
interface Sent {
  body: unknown;
  idempotencyKey: string | null;
}

function recordPost(path: string, answer: () => Response): Sent[] {
  const sent: Sent[] = [];
  server.use(
    http.post(path, async ({ request }) => {
      const text = await request.text();
      sent.push({
        body: text ? JSON.parse(text) : undefined,
        idempotencyKey: request.headers.get("Idempotency-Key"),
      });
      return answer();
    }),
  );
  return sent;
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => {
  server.resetHandlers();
  signOut();
});

describe("Bridge page", () => {
  it("says a bridge registered by hand overlaps the offering, and links to it", async () => {
    // The mock server's `whatsapp` registration was made from the catalogue's WhatsApp entry,
    // and WhatsApp is offered: the demo's case (RFC 0017 section 6).
    renderBridge("whatsapp");
    const line = await screen.findByTestId("overlaps-offering");
    expect(line).toHaveTextContent(/WhatsApp is offered on this server now/);
    expect(line).toHaveTextContent(/messaging @whatsappbot:example.org/);
    expect(within(line).getByRole("link", { name: "The WhatsApp offering" })).toHaveAttribute(
      "href",
      "/bridges/offerings/mautrix-whatsapp",
    );
  });

  it("says nothing of the kind for a bridge no offering overlaps", async () => {
    renderBridge("telegram");
    expect(await screen.findByRole("heading", { level: 1 })).toBeInTheDocument();
    expect(screen.queryByTestId("overlaps-offering")).toBeNull();
  });

  it("says a bridge double puppets, and links its double-puppeting registration", async () => {
    // The mock server's Slack bridge has `slack-doublepuppet`, which it names in `double_puppet`;
    // nothing is probed by id.
    let probed = false;
    server.use(
      http.get("/api/v1/appservices/slack-doublepuppet", () => {
        probed = true;
        return HttpResponse.json({});
      }),
    );
    renderBridge("slack");
    const link = await screen.findByRole("link", { name: "slack-doublepuppet" });
    expect(link).toHaveAttribute("href", "/bridges/slack-doublepuppet");
    expect(screen.getByText(/On: it posts as its owner with/)).toBeInTheDocument();
    expect(probed).toBe(false);
  });

  it("reads the double-puppeting registration's name from the server, not from its id", async () => {
    server.use(
      http.get("/api/v1/appservices/telegram", () =>
        HttpResponse.json({
          id: "telegram",
          sender_localpart: "telegrambot",
          url: "http://mautrix-telegram.bridges.svc:29317",
          health: "healthy",
          double_puppet: "telegram-puppets",
          double_puppet_for: null,
        }),
      ),
    );
    renderBridge("telegram");
    const link = await screen.findByRole("link", { name: "telegram-puppets" });
    expect(link).toHaveAttribute("href", "/bridges/telegram-puppets");
  });

  it("says double puppeting is off when the bridge has no such registration", async () => {
    renderBridge("telegram");
    expect(await screen.findByText(/Off: it posts as its own ghosts only/)).toBeInTheDocument();
  });

  it("shows a double-puppeting registration as its bridge's key, not as a bridge", async () => {
    let healthAsked = false;
    server.use(
      // Named for its bridge by `double_puppet_for`, whatever its id.
      http.get("/api/v1/appservices/tg-key", () =>
        HttpResponse.json({
          id: "tg-key",
          sender_localpart: "tg-key",
          url: null,
          health: "unknown",
          double_puppet_for: "telegram",
          double_puppet: null,
          namespaces: { users: [{ regex: "@brandon:example\\.org", exclusive: false }] },
        }),
      ),
      http.get("/api/v1/appservices/tg-key/health", () => {
        healthAsked = true;
        return HttpResponse.json({ status: "unknown" });
      }),
    );
    renderBridge("tg-key");
    expect(
      await screen.findByRole("heading", { level: 1, name: "Double puppeting for Telegram" }),
    ).toBeInTheDocument();
    expect(screen.getByText(/This is not a bridge/)).toBeInTheDocument();
    expect(screen.getByText("Built in")).toBeInTheDocument();
    expect(screen.getByText("@brandon:example\\.org")).toBeInTheDocument();
    // Its bridge is linked, by the bridge's name, once its record is read.
    const links = await screen.findAllByRole("link", { name: /^Telegram$/ });
    for (const link of links) expect(link).toHaveAttribute("href", "/bridges/telegram");
    // Nothing that belongs to a bridge the server delivers to.
    expect(screen.queryByText("Unknown")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Test connection" })).not.toBeInTheDocument();
    expect(screen.queryByRole("tab")).not.toBeInTheDocument();
    expect(screen.queryByText(/^Off:/)).not.toBeInTheDocument();
    expect(healthAsked).toBe(false);
  });

  it("says a double-puppeting registration's bridge is gone, and offers to remove it", async () => {
    let deleted = false;
    server.use(
      http.get("/api/v1/appservices/gone-doublepuppet", () =>
        HttpResponse.json({
          id: "gone-doublepuppet",
          sender_localpart: "gone-doublepuppet",
          url: null,
          health: "unknown",
          double_puppet_for: "gone",
          double_puppet: null,
        }),
      ),
      http.get("/api/v1/appservices/gone", () =>
        HttpResponse.json(
          { type: "urn:hs:problem:not-found", title: "Not found", status: 404 },
          { status: 404 },
        ),
      ),
      http.delete("/api/v1/appservices/gone-doublepuppet", () => {
        deleted = true;
        return new HttpResponse(null, { status: 204 });
      }),
    );
    renderBridge("gone-doublepuppet");
    expect(
      await screen.findByText(/Its bridge has been removed, so nothing uses it any more/),
    ).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: /^Gone$/ })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Remove registration" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Remove registration" }));
    await waitFor(() => expect(deleted).toBe(true));
  });
});

describe("Bridge health", () => {
  it("says why a bridge is not healthy, in the server's words, beside its state", async () => {
    renderBridge("signal");
    const reason = await screen.findByTestId("health-reason");
    expect(reason).toHaveTextContent(
      /^Down: The last 24 transactions to it failed, 20 or more in a row; nothing is getting through\./,
    );
    expect(reason).toHaveTextContent(/The last error: Connection refused/);
  });

  it("says nothing of the kind for a healthy bridge", async () => {
    renderBridge("whatsapp");
    expect(await screen.findByRole("heading", { level: 1 })).toBeInTheDocument();
    await waitFor(() => expect(screen.getAllByText("Healthy").length).toBeGreaterThan(0));
    expect(screen.queryByTestId("health-reason")).toBeNull();
  });

  it("shows the last error from a server that gives no reason", async () => {
    server.use(
      http.get("/api/v1/appservices/signal/health", () =>
        HttpResponse.json({ status: "down", last_ping_at: null, last_error: "connection refused" }),
      ),
    );
    renderBridge("signal");
    expect(await screen.findByText("connection refused")).toBeInTheDocument();
    expect(screen.queryByTestId("health-reason")).toBeNull();
  });
});

describe("Bridge transactions", () => {
  it("lists each entry with when it was queued, what it carries, its rooms and its last error", async () => {
    await openTransactions("signal");
    const entry = await screen.findByTestId("backlog-entry-tx_700");
    expect(within(entry).getByText("Dead-lettered")).toBeInTheDocument();
    expect(entry).toHaveTextContent("queued 3 d ago");
    expect(entry).toHaveTextContent("10 attempts");
    expect(entry).toHaveTextContent("Carries 48 events");
    // At most five rooms are listed, so five may be the first of more.
    expect(entry).toHaveTextContent("in these rooms and maybe more:");
    const rooms = within(entry).getByRole("list", { name: "Rooms" });
    expect(within(rooms).getAllByRole("listitem")).toHaveLength(5);
    // Shortened on screen with its server kept; whole on hover and to a screen reader.
    expect(within(rooms).getByText("!OGEhHV…:matrix.org")).toHaveAttribute("aria-hidden", "true");
    expect(
      within(rooms).getByText("!OGEhHVWSdvArJzumhm:matrix.org").closest("code"),
    ).toHaveAttribute("title", "!OGEhHVWSdvArJzumhm:matrix.org");
    expect(entry).toHaveTextContent(
      "Last error: the bridge answered 400 M_UNKNOWN: unsupported event type",
    );
    expect(screen.getByTestId("backlog-entry-tx_701")).toHaveTextContent(
      "Carries 3 events · 2 ephemeral · 1 to-device in room",
    );
  });

  it("explains dead letters, how many there are and how old the oldest is", async () => {
    await openTransactions("signal");
    const box = await screen.findByTestId("dead-letters");
    expect(box).toHaveTextContent("2 dead-lettered transactions, the oldest queued 3d ago.");
    expect(box).toHaveTextContent(/the bridge refused each of them 10 times/);
    expect(box).toHaveTextContent(/answered 5xx, 408 or 429/);
    expect(box).toHaveTextContent(/discarding is usually right/);
  });

  it("says nothing waits for a bridge with an empty queue, and offers no replay or discard", async () => {
    await openTransactions("whatsapp");
    expect(
      await screen.findByText("No pending or dead-lettered transactions."),
    ).toBeInTheDocument();
    expect(screen.queryByTestId("dead-letters")).toBeNull();
    expect(screen.queryByRole("button", { name: "Replay all" })).toBeNull();
  });

  it("discards every dead letter only after a confirmation that says how many and that it is final", async () => {
    const sent = recordPost("/api/v1/appservices/signal/discard", () =>
      HttpResponse.json({ discarded: 2 }),
    );
    await openTransactions("signal");
    await userEvent.click(await screen.findByRole("button", { name: "Discard all dead letters" }));
    const dialog = await screen.findByRole("dialog");
    expect(
      within(dialog).getByRole("heading", { name: "Discard 2 dead-lettered transactions?" }),
    ).toBeInTheDocument();
    expect(dialog).toHaveTextContent(/is never sent them\. This cannot be undone\./);
    expect(dialog).toHaveTextContent(/still waiting to be sent are not touched/);
    expect(sent).toHaveLength(0);
    await userEvent.click(within(dialog).getByRole("button", { name: "Discard all" }));
    await waitFor(() => expect(sent).toHaveLength(1));
    // Neither filter: every dead letter. A fresh Idempotency-Key, as every action sends.
    expect(sent[0]!.body).toEqual({});
    expect(sent[0]!.idempotencyKey).toBeTruthy();
    expect(await screen.findByText("Discarded 2 dead-lettered transactions")).toBeInTheDocument();
  });

  it("sends nothing when the discard is cancelled", async () => {
    const sent = recordPost("/api/v1/appservices/signal/discard", () =>
      HttpResponse.json({ discarded: 2 }),
    );
    await openTransactions("signal");
    await userEvent.click(await screen.findByRole("button", { name: "Discard all dead letters" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(sent).toHaveLength(0);
  });

  it("discards one transaction from its row, and it leaves the list", async () => {
    await openTransactions("signal");
    await userEvent.click(
      await screen.findByRole("button", { name: "Discard transaction tx_701" }),
    );
    const dialog = await screen.findByRole("dialog");
    expect(
      within(dialog).getByRole("heading", { name: "Discard transaction tx_701?" }),
    ).toBeInTheDocument();
    expect(dialog).toHaveTextContent(
      /It carries 3 events · 2 ephemeral · 1 to-device\. It is deleted for good .* This cannot be undone\./,
    );
    await userEvent.click(within(dialog).getByRole("button", { name: "Discard" }));
    expect(await screen.findByText("Transaction tx_701 discarded")).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByTestId("backlog-entry-tx_701")).toBeNull());
    expect(screen.getByTestId("backlog-entry-tx_700")).toBeInTheDocument();
    expect(screen.getByTestId("dead-letters")).toHaveTextContent("1 dead-lettered transaction,");
  });

  it("says so when a discard is refused", async () => {
    recordPost("/api/v1/appservices/signal/discard", () =>
      HttpResponse.json(
        {
          type: "urn:hs:problem:not-found",
          title: "Not found",
          status: 404,
          detail: "no appservice signal",
        },
        { status: 404 },
      ),
    );
    await openTransactions("signal");
    await userEvent.click(await screen.findByRole("button", { name: "Discard all dead letters" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Discard all" }));
    expect(await screen.findByText("Couldn't discard the dead letters")).toBeInTheDocument();
    expect(screen.getByText("no appservice signal")).toBeInTheDocument();
  });

  it("replays only after a confirmation that says how many, how old, and that old events are sent now", async () => {
    const sent = recordPost("/api/v1/appservices/signal/replay", () =>
      HttpResponse.json(
        {
          id: "task-replay1",
          action: "appservice.replay",
          status: "running",
          resource: { type: "appservice", id: "signal" },
          created_at: new Date().toISOString(),
        },
        { status: 202 },
      ),
    );
    await openTransactions("signal");
    await userEvent.click(await screen.findByRole("button", { name: "Replay all" }));
    const dialog = await screen.findByRole("dialog");
    expect(
      within(dialog).getByRole("heading", { name: "Replay 2 dead-lettered transactions?" }),
    ).toBeInTheDocument();
    expect(dialog).toHaveTextContent(
      /The oldest was queued 3d ago: the bridge receives events that old now, as if they were new\./,
    );
    expect(dialog).toHaveTextContent(/discard them instead/);
    expect(sent).toHaveLength(0);
    await userEvent.click(within(dialog).getByRole("button", { name: "Replay all" }));
    await waitFor(() => expect(sent).toHaveLength(1));
    expect(sent[0]!.idempotencyKey).toBeTruthy();
    expect(
      await screen.findByText(/^Replaying 2 dead-lettered transactions to /),
    ).toBeInTheDocument();
    expect(screen.getByText("Task task-replay1")).toBeInTheDocument();
  });
});

describe("Bridge page: volume", () => {
  // The mock server's `slack-brandon` is Brandon's Slack bridge from the Slack offering, which
  // this server deploys: 98% of a 1Gi volume, stopped for want of space (the demo's, 2026-10-15).
  it("says a deployed bridge's volume is full, how full, and what to do", async () => {
    renderBridge("slack-brandon");
    const alert = await screen.findByTestId("volume-alert");
    expect(alert).toHaveTextContent(/Volume full\./);
    expect(alert).toHaveTextContent(/No space left on device/);
    const section = within(screen.getByRole("region", { name: "Volume" }));
    expect(section.getByText("1Gi")).toBeInTheDocument();
    expect(section.getByText("1003.5 MiB of 1.0 GiB used (98%)")).toBeInTheDocument();
    expect(section.getByRole("meter", { name: "Volume used" })).toHaveAttribute(
      "aria-valuenow",
      "98",
    );
    expect(section.getByText(/Measured/)).toBeInTheDocument();
    // The size control, once the offering's instance is found, suggests double the size and says
    // what the volume holds and what growing it does.
    expect(await section.findByRole("textbox", { name: "New size" })).toHaveValue("2Gi");
    const explained = section.getByText(/holds the bridge's database and configuration/);
    expect(explained).toHaveTextContent(/can only grow/);
    expect(explained).toHaveTextContent(/keeping everything on it, and the bridge restarts/);
    expect(explained).toHaveTextContent(/A new bridge gets 5Gi/);
    expect(section.getByText("A whole number of Mi, Gi or Ti, from 1Gi up to 1Ti.")).toBeVisible();
  });

  it("gives the bridge a larger volume through its offering's instance", async () => {
    let sent: { path: string; body: unknown } | undefined;
    server.events.on("request:start", async ({ request }) => {
      if (request.method === "PUT" && request.url.includes("/instances/"))
        sent = { path: new URL(request.url).pathname, body: await request.clone().json() };
    });
    try {
      renderBridge("slack-brandon");
      const field = await screen.findByRole("textbox", { name: "New size" });
      await userEvent.clear(field);
      await userEvent.type(field, "4Gi");
      await userEvent.click(screen.getByRole("button", { name: "Set size" }));
      await waitFor(() =>
        expect(sent).toEqual({
          path: "/api/v1/bridge-offerings/mautrix-slack/instances/%40brandon%3Aexample.org",
          body: { volume_size: "4Gi" },
        }),
      );
      // The registration is read again: the volume has room, and the alert is gone.
      await waitFor(() => expect(screen.queryByTestId("volume-alert")).toBeNull());
      const section = within(screen.getByRole("region", { name: "Volume" }));
      expect(await section.findByText("4Gi")).toBeInTheDocument();
      expect(section.getByText(/of 4\.0 GiB used \(25%\)/)).toBeInTheDocument();
    } finally {
      server.events.removeAllListeners();
    }
  });

  it("shows the server's refusal of a smaller size under the field", async () => {
    // Grown to 4Gi already; the mock server refuses less, as the real one does.
    setVolumeSize("mautrix-slack", "@brandon:example.org", "4Gi");
    renderBridge("slack-brandon");
    const field = await screen.findByRole("textbox", { name: "New size" });
    expect(field).toHaveValue("8Gi");
    await userEvent.clear(field);
    await userEvent.type(field, "2Gi");
    await userEvent.click(screen.getByRole("button", { name: "Set size" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "volume_size 2Gi is less than the 4Gi the volume has now; a volume cannot shrink.",
    );
    expect(field).toHaveAttribute("aria-invalid", "true");
    // Nothing changed.
    expect(screen.getByRole("region", { name: "Volume" })).toHaveTextContent("4Gi");
  });

  it("shows the server's refusal of a size below the smallest", async () => {
    renderBridge("slack-brandon");
    const field = await screen.findByRole("textbox", { name: "New size" });
    await userEvent.clear(field);
    await userEvent.type(field, "512Mi");
    await userEvent.click(screen.getByRole("button", { name: "Set size" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "volume_size 512Mi is less than 1Gi, the smallest volume a bridge gets.",
    );
    expect(screen.getByTestId("volume-alert")).toBeInTheDocument();
  });

  it("checks how a size is written before asking the server", async () => {
    renderBridge("slack-brandon");
    const field = await screen.findByRole("textbox", { name: "New size" });
    await userEvent.clear(field);
    await userEvent.type(field, "10GB");
    await userEvent.click(screen.getByRole("button", { name: "Set size" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "A size is a whole number of Mi, Gi or Ti, such as 10Gi.",
    );
  });

  it("cannot set the size without bridges:write", async () => {
    signOut();
    await signIn(["bridges:read"]);
    renderBridge("slack-brandon");
    expect(await screen.findByRole("textbox", { name: "New size" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Set size" })).toBeDisabled();
  });

  it("has no volume section for a bridge run elsewhere", async () => {
    renderBridge("telegram");
    expect(await screen.findByRole("heading", { level: 1 })).toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "Volume" })).toBeNull();
    expect(screen.queryByTestId("volume-alert")).toBeNull();
  });
});
