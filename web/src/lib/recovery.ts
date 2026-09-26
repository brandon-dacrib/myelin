/**
 * Administrator recovery: resetting an administrator's password through a link from `hs recover`.
 *
 * When nobody can sign in as an administrator (the only one forgot their password), the operator
 * runs `hs recover` where the server keeps its signing key. It prints a one-time link to this
 * interface's recovery page, `/admin/recover#token=...`, that expires fifteen minutes after issue.
 * This module is the browser's half: read the token out of the link, ask what the link can do
 * (which administrator accounts it may reset, and until when), and trade it plus a new password
 * for a signed-in session as the recovered account. It is the sibling of `lib/setup.ts`, which
 * does the same for a server that has no administrator yet.
 *
 * Plain `fetch`, not the shared `api` client, for the same reason `createFirstAdministrator`
 * uses it: that client attaches the current session's token, and the whole point here is that
 * there is no session.
 */
import type { components } from "@/api/schema";
import { signInWithToken, type Session } from "./auth";
import { setupTokenFromHash } from "./setup";

/** An administrator account a recovery link may reset, as the server lists it. */
export type RecoveryAdministrator = components["schemas"]["RecoveryAdministrator"];

/** What an open recovery link can do. */
export interface RecoveryInspection {
  /** The active administrator accounts, any of whose password this link may reset. */
  administrators: RecoveryAdministrator[];
  /** When the link stops working, milliseconds since the Unix epoch on the server's clock. */
  expiresAtMs: number;
}

/**
 * The recovery token from a recovery link's fragment (`/admin/recover#token=...`), or `null`.
 *
 * The same layout as the setup link, for the same reason: a fragment is never sent to the
 * server, so the token reaches no access log or `Referer` header on its way here. It is never
 * read from the query string.
 */
export function recoveryTokenFromHash(hash: string): string | null {
  return setupTokenFromHash(hash);
}

/** The field a refusal belongs beside, when it is about one. */
export type RecoveryField = "userId" | "password";

/** Why the server refused, in the terms the page decides what to show by. */
export type RecoveryRefusal =
  /** The token is not one this server issued (401): the operator has the wrong link. */
  | "wrong-link"
  /** No recovery link is open (409): none was issued, or the one that was is used or expired. */
  | "no-link-open"
  /** The server refused a field's value (400); `field` says which, `message` says why. */
  | "invalid"
  /** The server could not be reached, or answered something the page has no better word for. */
  | "failed";

/** A refusal from the recovery endpoints, already worded for the operator. */
export class RecoveryError extends Error {
  constructor(
    message: string,
    readonly refusal: RecoveryRefusal,
    readonly field: RecoveryField | null = null,
  ) {
    super(message);
  }
}

const FIELD_FOR_POINTER: Record<string, RecoveryField> = {
  "/user_id": "userId",
  "/password": "password",
};

interface ProblemBody {
  detail?: string;
  errors?: { pointer?: string; detail?: string }[];
}

/** The 401 copy: the token is not this server's. */
export const WRONG_LINK_MESSAGE =
  "This is not this server's recovery link. Check the link you were given.";

/** The 409 copy: nothing is open to use. Backticks mark the command, for `withInlineCode`. */
export const NO_LINK_OPEN_MESSAGE =
  "No recovery link is open. Run `hs recover` where the server keeps its signing key to get a fresh one.";

async function post(path: string, body: unknown): Promise<Response> {
  try {
    return await fetch(path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch {
    throw new RecoveryError(
      "Couldn't reach the server. Check that it's running and this page can reach /api/v1.",
      "failed",
    );
  }
}

/** Both endpoints refuse in the same terms, so one reading of a refusal serves both. */
async function refusal(res: Response, what: string): Promise<RecoveryError> {
  const problem = (await res.json().catch(() => ({}))) as ProblemBody;
  if (res.status === 401) return new RecoveryError(WRONG_LINK_MESSAGE, "wrong-link");
  if (res.status === 409) return new RecoveryError(NO_LINK_OPEN_MESSAGE, "no-link-open");
  if (res.status === 400) {
    const first = problem.errors?.[0];
    const field = first?.pointer ? (FIELD_FOR_POINTER[first.pointer] ?? null) : null;
    return new RecoveryError(
      first?.detail ?? problem.detail ?? "The server couldn't use that request.",
      "invalid",
      field,
    );
  }
  return new RecoveryError(problem.detail ?? `${what} failed (HTTP ${res.status}).`, "failed");
}

/**
 * Asks what a recovery link can do. This is the page's first call: with the right token the
 * answer is the administrators the link may reset and when it expires; with the wrong one, or
 * once the link is used or expired, it is a {@link RecoveryError} saying only that.
 */
export async function inspectRecoveryLink(recoveryToken: string): Promise<RecoveryInspection> {
  const res = await post("/api/v1/recovery/inspect", { recovery_token: recoveryToken.trim() });
  if (res.status === 200) {
    const body = (await res.json()) as components["schemas"]["RecoveryInspection"];
    return { administrators: body.administrators, expiresAtMs: body.expires_at_ms };
  }
  throw await refusal(res, "Recovery");
}

/**
 * Resets an administrator's password through the link and signs in as them.
 *
 * Consumes the link. Every session of the account is signed out by the server, and the session
 * it answers with is verified through `signInWithToken` exactly as the setup page's is, so
 * "the reset worked" and "you are signed in with admin access" are established by the same call
 * the rest of the app relies on.
 */
export async function resetAdministratorPassword(input: {
  recoveryToken: string;
  userId: string;
  password: string;
}): Promise<Session> {
  const res = await post("/api/v1/recovery/reset", {
    recovery_token: input.recoveryToken.trim(),
    user_id: input.userId.trim(),
    password: input.password,
  });
  if (res.status === 200) {
    const body = (await res.json()) as components["schemas"]["SetupSession"];
    return signInWithToken(body.access_token);
  }
  throw await refusal(res, "The reset");
}

/**
 * How long a link has left, for the sentence "This link works once and expires …": "in 14
 * minutes", "in a minute", "in less than a minute". Whole minutes, rounded down, so the page
 * never promises more time than there is. `null` once the moment has passed. `expiresAtMs` is
 * on the server's clock and `nowMs` on the browser's; they are close enough for a sentence.
 */
export function recoveryTimeLeft(expiresAtMs: number, nowMs = Date.now()): string | null {
  const left = expiresAtMs - nowMs;
  if (left <= 0) return null;
  const minutes = Math.floor(left / 60_000);
  if (minutes >= 2) return `in ${minutes} minutes`;
  if (minutes === 1) return "in a minute";
  return "in less than a minute";
}
