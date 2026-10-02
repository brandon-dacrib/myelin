import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { EditBridgeDialog } from "./EditBridgeDialog";
import { appservices } from "@/mocks/data/appservices";
import type { AppService, AppserviceNamespaces } from "@/api/bridges";
import { signIn, signOut } from "@/lib/auth";

const whatsapp = () => appservices.find((a) => a.id === "whatsapp")!;

/** A namespaces object with one exclusive users rule, typed as the generated schema has it. */
function withUsers(regex: string): AppService["namespaces"] {
  const namespaces: AppserviceNamespaces = { users: [{ regex, exclusive: true }] };
  return namespaces as unknown as AppService["namespaces"];
}
const before = JSON.parse(JSON.stringify(whatsapp())) as AppService;

function renderDialog(bridge: AppService = whatsapp()) {
  const onOpenChange = vi.fn();
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <EditBridgeDialog bridge={bridge} name="WhatsApp" open onOpenChange={onOpenChange} />
    </QueryClientProvider>,
  );
  return { onOpenChange };
}

beforeEach(async () => {
  await signIn();
});

afterEach(() => {
  Object.assign(whatsapp(), JSON.parse(JSON.stringify(before)));
  signOut();
});

describe("EditBridgeDialog", () => {
  it("adds a user namespace rule and sends every rule, with the url left alone", async () => {
    const user = userEvent.setup();
    const bodies: Record<string, unknown>[] = [];
    server.use(
      http.patch("*/api/v1/appservices/:id", async ({ request }) => {
        const body = (await request.json()) as Record<string, unknown>;
        bodies.push(body);
        return HttpResponse.json({ ...whatsapp(), ...body });
      }),
    );
    const { onOpenChange } = renderDialog({
      ...whatsapp(),
      namespaces: withUsers("@whatsapp_.*:example\\.org"),
    });
    const dialog = within(await screen.findByRole("dialog", { name: "Edit WhatsApp" }));
    expect(dialog.getByText(/tokens and bot name are not changed here/)).toBeInTheDocument();
    expect(dialog.getByLabelText("User namespaces pattern")).toHaveValue(
      "@whatsapp_.*:example\\.org",
    );
    await user.click(dialog.getAllByRole("button", { name: "Add rule" })[1]!);
    await user.type(dialog.getByLabelText("Alias namespaces pattern"), "#wa_.*:example\\.org");
    await user.click(dialog.getByRole("button", { name: "Save" }));
    await vi.waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
    expect(bodies).toEqual([
      {
        namespaces: {
          users: [{ regex: "@whatsapp_.*:example\\.org", exclusive: true }],
          aliases: [{ regex: "#wa_.*:example\\.org", exclusive: true }],
          rooms: [],
        },
      },
    ]);
  });

  it("clears the url to null and turns rate limiting on, through the mock server", async () => {
    const user = userEvent.setup();
    const { onOpenChange } = renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Edit WhatsApp" }));
    await user.clear(dialog.getByLabelText(/^URL/));
    await user.click(dialog.getByRole("switch", { name: /rate limited/i }));
    await user.click(dialog.getByRole("button", { name: "Save" }));
    await vi.waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
    expect(whatsapp().url).toBeNull();
    expect(whatsapp().rate_limited).toBe(true);
  });

  it("refuses an empty rule before asking the server, and shows a namespace conflict", async () => {
    const user = userEvent.setup();
    const { onOpenChange } = renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Edit WhatsApp" }));
    await user.click(dialog.getAllByRole("button", { name: "Add rule" })[0]!);
    await user.click(dialog.getByRole("button", { name: "Save" }));
    expect(await dialog.findByRole("alert")).toHaveTextContent(/Every rule needs a pattern/);

    await user.type(dialog.getByLabelText("User namespaces pattern"), "@irc_.*:example\\.org");
    await user.click(dialog.getByRole("button", { name: "Save" }));
    expect(await dialog.findByRole("alert")).toHaveTextContent(
      /overlaps the exclusive users namespace of irc/,
    );
    expect(onOpenChange).not.toHaveBeenCalled();
  });

  it("removes a rule", async () => {
    const user = userEvent.setup();
    const { onOpenChange } = renderDialog({
      ...whatsapp(),
      namespaces: withUsers("@whatsapp_.*:example\\.org"),
    });
    const dialog = within(await screen.findByRole("dialog", { name: "Edit WhatsApp" }));
    await user.click(dialog.getByRole("button", { name: "Remove user namespaces rule" }));
    expect(dialog.queryByLabelText("User namespaces pattern")).not.toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Save" }));
    await vi.waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
    expect(whatsapp().namespaces).toEqual({ users: [], aliases: [], rooms: [] });
  });
});
