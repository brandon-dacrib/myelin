import { describe, expect, it } from "vitest";
import type { ConfigSettingInfo } from "@/api/config-schema";
import {
  appliesHeadline,
  appliesOf,
  appliesOfInfo,
  countApplies,
  describeSaveOutcome,
  joinLabels,
} from "./config-applies";

function info(applies: ConfigSettingInfo["applies"], extra: Partial<ConfigSettingInfo> = {}) {
  return {
    origin: "default",
    secret: false,
    reloadable: applies === "hot",
    bootstrap: applies === "bootstrap",
    editable: applies !== "bootstrap",
    applies,
    ...extra,
  } satisfies ConfigSettingInfo;
}

const settings: Record<string, ConfigSettingInfo> = {
  "rate_limits.login.per_second": info("hot"),
  "rate_limits.login.burst_count": info("hot"),
  "auth.oidc_providers": info("restart"),
  "server.server_name": info("bootstrap"),
  "media.mixed.a": info("hot"),
  "media.mixed.b": info("restart"),
};

describe("appliesOfInfo", () => {
  it("reads the server's own answer", () => {
    expect(appliesOfInfo(info("restart"))).toBe("restart");
  });
  it("derives it from the older flags when a server does not send it", () => {
    expect(appliesOfInfo({ reloadable: true, bootstrap: false })).toBe("hot");
    expect(appliesOfInfo({ reloadable: false, bootstrap: true })).toBe("bootstrap");
    expect(appliesOfInfo({ reloadable: false, bootstrap: false })).toBe("restart");
  });
});

describe("appliesOf", () => {
  it("uses the setting's own row", () => {
    expect(appliesOf(settings, "server.server_name", "hot")).toBe("bootstrap");
  });
  it("uses the nearest row above a nested field", () => {
    expect(appliesOf(settings, "auth.oidc_providers.0.issuer", "hot")).toBe("restart");
  });
  it("uses the rows beneath a setting edited as one form when they agree", () => {
    expect(appliesOf(settings, "rate_limits.login", "restart")).toBe("hot");
  });
  it("says restart when the rows beneath disagree", () => {
    expect(appliesOf(settings, "media.mixed", "hot")).toBe("restart");
  });
  it("falls back to the section's answer with no rows at all", () => {
    expect(appliesOf(undefined, "telemetry.metrics", "restart")).toBe("restart");
    expect(appliesOf({}, "rate_limits.enabled", "hot")).toBe("hot");
  });
});

describe("countApplies and appliesHeadline", () => {
  it("counts each class and says what most of a section does", () => {
    const counts = countApplies(
      settings,
      ["rate_limits.login.per_second", "auth.oidc_providers", "server.server_name"],
      "hot",
    );
    expect(counts).toEqual({ hot: 1, restart: 1, bootstrap: 1 });
    expect(appliesHeadline(counts)).toBe(
      "Most changes here apply on save; some wait for a restart",
    );
    expect(appliesHeadline({ hot: 3, restart: 0, bootstrap: 1 })).toBe(
      "Every change here applies on save",
    );
    expect(appliesHeadline({ hot: 0, restart: 2, bootstrap: 0 })).toBe(
      "Changes here take effect at the next restart",
    );
    expect(appliesHeadline({ hot: 0, restart: 0, bootstrap: 2 })).toBe(
      "Every setting here is set per replica",
    );
  });
});

describe("describeSaveOutcome", () => {
  it("names what applied and what waits", () => {
    const outcome = describeSaveOutcome(
      [
        { label: "Login · Burst count", applies: "hot" },
        { label: "Login · Per second", applies: "hot" },
        { label: "Client timeout", applies: "restart" },
      ],
      { reloaded_sections: ["rate_limits"], requires_restart: [], errors: [] },
      "rate_limits",
    );
    expect(outcome.failed).toBe(false);
    expect(outcome.description).toBe(
      "Applied to the running server: Login · Burst count and Login · Per second. Stored, and waiting for the next restart: Client timeout.",
    );
  });
  it("says the running server kept the old values when it could not apply them", () => {
    const outcome = describeSaveOutcome(
      [{ label: "Log level", applies: "hot" }],
      {
        reloaded_sections: [],
        requires_restart: [],
        errors: [{ pointer: "/telemetry", detail: "RUST_LOG overrides it" }],
      },
      "telemetry",
    );
    expect(outcome.failed).toBe(true);
    expect(outcome.description).toBe(
      "Stored, but the running server could not apply Log level and keeps the old value: RUST_LOG overrides it",
    );
  });
});

describe("joinLabels", () => {
  it("joins as a sentence", () => {
    expect(joinLabels(["a"])).toBe("a");
    expect(joinLabels(["a", "b", "c"])).toBe("a, b and c");
  });
});
