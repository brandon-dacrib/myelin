import { useState, type FormEvent } from "react";
import { useUpdateRegistrationToken } from "@/api/registration-tokens";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { toast } from "@/components/ui/toast/toast-store";
import {
  expiryFor,
  parseUses,
  toDateTimeLocal,
  type ExpiryChoice,
  type RegistrationTokenView,
} from "@/lib/registration-tokens";
import { ExpiryControl, UsesControl } from "./token-controls";

type FieldName = "uses" | "expiry";

/**
 * Changes a token's limits (`PATCH /registration-tokens/{token}`): how many uses it has and when
 * it expires. "Expire now" is the quick way to stop a link that went somewhere it should not
 * have without losing the record of who used it (Delete loses that).
 *
 * Mounted with a `key` per token by the page, so its fields start from the token it edits.
 */
export function EditTokenDialog({
  token,
  onClose,
}: {
  token: RegistrationTokenView;
  onClose: () => void;
}) {
  const update = useUpdateRegistrationToken();
  const [uses, setUses] = useState(token.usesAllowed === null ? "1" : String(token.usesAllowed));
  const [unlimited, setUnlimited] = useState(token.usesAllowed === null);
  const [expiry, setExpiry] = useState<ExpiryChoice>(token.expiresAt ? "custom" : "never");
  const [expiryAt, setExpiryAt] = useState(token.expiresAt ? toDateTimeLocal(token.expiresAt) : "");
  const [error, setError] = useState<{ message: string; field: FieldName | null } | null>(null);

  async function send(change: { usesAllowed?: number | null; expiresAt?: string | null }) {
    try {
      await update.mutateAsync({ token: token.token, ...change });
      return true;
    } catch (err) {
      if (err instanceof ApiProblemError) {
        const first = err.problem.errors?.[0];
        const field =
          first?.pointer === "/uses_allowed"
            ? "uses"
            : first?.pointer === "/expires_at"
              ? "expiry"
              : null;
        setError({
          message:
            first?.detail ?? err.problem.detail ?? err.problem.title ?? "The server refused.",
          field,
        });
      } else {
        setError({ message: "Couldn’t reach the server.", field: null });
      }
      return false;
    }
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    let usesAllowed: number | null = null;
    if (!unlimited) {
      const parsed = parseUses(uses);
      if ("error" in parsed) {
        setError({ message: parsed.error, field: "uses" });
        return;
      }
      usesAllowed = parsed.value;
    }
    const expiresAt = expiryFor(expiry, expiryAt);
    if (expiresAt === undefined) {
      setError({ message: "Choose the date and time it expires.", field: "expiry" });
      return;
    }
    if (await send({ usesAllowed, expiresAt })) {
      toast({ title: `Token ${token.token} updated` });
      onClose();
    }
  }

  async function expireNow() {
    setError(null);
    if (await send({ expiresAt: new Date().toISOString() })) {
      toast({
        title: `Token ${token.token} expired`,
        description: "Its invite link stops working.",
      });
      onClose();
    }
  }

  const fieldError = (field: FieldName) => (error?.field === field ? error.message : undefined);

  return (
    <Dialog open onOpenChange={(next) => !next && onClose()}>
      <DialogContent
        size="form"
        title={`Edit ${token.token}`}
        description={`${token.completed} ${token.completed === 1 ? "account has" : "accounts have"} been created with it so far.`}
      >
        <form className="flex flex-col gap-5" onSubmit={handleSubmit} noValidate>
          <UsesControl
            uses={uses}
            onUsesChange={setUses}
            unlimited={unlimited}
            onUnlimitedChange={setUnlimited}
            error={fieldError("uses")}
          />
          <ExpiryControl
            choice={expiry}
            onChoiceChange={setExpiry}
            custom={expiryAt}
            onCustomChange={setExpiryAt}
            error={fieldError("expiry")}
            presets={false}
          />

          {error && error.field === null && (
            <p role="alert" className="text-sm text-danger">
              {error.message}
            </p>
          )}

          <div className="mt-1 flex flex-wrap items-center justify-between gap-2">
            <Button
              type="button"
              variant="secondary"
              disabled={update.isPending}
              onClick={expireNow}
            >
              Expire now
            </Button>
            <div className="flex gap-2">
              <Button type="button" variant="ghost" onClick={onClose}>
                Cancel
              </Button>
              <Button type="submit" disabled={update.isPending}>
                {update.isPending ? "Saving…" : "Save"}
              </Button>
            </div>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
