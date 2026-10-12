import { execSync } from "node:child_process";
import { deflateSync } from "node:zlib";
import { test, expect, type APIRequestContext, type Browser, type Page } from "@playwright/test";
import { settle } from "./settle";

/**
 * The screenshots in `docs/user-guide.md`, taken from a real `hs serve` with nothing mocked.
 *
 * Needs a server just started on an empty data directory with the server name `example.org`,
 * and the setup token from its log (the part of the setup link after `#token=`):
 *
 *   HS_REAL_SERVER_URL=http://127.0.0.1:8461 HS_REAL_UI_PORT=4461 \
 *   HS_USER_GUIDE_SETUP_TOKEN=... \
 *   HS_USER_GUIDE_RECOVER_CMD="../target/debug/hs recover --data-dir <dir> --server http://127.0.0.1:8461" \
 *   HS_USER_GUIDE_SCREENSHOTS=1 \
 *     npx playwright test --config playwright.real.config.ts user-guide
 *
 * It makes the first administrator through the setup page, seeds made-up people and rooms
 * (alice, bob, carol and dave; #general, #random and #old-project) through the admin and
 * client-server APIs, and then does what the guide describes through the interface: adds a
 * user, deactivates one, makes an invite link, offers WhatsApp, deletes a room, sends a server
 * notice. Those change the server, so each run needs a fresh one.
 *
 * `HS_USER_GUIDE_SCREENSHOTS=1` writes the images to `docs/user-guide/`; without it they go to
 * `test-results/user-guide/`, so a run to check something leaves the committed ones alone.
 * `HS_USER_GUIDE_RECOVER_CMD` is optional: a command whose standard output is a recovery link
 * (`hs recover` prints only the link there); without it the recovery screenshot is skipped.
 *
 * No secret is readable in any image: password fields are masked, the setup and recovery
 * tokens live in the address's fragment (which a page screenshot does not show), and the one
 * token that is shown, the invite link's, is a made-up word on a throwaway server.
 *
 * Skipped, like the rest of `e2e-real/`, unless `HS_REAL_SERVER_URL` is set.
 */
const hasServer = Boolean(process.env.HS_REAL_SERVER_URL);
const setupToken = process.env.HS_USER_GUIDE_SETUP_TOKEN;
const recoverCmd = process.env.HS_USER_GUIDE_RECOVER_CMD;
const OUT =
  process.env.HS_USER_GUIDE_SCREENSHOTS === "1" ? "../docs/user-guide" : "test-results/user-guide";

// A throwaway demo server's accounts. Nothing here is a real person's or a real password.
const ADMIN = "admin";
const ADMIN_PASSWORD = "demo-admin-password-1";
const PEOPLE = [
  { localpart: "alice", display_name: "Alice Example" },
  { localpart: "bob", display_name: "Bob Example" },
  { localpart: "carol", display_name: "Carol Example" },
  { localpart: "dave", display_name: "Dave Example" },
] as const;
const passwordOf = (localpart: string) => `demo-password-${localpart}-1`;
const INVITE_TOKEN = "welcome-to-example-org";
const VIEWPORT = { width: 1280, height: 800 };

test.use({ viewport: VIEWPORT, colorScheme: "light", actionTimeout: 15_000 });

/**
 * Saves the viewport as `<OUT>/<name>.png`. Toasts are dismissed first, since one confirming
 * the last action would cover the corner of the page it is about, unless the toast is the
 * point (`keepToasts`).
 */
async function shot(page: Page, name: string, options: { keepToasts?: boolean } = {}) {
  const dismiss = page.getByRole("button", { name: "Dismiss" });
  for (let i = 0; !options.keepToasts && i < 10 && (await dismiss.count()) > 0; i++) {
    await dismiss
      .first()
      .click({ timeout: 2_000 })
      .catch(() => {});
    await page.waitForTimeout(250);
  }
  await settle(page);
  await page.screenshot({ path: `${OUT}/${name}.png`, animations: "disabled" });
}

/** A page in a new browser context: nobody signed in, as somebody else's browser would be. */
async function freshPage(browser: Browser, baseURL: string | undefined): Promise<Page> {
  const context = await browser.newContext({ viewport: VIEWPORT, colorScheme: "light", baseURL });
  return context.newPage();
}

async function login(request: APIRequestContext, user: string, password: string) {
  const response = await request.post("/_matrix/client/v3/login", {
    data: { type: "m.login.password", identifier: { type: "m.id.user", user }, password },
  });
  expect(response.ok(), await response.text()).toBe(true);
  return (await response.json()) as { access_token: string; user_id: string };
}

/** A 96x96 PNG with a soft diagonal gradient, so the media page has a picture to show. */
function demoPng(): Buffer {
  const size = 96;
  const raw = Buffer.alloc((size * 3 + 1) * size);
  for (let y = 0; y < size; y++) {
    const row = y * (size * 3 + 1);
    raw[row] = 0; // filter: none
    for (let x = 0; x < size; x++) {
      const i = row + 1 + x * 3;
      raw[i] = 60 + Math.round((x / size) * 120);
      raw[i + 1] = 120 + Math.round((y / size) * 100);
      raw[i + 2] = 200;
    }
  }
  const crcTable = Array.from({ length: 256 }, (_, n) => {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });
  const crc = (bytes: Buffer) => {
    let c = 0xffffffff;
    for (const b of bytes) c = crcTable[(c ^ b) & 0xff]! ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const chunk = (type: string, data: Buffer) => {
    const length = Buffer.alloc(4);
    length.writeUInt32BE(data.length);
    const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
    const sum = Buffer.alloc(4);
    sum.writeUInt32BE(crc(body));
    return Buffer.concat([length, body, sum]);
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header[8] = 8; // bit depth
  header[9] = 2; // colour type: RGB
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

interface Seeded {
  server: string;
  generalId: string;
  oldProjectId: string;
}

/**
 * Makes the demo people and rooms: users through the admin API, rooms and messages as those
 * users through the client-server API.
 */
async function seed(request: APIRequestContext): Promise<Seeded> {
  const admin = await login(request, ADMIN, ADMIN_PASSWORD);
  const server = admin.user_id.slice(admin.user_id.indexOf(":") + 1);
  for (const person of PEOPLE) {
    const created = await request.post("/api/v1/users", {
      headers: {
        authorization: `Bearer ${admin.access_token}`,
        "idempotency-key": `user-guide-${person.localpart}`,
      },
      data: { ...person, admin: false, password: passwordOf(person.localpart) },
    });
    expect(created.ok(), await created.text()).toBe(true);
  }
  const tokens: Record<string, string> = {};
  for (const person of PEOPLE) {
    const session = await login(request, person.localpart, passwordOf(person.localpart));
    tokens[person.localpart] = session.access_token;
  }
  const as = (localpart: string) => ({ authorization: `Bearer ${tokens[localpart]}` });
  let txn = 0;

  async function room(by: string, alias: string, name: string, topic: string) {
    const created = await request.post("/_matrix/client/v3/createRoom", {
      headers: as(by),
      data: { name, topic, room_alias_name: alias, preset: "public_chat", visibility: "public" },
    });
    expect(created.ok(), await created.text()).toBe(true);
    return ((await created.json()) as { room_id: string }).room_id;
  }
  async function join(localpart: string, roomId: string) {
    const response = await request.post(`/_matrix/client/v3/join/${encodeURIComponent(roomId)}`, {
      headers: as(localpart),
      data: {},
    });
    expect(response.ok(), await response.text()).toBe(true);
  }
  async function send(localpart: string, roomId: string, content: Record<string, unknown>) {
    const response = await request.put(
      `/_matrix/client/v3/rooms/${encodeURIComponent(roomId)}/send/m.room.message/guide-${Date.now()}-${txn++}`,
      { headers: as(localpart), data: content },
    );
    expect(response.ok(), await response.text()).toBe(true);
    return ((await response.json()) as { event_id: string }).event_id;
  }
  const say = (localpart: string, roomId: string, body: string) =>
    send(localpart, roomId, { msgtype: "m.text", body });

  const generalId = await room("alice", "general", "General", "Everything about example.org");
  for (const person of ["bob", "carol", "dave"]) await join(person, generalId);
  await say("alice", generalId, "Good morning, everyone.");
  await say("bob", generalId, "Morning! The new server is quick.");
  await say("carol", generalId, "Has anyone tried the WhatsApp bridge yet?");
  await say("alice", generalId, "Not yet. I'll ask the admin.");
  const spam = await say("dave", generalId, "Cheap watches at totally-real-shop.example");
  // carol reports dave's message, so the Reports page has something in it.
  const report = await request.post(
    `/_matrix/client/v3/rooms/${encodeURIComponent(generalId)}/report/${encodeURIComponent(spam)}`,
    { headers: as("carol"), data: { reason: "Spam" } },
  );
  expect(report.ok(), await report.text()).toBe(true);

  const randomId = await room("bob", "random", "Random", "Anything else");
  await join("alice", randomId);
  await say("bob", randomId, "Lunch at noon?");
  await say("alice", randomId, "Sounds good.");
  const upload = await request.post("/_matrix/media/v3/upload?filename=gradient.png", {
    headers: { ...as("alice"), "content-type": "image/png" },
    data: demoPng(),
  });
  expect(upload.ok(), await upload.text()).toBe(true);
  const { content_uri } = (await upload.json()) as { content_uri: string };
  await send("alice", randomId, { msgtype: "m.image", body: "gradient.png", url: content_uri });
  // An invitation to a server that cannot exist (`.invalid` never resolves), so the Federation
  // page has a destination to show, failing.
  const invite = await request.post(
    `/_matrix/client/v3/rooms/${encodeURIComponent(randomId)}/invite`,
    { headers: as("bob"), data: { user_id: "@frank:example.invalid" } },
  );
  expect(await invite.text()).toContain("example.invalid");

  // A room nobody needs any more, for "delete a room".
  const oldProjectId = await room("carol", "old-project", "Old project", "Finished last year");
  await say("carol", oldProjectId, "Closing this room down.");

  return { server, generalId, oldProjectId };
}

test.describe("user guide screenshots", () => {
  test.skip(!hasServer, "HS_REAL_SERVER_URL not set — see playwright.real.config.ts");

  test("walk the interface in the guide's order", async ({ page, request, browser, baseURL }) => {
    test.setTimeout(600_000);

    // First sign-in: the setup page the log's link opens.
    const setup = (await (await request.get("/api/v1/setup")).json()) as { needs_setup: boolean };
    test.skip(!setup.needs_setup, "needs a fresh server: this one already has an administrator");
    test.skip(!setupToken, "set HS_USER_GUIDE_SETUP_TOKEN to the setup link's token");
    await page.goto(`/admin/setup#token=${setupToken}`);
    await expect(
      page.getByRole("heading", { name: "Create the first administrator" }),
    ).toBeVisible();
    await page.getByLabel("Username").fill(ADMIN);
    await page.getByLabel("Password").first().fill(ADMIN_PASSWORD);
    await page.getByLabel("Confirm password").fill(ADMIN_PASSWORD);
    await shot(page, "setup");
    await page.getByRole("button", { name: "Create administrator" }).click();
    await expect(page.getByRole("banner")).toBeVisible({ timeout: 20_000 });
    // The Overview's counts are recounted at most once a minute (hs-cli's overview source).
    const countedAt = Date.now();

    const seeded = await seed(request);
    const userPage = (localpart: string) =>
      `/admin/users/${encodeURIComponent(`@${localpart}:${seeded.server}`)}`;

    // The Overview, counted after the seeding rather than before it. Its count of users also
    // decides whether the sidebar offers Migration (only on a server with one user), so every
    // screenshot after this one shows the sidebar of a server in use.
    await page.waitForTimeout(Math.max(0, countedAt + 62_000 - Date.now()));
    await page.goto("/admin/");
    await expect(page.getByRole("heading", { name: "Overview", level: 1 })).toBeVisible();
    await expect(page.getByRole("link", { name: "Migration" })).toBeHidden();
    await shot(page, "overview");

    // Users.
    await page.goto("/admin/users");
    await expect(page.getByRole("cell", { name: `@dave:${seeded.server}` })).toBeVisible();
    await shot(page, "users-list");

    await page.getByRole("button", { name: "Add user" }).click();
    let dialog = page.getByRole("dialog");
    await dialog.getByLabel("Username").fill("erin");
    await dialog.getByLabel("Display name").fill("Erin Example");
    await dialog.getByLabel("Password").fill(passwordOf("erin"));
    await shot(page, "add-user");
    await dialog.getByRole("button", { name: "Create account" }).click();
    await expect(dialog.getByRole("heading", { name: "Account created" })).toBeVisible();
    await dialog.getByRole("button", { name: "Done" }).click();
    await expect(dialog).toBeHidden();

    await page.goto(userPage("alice"));
    await expect(page.getByRole("heading", { name: "Alice Example" })).toBeVisible();
    await shot(page, "user-page");

    await page.getByRole("button", { name: "Reset password" }).click();
    dialog = page.getByRole("dialog");
    await expect(dialog.getByLabel("New password")).toBeVisible();
    await shot(page, "reset-password");
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).toBeHidden();

    await page.goto(userPage("dave"));
    await expect(page.getByRole("heading", { name: "Dave Example" })).toBeVisible();
    await page.getByRole("button", { name: "Deactivate", exact: true }).click();
    dialog = page.getByRole("dialog");
    await expect(dialog.getByText(`Deactivate @dave:${seeded.server}?`)).toBeVisible();
    await shot(page, "deactivate-user");
    await dialog.getByRole("button", { name: "Deactivate", exact: true }).click();
    await expect(page.getByText("Deactivated").first()).toBeVisible({ timeout: 15_000 });

    // Invite links, and what the invited person sees.
    await page.goto("/admin/settings/registration-tokens");
    await page.getByRole("button", { name: "Create invite link" }).first().click();
    dialog = page.getByRole("dialog");
    await dialog.getByRole("radio", { name: "Choose my own" }).click();
    await dialog.getByLabel("Custom token").fill(INVITE_TOKEN);
    await shot(page, "invite-link-create");
    await dialog.getByRole("button", { name: "Create invite link" }).click();
    await expect(dialog.getByRole("heading", { name: "Invite link ready" })).toBeVisible();
    await dialog.getByRole("button", { name: "Done" }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByText(INVITE_TOKEN).first()).toBeVisible();
    await shot(page, "invite-links");

    const invited = await freshPage(browser, baseURL);
    await invited.goto(`/admin/register?token=${INVITE_TOKEN}`);
    await expect(invited.getByRole("heading", { name: "Create your account" })).toBeVisible();
    await invited.getByLabel("Username").fill("frances");
    await expect(invited.getByLabel("Password").first()).toBeVisible();
    await shot(invited, "register-with-invite");
    await invited.context().close();

    await page.goto("/admin/configuration/auth");
    await expect(page.getByText("auth.enable_registration")).toBeVisible();
    await shot(page, "registration-setting");

    // Rooms.
    await page.goto("/admin/rooms");
    await expect(page.getByRole("cell", { name: "Old project" })).toBeVisible();
    await shot(page, "rooms-list");

    await page.goto(`/admin/rooms/${encodeURIComponent(seeded.generalId)}`);
    await expect(page.getByRole("heading", { name: "General", level: 1 })).toBeVisible();
    await shot(page, "room-page");
    await page.getByRole("tab", { name: "Timeline" }).click();
    await expect(
      page.getByRole("list", { name: "Messages" }).getByText("Good morning, everyone."),
    ).toBeVisible();
    await shot(page, "room-timeline");

    await page.goto(`/admin/rooms/${encodeURIComponent(seeded.oldProjectId)}`);
    await expect(page.getByRole("heading", { name: "Old project", level: 1 })).toBeVisible();
    await page.getByRole("button", { name: "Delete room" }).click();
    dialog = page.getByRole("dialog");
    await dialog.getByLabel(/to confirm/).fill("Old project");
    await shot(page, "room-delete");
    await dialog.getByRole("button", { name: "Delete room" }).click();
    await expect(page.getByRole("heading", { name: "Rooms", level: 1 })).toBeVisible({
      timeout: 60_000,
    });

    // Bridges: offer WhatsApp, and add one for alice.
    await page.goto("/admin/bridges");
    await expect(page.getByText("No bridges offered yet")).toBeVisible();
    await shot(page, "bridges-empty");
    await page.getByRole("button", { name: "Offer a bridge" }).first().click();
    await page.getByRole("heading", { name: "Messaging" }).waitFor();
    await page.getByRole("radio", { name: /WhatsApp/ }).click();
    await shot(page, "bridge-wizard");
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page.getByRole("heading", { name: "Access" })).toBeVisible();
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page.getByRole("heading", { name: "Runtime" })).toBeVisible();
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page.getByRole("heading", { name: "Options" })).toBeVisible();
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page.getByRole("heading", { name: "Review" })).toBeVisible();
    await page.getByRole("button", { name: "Offer WhatsApp" }).click();
    await expect(page).toHaveURL(/\/bridges\/offerings\/mautrix-whatsapp$/);
    await page.getByRole("button", { name: "Add for a user" }).click();
    dialog = page.getByRole("dialog");
    await dialog.getByLabel(/Matrix ID/).fill(`@alice:${seeded.server}`);
    await dialog.getByRole("button", { name: "Add bridge" }).click();
    await dialog.getByRole("button", { name: "Done" }).click();
    await expect(dialog).toBeHidden();
    const row = page.getByRole("table").getByRole("row", { name: /@alice/ });
    await expect(row).toContainText("Starting", { timeout: 30_000 });
    await shot(page, "bridge-offering");
    await page.goto("/admin/bridges");
    await expect(page.getByRole("table").getByRole("link", { name: "WhatsApp" })).toBeVisible();
    await shot(page, "bridges-offered");
    await page.goto("/admin/bridges/registrations");
    await expect(page.getByRole("heading", { name: "Bridges", level: 1 })).toBeVisible();
    await shot(page, "bridge-registrations");

    // Federation.
    await page.goto("/admin/federation");
    const destination = page.getByRole("link", { name: "example.invalid" });
    await expect(destination).toBeVisible();
    await shot(page, "federation-list");
    await destination.click();
    await expect(page.getByRole("heading", { name: "example.invalid", level: 1 })).toBeVisible();
    await shot(page, "federation-destination");

    // Configuration.
    await page.goto("/admin/configuration");
    await expect(page.getByRole("heading", { name: "Configuration", level: 1 })).toBeVisible();
    await shot(page, "configuration");
    await page.goto("/admin/configuration/auth");
    await page.getByLabel("Allow guest access").click();
    await shot(page, "configuration-change");
    await page.getByRole("button", { name: "Review and save" }).click();
    dialog = page.getByRole("dialog");
    await expect(dialog.getByText(/straight away/)).toBeVisible();
    await shot(page, "configuration-review");
    await dialog.getByRole("button", { name: "Save changes" }).click();
    await expect(page.getByText("Sign-in and registration saved")).toBeVisible();
    await shot(page, "configuration-saved", { keepToasts: true });

    // The other pages.
    await page.goto("/admin/media");
    await expect(page.getByRole("button", { name: "gradient.png" })).toBeVisible();
    await shot(page, "media");

    await page.goto("/admin/reports");
    const reported = page.getByRole("table").getByRole("link").first();
    await expect(reported).toBeVisible();
    await shot(page, "reports");
    await reported.click();
    await expect(page.getByText("Cheap watches").first()).toBeVisible();
    await shot(page, "report");
    // dave was deactivated above; the decision records that.
    await page.getByRole("heading", { name: "Decide" }).scrollIntoViewIfNeeded();
    await page.getByRole("radio", { name: /Deactivated the user/ }).check();
    await page
      .getByRole("textbox", { name: "Note" })
      .fill("Spam account. Deactivated from their user page.");
    await shot(page, "report-decide");
    await page.getByRole("button", { name: "Resolve report" }).click();
    await expect(page.getByText("Resolved").first()).toBeVisible();

    await page.goto("/admin/tasks");
    await expect(page.getByRole("table")).toBeVisible();
    await shot(page, "tasks");

    await page.goto("/admin/audit");
    await expect(page.getByRole("table")).toBeVisible();
    await shot(page, "audit-log");

    await page.goto("/admin/settings/admin-tokens");
    await page.getByRole("button", { name: /Mint/ }).first().click();
    dialog = page.getByRole("dialog");
    await dialog.getByLabel("Name").fill("Nightly report script");
    await shot(page, "api-token-mint");
    await dialog.getByRole("button", { name: "Cancel" }).click();

    await page.goto("/admin/settings/server-notices");
    await page.getByLabel(/Recipients/).fill(`@alice:${seeded.server}`);
    await page.getByLabel(/Recipients/).press("Enter");
    await page
      .getByLabel("Message")
      .fill("The server restarts for an upgrade on Saturday at 08:00 UTC.");
    await page.getByRole("button", { name: "Send notice" }).click();
    await expect(page.getByText("Notice sent to 1 user.")).toBeVisible();
    await shot(page, "server-notices");

    await page.goto("/admin/cluster");
    await expect(page.getByRole("heading", { name: "Cluster", level: 1 })).toBeVisible();
    await shot(page, "cluster");

    await page.goto("/admin/migration");
    await expect(page.getByRole("heading", { name: "Migration from Synapse" })).toBeVisible();
    await shot(page, "migration");

    await page.goto("/admin/statistics");
    await expect(page.getByRole("heading", { name: "Statistics", level: 1 })).toBeVisible();
    await shot(page, "statistics");

    // Signing in again later, from a browser with no session.
    const later = await freshPage(browser, baseURL);
    await later.goto("/admin/");
    await expect(later.getByRole("tab", { name: "Username & password" })).toBeVisible();
    await shot(later, "sign-in");
    await later.context().close();

    if (recoverCmd) {
      const link = execSync(recoverCmd, { encoding: "utf8" }).trim();
      const locked = await freshPage(browser, baseURL);
      await locked.goto(`/admin/recover${new URL(link).hash}`);
      await expect(locked.getByLabel("New password")).toBeVisible();
      await shot(locked, "recover");
      await locked.context().close();
    }
  });
});
