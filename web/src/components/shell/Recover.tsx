import { useEffect, useState, type FormEvent } from "react";
import { useNavigate, useRouterState } from "@tanstack/react-router";
import {
  RecoveryError,
  inspectRecoveryLink,
  recoveryTimeLeft,
  recoveryTokenFromHash,
  resetAdministratorPassword,
  type RecoveryField,
  type RecoveryInspection,
} from "@/lib/recovery";
import { withInlineCode } from "@/lib/inline-code";
import { Button } from "../ui/button/Button";
import { Field, Input } from "../ui/input/Input";
import { SignInShell } from "./SignIn";

const HEADING = "Recover administrator access";

/** What the page knows about the link it was opened with. */
type LinkState =
  | { kind: "checking" }
  | { kind: "open"; inspection: RecoveryInspection }
  | { kind: "refused"; error: RecoveryError };

/**
 * Administrator recovery: the page the link from `hs recover` opens.
 *
 * The sibling of `Setup`: a server whose administrators cannot sign in cannot be signed in to
 * either, so this is rendered by `AppShell` in place of the sign-in form rather than as a route
 * behind it. The token is already in the link, so the page asks the server what the link can do
 * as soon as it opens, and then asks the operator for as little as it can: which administrator
 * (only when there is more than one) and a new password. The reset signs the operator in.
 */
export function Recover() {
  const navigate = useNavigate();
  // Read once, from the address this page was opened at: the token must keep working after the
  // fragment is gone, and must not change under a half-filled form.
  const initialHash = useRouterState({ select: (state) => state.location.hash });
  const [token] = useState(() => recoveryTokenFromHash(initialHash));
  const [link, setLink] = useState<LinkState>({ kind: "checking" });
  const [userId, setUserId] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<{ message: string; field: RecoveryField | null } | null>(null);

  useEffect(() => {
    if (!token) return;
    let cancelled = false;
    inspectRecoveryLink(token).then(
      (inspection) => {
        if (cancelled) return;
        setLink({ kind: "open", inspection });
        // One administrator needs no choosing.
        if (inspection.administrators.length === 1) {
          setUserId(inspection.administrators[0]!.user_id);
        }
      },
      (err: unknown) => {
        if (cancelled) return;
        setLink({
          kind: "refused",
          error:
            err instanceof RecoveryError
              ? err
              : new RecoveryError("Couldn't check this link.", "failed"),
        });
      },
    );
    return () => {
      cancelled = true;
    };
  }, [token]);

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!token) return;
    setError(null);
    if (!userId) {
      setError({ message: "Choose the account to recover.", field: "userId" });
      return;
    }
    if (password !== confirm) {
      setError({ message: "The two passwords don’t match.", field: "password" });
      return;
    }
    setPending(true);
    try {
      await resetAdministratorPassword({ recoveryToken: token, userId, password });
      // Signed in now, so `AppShell` renders the app. Leave the recovery address -- and the token
      // in its fragment -- behind, out of the history too.
      await navigate({ to: "/", replace: true });
    } catch (err) {
      if (err instanceof RecoveryError && err.refusal !== "invalid" && err.refusal !== "failed") {
        // The link stopped working under the form: say so in place of it.
        setLink({ kind: "refused", error: err });
      } else if (err instanceof RecoveryError) {
        setError({ message: err.message, field: err.field });
      } else {
        setError({ message: "The reset failed.", field: null });
      }
    } finally {
      setPending(false);
    }
  }

  const goToSignIn = () => void navigate({ to: "/", replace: true });

  if (!token) {
    return (
      <SignInShell>
        <h2 className="mt-4 text-base font-medium text-text">{HEADING}</h2>
        <p className="mt-2 text-sm text-text-muted">
          This page resets an administrator&apos;s password when nobody can sign in. It needs the
          link that{" "}
          <code className="rounded-xs bg-surface-sunken px-1 py-0.5 font-identifier text-text">
            hs recover
          </code>{" "}
          prints: run it where the server keeps its signing key, then open the link it gives you.
          The link works once and expires after fifteen minutes.
        </p>
        <Button className="mt-6 w-full" onClick={goToSignIn}>
          Go to sign in
        </Button>
      </SignInShell>
    );
  }

  if (link.kind === "checking") {
    return (
      <SignInShell>
        <h2 className="mt-4 text-base font-medium text-text">{HEADING}</h2>
        <p role="status" className="mt-2 text-sm text-text-muted">
          Checking the link…
        </p>
      </SignInShell>
    );
  }

  if (link.kind === "refused") {
    return (
      <SignInShell>
        <h2 className="mt-4 text-base font-medium text-text">{HEADING}</h2>
        <p role="alert" className="mt-2 text-sm text-text-muted">
          {withInlineCode(link.error.message)}
        </p>
        <Button className="mt-6 w-full" onClick={goToSignIn}>
          Go to sign in
        </Button>
      </SignInShell>
    );
  }

  const { administrators, expiresAtMs } = link.inspection;
  const fieldError = (field: RecoveryField) => (error?.field === field ? error.message : undefined);

  return (
    <SignInShell>
      <h2 className="mt-4 text-base font-medium text-text">{HEADING}</h2>
      <p className="mt-2 text-sm text-text-muted">
        Choose a new password for an administrator account. You&apos;ll be signed in with it, and
        every other session of that account will be signed out.
      </p>
      <ExpiryNote expiresAtMs={expiresAtMs} />

      <form className="mt-6 flex flex-col gap-3" onSubmit={handleSubmit} noValidate>
        {administrators.length === 1 ? (
          <div className="flex flex-col gap-1.5">
            <span className="text-sm font-medium text-text">Account</span>
            <p className="font-identifier text-base text-text">{administrators[0]!.user_id}</p>
          </div>
        ) : (
          <AccountChoice
            administrators={administrators}
            value={userId}
            onChange={setUserId}
            error={fieldError("userId")}
          />
        )}
        <Field label="New password" error={fieldError("password")} required>
          {(fieldProps) => (
            <Input
              {...fieldProps}
              type="password"
              autoComplete="new-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          )}
        </Field>
        <Field label="Confirm password" required>
          {(fieldProps) => (
            <Input
              {...fieldProps}
              type="password"
              autoComplete="new-password"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
            />
          )}
        </Field>

        {error && error.field === null && (
          <p role="alert" className="text-sm text-danger">
            {error.message}
          </p>
        )}

        <Button type="submit" disabled={pending} className="mt-1">
          {pending ? "Resetting…" : "Reset password and sign in"}
        </Button>
      </form>
    </SignInShell>
  );
}

/**
 * "This link works once and expires in 14 minutes", kept current: the sentence is re-read every
 * half minute, so a form left open says something true when the operator comes back to it. Once
 * the moment has passed it says so, but the form stays usable: the server is the authority on
 * its own clock, and a browser whose clock runs fast should not lock anybody out.
 */
function ExpiryNote({ expiresAtMs }: { expiresAtMs: number }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 30_000);
    return () => clearInterval(timer);
  }, []);
  const left = recoveryTimeLeft(expiresAtMs, now);
  return (
    <p className="mt-2 text-xs text-text-faint">
      {left
        ? `This link works once and expires ${left}.`
        : withInlineCode("This link has expired. Run `hs recover` again for a fresh one.")}
    </p>
  );
}

/**
 * Which administrator to recover, when there is more than one. Native radios in a fieldset: the
 * list is short, the choice is required, and a screen reader should hear "Account, radio group"
 * rather than a row of buttons.
 */
function AccountChoice({
  administrators,
  value,
  onChange,
  error,
}: {
  administrators: { user_id: string }[];
  value: string;
  onChange: (userId: string) => void;
  error?: string;
}) {
  const errorId = error ? "recover-account-error" : undefined;
  return (
    <fieldset className="flex flex-col gap-1.5" aria-describedby={errorId} aria-invalid={!!error}>
      <legend className="text-sm font-medium text-text">
        Account
        <span aria-hidden="true" className="text-danger">
          {" "}
          *
        </span>
      </legend>
      <div
        className={`flex flex-col gap-0.5 rounded-sm border bg-surface p-1 ${
          error ? "border-danger" : "border-border-strong"
        }`}
      >
        {administrators.map((admin) => (
          <label
            key={admin.user_id}
            className="flex cursor-pointer items-center gap-2 rounded-xs px-2 py-1.5 text-sm text-text hover:bg-surface-sunken"
          >
            <input
              type="radio"
              name="account"
              value={admin.user_id}
              checked={value === admin.user_id}
              onChange={() => onChange(admin.user_id)}
              className="accent-accent focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
            />
            <span className="font-identifier">{admin.user_id}</span>
          </label>
        ))}
      </div>
      {error && (
        <p id={errorId} role="alert" className="text-xs text-danger">
          {error}
        </p>
      )}
    </fieldset>
  );
}
