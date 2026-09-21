import { useEffect, useState, type FormEvent } from "react";
import { useNavigate, useRouterState } from "@tanstack/react-router";
import {
  SetupError,
  createFirstAdministrator,
  fetchNeedsSetup,
  setupTokenFromHash,
  type SetupField,
} from "@/lib/setup";
import { Button } from "../ui/button/Button";
import { Field, Input } from "../ui/input/Input";
import { SignInShell } from "./SignIn";

/**
 * First-run setup: the page the setup link in the server's log opens.
 *
 * A server with no administrator cannot be signed in to, so this is rendered by `AppShell` in
 * place of the sign-in form rather than as a route behind it. It asks for as little as it can:
 * the token is already in the link, so an operator who followed it chooses a username and a
 * password and is in. One who arrived without the link is asked for the token too, and may paste
 * either the token or the whole link.
 */
export function Setup() {
  const navigate = useNavigate();
  // Read once, from the address this page was opened at: the token must keep working after the
  // fragment is gone, and must not change under a half-filled form.
  const initialHash = useRouterState({ select: (state) => state.location.hash });
  const [linkToken] = useState(() => setupTokenFromHash(initialHash));
  const [needsSetup, setNeedsSetup] = useState<boolean | null | "loading">("loading");
  const [pastedToken, setPastedToken] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<{ message: string; field: SetupField | null } | null>(null);
  const [alreadySetUp, setAlreadySetUp] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void fetchNeedsSetup().then((value) => {
      if (!cancelled) setNeedsSetup(value);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    // Accept the whole link where the token was asked for: it is what people have on their
    // clipboard.
    const setupToken =
      linkToken ?? setupTokenFromHash(pastedToken.split("#")[1] ?? "") ?? pastedToken;
    if (!setupToken.trim()) {
      setError({ message: "Paste the setup token from the server’s log.", field: "setupToken" });
      return;
    }
    if (!username.trim()) {
      setError({ message: "Choose a username.", field: "username" });
      return;
    }
    if (password !== confirm) {
      setError({ message: "The two passwords don’t match.", field: "password" });
      return;
    }
    setPending(true);
    try {
      await createFirstAdministrator({ setupToken, username, password });
      // Signed in now, so `AppShell` renders the app. Leave the setup address -- and the token
      // in its fragment -- behind, out of the history too.
      await navigate({ to: "/", replace: true });
    } catch (err) {
      if (err instanceof SetupError) {
        setAlreadySetUp(err.alreadySetUp);
        setError({ message: err.message, field: err.field });
      } else {
        setError({ message: "Setup failed.", field: null });
      }
    } finally {
      setPending(false);
    }
  }

  if (needsSetup === false || alreadySetUp) {
    return (
      <SignInShell>
        <h2 className="mt-4 text-base font-medium text-text">This server is already set up</h2>
        <p className="mt-2 text-sm text-text-muted">
          It has an administrator, so the setup link no longer does anything. Sign in with that
          account instead.
        </p>
        <Button className="mt-6 w-full" onClick={() => void navigate({ to: "/", replace: true })}>
          Go to sign in
        </Button>
      </SignInShell>
    );
  }

  const fieldError = (field: SetupField) => (error?.field === field ? error.message : undefined);

  return (
    <SignInShell>
      <h2 className="mt-4 text-base font-medium text-text">Create the first administrator</h2>
      <p className="mt-2 text-sm text-text-muted">
        This server doesn&apos;t have an administrator yet. Choose the account you&apos;ll manage it
        with. It&apos;s an ordinary Matrix account too, so you can chat from it.
      </p>

      <form className="mt-6 flex flex-col gap-3" onSubmit={handleSubmit} noValidate>
        {!linkToken && (
          <Field
            label="Setup token"
            hint="The server writes a setup link to its log each time it starts. Paste the link, or just the part after #token=."
            error={fieldError("setupToken")}
            required
          >
            {(fieldProps) => (
              <Input
                {...fieldProps}
                autoComplete="off"
                spellCheck={false}
                value={pastedToken}
                onChange={(e) => setPastedToken(e.target.value)}
              />
            )}
          </Field>
        )}
        <Field
          label="Username"
          hint="Lowercase letters and digits. You’ll be @username on this server."
          error={fieldError("username")}
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
          {pending ? "Creating…" : "Create administrator"}
        </Button>
      </form>
    </SignInShell>
  );
}
