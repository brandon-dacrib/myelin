import { useEffect, useState, type FormEvent } from "react";
import { useRouterState } from "@tanstack/react-router";
import { CircleCheck } from "lucide-react";
import {
  RegistrationError,
  TOKEN_INVALID_MESSAGE,
  checkTokenValidity,
  checkUsernameAvailable,
  registerWithToken,
  type Availability,
  type RegistrationField,
} from "@/lib/registration";
import { Button } from "../ui/button/Button";
import { Field, Input } from "../ui/input/Input";
import { CopyableId } from "../CopyableId";
import { SignInShell } from "./SignIn";

const HEADING = "Create your account";

type LinkState = "checking" | "open" | "invalid";

/** The `token` query parameter of the address the page was opened at, or `null`. */
function tokenFromSearch(searchStr: string): string | null {
  const token = new URLSearchParams(searchStr).get("token");
  return token && token.trim() ? token.trim() : null;
}

/**
 * The page an invite link opens: `/admin/register?token=...`.
 *
 * For the person being invited, not for an administrator, so it is rendered by `AppShell` in
 * place of everything else whether or not anybody is signed in to this browser, and it talks
 * only to the Matrix client-server API (`lib/registration.ts`), never with an administrator's
 * session. It checks the link first, so somebody holding a spent one is told before they fill
 * anything in; then it asks for a username and a password and registers the account, ending on
 * the new user ID and where to sign in with it.
 */
export function Register() {
  const searchStr = useRouterState({ select: (state) => state.location.searchStr });
  const [token] = useState(() => tokenFromSearch(searchStr));
  const [link, setLink] = useState<LinkState>("checking");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [availability, setAvailability] = useState<{
    username: string;
    result: Availability;
  } | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<{
    message: string;
    field: RegistrationField | "confirm" | null;
  } | null>(null);
  const [registered, setRegistered] = useState<string | null>(null);

  useEffect(() => {
    if (!token) return;
    let cancelled = false;
    checkTokenValidity(token).then((valid) => {
      if (cancelled) return;
      // `null` is "could not tell": let the registration itself be the judge.
      setLink(valid === false ? "invalid" : "open");
    });
    return () => {
      cancelled = true;
    };
  }, [token]);

  async function checkAvailability() {
    const name = normalise(username);
    if (!name) return;
    const result = await checkUsernameAvailable(name);
    setAvailability({ username: name, result });
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!token) return;
    setError(null);
    const name = normalise(username);
    if (!name) {
      setError({ message: "Choose a username.", field: "username" });
      return;
    }
    if (!password) {
      setError({ message: "Choose a password.", field: "password" });
      return;
    }
    if (password !== confirm) {
      setError({ message: "The two passwords don’t match.", field: "confirm" });
      return;
    }
    setPending(true);
    try {
      const { userId } = await registerWithToken({ username: name, password, token });
      setPassword("");
      setConfirm("");
      setRegistered(userId);
    } catch (err) {
      if (err instanceof RegistrationError && err.refusal === "token-invalid") {
        setLink("invalid");
      } else if (err instanceof RegistrationError) {
        setError({ message: err.message, field: err.field });
      } else {
        setError({ message: "The registration failed.", field: null });
      }
    } finally {
      setPending(false);
    }
  }

  if (!token) {
    return (
      <SignInShell>
        <h2 className="mt-4 text-base font-medium text-text">{HEADING}</h2>
        <p role="alert" className="mt-2 text-sm text-text-muted">
          This page needs an invite link. Ask an administrator of this server to send you one.
        </p>
      </SignInShell>
    );
  }

  if (registered) {
    const origin = window.location.origin;
    return (
      <SignInShell>
        <h2 className="mt-4 flex items-center gap-2 text-base font-medium text-text">
          <CircleCheck size={18} aria-hidden="true" className="text-success" />
          Your account is ready
        </h2>
        <dl className="mt-4 grid grid-cols-[auto_1fr] items-center gap-x-4 gap-y-2 text-sm">
          <dt className="text-text-muted">User ID</dt>
          <dd>
            <CopyableId value={registered} />
          </dd>
          <dt className="text-text-muted">Server</dt>
          <dd>
            <CopyableId value={origin} />
          </dd>
        </dl>
        <p className="mt-4 text-sm text-text-muted">
          Sign in with any Matrix client, such as Element, using this server&apos;s address (
          <span className="font-identifier text-text">{origin}</span>), your username and the
          password you just chose.
        </p>
      </SignInShell>
    );
  }

  if (link === "checking") {
    return (
      <SignInShell>
        <h2 className="mt-4 text-base font-medium text-text">{HEADING}</h2>
        <p role="status" className="mt-2 text-sm text-text-muted">
          Checking your invite link…
        </p>
      </SignInShell>
    );
  }

  if (link === "invalid") {
    return (
      <SignInShell>
        <h2 className="mt-4 text-base font-medium text-text">{HEADING}</h2>
        <p role="alert" className="mt-2 text-sm text-text-muted">
          {TOKEN_INVALID_MESSAGE}
        </p>
      </SignInShell>
    );
  }

  const fieldError = (field: RegistrationField | "confirm") =>
    error?.field === field ? error.message : undefined;
  const checked =
    availability && availability.username === normalise(username) ? availability.result : null;
  const usernameError =
    fieldError("username") ??
    (checked && (checked.kind === "taken" || checked.kind === "invalid")
      ? checked.message
      : undefined);

  return (
    <SignInShell>
      <h2 className="mt-4 text-base font-medium text-text">{HEADING}</h2>
      <p className="mt-2 text-sm text-text-muted">
        You&apos;ve been invited to this Matrix server. Choose a username and a password.
      </p>

      <form className="mt-6 flex flex-col gap-3" onSubmit={handleSubmit} noValidate>
        <Field
          label="Username"
          hint={
            checked?.kind === "available"
              ? "Available."
              : "Lowercase letters, digits and . _ = - /. Others will see it."
          }
          error={usernameError}
          required
        >
          {(fieldProps) => (
            <Input
              {...fieldProps}
              autoComplete="username"
              autoCapitalize="none"
              spellCheck={false}
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              onBlur={checkAvailability}
            />
          )}
        </Field>
        <Field label="Password" error={fieldError("password")} required>
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
        <Field label="Confirm password" error={fieldError("confirm")} required>
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
          {pending ? "Creating account…" : "Create account"}
        </Button>
      </form>
    </SignInShell>
  );
}

/** What was typed, as a username: no leading `@`, no server part, lowercase, trimmed. */
function normalise(input: string): string {
  return input.trim().replace(/^@/, "").split(":")[0]!.toLowerCase();
}
