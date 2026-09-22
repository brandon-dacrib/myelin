import { useId, useState, type FormEvent } from "react";
import { useResetPassword } from "@/api/users";
import { ApiProblemError } from "@/api/problem";
import { generatePassword } from "@/lib/password";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { Switch } from "@/components/ui/switch/Switch";
import { CopyableId } from "@/components/CopyableId";

interface ResetPasswordDialogProps {
  userId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * Sets a new password for somebody who has forgotten theirs. Ends on a hand-over view, like
 * adding a user does, because the password is about to be unrecoverable and still has to
 * reach a person. Signs them out everywhere by default: a password is reset because the old
 * one is not trusted any more.
 */
export function ResetPasswordDialog({ userId, open, onOpenChange }: ResetPasswordDialogProps) {
  const reset = useResetPassword();
  const logoutLabelId = useId();
  const logoutHintId = useId();
  const [password, setPassword] = useState("");
  const [passwordVisible, setPasswordVisible] = useState(false);
  const [logoutDevices, setLogoutDevices] = useState(true);
  const [error, setError] = useState<{ message: string; inField: boolean } | null>(null);
  const [done, setDone] = useState<{ password: string; loggedOut: boolean } | null>(null);

  function clear() {
    setPassword("");
    setPasswordVisible(false);
    setLogoutDevices(true);
    setError(null);
    setDone(null);
  }

  function handleOpenChange(next: boolean) {
    // Nothing typed here should outlive the dialog, least of all a password.
    if (!next) clear();
    onOpenChange(next);
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    if (!password) {
      setError({ message: "Set a password, or generate one.", inField: true });
      return;
    }
    try {
      await reset.mutateAsync({ userId, password, logoutDevices });
      setDone({ password, loggedOut: logoutDevices });
    } catch (err) {
      if (err instanceof ApiProblemError) {
        const { problem } = err;
        const first = problem.errors?.[0];
        setError({
          message: first?.detail ?? problem.detail ?? problem.title ?? "The server refused.",
          inField: first?.pointer === "/password",
        });
        return;
      }
      setError({ message: "Couldn’t reach the server.", inField: false });
    }
  }

  if (done) {
    return (
      <Dialog open={open} onOpenChange={handleOpenChange}>
        <DialogContent
          size="form"
          title="Password reset"
          description="Give this to them. It can’t be shown again after you close this."
          footer={<Button onClick={() => handleOpenChange(false)}>Done</Button>}
        >
          <dl className="grid grid-cols-[auto_1fr] items-center gap-x-4 gap-y-3 text-sm">
            <dt className="text-text-muted">User ID</dt>
            <dd>
              <CopyableId value={userId} />
            </dd>
            <dt className="text-text-muted">Password</dt>
            <dd>
              <CopyableId value={done.password} label="password" />
            </dd>
          </dl>
          <p className="mt-4 text-sm text-text-muted">
            {done.loggedOut
              ? "Every device they were signed in on has been signed out."
              : "Their existing sessions are still signed in."}
          </p>
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        size="form"
        title="Reset password"
        description={`Set a new password for ${userId}. The old one stops working at once.`}
      >
        <form onSubmit={handleSubmit} className="flex flex-col gap-4" noValidate>
          <Field label="New password" error={error?.inField ? error.message : undefined} required>
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
              <p id={logoutLabelId} className="text-sm font-medium text-text">
                Sign out everywhere
              </p>
              <p id={logoutHintId} className="text-sm text-text-muted">
                Every device they are signed in on has to sign in again with the new password.
              </p>
            </div>
            <Switch
              checked={logoutDevices}
              onCheckedChange={setLogoutDevices}
              aria-labelledby={logoutLabelId}
              aria-describedby={logoutHintId}
            />
          </div>

          {error && !error.inField && (
            <p role="alert" className="text-sm text-danger">
              {error.message}
            </p>
          )}

          <div className="mt-2 flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={reset.isPending}>
              {reset.isPending ? "Resetting…" : "Reset password"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
