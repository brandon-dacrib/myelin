import { afterEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { UserDetailPage } from "../UserDetailPage";

/**
 * The devices-and-identity controls on a user's page: renaming a device, signing several out
 * at once, binding and removing an email address, linking an upstream identity, switching an
 * experimental feature, and reading what their clients stored. Against the MSW handlers, which
 * answer as the real server does (`crates/hs-cli/tests/admin_user_identity.rs` is the proof
 * against the binary).
 */
const ROUTES = [{ path: "/users/$userId", component: UserDetailPage }];
const KNOWN = ["/", "/users", "/rooms/$roomId", "/audit"];
const ALICE = "/users/%40alice%3Aexample.org";
const ADMIN = "/users/%40admin%3Aexample.org";

/**
 * How long to wait for the page to mount and for a round trip to the mock server. The default
 * one second is too short when several agents' test suites share the machine: the first render
 * of the user page alone took 1.5 s with two suites running, and "renames a device", the first
 * test in the file, pays for the page's first load.
 */
const SLOW = { timeout: 5000 };
/** Room for three or four slow waits in one test. */
const SUITE = { timeout: 20_000 };

afterEach(() => {
  server.events.removeAllListeners();
  signOut();
});

function recordRequests(method: string, suffix: string) {
  const bodies: unknown[] = [];
  server.events.on("request:start", async ({ request }) => {
    if (request.method === method && new URL(request.url).pathname.endsWith(suffix)) {
      bodies.push(await request.clone().json());
    }
  });
  return bodies;
}

describe("A user's devices", SUITE, () => {
  it("renames a device", async () => {
    await signIn();
    const user = userEvent.setup();
    const sent = recordRequests("PATCH", "/devices/MOBILE1");
    renderRoutes(ROUTES, ADMIN, KNOWN);

    await user.click(await screen.findByRole("button", { name: "Rename Element iOS" }, SLOW));
    const dialog = within(await screen.findByRole("dialog", { name: "Rename MOBILE1" }));
    const input = dialog.getByLabelText("Device name");
    await user.clear(input);
    await user.type(input, "Old iPhone");
    await user.click(dialog.getByRole("button", { name: "Save name" }));

    expect(await screen.findByText("Old iPhone", {}, SLOW)).toBeInTheDocument();
    // The body is recorded when the request starts, after reading it, which is asynchronous.
    await waitFor(() => expect(sent).toEqual([{ display_name: "Old iPhone" }]));
  });

  it("signs the selected devices out together", async () => {
    await signIn();
    const user = userEvent.setup();
    const sent = recordRequests("POST", "/devices/bulk-delete");
    renderRoutes(ROUTES, ADMIN, KNOWN);

    const bulk = await screen.findByRole("button", { name: /^Sign out selected/ }, SLOW);
    expect(bulk).toBeDisabled();
    await user.click(screen.getByRole("checkbox", { name: /^Select Firefox/ }));
    await user.click(screen.getByRole("checkbox", { name: /^Select Old iPhone|^Select Element/ }));
    await user.click(screen.getByRole("button", { name: "Sign out selected (2)" }));
    const dialog = within(await screen.findByRole("dialog", { name: "Sign out 2 devices?" }));
    await user.click(dialog.getByRole("button", { name: "Sign out" }));

    expect(await screen.findByText("No devices.", {}, SLOW)).toBeInTheDocument();
    await waitFor(() => expect(sent).toEqual([{ device_ids: ["WEBDEV1", "MOBILE1"] }]));
  });
});

describe("A user's email addresses and linked identities", SUITE, () => {
  it("adds an email address and removes one", async () => {
    await signIn();
    const user = userEvent.setup();
    renderRoutes(ROUTES, ALICE, KNOWN);

    const section = within(
      (await screen.findByRole("heading", { name: "Email and phone" }, SLOW)).closest("section")!,
    );
    expect(await section.findByText("alice@example.org", {}, SLOW)).toBeInTheDocument();
    await user.type(section.getByLabelText("Email address"), "Alice.Work@Example.org");
    await user.click(section.getByRole("button", { name: "Add" }));
    expect(await section.findByText("alice.work@example.org", {}, SLOW)).toBeInTheDocument();

    await user.click(section.getByRole("button", { name: "Remove alice@example.org" }));
    const dialog = within(await screen.findByRole("dialog", { name: "Remove alice@example.org?" }));
    await user.click(dialog.getByRole("button", { name: "Remove" }));
    expect(section.queryByText("alice@example.org")).not.toBeInTheDocument();
  });

  it("shows the server's reason beside the address it refused", async () => {
    await signIn();
    const user = userEvent.setup();
    renderRoutes(ROUTES, ALICE, KNOWN);
    const section = within(
      (await screen.findByRole("heading", { name: "Email and phone" })).closest("section")!,
    );
    await user.type(await section.findByLabelText("Email address"), "not-an-address");
    await user.click(section.getByRole("button", { name: "Add" }));
    expect(await section.findByText(/is not an email address/, {}, SLOW)).toBeInTheDocument();
  });

  it("says who already has an identity another account links", async () => {
    await signIn();
    const user = userEvent.setup();
    renderRoutes(ROUTES, "/users/%40spammer42%3Aexample.org", KNOWN);
    const section = within(
      (await screen.findByRole("heading", { name: "Linked identities" }, SLOW)).closest("section")!,
    );
    await user.type(await section.findByLabelText("Provider"), "oidc-corp");
    await user.type(section.getByLabelText("Subject at the provider"), "248289761001");
    await user.click(section.getByRole("button", { name: "Link" }));
    expect(
      await section.findByText(/is linked to @alice:example.org/, {}, SLOW),
    ).toBeInTheDocument();
  });

  it("offers no add forms to somebody who can only read", async () => {
    await signIn(["admin:read"]);
    renderRoutes(ROUTES, ALICE, KNOWN);
    // (An earlier case removed alice@example.org from the shared mock, so this reads her link.)
    await screen.findByText("248289761001", {}, SLOW);
    expect(screen.queryByRole("button", { name: "Add" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Link" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Unlink 248289761001 at oidc-corp" })).toBeDisabled();
  });
});

describe("A user's features and client data", SUITE, () => {
  it("switches an experimental feature on for them alone", async () => {
    await signIn();
    const user = userEvent.setup();
    const sent: unknown[] = [];
    server.use(
      http.put("*/api/v1/users/:user_id/experimental-features", async ({ request }) => {
        sent.push(await request.json());
        return HttpResponse.json({ msc3575: false, msc3881: true, msc4222: true });
      }),
    );
    renderRoutes(ROUTES, ALICE, KNOWN);
    const remote = await screen.findByRole("switch", { name: /Remote push toggles/ }, SLOW);
    expect(remote).not.toBeChecked();
    expect(screen.getByRole("switch", { name: /state_after in sync/ })).toBeChecked();
    await user.click(remote);
    expect(
      await screen.findByRole("switch", { name: /Remote push toggles/, checked: true }, SLOW),
    ).toBeChecked();
    expect(sent).toEqual([{ msc3881: true }]);
  });

  it("lists their pushers and account data", async () => {
    await signIn();
    renderRoutes(ROUTES, ALICE, KNOWN);
    expect(await screen.findByText("Pixel 8", {}, SLOW)).toBeInTheDocument();
    expect(await screen.findByText("m.direct", {}, SLOW)).toBeInTheDocument();
    // An email pusher says it is one, and where the emails go.
    expect(screen.getByText(/^Email to/)).toHaveTextContent(
      "Email to alice@example.org · notification emails about unread messages",
    );
  });
});
