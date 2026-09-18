/**
 * Authentication against track 07's OAuth issuer (authorization code + PKCE,
 * admin scopes), per docs/design/information-architecture.md #2 and the
 * brief's day-one work ("authentication against a mocked issuer").
 *
 * Track 07 has not started (see its status file); there is no real issuer to
 * point `oauth4webapi` at yet. Until then this module is backed by the mock
 * issuer's `/oauth2/token` endpoint (src/mocks/handlers.ts), which issues a
 * bearer token carrying a scope set chosen at sign-in. The public surface
 * (`getAccessToken`, `hasScope`, `signIn`, `signOut`) is what the rest of the
 * app depends on, so swapping the mock for a real `oauth4webapi` PKCE flow
 * later does not touch call sites.
 */

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

export function signOut(): void {
  current = null;
  persist(null);
  notify();
}
