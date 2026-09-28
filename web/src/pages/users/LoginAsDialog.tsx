import { useState, type FormEvent, type ReactNode } from "react";
import { TriangleAlert } from "lucide-react";
import { useLoginAs, type LoginAsToken } from "@/api/user-moderation";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Textarea } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { CopyableId } from "@/components/CopyableId";
import { MutationError } from "@/components/MutationError";

/** How long a support token may last; the server allows 1 second to 24 hours. */
const LOGIN_AS_DURATIONS = [
  { value: "900", label: "15 minutes" },
  { value: "3600", label: "1 hour" },
  { value: "28800", label: "8 hours" },
  { value: "86400", label: "24 hours" },
] as const;

interface LoginAsDialogProps {
  userId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * Mints a support token that acts as the user (`POST /users/{user_id}/login-as`). The first
 * step says plainly what that means and asks why; the second shows the token once. Closing the
 * dialog forgets it: it is never kept anywhere the interface can show it again.
 */
export function LoginAsDialog({ userId, open, onOpenChange }: LoginAsDialogProps) {
  const loginAs = useLoginAs();
  const [reason, setReason] = useState("");
  const [reasonError, setReasonError] = useState<string>();
  const [duration, setDuration] = useState<string>("3600");
  const [minted, setMinted] = useState<LoginAsToken | null>(null);

  function handleOpenChange(next: boolean) {
    if (!next) {
      // The token must not outlive the dialog.
      setMinted(null);
      setReason("");
      setReasonError(undefined);
      setDuration("3600");
      loginAs.reset();
    }
    onOpenChange(next);
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!reason.trim()) {
      setReasonError("Say why: it is recorded with the token.");
      return;
    }
    setReasonError(undefined);
    try {
      const token = await loginAs.mutateAsync({
        userId,
        reason: reason.trim(),
        validForSeconds: Number(duration),
      });
      setMinted(token);
    } catch {
      /* shown below from loginAs.error */
    }
  }

  if (minted) {
    const expires = new Date(minted.expires_at);
    return (
      <Dialog open={open} onOpenChange={handleOpenChange}>
        <DialogContent
          size="form"
          title={`Support token for ${minted.user_id}`}
          description="Shown once. Copy it now: it cannot be shown again after you close this."
          footer={<Button onClick={() => handleOpenChange(false)}>Done</Button>}
        >
          <Warning>
            Anything done with this token is done as {minted.user_id}, and is recorded as a support
            session in their session list and in the audit log under your name.
          </Warning>
          <dl className="mt-4 grid grid-cols-[auto_1fr] items-center gap-x-4 gap-y-3 text-sm">
            <dt className="text-text-muted">Access token</dt>
            <dd className="break-all" data-testid="login-as-token">
              <CopyableId value={minted.access_token} label="access token" />
            </dd>
            <dt className="text-text-muted">Device</dt>
            <dd>
              <CopyableId value={minted.device_id} />
            </dd>
            <dt className="text-text-muted">Expires</dt>
            <dd>
              <time dateTime={minted.expires_at}>{expires.toLocaleString()}</time>
            </dd>
          </dl>
          <p className="mt-4 text-sm text-text-muted">
            Sign the session out from their session list to end it early.
          </p>
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        size="form"
        title={`Sign in as ${userId}?`}
        description="Creates an access token that acts as this user, for support."
      >
        <form onSubmit={handleSubmit} className="flex flex-col gap-4" noValidate>
          <Warning>
            You will be able to read and send as {userId}. The token, who made it, for how long and
            why are recorded in the audit log, and the session shows in their session list.
          </Warning>
          <Field label="Reason" required error={reasonError}>
            {(fieldProps) => (
              <Textarea
                {...fieldProps}
                value={reason}
                maxLength={500}
                onChange={(e) => setReason(e.target.value)}
              />
            )}
          </Field>
          <Field label="Valid for">
            {(fieldProps) => (
              <Select
                id={fieldProps.id}
                aria-describedby={fieldProps["aria-describedby"]}
                value={duration}
                onValueChange={setDuration}
                options={LOGIN_AS_DURATIONS.map((d) => ({ value: d.value, label: d.label }))}
              />
            )}
          </Field>
          {loginAs.error != null && (
            <MutationError error={loginAs.error} action="create a support token" />
          )}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="secondary" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" variant="danger" disabled={loginAs.isPending}>
              {loginAs.isPending ? "Creating…" : "Create support token"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function Warning({ children }: { children: ReactNode }) {
  return (
    <p
      role="note"
      className="flex gap-2 rounded-sm border border-warning-border bg-warning-bg p-3 text-sm text-text"
    >
      <TriangleAlert size={16} aria-hidden="true" className="mt-0.5 shrink-0 text-warning" />
      <span>{children}</span>
    </p>
  );
}
