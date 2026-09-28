/**
 * Registering an account with an invite link: the browser's half of the public page at
 * `/admin/register?token=...` (`components/shell/Register.tsx`).
 *
 * Everything here is the ordinary Matrix client-server API on this origin -- the same calls any
 * Matrix client makes to register with a registration token -- and plain `fetch`, never the
 * admin `api` client: the person following an invite link has no session, and must not borrow
 * an administrator's if one happens to be signed in in the same browser.
 *
 * - `GET /_matrix/client/v1/register/m.login.registration_token/validity` says whether the token
 *   would admit a registration right now.
 * - `GET /_matrix/client/v3/register/available` says whether a username is free.
 * - `POST /_matrix/client/v3/register` registers, completing user-interactive authentication with
 *   the `m.login.registration_token` stage (and an `m.login.dummy` stage after it, when that is
 *   all the server still wants).
 */

/** The field a refusal belongs beside, when it is about one. */
export type RegistrationField = "username" | "password";

/** Why a registration did not happen, in the terms the page decides what to show by. */
export type RegistrationRefusal =
  /** The invite link's token is no longer accepted: expired, used up or deleted. */
  | "token-invalid"
  /** The server refused a field's value; `field` says which. */
  | "invalid"
  /** The server wants a step this page cannot do (an email address, a CAPTCHA...). */
  | "unsupported"
  /** The server could not be reached, or refused for another reason; the message says what. */
  | "failed";

/** A refusal from the registration endpoints, already worded for the person registering. */
export class RegistrationError extends Error {
  constructor(
    message: string,
    readonly refusal: RegistrationRefusal,
    readonly field: RegistrationField | null = null,
  ) {
    super(message);
  }
}

/** The copy for a token that no longer works, shared by the page and the errors below. */
export const TOKEN_INVALID_MESSAGE =
  "This invite link is no longer valid. It may have expired or been used up; ask whoever sent it for a new one.";

const TOKEN_STAGE = "m.login.registration_token";
const DUMMY_STAGE = "m.login.dummy";

interface MatrixError {
  errcode?: string;
  error?: string;
}

interface UiaResponse extends MatrixError {
  session?: string;
  flows?: { stages?: string[] }[];
  completed?: string[];
}

function url(path: string): string {
  return new URL(path, window.location.origin).toString();
}

async function readJson<T>(res: Response): Promise<T> {
  return (await res.json().catch(() => ({}))) as T;
}

/**
 * Whether `token` would admit a registration now. `null` when the server could not say (it is
 * unreachable, or does not have the endpoint): the page then lets the registration itself be
 * the judge rather than turning somebody away on a guess.
 */
export async function checkTokenValidity(token: string): Promise<boolean | null> {
  try {
    const res = await fetch(
      url(
        `/_matrix/client/v1/register/m.login.registration_token/validity?token=${encodeURIComponent(token)}`,
      ),
    );
    if (!res.ok) return null;
    const body = await readJson<{ valid?: unknown }>(res);
    return typeof body.valid === "boolean" ? body.valid : null;
  } catch {
    return null;
  }
}

/** The answer to "is this username free?". */
export type Availability =
  { kind: "available" } | { kind: "taken" | "invalid"; message: string } | { kind: "unknown" };

/** Asks whether `username` is free. Never throws: an unanswered question is `unknown`. */
export async function checkUsernameAvailable(username: string): Promise<Availability> {
  try {
    const res = await fetch(
      url(`/_matrix/client/v3/register/available?username=${encodeURIComponent(username)}`),
    );
    if (res.ok) return { kind: "available" };
    const body = await readJson<MatrixError>(res);
    if (body.errcode === "M_USER_IN_USE") {
      return { kind: "taken", message: "That username is taken. Try another." };
    }
    if (body.errcode === "M_INVALID_USERNAME") {
      return { kind: "invalid", message: body.error ?? INVALID_USERNAME_MESSAGE };
    }
    if (body.errcode === "M_EXCLUSIVE") {
      return { kind: "taken", message: "That username is reserved. Try another." };
    }
    return { kind: "unknown" };
  } catch {
    return { kind: "unknown" };
  }
}

const INVALID_USERNAME_MESSAGE = "Use lowercase letters, digits and . _ = - / only.";

/** What a successful registration made. */
export interface Registered {
  userId: string;
}

/**
 * The stages a server still wants, going by the first flow that contains every stage already
 * completed. `null` when no flow fits.
 */
export function remainingStages(uia: UiaResponse): string[] | null {
  const completed = uia.completed ?? [];
  for (const flow of uia.flows ?? []) {
    const stages = flow.stages ?? [];
    if (completed.every((stage) => stages.includes(stage))) {
      return stages.filter((stage) => !completed.includes(stage));
    }
  }
  return null;
}

/**
 * Registers `username` with `password`, spending `token`. Resolves with the new user ID, or
 * rejects with a {@link RegistrationError}.
 *
 * The first request carries the token stage. When the server answers 401 with a session -- it
 * wants the stages done inside one -- the page keeps going as far as it can: the token stage
 * again with the session if that is still wanted, then `m.login.dummy` when that is all that is
 * left. Any other stage is one this page cannot do, and says so.
 */
export async function registerWithToken(input: {
  username: string;
  password: string;
  token: string;
}): Promise<Registered> {
  const base = { username: input.username, password: input.password, inhibit_login: true };
  let auth: Record<string, string> = { type: TOKEN_STAGE, token: input.token };
  let sentTokenInSession = false;

  // At most three rounds: token, token in the session, dummy. A server asking for more than
  // that is asking for something this page does not do.
  for (let round = 0; round < 3; round += 1) {
    let res: Response;
    try {
      res = await fetch(url("/_matrix/client/v3/register"), {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ...base, auth }),
      });
    } catch {
      throw new RegistrationError("Couldn't reach the server. Try again in a moment.", "failed");
    }

    if (res.ok) {
      const body = await readJson<{ user_id?: string }>(res);
      return { userId: body.user_id ?? `@${input.username}` };
    }

    const body = await readJson<UiaResponse>(res);
    if (res.status !== 401) throw refusalFor(res.status, body);

    // A 401 with an error is a stage that failed; with only flows, the server wants more.
    if (body.errcode === "M_FORBIDDEN" || body.errcode === "M_UNAUTHORIZED") {
      throw new RegistrationError(TOKEN_INVALID_MESSAGE, "token-invalid");
    }
    const remaining = remainingStages(body);
    if (!body.session || !remaining) {
      throw new RegistrationError(
        body.error ?? "The server would not register this account.",
        "failed",
      );
    }
    if (remaining.length === 1 && remaining[0] === DUMMY_STAGE) {
      auth = { type: DUMMY_STAGE, session: body.session };
    } else if (remaining[0] === TOKEN_STAGE && !sentTokenInSession) {
      auth = { type: TOKEN_STAGE, token: input.token, session: body.session };
      sentTokenInSession = true;
    } else {
      throw unsupported();
    }
  }
  throw unsupported();
}

function unsupported(): RegistrationError {
  return new RegistrationError(
    "This server asks for more than an invite link to register (an email address, for example). Register from a Matrix client instead.",
    "unsupported",
  );
}

function refusalFor(status: number, body: MatrixError): RegistrationError {
  switch (body.errcode) {
    case "M_USER_IN_USE":
      return new RegistrationError("That username is taken. Try another.", "invalid", "username");
    case "M_EXCLUSIVE":
      return new RegistrationError(
        "That username is reserved. Try another.",
        "invalid",
        "username",
      );
    case "M_INVALID_USERNAME":
      return new RegistrationError(body.error ?? INVALID_USERNAME_MESSAGE, "invalid", "username");
    case "M_WEAK_PASSWORD":
      return new RegistrationError(
        body.error ?? "Choose a stronger password.",
        "invalid",
        "password",
      );
    case "M_FORBIDDEN":
    case "M_UNAUTHORIZED":
      return new RegistrationError(TOKEN_INVALID_MESSAGE, "token-invalid");
    case "M_LIMIT_EXCEEDED":
      return new RegistrationError("Too many attempts. Wait a minute and try again.", "failed");
    default:
      return new RegistrationError(
        body.error ?? `The server refused the registration (HTTP ${status}).`,
        "failed",
      );
  }
}
