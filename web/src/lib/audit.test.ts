import { describe, expect, it } from "vitest";
import { describeAction, succeeded, targetRoute, targetTypeLabel } from "./audit";

describe("describeAction", () => {
  it("reads a generic verb and resource as a sentence", () => {
    expect(describeAction("users.suspend")).toBe("Suspended user");
    expect(describeAction("registration_tokens.create")).toBe("Created registration token");
    expect(describeAction("federation.destinations.reset")).toBe("Reset destination backoff");
  });

  it("uses the exact phrase where the generic reading would be wrong", () => {
    expect(describeAction("users.reset_password")).toBe("Reset password");
    expect(describeAction("setup.create")).toBe("Created the first administrator");
    expect(describeAction("rooms.make_admin")).toBe("Granted admin in room");
  });

  it("leaves an action it cannot read as it is, so a new operation is still legible", () => {
    expect(describeAction("widgets.frobnicate")).toBe("widgets.frobnicate");
    expect(describeAction("no-dot")).toBe("no-dot");
  });
});

describe("targetRoute", () => {
  it("links the resources the interface has pages for", () => {
    expect(targetRoute({ type: "user", id: "@a:example.org" })).toEqual({
      to: "/users/$userId",
      params: { userId: "@a:example.org" },
    });
    expect(targetRoute({ type: "config_section", id: "federation" })).toEqual({
      to: "/configuration/$section",
      params: { section: "federation" },
    });
  });

  it("has no link for a resource without a page, rather than a broken one", () => {
    expect(targetRoute({ type: "registration_token", id: "INVITE" })).toBeNull();
    expect(targetRoute({ type: "something_new", id: "x" })).toBeNull();
  });
});

describe("targetTypeLabel", () => {
  it("names known types and spells out unknown ones", () => {
    expect(targetTypeLabel("appservice")).toBe("Bridge");
    expect(targetTypeLabel("registration_token")).toBe("Registration token");
    expect(targetTypeLabel("policy_rule")).toBe("Policy rule");
  });
});

describe("succeeded", () => {
  it("counts 2xx and 3xx as success and everything from 400 as failure", () => {
    expect(succeeded({ outcome: { status: 200 } })).toBe(true);
    expect(succeeded({ outcome: { status: 100 } })).toBe(false);
    expect(succeeded({ outcome: { status: 303 } })).toBe(true);
    expect(succeeded({ outcome: { status: 400 } })).toBe(false);
    expect(succeeded({ outcome: { status: 503 } })).toBe(false);
  });

  it("treats a missing status as success, which is what the server writes for one", () => {
    expect(succeeded({ outcome: {} })).toBe(true);
  });
});
