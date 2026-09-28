/**
 * Registration tokens, the parts that are not a request: what a token's state means to an
 * administrator, the invite link that carries it, and when a chosen expiry falls.
 *
 * A registration token lets somebody create an account while open registration is off. The
 * interface hands it out as an invite link to this interface's public registration page
 * (`/admin/register?token=...`, `components/shell/Register.tsx`), which spends it through the
 * ordinary Matrix client-server registration.
 */

/** A registration token as the interface uses it: every field the server sends, filled in. */
export interface RegistrationTokenView {
  token: string;
  valid: boolean;
  usesAllowed: number | null;
  pending: number;
  completed: number;
  expiresAt: string | null;
  createdAt: string | null;
}

/** The characters a token may contain, and how long it may be (`RegistrationTokenCreate.token`). */
export const TOKEN_PATTERN = /^[A-Za-z0-9._~-]+$/;
export const TOKEN_MAX_LENGTH = 64;

/**
 * Why a custom token cannot be used, or `null` when it can. Checked before asking the server,
 * which refuses the same things with a 400.
 */
export function customTokenProblem(token: string): string | null {
  if (!token) return "Type the token, or let the server generate one.";
  if (token.length > TOKEN_MAX_LENGTH) return `At most ${TOKEN_MAX_LENGTH} characters.`;
  if (!TOKEN_PATTERN.test(token)) {
    return "Only letters, digits and . _ ~ - can be used.";
  }
  return null;
}

/** Parses the uses field: a whole number of at least 0, or an error to show beside it. */
export function parseUses(uses: string): { value: number } | { error: string } {
  const trimmed = uses.trim();
  if (!/^\d+$/.test(trimmed)) return { error: "A whole number, or turn on Unlimited." };
  return { value: Number(trimmed) };
}

/** What a token's state is, in the words the tokens table uses. */
export type TokenStatusKind = "valid" | "expired" | "used-up" | "reserved" | "invalid";

export interface TokenStatus {
  kind: TokenStatusKind;
  label: string;
  /** Why, when the label alone does not say it. */
  detail?: string;
}

/**
 * Says whether a token admits a new registration and, when it does not, why.
 *
 * The server's `valid` is the authority; the other fields only explain it. A token the server
 * calls valid is "Valid" even if this browser's clock thinks it has expired, and one it calls
 * invalid is never shown as usable.
 */
export function tokenStatus(t: RegistrationTokenView, nowMs = Date.now()): TokenStatus {
  if (t.valid) return { kind: "valid", label: "Valid" };
  if (t.expiresAt && Date.parse(t.expiresAt) <= nowMs) {
    return { kind: "expired", label: "Expired" };
  }
  if (t.usesAllowed !== null && t.completed >= t.usesAllowed) {
    return { kind: "used-up", label: "Used up" };
  }
  if (t.usesAllowed !== null && t.pending + t.completed >= t.usesAllowed) {
    return {
      kind: "reserved",
      label: "Uses in progress",
      detail: `${t.pending} ${t.pending === 1 ? "registration is" : "registrations are"} still finishing with it.`,
    };
  }
  return { kind: "invalid", label: "Not valid" };
}

/** "3 of 5", or "3 of unlimited" when the token has no limit. */
export function formatUses(t: Pick<RegistrationTokenView, "completed" | "usesAllowed">): string {
  return `${t.completed} of ${t.usesAllowed === null ? "unlimited" : t.usesAllowed}`;
}

/**
 * The invite link for a token: this interface's public registration page, on the origin the
 * administrator is looking at it from (which is the homeserver's, since it serves `/admin/`).
 */
export function inviteLink(token: string, origin = window.location.origin): string {
  return `${origin}/admin/register?token=${encodeURIComponent(token)}`;
}

/** The quick expiry choices the create dialog offers, beside "Never" and a date and time. */
export const EXPIRY_PRESETS = [
  { id: "1d", label: "1 day", ms: 24 * 3_600_000 },
  { id: "7d", label: "7 days", ms: 7 * 24 * 3_600_000 },
  { id: "30d", label: "30 days", ms: 30 * 24 * 3_600_000 },
] as const;

export type ExpiryChoice = "never" | (typeof EXPIRY_PRESETS)[number]["id"] | "custom";

/**
 * The `expires_at` an expiry choice means, as RFC 3339, or `null` for never. `custom` is the
 * value of a `datetime-local` input (local time, no zone), which is `undefined` when it is empty
 * or not a date.
 */
export function expiryFor(
  choice: ExpiryChoice,
  custom: string,
  nowMs = Date.now(),
): string | null | undefined {
  if (choice === "never") return null;
  if (choice === "custom") {
    if (!custom) return undefined;
    const ms = new Date(custom).getTime();
    return Number.isFinite(ms) ? new Date(ms).toISOString() : undefined;
  }
  const preset = EXPIRY_PRESETS.find((p) => p.id === choice);
  return preset ? new Date(nowMs + preset.ms).toISOString() : null;
}

/** An RFC 3339 instant as a `datetime-local` input's value (local time, to the minute). */
export function toDateTimeLocal(iso: string): string {
  const d = new Date(iso);
  if (!Number.isFinite(d.getTime())) return "";
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/**
 * When a token expires, said relative to now: "in 6 days", "in 3 h", "2 days ago", "Never".
 * Up to a month either way (the longest preset is 30 days, and it should read "in 30 days" when
 * it is made); further out it is the date.
 */
export function formatExpiry(at: string | null, nowMs = Date.now()): string {
  if (!at) return "Never";
  const ms = Date.parse(at);
  if (!Number.isFinite(ms)) return at;
  const diff = ms - nowMs;
  const abs = Math.abs(diff);
  const month = 31 * 86_400_000;
  if (abs >= month) {
    return new Date(ms).toLocaleDateString(undefined, {
      year: "numeric",
      month: "short",
      day: "numeric",
    });
  }
  let amount: string;
  if (abs < 60_000) amount = "less than a minute";
  else if (abs < 3_600_000) amount = `${Math.round(abs / 60_000)} min`;
  else if (abs < 86_400_000) amount = `${Math.round(abs / 3_600_000)} h`;
  else {
    const days = Math.round(abs / 86_400_000);
    amount = `${days} ${days === 1 ? "day" : "days"}`;
  }
  return diff > 0 ? `in ${amount}` : `${amount} ago`;
}
