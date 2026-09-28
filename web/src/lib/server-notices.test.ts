import { describe, expect, it } from "vitest";
import { parseRecipient, splitRecipients } from "./server-notices";

describe("parseRecipient", () => {
  it("takes a local user ID as it is", () => {
    expect(parseRecipient(" @alice:example.org ", "example.org")).toEqual({
      userId: "@alice:example.org",
    });
  });

  it("completes a bare username with this server's name", () => {
    expect(parseRecipient("Alice", "example.org")).toEqual({ userId: "@alice:example.org" });
    expect(parseRecipient("@bob", "example.org")).toEqual({ userId: "@bob:example.org" });
  });

  it("refuses a user on another server, and anything that is not a user ID", () => {
    expect(parseRecipient("@carol:elsewhere.net", "example.org")).toEqual({
      error: "@carol:elsewhere.net is not on this server; notices only go to users on example.org.",
    });
    expect(parseRecipient("#room:example.org", "example.org")).toHaveProperty("error");
    expect(parseRecipient("", "example.org")).toEqual({ error: "Type a user ID." });
  });

  it("checks only the shape when the server's name is not known yet", () => {
    expect(parseRecipient("@carol:elsewhere.net")).toEqual({ userId: "@carol:elsewhere.net" });
    expect(parseRecipient("carol")).toHaveProperty("error");
  });
});

describe("splitRecipients", () => {
  it("splits a pasted list on commas, semicolons and spaces", () => {
    expect(splitRecipients("@a:x, @b:x;@c:x  @d:x\n")).toEqual(["@a:x", "@b:x", "@c:x", "@d:x"]);
  });
});
