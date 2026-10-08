import { describe, expect, it } from "vitest";
import {
  describeBucket,
  rateLimitContext,
  serverLimitOn,
  type ServerRateLimits,
} from "./rate-limits";

const SERVER: ServerRateLimits = {
  enabled: true,
  message: { per_second: 0.2, burst_count: 10 },
  admin_redaction: { per_second: 1, burst_count: 50 },
};

describe("rate-limit context", () => {
  it("words a bucket", () => {
    expect(describeBucket({ per_second: 0.2, burst_count: 10 })).toBe(
      "0.2 messages a second, bursts of 10",
    );
    expect(describeBucket({ per_second: 1, burst_count: 50 }, "redaction")).toBe(
      "1 redaction a second, bursts of 50",
    );
  });

  it("gives the server-wide limit and what clearing an override does", () => {
    expect(rateLimitContext(SERVER, false, false)).toEqual({
      serverWide: "Server-wide limit: 0.2 messages a second, bursts of 10.",
      override: null,
      redactions: null,
    });
    expect(rateLimitContext(SERVER, true, false).override).toBe(
      "This override replaces it for them; clearing the override puts them back on it.",
    );
  });

  it("treats a switched-off server limit, or one at 0 a second, as no limit", () => {
    const off = { ...SERVER, enabled: false };
    expect(serverLimitOn(off)).toBe(false);
    expect(rateLimitContext(off, true, false).override).toBe(
      "This override still applies to them; clearing it leaves them unlimited.",
    );
    const zero = { ...SERVER, message: { per_second: 0, burst_count: 10 } };
    expect(serverLimitOn(zero)).toBe(false);
    expect(rateLimitContext(zero, false, false).serverWide).toMatch(/message limit is off/);
  });

  it("gives an administrator's redaction limit, which an override replaces", () => {
    expect(rateLimitContext(SERVER, false, true).redactions).toBe(
      "As a server administrator, their redactions have the administrator redaction limit instead: 1 redaction a second, bursts of 50.",
    );
    expect(rateLimitContext(SERVER, true, true).redactions).toMatch(
      /the override covers their redactions too\.$/,
    );
    expect(rateLimitContext({ ...SERVER, enabled: false }, false, true).redactions).toBe(
      "As a server administrator, their redactions have no limit (the administrator redaction limit is off).",
    );
  });

  it("says when the server-wide limits could not be read", () => {
    expect(rateLimitContext(undefined, true, true)).toEqual({
      serverWide: "The server-wide limit could not be read here.",
      override: "This override replaces it; clearing the override puts them back on it.",
      redactions: null,
    });
  });
});
