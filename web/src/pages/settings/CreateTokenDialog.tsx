import { useState, type FormEvent } from "react";
import { useCreateRegistrationToken } from "@/api/registration-tokens";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { CopyableId } from "@/components/CopyableId";
import {
  customTokenProblem,
  expiryFor,
  formatExpiry,
  inviteLink,
  parseUses,
  type ExpiryChoice,
  type RegistrationTokenView,
} from "@/lib/registration-tokens";
import { ChoiceGroup, ExpiryControl, InviteLinkPanel, UsesControl } from "./token-controls";

type FieldName = "token" | "length" | "uses" | "expiry";

const FIELD_FOR_POINTER: Record<string, FieldName> = {
  "/token": "token",
  "/length": "length",
  "/uses_allowed": "uses",
  "/expires_at": "expiry",
};

type TokenSource = "generate" | "custom";

/**
 * Makes a registration token (`POST /registration-tokens`) and hands over its invite link.
 *
 * Written for the usual case -- inviting one person -- so it starts at one use, expiring in a
 * week, with the token generated. It ends on the invite link rather than closing, because the
 * link is the thing the administrator came for: it still has to reach a person.
 */
export function CreateTokenDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const create = useCreateRegistrationToken();
  const [source, setSource] = useState<TokenSource>("generate");
  const [custom, setCustom] = useState("");
  const [length, setLength] = useState("16");
  const [uses, setUses] = useState("1");
  const [unlimited, setUnlimited] = useState(false);
  const [expiry, setExpiry] = useState<ExpiryChoice>("7d");
  const [expiryAt, setExpiryAt] = useState("");
  const [error, setError] = useState<{ message: string; field: FieldName | null } | null>(null);
  const [created, setCreated] = useState<RegistrationTokenView | null>(null);

  function reset() {
    setSource("generate");
    setCustom("");
    setLength("16");
    setUses("1");
    setUnlimited(false);
    setExpiry("7d");
    setExpiryAt("");
    setError(null);
    setCreated(null);
  }

  function handleOpenChange(next: boolean) {
    if (!next) reset();
    onOpenChange(next);
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);

    let token: string | undefined;
    if (source === "custom") {
      const problem = customTokenProblem(custom.trim());
      if (problem) {
        setError({ message: problem, field: "token" });
        return;
      }
      token = custom.trim();
    }
    const lengthValue = Number(length);
    if (source === "generate" && (!/^\d+$/.test(length) || lengthValue < 1 || lengthValue > 64)) {
      setError({ message: "Between 1 and 64 characters.", field: "length" });
      return;
    }
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
    if (expiresAt !== null && Date.parse(expiresAt) <= Date.now()) {
      setError({ message: "Choose a time in the future.", field: "expiry" });
      return;
    }

    try {
      const result = await create.mutateAsync({
        token,
        length: source === "generate" ? lengthValue : 16,
        usesAllowed,
        expiresAt,
      });
      setCreated(result);
    } catch (err) {
      if (err instanceof ApiProblemError) {
        const { problem } = err;
        if (problem.status === 409) {
          setError({
            message: problem.detail ?? "A token with that name already exists.",
            field: source === "custom" ? "token" : null,
          });
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
          title="Invite link ready"
          description="Send this link to the person you are inviting. It opens a page where they choose a username and password."
          footer={
            <>
              <Button variant="secondary" onClick={reset}>
                Create another
              </Button>
              <Button onClick={() => handleOpenChange(false)}>Done</Button>
            </>
          }
        >
          <InviteLinkPanel link={inviteLink(created.token)} />
          <dl className="mt-4 grid grid-cols-[auto_1fr] items-center gap-x-4 gap-y-2 text-sm">
            <dt className="text-text-muted">Token</dt>
            <dd>
              <CopyableId value={created.token} />
            </dd>
            <dt className="text-text-muted">Uses allowed</dt>
            <dd className="text-text">{created.usesAllowed ?? "Unlimited"}</dd>
            <dt className="text-text-muted">Expires</dt>
            <dd className="text-text">{formatExpiry(created.expiresAt)}</dd>
          </dl>
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        size="form"
        title="Create an invite link"
        description="Makes a registration token: whoever has its link can create an account, even while registration is closed."
      >
        <form className="flex flex-col gap-5" onSubmit={handleSubmit} noValidate>
          <ChoiceGroup<TokenSource>
            legend="Token"
            name="token-source"
            value={source}
            onChange={(next) => {
              setSource(next);
              setError(null);
            }}
            choices={[
              { value: "generate", label: "Generate one" },
              { value: "custom", label: "Choose my own" },
            ]}
          />
          {source === "generate" ? (
            <Field
              label="Length"
              hint="Characters in the generated token."
              error={fieldError("length")}
            >
              {(fieldProps) => (
                <Input
                  {...fieldProps}
                  type="number"
                  inputMode="numeric"
                  min={1}
                  max={64}
                  className="max-w-28"
                  value={length}
                  onChange={(e) => setLength(e.target.value)}
                />
              )}
            </Field>
          ) : (
            <Field
              label="Custom token"
              hint="Up to 64 letters, digits and . _ ~ -. It appears in the link."
              error={fieldError("token")}
              required
            >
              {(fieldProps) => (
                <Input
                  {...fieldProps}
                  autoComplete="off"
                  autoCapitalize="none"
                  spellCheck={false}
                  maxLength={64}
                  className="font-identifier"
                  value={custom}
                  onChange={(e) => setCustom(e.target.value)}
                />
              )}
            </Field>
          )}

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
          />
          {error?.field === "expiry" && expiry !== "custom" && (
            <p role="alert" className="text-xs text-danger">
              {error.message}
            </p>
          )}

          {error && error.field === null && (
            <p role="alert" className="text-sm text-danger">
              {error.message}
            </p>
          )}

          <div className="mt-1 flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={create.isPending}>
              {create.isPending ? "Creating…" : "Create invite link"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
