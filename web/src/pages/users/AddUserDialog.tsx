import { useId, useState, type FormEvent } from "react";
import { Link } from "@tanstack/react-router";
import { useCreateUser, type User } from "@/api/users";
import { ApiProblemError } from "@/api/problem";
import { generatePassword } from "@/lib/password";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { Switch } from "@/components/ui/switch/Switch";
import { CopyableId } from "@/components/CopyableId";

type FieldName = "username" | "password";

const FIELD_FOR_POINTER: Record<string, FieldName> = {
  "/localpart": "username",
  "/user_id": "username",
  "/password": "password",
};

/**
 * Adds an account (`POST /users`).
 *
 * Registration is closed by default, so on most servers this dialog is the only way anybody
 * after the first administrator gets an account. It is written for the usual case -- an
 * administrator making an account *for somebody else* -- which is why it offers to generate the
 * password and why it ends on a hand-over view rather than closing: the password is about to
 * be unrecoverable, and the administrator still has to get it to a person.
 */
export function AddUserDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const createUser = useCreateUser();
  const adminLabelId = useId();
  const adminHintId = useId();
  const [username, setUsername] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [passwordVisible, setPasswordVisible] = useState(false);
  const [admin, setAdmin] = useState(false);
  const [error, setError] = useState<{ message: string; field: FieldName | null } | null>(null);
  const [created, setCreated] = useState<{ user: User; password: string } | null>(null);

  function reset() {
    setUsername("");
    setDisplayName("");
    setPassword("");
    setPasswordVisible(false);
    setAdmin(false);
    setError(null);
    setCreated(null);
  }

  function handleOpenChange(next: boolean) {
    // Nothing typed here should outlive the dialog, least of all a password.
    if (!next) reset();
    onOpenChange(next);
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    if (!username.trim()) {
      setError({ message: "Choose a username.", field: "username" });
      return;
    }
    if (!password) {
      setError({ message: "Set a password, or generate one.", field: "password" });
      return;
    }
    try {
      const user = await createUser.mutateAsync({
        localpart: username.trim().replace(/^@/, "").split(":")[0],
        password,
        display_name: displayName.trim() || undefined,
        admin,
      });
      setCreated({ user, password });
    } catch (err) {
      if (err instanceof ApiProblemError) {
        const { problem } = err;
        if (problem.status === 409) {
          setError({ message: problem.detail ?? "That username is taken.", field: "username" });
          return;
        }
        const first = problem.errors?.[0];
        setError({
          message: first?.detail ?? problem.detail ?? problem.title ?? "The server refused.",
          field: first?.pointer ? (FIELD_FOR_POINTER[first.pointer] ?? null) : null,
        });
        return;
      }
      setError({ message: "Couldn’t reach the server.", field: null });
    }
  }

  const fieldError = (field: FieldName) => (error?.field === field ? error.message : undefined);

  if (created) {
    return (
      <Dialog open={open} onOpenChange={handleOpenChange}>
        <DialogContent
          size="form"
          title="Account created"
          description="Give these to the person who will use it. The password can’t be shown again after you close this."
          footer={
            <>
              <Button variant="secondary" onClick={reset}>
                Add another
              </Button>
              <Button onClick={() => handleOpenChange(false)}>Done</Button>
            </>
          }
        >
          <dl className="grid grid-cols-[auto_1fr] items-center gap-x-4 gap-y-3 text-sm">
            <dt className="text-text-muted">User ID</dt>
            <dd>
              <CopyableId value={created.user.user_id} />
            </dd>
            <dt className="text-text-muted">Password</dt>
            <dd>
              <CopyableId value={created.password} label="password" />
            </dd>
          </dl>
          <p className="mt-4 text-sm text-text-muted">
            {created.user.admin
              ? "They are a server administrator, and can sign in here as well as in any Matrix client."
              : "They can sign in from any Matrix client."}{" "}
            <Link
              to="/users/$userId"
              params={{ userId: created.user.user_id }}
              className="text-accent underline underline-offset-2 hover:no-underline"
              onClick={() => handleOpenChange(false)}
            >
              Open the account
            </Link>
          </p>
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        size="form"
        title="Add a user"
        description="Creates an account on this server. They can change the password once they’ve signed in."
      >
        <form className="flex flex-col gap-4" onSubmit={handleSubmit} noValidate>
          <Field
            label="Username"
            hint="Lowercase letters and digits. They’ll be @username on this server."
            error={fieldError("username")}
            required
          >
            {(fieldProps) => (
              <Input
                {...fieldProps}
                autoComplete="off"
                autoCapitalize="none"
                spellCheck={false}
                value={username}
                onChange={(e) => setUsername(e.target.value)}
              />
            )}
          </Field>
          <Field label="Display name" hint="Optional. What other people see in rooms.">
            {(fieldProps) => (
              <Input
                {...fieldProps}
                autoComplete="off"
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
              />
            )}
          </Field>
          <Field label="Password" error={fieldError("password")} required>
            {(fieldProps) => (
              <div className="flex gap-2">
                <Input
                  {...fieldProps}
                  type={passwordVisible ? "text" : "password"}
                  autoComplete="new-password"
                  spellCheck={false}
                  className={passwordVisible ? "font-identifier" : undefined}
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                />
                <Button
                  type="button"
                  variant="secondary"
                  onClick={() => {
                    // Shown once generated: nobody can hand over a password they cannot read.
                    setPassword(generatePassword());
                    setPasswordVisible(true);
                  }}
                >
                  Generate
                </Button>
              </div>
            )}
          </Field>
          <div className="flex items-start justify-between gap-4">
            <div>
              <p id={adminLabelId} className="text-sm font-medium text-text">
                Server administrator
              </p>
              <p id={adminHintId} className="text-sm text-text-muted">
                Can sign in to this interface and change anything on the server.
              </p>
            </div>
            <Switch
              checked={admin}
              onCheckedChange={setAdmin}
              aria-labelledby={adminLabelId}
              aria-describedby={adminHintId}
            />
          </div>

          {error && error.field === null && (
            <p role="alert" className="text-sm text-danger">
              {error.message}
            </p>
          )}

          <div className="mt-2 flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={createUser.isPending}>
              {createUser.isPending ? "Creating…" : "Create account"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
