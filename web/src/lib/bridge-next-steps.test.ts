import { describe, expect, it } from "vitest";
import type { BridgeLogins, BridgeType } from "@/api/bridges";
import {
  encodeLocalpart,
  firstCommand,
  instanceBotId,
  nextStepsMessage,
  nextStepsPhase,
  phaseLabel,
} from "./bridge-next-steps";

const whatsapp = {
  sign_in: {
    steps: [
      "Start a direct chat with {bot} and send `login qr`, or `login phone` to get a pairing code instead.",
      "Scan the code with WhatsApp on the phone.",
    ],
    notes: "WhatsApp unlinks the bridge if the phone stays offline for more than two weeks.",
  },
} as BridgeType;

function logins(overrides: Partial<BridgeLogins>): BridgeLogins {
  return {
    appservice_id: "whatsapp-carol",
    provisioning_api: "mautrix_v3",
    supported: true,
    cached: false,
    logins: [],
    signed_in: false,
    ...overrides,
  };
}

describe("encodeLocalpart", () => {
  it("keeps what the catalogue allows and hex-escapes the rest, RFC 0017 section 3", () => {
    expect(encodeLocalpart("alice")).toBe("alice");
    expect(encodeLocalpart("a.b-c/d9")).toBe("a.b-c/d9");
    expect(encodeLocalpart("Alice_1")).toBe("=41lice=5f1");
    expect(encodeLocalpart("é")).toBe("=c3=a9");
  });
});

describe("instanceBotId", () => {
  const door = { front_door: "@whatsappbot:example.org" };

  it("takes the server's word when it gives one", () => {
    expect(
      instanceBotId({ bot: "@whatsappbot_carol:example.org", user_id: "@carol:example.org" }, door),
    ).toBe("@whatsappbot_carol:example.org");
  });

  it("derives it from the front door and the owner otherwise, RFC 0017 section 4.1", () => {
    expect(instanceBotId({ bot: null, user_id: "@carol:example.org" }, door)).toBe(
      "@whatsappbot_carol:example.org",
    );
    expect(instanceBotId({ user_id: "@Carol_X:example.org" }, door)).toBe(
      "@whatsappbot_=43arol=5f=58:example.org",
    );
  });

  it("has nothing to say for a shared instance or an offering without a front door", () => {
    expect(instanceBotId({ bot: null, user_id: null }, door)).toBeUndefined();
    expect(
      instanceBotId({ bot: null, user_id: "@carol:example.org" }, { front_door: null }),
    ).toBeUndefined();
    expect(instanceBotId({ bot: null, user_id: "carol" }, door)).toBeUndefined();
  });
});

describe("nextStepsPhase", () => {
  const ready = { state: "ready" as const, appservice_id: "whatsapp-carol" };
  const pending = { status: "pending" as const };

  it("says the bridge is on its way, in this cluster or elsewhere", () => {
    for (const state of ["requested", "registered", "deploying", "starting"] as const) {
      expect(nextStepsPhase({ state, appservice_id: null }, "cluster", pending)).toEqual({
        kind: "setting-up",
      });
      expect(nextStepsPhase({ state, appservice_id: null }, "elsewhere", pending)).toEqual({
        kind: "waiting-elsewhere",
      });
    }
  });

  it("has nothing to tell anyone for a failed or departing bridge", () => {
    expect(nextStepsPhase({ state: "failed", appservice_id: "x" }, "cluster", pending)).toEqual({
      kind: "failed",
    });
    expect(nextStepsPhase({ state: "removing", appservice_id: "x" }, "cluster", pending)).toEqual({
      kind: "removing",
    });
  });

  it("waits for the bridge's answer, then shows the steps to someone who has not signed in", () => {
    expect(nextStepsPhase(ready, "cluster", pending)).toEqual({ kind: "asking" });
    expect(nextStepsPhase(ready, "cluster", { status: "success", data: logins({}) })).toEqual({
      kind: "sign-in",
      asked: "no",
    });
  });

  it("hides the steps once they have signed in, naming the account", () => {
    expect(
      nextStepsPhase(ready, "cluster", {
        status: "success",
        data: logins({
          signed_in: true,
          logins: [
            {
              remote_id: "15551234567",
              remote_name: "+1 555-123-4567",
              state: "connected",
              user_id: "@carol:example.org",
            },
          ],
        }),
      }),
    ).toEqual({ kind: "signed-in", as: "+1 555-123-4567" });
  });

  it("still shows the steps when the bridge could not be asked, and says why", () => {
    expect(nextStepsPhase(ready, "cluster", { status: "error" })).toMatchObject({
      kind: "sign-in",
      asked: "could-not",
    });
    expect(
      nextStepsPhase(ready, "cluster", {
        status: "success",
        data: logins({
          error: { status: 502, reason: "unreachable", detail: "connection refused" },
        }),
      }),
    ).toEqual({ kind: "sign-in", asked: "could-not", detail: "connection refused" });
    expect(
      nextStepsPhase(ready, "cluster", {
        status: "success",
        data: logins({ supported: false, reason: "No provisioning API." }),
      }),
    ).toEqual({ kind: "sign-in", asked: "not-reported", detail: "No provisioning API." });
    expect(
      nextStepsPhase({ state: "ready", appservice_id: null }, "cluster", pending),
    ).toMatchObject({ kind: "sign-in", asked: "could-not" });
  });

  it("labels each phase for the operator, and for the operator's own bridge", () => {
    expect(phaseLabel({ kind: "sign-in", asked: "no" }, false)).toBe(
      "Ready: tell them how to sign in",
    );
    expect(phaseLabel({ kind: "sign-in", asked: "no" }, true)).toBe("Ready: sign in");
    expect(phaseLabel({ kind: "setting-up" }, false)).toBe("Setting up");
  });
});

describe("firstCommand", () => {
  it("lifts the command out of the first step", () => {
    expect(firstCommand(whatsapp.sign_in!.steps!)).toBe("login qr");
    expect(firstCommand(["Open the provisioning page."])).toBeUndefined();
    expect(firstCommand([])).toBeUndefined();
  });
});

describe("nextStepsMessage", () => {
  it("is a message to paste: the bot, the numbered steps with the bot filled in, the note", () => {
    const text = nextStepsMessage(whatsapp, "WhatsApp", "@whatsappbot_carol:example.org");
    expect(text.split("\n")).toEqual([
      "Your WhatsApp bridge is ready. Its bot, @whatsappbot_carol:example.org, has invited you to a direct chat: accept it, then:",
      "1. Start a direct chat with @whatsappbot_carol:example.org and send `login qr`, or `login phone` to get a pairing code instead.",
      "2. Scan the code with WhatsApp on the phone.",
      "If you can't find the invite, start a direct chat with @whatsappbot_carol:example.org yourself.",
      "Note: WhatsApp unlinks the bridge if the phone stays offline for more than two weeks.",
    ]);
  });

  it("falls back to a generic step for a type without a guide", () => {
    const text = nextStepsMessage(undefined, "Something", undefined);
    expect(text).toContain("Its bot has invited you");
    expect(text).toContain("1. Send `login` to the bot");
    expect(text).not.toContain("Note:");
  });
});
