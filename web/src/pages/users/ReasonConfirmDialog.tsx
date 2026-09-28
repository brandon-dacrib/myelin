import { useState, type FormEvent, type ReactNode } from "react";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Textarea } from "@/components/ui/input/Input";
import { MutationError } from "@/components/MutationError";

/**
 * A confirmation that names the user and what will happen, with an optional reason that goes
 * to the audit log. Stays open on a refusal and says why.
 */
export function ReasonConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  confirmLabel,
  pendingLabel,
  action,
  onConfirm,
  pending,
  error,
  children,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description: ReactNode;
  confirmLabel: string;
  pendingLabel: string;
  /** What is being attempted, as it reads after "Couldn't": "suspend them". */
  action: string;
  onConfirm: (reason: string) => void;
  pending: boolean;
  error: unknown;
  children?: ReactNode;
}) {
  const [reason, setReason] = useState("");

  function handleOpenChange(next: boolean) {
    if (!next) setReason("");
    onOpenChange(next);
  }

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    onConfirm(reason.trim());
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent title={title} description={description}>
        <form onSubmit={handleSubmit} className="flex flex-col gap-4" noValidate>
          {children}
          <Field label="Reason" hint="Recorded in the audit log. They are not shown it.">
            {(fieldProps) => (
              <Textarea
                {...fieldProps}
                value={reason}
                onChange={(e) => setReason(e.target.value)}
                maxLength={500}
              />
            )}
          </Field>
          {error != null && <MutationError error={error} action={action} />}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="secondary" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" variant="danger" disabled={pending}>
              {pending ? pendingLabel : confirmLabel}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
