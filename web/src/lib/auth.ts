/**
 * Authentication.
 *
 * Track 07's native OAuth issuer (authorization code + PKCE, admin scopes,
 * docs/design/information-architecture.md #2) is still Phase 1/2 design-only
 * work (see its status file) — there is no authorization-code flow to point
 * `oauth4webapi` at yet. What track 07 *has* shipped, and what this module
 * uses for real-server sign-in, is `hs_auth::admin_verifier::AdminTokenVerifier`:
 * "there is no separate admin login" — a normal Matrix `syt_...` access
 * token belonging to a user with `is_admin: true` is, itself, the admin
 * credential. `signInWithToken`/`signInWithPassword` below are that: paste a
 * token, or sign in with a username and password (which is just
 * `POST /_matrix/client/v3/login`, same as any Matrix client), and this
 * module verifies it by calling `GET /api/v1/me` — the same call the rest of
 * the app would eventually get a 401/403 from anyway, so "is this token
 * good, and is it an admin's" is answered once, honestly, at sign-in instead
 * of on the first page that happens to need a scope.
 *
 * In mock mode (`VITE_HS_MOCK=1`, e2e and `npm run dev:mock`) `signIn` below
 * still talks to the mock issuer's `/oauth2/token` endpoint
 * (`src/mocks/handlers.ts`) so the test suite doesn't need a running server
 * or a real admin account. `MOCK_MODE` is what `SignIn.tsx` uses to choose
 * which form to show.
 */
export const MOCK_MODE = import.meta.env.VITE_HS_MOCK === "1";

/** Thrown by the real-mode sign-in functions with a message already fit to show the operator. */
export class AuthSignInError extends Error {}

const SESSION_KEY = "hs-admin:session";

/**
 * Matches the `securitySchemes.OAuth2` scope list in
 * `crates/hs-admin/openapi/openapi.yaml` exactly (reconciled 2026-09-18;
 * `docs/status/16-management-web-interface.md`). Earlier drafts of this
 * file guessed `moderation:*`; the real API splits it into `:read`/`:write`
 * like every other resource.
 */
export type Scope =
  | "admin:read"
  | "admin:write"
  | "bridges:read"
  | "bridges:write"
  | "moderation:read"
  | "moderation:write";

export const ALL_SCOPES: readonly Scope[] = [
  "admin:read",
  "admin:write",
  "bridges:read",
  "bridges:write",
  "moderation:read",
  "moderation:write",
];

export interface Session {
  accessToken: string;
  operator: { name: string; subject: string };
  scopes: Scope[];
}

let current: Session | null = load();

function load(): Session | null {
  try {
    const raw = sessionStorage.getItem(SESSION_KEY);
    return raw ? (JSON.parse(raw) as Session) : null;
  } catch {
    return null;
  }
}

function persist(session: Session | null) {
  try {
    if (session) sessionStorage.setItem(SESSION_KEY, JSON.stringify(session));
    else sessionStorage.removeItem(SESSION_KEY);
  } catch {
    /* storage unavailable: session still works for this tab via `current` */
  }
}

const listeners = new Set<() => void>();
export function subscribeSession(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}
function notify() {
  for (const fn of listeners) fn();
}

export function getSession(): Session | null {
  return current;
}

export function getAccessToken(): string | null {
  return current?.accessToken ?? null;
}

export function hasScope(scope: Scope): boolean {
  if (!current) return false;
  const granted = current.scopes;
  // admin:write "implies every other scope" (openapi.yaml's OAuth2 scheme
  // description); bridges:write/moderation:write each imply their own :read.
  if (granted.includes("admin:write")) return true;
  if (scope === "bridges:read" && granted.includes("bridges:write")) return true;
  if (scope === "moderation:read" && granted.includes("moderation:write")) return true;
  return granted.includes(scope);
}

/** Mock sign-in: exchanges a chosen scope set for a token from the mock issuer. */
export async function signIn(scopes: Scope[] = [...ALL_SCOPES]): Promise<Session> {
  const res = await fetch("/oauth2/token", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ grant_type: "mock", scopes }),
  });
  if (!res.ok) throw new Error(`Mock sign-in failed: ${res.status}`);
  const body = (await res.json()) as {
    access_token: string;
    operator: { name: string; subject: string };
    scopes: Scope[];
  };
  current = {
    accessToken: body.access_token,
    operator: body.operator,
    scopes: body.scopes,
  };
  persist(current);
  notify();
  return current;
}

/**
 * Verifies `accessToken` against the real `/api/v1/me` (deliberately a plain `fetch`, not the
 * shared `api` client in `api/client.ts`: that client auto-attaches the *current* session's
 * token via `getAccessToken()`, which is exactly wrong here — we're verifying a *candidate*
 * token before there is a current session). On success, `Principal.scopes` becomes the session's
 * scopes: `AdminTokenVerifier` grants a legacy admin token both `admin:read` and `admin:write`
 * unconditionally (`hs_auth::admin_verifier` module doc), so a successful call always means full
 * access today, but reading real scopes back rather than assuming them is what makes this
 * forward-compatible with the native OAuth issuer's finer-grained tokens later.
 */
export async function signInWithToken(accessToken: string): Promise<Session> {
  const token = accessToken.trim();
  if (!token) throw new AuthSignInError("Enter an access token.");

  let res: Response;
  try {
    res = await fetch("/api/v1/me", { headers: { Authorization: `Bearer ${token}` } });
  } catch {
    throw new AuthSignInError(
      "Couldn't reach the server. Check that it's running and this page can reach /api/v1.",
    );
  }

  if (res.status === 401) {
    throw new AuthSignInError("That token wasn't recognized, or has expired.");
  }
  if (res.status === 403) {
    throw new AuthSignInError(
      "That token is valid but isn't a server administrator's (needs is_admin set).",
    );
  }
  if (res.status === 503) {
    throw new AuthSignInError("The server isn't ready to verify tokens yet (503). Try again shortly.");
  }
  if (!res.ok) {
    throw new AuthSignInError(`Sign-in failed (HTTP ${res.status}).`);
  }

  const principal = (await res.json()) as {
    id: string;
    display_name?: string;
    scopes: string[];
  };

  current = {
    accessToken: token,
    operator: { name: principal.display_name ?? principal.id, subject: principal.id },
    scopes: principal.scopes as Scope[],
  };
  persist(current);
  notify();
  return current;
}

/**
 * Signs in with a username and password through the ordinary Matrix client-server login
 * (`POST /_matrix/client/v3/login`, `m.login.password`) — the same call any Matrix client makes,
 * since "there is no separate admin login" (this track's brief). The resulting access token is
 * then verified the same way `signInWithToken` verifies a pasted one; a login that succeeds for a
 * non-admin user still fails sign-in here with the same 403 message.
 */
export async function signInWithPassword(username: string, password: string): Promise<Session> {
  if (!username.trim() || !password) {
    throw new AuthSignInError("Enter a username and password.");
  }

  let res: Response;
  try {
    res = await fetch("/_matrix/client/v3/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        type: "m.login.password",
        identifier: { type: "m.id.user", user: username.trim() },
        password,
      }),
    });
  } catch {
    throw new AuthSignInError(
      "Couldn't reach the server. Check that it's running and this page can reach /_matrix.",
    );
  }

  if (!res.ok) {
    const body = (await res.json().catch(() => ({}))) as { error?: string; errcode?: string };
    throw new AuthSignInError(
      body.error ?? `Sign-in failed (${body.errcode ?? `HTTP ${res.status}`}).`,
    );
  }

  const body = (await res.json()) as { access_token: string };
  return signInWithToken(body.access_token);
}

export function signOut(): void {
  current = null;
  persist(null);
  notify();
}
