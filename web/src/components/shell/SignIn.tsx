import { useState } from "react";
import { ALL_SCOPES, signIn, type Scope } from "@/lib/auth";
import { Button } from "../ui/button/Button";

/**
 * Stands in for track 07's OAuth issuer redirect (docs/decisions/0003-web-stack.md).
 * Two presets let a reviewer see the permissions model (information-architecture.md #8)
 * without hand-editing scopes.
 */
export function SignIn() {
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
    <div className="flex min-h-screen items-center justify-center bg-canvas px-4">
      <div className="w-full max-w-sm rounded-lg border border-border bg-surface p-8 shadow-2">
        <h1 className="text-xl text-text">hs admin</h1>
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
          Track 07 has not published a real issuer yet; this mock stands in for it (see
          docs/decisions/0003-web-stack.md).
        </p>
      </div>
    </div>
  );
}
