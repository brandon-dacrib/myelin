import { useState, type FormEvent, type ReactNode } from "react";
import {
  ALL_SCOPES,
  AuthSignInError,
  MOCK_MODE,
  signIn,
  signInWithPassword,
  signInWithToken,
  type Scope,
} from "@/lib/auth";
import { Button } from "../ui/button/Button";
import { Input, Field } from "../ui/input/Input";

/**
 * In mock mode (`VITE_HS_MOCK=1`) this stands in for track 07's OAuth issuer redirect
 * (docs/decisions/0003-web-stack.md) with two one-click presets. Otherwise it's the real
 * sign-in this track's brief asks for: "a sign-in that takes a real Matrix access token
 * belonging to a server-admin user" — there is no separate admin login
 * (`hs_auth::admin_verifier::AdminTokenVerifier`'s module doc), so either pasting a token or
 * logging in with a username and password (which mints one via the ordinary client-server
 * `/login`) both work; `src/lib/auth.ts` verifies whichever token results against `GET
 * /api/v1/me` before calling it a session.
 */
export function SignIn() {
  return MOCK_MODE ? <MockSignIn /> : <RealSignIn />;
}

function MockSignIn() {
  const [pending, setPending] = useState(false);

  async function handleSignIn(scopes: Scope[]) {
    setPending(true);
    try {
      await signIn(scopes);
    } finally {
      setPending(false);
    }
  }

  return (
    <SignInShell>
      <p className="mt-2 text-sm text-text-muted">
        Sign in through your organisation&apos;s issuer to manage this server.
      </p>
      <div className="mt-6 flex flex-col gap-2">
        <Button disabled={pending} onClick={() => handleSignIn([...ALL_SCOPES])}>
          Sign in as operator
        </Button>
        <Button
          variant="secondary"
          disabled={pending}
          onClick={() => handleSignIn(["admin:read", "bridges:read"])}
        >
          Sign in read-only (demo)
        </Button>
      </div>
      <p className="mt-6 text-xs text-text-faint">
        Mock mode (<code className="font-identifier">VITE_HS_MOCK=1</code>): this stands in for
        track 07&apos;s issuer. See docs/decisions/0003-web-stack.md.
      </p>
    </SignInShell>
  );
}

type RealMode = "token" | "password";

function RealSignIn() {
  const [mode, setMode] = useState<RealMode>("password");
  const [token, setToken] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setPending(true);
    setError(null);
    try {
      if (mode === "token") {
        await signInWithToken(token);
      } else {
        await signInWithPassword(username, password);
      }
    } catch (err) {
      setError(err instanceof AuthSignInError ? err.message : "Sign-in failed.");
    } finally {
      setPending(false);
    }
  }

  return (
    <SignInShell>
      <p className="mt-2 text-sm text-text-muted">
        Sign in with a server administrator&apos;s Matrix account. There is no separate admin
        login — any account with <code className="font-identifier">is_admin</code> set works here.
      </p>

      <div
        role="tablist"
        aria-label="Sign-in method"
        className="mt-6 flex gap-1 rounded-md border border-border bg-surface-sunken p-1"
      >
        <button
          type="button"
          role="tab"
          aria-selected={mode === "password"}
          onClick={() => setMode("password")}
          className={`flex-1 rounded-sm px-3 py-1.5 text-sm font-medium ${
            mode === "password" ? "bg-surface text-text shadow-1" : "text-text-muted"
          }`}
        >
          Username &amp; password
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={mode === "token"}
          onClick={() => setMode("token")}
          className={`flex-1 rounded-sm px-3 py-1.5 text-sm font-medium ${
            mode === "token" ? "bg-surface text-text shadow-1" : "text-text-muted"
          }`}
        >
          Access token
        </button>
      </div>

      <form className="mt-4 flex flex-col gap-3" onSubmit={handleSubmit}>
        {mode === "password" ? (
          <>
            <Field label="Username">
              {(fieldProps) => (
                <Input
                  {...fieldProps}
                  autoComplete="username"
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                  placeholder="@alice:example.org or alice"
                />
              )}
            </Field>
            <Field label="Password">
              {(fieldProps) => (
                <Input
                  {...fieldProps}
                  type="password"
                  autoComplete="current-password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                />
              )}
            </Field>
          </>
        ) : (
          <Field label="Access token">
            {(fieldProps) => (
              <Input
                {...fieldProps}
                type="password"
                autoComplete="off"
                spellCheck={false}
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder="syt_..."
              />
            )}
          </Field>
        )}

        {error && (
          <p role="alert" className="text-sm text-danger">
            {error}
          </p>
        )}

        <Button type="submit" disabled={pending} className="mt-1">
          {pending ? "Signing in…" : "Sign in"}
        </Button>
      </form>
    </SignInShell>
  );
}

function SignInShell({ children }: { children: ReactNode }) {
  return (
    <div className="flex min-h-screen items-center justify-center bg-canvas px-4">
      <div className="w-full max-w-sm rounded-lg border border-border bg-surface p-8 shadow-2">
        <h1 className="text-xl text-text">hs admin</h1>
        {children}
      </div>
    </div>
  );
}
