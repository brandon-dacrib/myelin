import { afterEach, describe, expect, it } from "vitest";
import { getSession, signOut } from "./auth";
import {
  NO_LINK_OPEN_MESSAGE,
  RecoveryError,
  WRONG_LINK_MESSAGE,
  inspectRecoveryLink,
  recoveryTimeLeft,
  recoveryTokenFromHash,
  resetAdministratorPassword,
} from "./recovery";
import { MOCK_RECOVERY_TOKEN, resetMockRecovery, useMockRecoveryLink } from "@/mocks/data/recovery";

afterEach(() => {
  signOut();
  resetMockRecovery();
});

describe("recoveryTokenFromHash", () => {
  it("reads the token out of a recovery link's fragment", () => {
    expect(recoveryTokenFromHash(`#token=${MOCK_RECOVERY_TOKEN}`)).toBe(MOCK_RECOVERY_TOKEN);
    expect(recoveryTokenFromHash(`token=${MOCK_RECOVERY_TOKEN}`)).toBe(MOCK_RECOVERY_TOKEN);
  });

  it("is null when there is no token to read", () => {
    expect(recoveryTokenFromHash("")).toBeNull();
    expect(recoveryTokenFromHash("#")).toBeNull();
    expect(recoveryTokenFromHash("#token=")).toBeNull();
    expect(recoveryTokenFromHash("#other=1")).toBeNull();
  });
});

describe("recoveryTimeLeft", () => {
  const now = 1_700_000_000_000;

  it("counts whole minutes down, never promising more than there is", () => {
    expect(recoveryTimeLeft(now + 15 * 60_000 - 5_000, now)).toBe("in 14 minutes");
    expect(recoveryTimeLeft(now + 2 * 60_000, now)).toBe("in 2 minutes");
    expect(recoveryTimeLeft(now + 119_000, now)).toBe("in a minute");
    expect(recoveryTimeLeft(now + 59_000, now)).toBe("in less than a minute");
  });

  it("is null once the moment has passed", () => {
    expect(recoveryTimeLeft(now, now)).toBeNull();
    expect(recoveryTimeLeft(now - 1, now)).toBeNull();
  });
});

describe("administrator recovery", () => {
  it("says what the link can do: the administrators, and when it expires", async () => {
    const before = Date.now();
    const inspection = await inspectRecoveryLink(` ${MOCK_RECOVERY_TOKEN} `);
    expect(inspection.administrators.map((a) => a.user_id)).toEqual([
      "@admin:example.org",
      "@ops:example.org",
    ]);
    expect(inspection.expiresAtMs).toBeGreaterThanOrEqual(before + 15 * 60_000);
    expect(getSession()).toBeNull();
  });

  it("refuses a wrong token as not this server's link, and spends nothing", async () => {
    await expect(inspectRecoveryLink("a-guess")).rejects.toMatchObject({
      refusal: "wrong-link",
      message: WRONG_LINK_MESSAGE,
    });
    // The right link still works afterwards.
    await expect(inspectRecoveryLink(MOCK_RECOVERY_TOKEN)).resolves.toBeTruthy();
  });

  it("says no link is open once it has been used, whatever token is sent", async () => {
    useMockRecoveryLink();
    await expect(inspectRecoveryLink(MOCK_RECOVERY_TOKEN)).rejects.toMatchObject({
      refusal: "no-link-open",
      message: NO_LINK_OPEN_MESSAGE,
    });
    await expect(
      resetAdministratorPassword({
        recoveryToken: MOCK_RECOVERY_TOKEN,
        userId: "@admin:example.org",
        password: "hunter2-ops",
      }),
    ).rejects.toMatchObject({ refusal: "no-link-open" });
    expect(getSession()).toBeNull();
  });

  it("puts the server's own reason beside the field it is about", async () => {
    await expect(
      resetAdministratorPassword({
        recoveryToken: MOCK_RECOVERY_TOKEN,
        userId: "@admin:example.org",
        password: "short",
      }),
    ).rejects.toMatchObject({
      refusal: "invalid",
      field: "password",
      message: expect.stringContaining("8"),
    });
    await expect(
      resetAdministratorPassword({
        recoveryToken: MOCK_RECOVERY_TOKEN,
        userId: "@nobody:example.org",
        password: "hunter2-ops",
      }),
    ).rejects.toMatchObject({ refusal: "invalid", field: "userId" });
    // Neither refusal consumed the link.
    await expect(inspectRecoveryLink(MOCK_RECOVERY_TOKEN)).resolves.toBeTruthy();
  });

  it("resets the password, signs in as the account, and is then over", async () => {
    const session = await resetAdministratorPassword({
      recoveryToken: MOCK_RECOVERY_TOKEN,
      userId: "@ops:example.org",
      password: "hunter2-ops",
    });
    expect(session.scopes).toContain("admin:write");
    expect(getSession()).toBe(session);

    const again = await inspectRecoveryLink(MOCK_RECOVERY_TOKEN).catch((e: unknown) => e);
    expect(again).toBeInstanceOf(RecoveryError);
    expect((again as RecoveryError).refusal).toBe("no-link-open");
  });
});
