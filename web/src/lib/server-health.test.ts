import { describe, expect, it } from "vitest";
import { checkLabel, checkMeaning, checkStatus, checkWord, healthSummary } from "./server-health";

describe("server health, in words", () => {
  it("names the server's checks and humanises an unknown one", () => {
    expect(checkLabel("audit")).toBe("Audit log");
    expect(checkLabel("users")).toBe("User directory");
    expect(checkLabel("search_index")).toBe("Search index");
    expect(checkLabel("push.gateway")).toBe("Push gateway");
  });

  it("maps each state to a badge, a word and a meaning", () => {
    expect([checkStatus("ok"), checkWord("ok")]).toEqual(["success", "Ok"]);
    expect([checkStatus("unknown"), checkWord("unknown")]).toEqual(["warning", "Unknown"]);
    expect([checkStatus("down"), checkWord("down")]).toEqual(["danger", "Down"]);
    expect(checkStatus("degraded")).toBe("warning");
    expect(checkStatus("odd")).toBe("muted");
    expect(checkMeaning("unknown")).toMatch(/not wired up here/);
    expect(checkMeaning("down")).toMatch(/Not answering/);
  });

  it("summarises the answer naming what is not ok", () => {
    expect(healthSummary("ok", { audit: "ok", events: "ok", users: "ok" })).toBe(
      "Every probe answered: audit log, event stream, user directory.",
    );
    expect(healthSummary("degraded", { audit: "ok", users: "unknown" })).toBe(
      "Server health is degraded: user directory unknown.",
    );
    expect(healthSummary("down", { audit: "down", users: "unknown" })).toBe(
      "Server health is down: audit log down, user directory unknown.",
    );
    expect(healthSummary("ok", {})).toBe("Every probe answered.");
  });
});
