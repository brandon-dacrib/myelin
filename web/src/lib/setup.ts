/**
 * First-run setup: creating the first administrator on a server that has none.
 *
 * While a server has no administrator it holds a one-time setup token and writes a link
 * containing it to its log at every start (`hs_auth::setup`). This module is the browser's half:
 * ask whether setup is on offer, read the token out of the link, and trade it plus a username
 * and password for a signed-in session.
 *
 * Plain `fetch`, not the shared `api` client, for the same reason `signInWithToken` uses it:
 * that client attaches the current session's token, and the whole point here is that there is
 * no session yet.
 */
import { signInWithToken, type Session } from "./auth";

/**
 * The setup token from a setup link's fragment (`/admin/setup#token=...`), or `null`.
 *
 * It travels in the fragment because browsers do not send one to the server, so the token never
 * reaches an access log or a `Referer` header on its way here.
 */
export function setupTokenFromHash(hash: string): string | null {
  const params = new URLSearchParams(hash.startsWith("#") ? hash.slice(1) : hash);
  const token = params.get("token")?.trim();
  return token ? token : null;
}

/**
 * Whether this server is offering first-run setup. `null` means it could not be asked, which is
 * not the same as "no": a caller should neither promise a setup page nor rule one out.
 */
export async function fetchNeedsSetup(): Promise<boolean | null> {
  try {
    const res = await fetch("/api/v1/setup", { cache: "no-store" });
    if (!res.ok) return null;
    const body = (await res.json()) as { needs_setup?: unknown };
    return typeof body.needs_setup === "boolean" ? body.needs_setup : null;
  } catch {
    return null;
  }
}

export type SetupField = "setupToken" | "username" | "password";

/** A refusal from `createFirstAdministrator`, already worded for the operator. */
export class SetupError extends Error {
  constructor(
    message: string,
    /** The field the message belongs beside, when it is about one. */
    readonly field: SetupField | null = null,
    /** True when the server already has an administrator, so the remedy is to sign in. */
    readonly alreadySetUp = false,
  ) {
    super(message);
  }
}

const FIELD_FOR_POINTER: Record<string, SetupField> = {
  "/setup_token": "setupToken",
  "/username": "username",
  "/password": "password",
};

interface ProblemBody {
  detail?: string;
  errors?: { pointer?: string; detail?: string }[];
}

/**
 * Creates the first administrator and signs in as them.
 *
 * The server answers with a session for the new account, which is then verified through
 * `signInWithToken` exactly as a pasted token would be -- so "setup worked" and "you are signed
 * in with admin access" are established by the same call the rest of the app relies on.
 */
export async function createFirstAdministrator(input: {
  setupToken: string;
  username: string;
  password: string;
}): Promise<Session> {
  let res: Response;
  try {
    res = await fetch("/api/v1/setup", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        setup_token: input.setupToken.trim(),
        username: input.username.trim(),
        password: input.password,
      }),
    });
  } catch {
    throw new SetupError(
      "Couldn't reach the server. Check that it's running and this page can reach /api/v1.",
    );
  }

  if (res.status === 201) {
    const body = (await res.json()) as { access_token: string };
    return signInWithToken(body.access_token);
  }

  const problem = (await res.json().catch(() => ({}))) as ProblemBody;
  if (res.status === 401) {
    throw new SetupError(
      "That isn't this server's setup token. Copy the setup link from the most recent start in the server's log.",
      "setupToken",
    );
  }
  if (res.status === 409) {
    throw new SetupError("This server already has an administrator.", null, true);
  }
  if (res.status === 400) {
    const first = problem.errors?.[0];
    const field = first?.pointer ? (FIELD_FOR_POINTER[first.pointer] ?? null) : null;
    throw new SetupError(
      first?.detail ?? problem.detail ?? "The server couldn't use that request.",
      field,
    );
  }
  throw new SetupError(problem.detail ?? `Setup failed (HTTP ${res.status}).`);
}
