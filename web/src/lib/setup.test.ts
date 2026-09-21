import { afterEach, describe, expect, it } from "vitest";
import { getSession, hasScope, signOut } from "./auth";
import { SetupError, createFirstAdministrator, fetchNeedsSetup, setupTokenFromHash } from "./setup";
import { MOCK_SETUP_TOKEN_KEY } from "@/mocks/handlers";

const TOKEN = "mockSetupTokenMockSetupTokenMockSetupTok";

function openSetup() {
  sessionStorage.setItem(MOCK_SETUP_TOKEN_KEY, TOKEN);
}

afterEach(() => {
  signOut();
  sessionStorage.removeItem(MOCK_SETUP_TOKEN_KEY);
});

describe("setupTokenFromHash", () => {
  it("reads the token out of a setup link's fragment", () => {
    expect(setupTokenFromHash(`#token=${TOKEN}`)).toBe(TOKEN);
    expect(setupTokenFromHash(`token=${TOKEN}`)).toBe(TOKEN);
  });

  it("is null when there is no token to read", () => {
    expect(setupTokenFromHash("")).toBeNull();
    expect(setupTokenFromHash("#")).toBeNull();
    expect(setupTokenFromHash("#token=")).toBeNull();
    expect(setupTokenFromHash("#other=1")).toBeNull();
  });
});

describe("first-run setup", () => {
  it("is not on offer from a server that has its administrator", async () => {
    expect(await fetchNeedsSetup()).toBe(false);
    await expect(
      createFirstAdministrator({ setupToken: TOKEN, username: "ops", password: "hunter2-ops" }),
    ).rejects.toMatchObject({ alreadySetUp: true });
    expect(getSession()).toBeNull();
  });

  it("creates the administrator, signs in as them, and is then over", async () => {
    openSetup();
    expect(await fetchNeedsSetup()).toBe(true);

    const session = await createFirstAdministrator({
      setupToken: ` ${TOKEN} `,
      username: "Ops",
      password: "hunter2-ops",
    });
    expect(session.scopes).toContain("admin:write");
    expect(getSession()).toBe(session);
    expect(hasScope("bridges:write")).toBe(true);

    expect(await fetchNeedsSetup()).toBe(false);
  });

  it("puts a wrong token's message beside the token, and spends nothing", async () => {
    openSetup();
    const error = await createFirstAdministrator({
      setupToken: "a-guess",
      username: "ops",
      password: "hunter2-ops",
    }).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(SetupError);
    expect((error as SetupError).field).toBe("setupToken");
    expect(getSession()).toBeNull();
    expect(await fetchNeedsSetup()).toBe(true);
  });

  it("puts the server's own reason beside the field it is about", async () => {
    openSetup();
    await expect(
      createFirstAdministrator({ setupToken: TOKEN, username: "ops", password: "short" }),
    ).rejects.toMatchObject({ field: "password", message: expect.stringContaining("8") });
    await expect(
      createFirstAdministrator({
        setupToken: TOKEN,
        username: "not a username",
        password: "hunter2-ops",
      }),
    ).rejects.toMatchObject({ field: "username" });
    // Neither refusal closed the offer.
    expect(await fetchNeedsSetup()).toBe(true);
  });
});
