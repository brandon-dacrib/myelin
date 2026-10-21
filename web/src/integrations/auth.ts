/**
 * The integrations page's sign-in: a person's own Matrix account, through the ordinary
 * `POST /_matrix/client/v3/login` (`m.login.password`), the same call any Matrix client makes.
 * No administrator flag is needed or checked: the user webhooks API answers to any user's
 * token. The session (token and user id) is kept for the tab, under its own key, so signing in
 * here never touches an admin session in the same browser and the other way round.
 */

/** Thrown by {@link signInWithPassword} with a message fit to show. */
export class IntegrationsSignInError extends Error {}

export interface UserSession {
  accessToken: string;
  userId: string;
}

const SESSION_KEY = "hs-integrations:session";

let current: UserSession | null = load();

function load(): UserSession | null {
  try {
    const raw = sessionStorage.getItem(SESSION_KEY);
    return raw ? (JSON.parse(raw) as UserSession) : null;
  } catch {
    return null;
  }
}

function persist(session: UserSession | null) {
  try {
    if (session) sessionStorage.setItem(SESSION_KEY, JSON.stringify(session));
    else sessionStorage.removeItem(SESSION_KEY);
  } catch {
    /* storage unavailable: the session still works for this tab via `current` */
  }
}

const listeners = new Set<() => void>();

export function subscribeUserSession(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

function notify() {
  for (const fn of listeners) fn();
}

export function getUserSession(): UserSession | null {
  return current;
}

/** What the sign-in says when the server has password sign-in turned off. */
export const PASSWORD_LOGIN_OFF =
  "This server has password sign-in turned off. Ask an administrator for another way to sign in.";

export async function signInWithPassword(username: string, password: string): Promise<UserSession> {
  if (!username.trim() || !password) {
    throw new IntegrationsSignInError("Enter your username and password.");
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
        initial_device_display_name: "Myelin integrations page",
      }),
    });
  } catch {
    throw new IntegrationsSignInError(
      "Couldn't reach the server. Check that it's running and this page can reach /_matrix.",
    );
  }
  if (!res.ok) {
    const body = (await res.json().catch(() => ({}))) as { error?: string; errcode?: string };
    if (res.status === 403 && /password login has been disabled/i.test(body.error ?? "")) {
      throw new IntegrationsSignInError(PASSWORD_LOGIN_OFF);
    }
    if (res.status === 403) {
      throw new IntegrationsSignInError("That username and password were not accepted.");
    }
    if (res.status === 429) {
      throw new IntegrationsSignInError("Too many attempts; wait a moment and try again.");
    }
    throw new IntegrationsSignInError(
      body.error ?? `Sign-in failed (${body.errcode ?? `HTTP ${res.status}`}).`,
    );
  }
  const body = (await res.json()) as { access_token: string; user_id: string };
  current = { accessToken: body.access_token, userId: body.user_id };
  persist(current);
  notify();
  return current;
}

/** Signs out: the token is revoked on the server (best effort) and forgotten here. */
export async function signOut(): Promise<void> {
  const token = current?.accessToken;
  current = null;
  persist(null);
  notify();
  if (token) {
    try {
      await fetch("/_matrix/client/v3/logout", {
        method: "POST",
        headers: { Authorization: `Bearer ${token}` },
      });
    } catch {
      /* the token is forgotten here either way */
    }
  }
}

/** Forgets the session without a logout call: for a token the server no longer recognizes. */
export function forgetSession(): void {
  current = null;
  persist(null);
  notify();
}
