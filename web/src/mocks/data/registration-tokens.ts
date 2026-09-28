import type { RegistrationToken } from "@/api/registration-tokens";

/**
 * The mock's registration tokens: one of each state the tokens table explains (valid with and
 * without a limit, expired, used up, and one whose last use is still being registered).
 * Mutable module state -- create, update, delete and a registration change it -- restored
 * after every Vitest test by {@link resetRegistrationTokens} (`src/test/setup.ts`).
 */
const DAY = 24 * 3_600_000;

function seed(now = Date.now()): RegistrationToken[] {
  const iso = (offsetMs: number) => new Date(now + offsetMs).toISOString();
  return [
    {
      token: "welcome-team",
      valid: true,
      uses_allowed: null,
      pending: 0,
      completed: 4,
      expires_at: null,
      created_at: iso(-20 * DAY),
    },
    {
      token: "carol-invite",
      valid: true,
      uses_allowed: 1,
      pending: 0,
      completed: 0,
      expires_at: iso(6 * DAY),
      created_at: iso(-DAY),
    },
    {
      token: "spring-cohort",
      valid: false,
      uses_allowed: 10,
      pending: 0,
      completed: 3,
      expires_at: iso(-2 * DAY),
      created_at: iso(-40 * DAY),
    },
    {
      token: "dave-invite",
      valid: false,
      uses_allowed: 1,
      pending: 0,
      completed: 1,
      expires_at: null,
      created_at: iso(-5 * DAY),
    },
    {
      token: "erin-invite",
      valid: false,
      uses_allowed: 1,
      pending: 1,
      completed: 0,
      expires_at: iso(2 * DAY),
      created_at: iso(-3_600_000),
    },
  ];
}

export const registrationTokens: RegistrationToken[] = seed();

/** Puts the fixtures back as they started. */
export function resetRegistrationTokens(): void {
  registrationTokens.splice(0, registrationTokens.length, ...seed());
}

export function findRegistrationToken(token: string): RegistrationToken | undefined {
  return registrationTokens.find((t) => t.token === token);
}

/** Recomputes `valid` from the other fields, as the server does on every read. */
export function refreshValidity(t: RegistrationToken, now = Date.now()): RegistrationToken {
  const expired = t.expires_at != null && Date.parse(t.expires_at) <= now;
  const exhausted =
    t.uses_allowed != null && (t.pending ?? 0) + (t.completed ?? 0) >= t.uses_allowed;
  t.valid = !expired && !exhausted;
  return t;
}

const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/** A generated token of `length` characters, as the server makes one. */
export function generateMockToken(length: number): string {
  const bytes = crypto.getRandomValues(new Uint8Array(length));
  return Array.from(bytes, (b) => ALPHABET[b % ALPHABET.length]).join("");
}
