import { describe, expect, it } from "vitest";
import type { BridgeType } from "@/api/bridges";
import {
  bridgeKind,
  bridgeTitle,
  botMatrixId,
  groupByCategory,
  healthCounts,
  signInSteps,
  sortByAttention,
} from "./bridge-catalogue";

function type(id: string, name: string, category: string, description = ""): BridgeType {
  return { id, name, category, description, upstream_project: id } as BridgeType;
}

describe("groupByCategory", () => {
  const catalogue = [
    type("mautrix-whatsapp", "WhatsApp", "messaging", "Chats linked to your phone."),
    type("mautrix-discord", "Discord", "social"),
    type("heisenbridge", "IRC (heisenbridge)", "irc"),
    type("matrix-hookshot", "Hookshot", "integrations", "Webhooks and feeds."),
    type("something", "Something", "unheard-of"),
  ];

  it("groups in the catalogue's order and puts an unknown category last", () => {
    const groups = groupByCategory(catalogue);
    expect(groups.map((g) => g.category)).toEqual([
      "messaging",
      "social",
      "irc",
      "integrations",
      "other",
    ]);
    expect(groups[0].label).toBe("Messaging");
    expect(groups[4].types[0].id).toBe("something");
  });

  it("filters by name, description or id and drops empty groups", () => {
    expect(groupByCategory(catalogue, "phone").map((g) => g.types.map((t) => t.id))).toEqual([
      ["mautrix-whatsapp"],
    ]);
    expect(groupByCategory(catalogue, "HOOK").map((g) => g.category)).toEqual(["integrations"]);
    expect(groupByCategory(catalogue, "nothing here")).toEqual([]);
  });
});

describe("bridgeTitle and bridgeKind", () => {
  const whatsapp = type("mautrix-whatsapp", "WhatsApp", "messaging");

  it("uses the catalogue's name when the operator kept the default id", () => {
    expect(bridgeTitle({ id: "whatsapp", bridge_type: "mautrix-whatsapp" }, whatsapp)).toBe(
      "WhatsApp",
    );
    expect(bridgeTitle({ id: "mautrix-whatsapp", bridge_type: "mautrix-whatsapp" }, whatsapp)).toBe(
      "WhatsApp",
    );
  });

  it("humanises a custom id and keeps the kind beside it", () => {
    expect(bridgeTitle({ id: "work-whatsapp", bridge_type: "mautrix-whatsapp" }, whatsapp)).toBe(
      "Work Whatsapp",
    );
    expect(bridgeKind({ protocols: [], bridge_type: "mautrix-whatsapp" }, whatsapp)).toBe(
      "WhatsApp",
    );
  });

  it("falls back to the registration for an appservice that did not come through the catalogue", () => {
    expect(bridgeTitle({ id: "heisenbridge", bridge_type: null }, undefined)).toBe("Heisenbridge");
    expect(bridgeKind({ protocols: ["irc"], bridge_type: null }, undefined)).toBe("irc");
    expect(bridgeKind({ protocols: [], bridge_type: null }, undefined)).toBe("Custom appservice");
  });
});

describe("sign-in guidance", () => {
  it("names the bot in every step", () => {
    const signal = {
      sign_in: { steps: ["Start a chat with {bot} and send `login`.", "Scan the code."] },
    };
    expect(signInSteps(signal, "@signalbot:example.org")).toEqual([
      "Start a chat with @signalbot:example.org and send `login`.",
      "Scan the code.",
    ]);
    expect(signInSteps(undefined, "@x:y")).toEqual([]);
  });

  it("writes the bot's Matrix ID, or just its localpart until the server's name is known", () => {
    expect(botMatrixId("signalbot", "example.org")).toBe("@signalbot:example.org");
    expect(botMatrixId("signalbot", undefined)).toBe("@signalbot");
  });
});

describe("the list's order and its summary", () => {
  const bridges = [
    { id: "a", health: "healthy" as const, paused: false },
    { id: "b", health: "down" as const, paused: false },
    { id: "c", health: "healthy" as const, paused: true },
    { id: "d", health: "unknown" as const, paused: false },
    { id: "e", health: "degraded" as const, paused: false },
  ];

  it("reads what is broken first and what was set aside last", () => {
    expect(sortByAttention(bridges).map((b) => b.id)).toEqual(["b", "e", "d", "a", "c"]);
  });

  it("counts a paused bridge as paused whatever its stale health says", () => {
    expect(healthCounts(bridges)).toEqual({
      healthy: 1,
      degraded: 1,
      down: 1,
      paused: 1,
      unknown: 1,
    });
  });
});

describe("the wizard's derived addresses", async () => {
  const { applyPatch, defaultsForKind, initialWizardState } =
    await import("@/pages/bridges/wizard/wizard-state");

  it("follow the kind, the id and the deployment until the operator types into them", () => {
    let state = applyPatch(
      initialWizardState,
      defaultsForKind("mautrix-whatsapp", { name: "WhatsApp", port: 29318 }),
    );
    expect(state.bridgeAddress).toBe("http://whatsapp:29318");
    expect(state.homeserverAddress).toBe("http://myelin:8008");

    state = applyPatch(state, { id: "wa" });
    expect(state.bridgeAddress).toBe("http://wa:29318");

    state = applyPatch(state, { deployment: "kubernetes", namespace: "chat" });
    expect(state.bridgeAddress).toBe("http://wa.chat.svc:29318");
    expect(state.homeserverAddress).toBe("http://myelin.chat.svc:8008");

    state = applyPatch(state, { bridgeAddress: "http://127.0.0.1:29318" });
    state = applyPatch(state, { id: "work-wa", deployment: "self-managed" });
    expect(state.bridgeAddress).toBe("http://127.0.0.1:29318");
    expect(state.homeserverAddress).toBe("http://myelin:8008");
  });
});
