import { describe, expect, it } from "vitest";
import { generatePassword } from "./password";

describe("generatePassword", () => {
  it("is the length asked for, from letters and digits nobody misreads", () => {
    const password = generatePassword();
    expect(password).toHaveLength(20);
    expect(password).toMatch(/^[a-km-zA-HJ-NP-Z2-9]+$/);
    expect(generatePassword(32)).toHaveLength(32);
  });

  it("does not repeat itself", () => {
    const seen = new Set(Array.from({ length: 200 }, () => generatePassword()));
    expect(seen.size).toBe(200);
  });

  it("uses the whole alphabet rather than favouring its start", () => {
    const counts = new Map<string, number>();
    for (const ch of Array.from({ length: 400 }, () => generatePassword()).join("")) {
      counts.set(ch, (counts.get(ch) ?? 0) + 1);
    }
    // 25 lowercase (no l), 24 uppercase (no I or O), 8 digits (no 0 or 1).
    expect(counts.size).toBe(57);
    // 8000 draws over 57 symbols is ~140 each; a modulo bias or a stuck generator is far
    // outside this, and honest randomness essentially never is.
    for (const n of counts.values()) {
      expect(n).toBeGreaterThan(80);
      expect(n).toBeLessThan(220);
    }
  });
});
