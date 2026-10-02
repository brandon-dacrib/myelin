import { useState, type FormEvent } from "react";
import { Check, Copy } from "lucide-react";
import { useCreateAdminToken, type MintedAdminToken } from "@/api/admin-tokens";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { CopyableId } from "@/components/CopyableId";
import type { Scope } from "@/lib/auth";
import { expiryFor, formatExpiry, type ExpiryChoice } from "@/lib/registration-tokens";
import { SCOPE_DESCRIPTIONS, describeScopes, normalizeScopes, redundantScopes } from "@/lib/scopes";
import { ExpiryControl } from "./token-controls";

type FieldName = "name" | "scopes" | "expiry";

const FIELD_FOR_POINTER: Record<string, FieldName> = {
  "/name": "name",
  "/scopes": "scopes",
  "/expires_at": "expiry",
};

/** What a token starts with: a full administrator's, so nothing is narrower by accident. */
const DEFAULT_SCOPES: Scope[] = ["admin:read", "admin:write"];

/**
 * Mints an admin token (`POST /admin-tokens`) and shows it once.
 *
 * The scope picker is the point of the dialog: each scope is a checkbox with a sentence saying
 * what it lets the holder do, so an administrator handing a token to a bridge team or a
 * moderation bot can give it exactly that and nothing more. It starts as a full administrator's
 * token, which is what a token minted without thinking about scopes should be. It ends on the
 * token rather than closing, because the token is shown this once: the server keeps only its
 * hash.
 */
export function CreateAdminTokenDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const create = useCreateAdminToken();
  const [name, setName] = useState("");
  const [scopes, setScopes] = useState<Scope[]>(DEFAULT_SCOPES);
  const [expiry, setExpiry] = useState<ExpiryChoice>("never");
  const [expiryAt, setExpiryAt] = useState("");
  const [error, setError] = useState<{ message: string; field: FieldName | null } | null>(null);
  const [created, setCreated] = useState<MintedAdminToken | null>(null);
  const [copied, setCopied] = useState(false);

  function reset() {
    setName("");
    setScopes(DEFAULT_SCOPES);
    setExpiry("never");
    setExpiryAt("");
    setError(null);
    setCreated(null);
    setCopied(false);
  }

  function handleOpenChange(next: boolean) {
    if (!next) reset();
    onOpenChange(next);
  }

  function toggle(scope: Scope, checked: boolean) {
    setError(null);
    setScopes((current) =>
      normalizeScopes(checked ? [...current, scope] : current.filter((s) => s !== scope)),
    );
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    const trimmed = name.trim();
    if (!trimmed) {
      setError({ message: "Say what the token is for.", field: "name" });
      return;
    }
    if (scopes.length === 0) {
      setError({ message: "Choose at least one scope.", field: "scopes" });
      return;
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
      setCreated(await create.mutateAsync({ name: trimmed, scopes, expiresAt }));
    } catch (err) {
      if (err instanceof ApiProblemError) {
        const { problem } = err;
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

  async function copyToken() {
    if (!created) return;
    try {
      await navigator.clipboard.writeText(created.token);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable: the token stays visible and selectable */
    }
  }

  const fieldError = (field: FieldName) => (error?.field === field ? error.message : undefined);
  const redundant = redundantScopes(scopes);

  if (created) {
    return (
      <Dialog open={open} onOpenChange={handleOpenChange}>
        <DialogContent
          size="form"
          title="Admin token ready"
          description="Copy it now. This is the only time it is shown: the server keeps a hash, not the token. If it is lost, revoke it and mint another."
          footer={
            <>
              <Button variant="secondary" onClick={reset}>
                Mint another
              </Button>
              <Button onClick={() => handleOpenChange(false)}>Done</Button>
            </>
          }
        >
          <div className="rounded-md border border-accent bg-accent-muted p-4">
            <p className="text-sm font-medium text-text">Token</p>
            <p className="mt-1 break-all font-identifier text-base text-text" data-testid="token">
              {created.token}
            </p>
            <Button
              className="mt-3"
              size="sm"
              onClick={copyToken}
              leadingIcon={
                copied ? (
                  <Check size={14} aria-hidden="true" />
                ) : (
                  <Copy size={14} aria-hidden="true" />
                )
              }
            >
              {copied ? "Copied" : "Copy token"}
            </Button>
          </div>
          <dl className="mt-4 grid grid-cols-[auto_1fr] items-center gap-x-4 gap-y-2 text-sm">
            <dt className="text-text-muted">Name</dt>
            <dd className="text-text">{created.name}</dd>
            <dt className="text-text-muted">Scopes</dt>
            <dd className="text-text">{describeScopes(created.scopes)}</dd>
            <dt className="text-text-muted">Expires</dt>
            <dd className="text-text">{formatExpiry(created.expiresAt)}</dd>
            <dt className="text-text-muted">Id</dt>
            <dd>
              <CopyableId value={created.id} label="token id" />
            </dd>
          </dl>
          <p className="mt-4 text-sm text-text-muted">
            Use it as a bearer token:{" "}
            <code className="font-identifier text-text">Authorization: Bearer {created.token}</code>
          </p>
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        size="form"
        title="Mint an admin token"
        description="A token for a script, a bot or a team that needs part of the admin API. It carries only the scopes you choose: a request outside them is refused and the refusal names the scope."
      >
        <form className="flex flex-col gap-5" onSubmit={handleSubmit} noValidate>
          <Field
            label="Name"
            hint="What it is for, so it can be told from the others in the list and the audit log."
            error={fieldError("name")}
            required
          >
            {(fieldProps) => (
              <Input
                {...fieldProps}
                autoComplete="off"
                maxLength={100}
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="Bridge team dashboard"
              />
            )}
          </Field>

          <fieldset className="flex flex-col gap-2">
            <legend className="text-sm font-medium text-text">Scopes</legend>
            <p className="text-xs text-text-muted">
              Each scope opens one part of the interface and the API behind it. A write scope
              includes its read scope; admin:write includes everything.
            </p>
            <ul className="mt-1 flex flex-col gap-2">
              {SCOPE_DESCRIPTIONS.map((d) => {
                const id = `scope-${d.scope.replace(":", "-")}`;
                const checked = scopes.includes(d.scope);
                return (
                  <li key={d.scope} className="flex items-start gap-3">
                    <input
                      id={id}
                      type="checkbox"
                      className="mt-1 h-4 w-4 shrink-0 accent-[var(--color-accent)]"
                      checked={checked}
                      onChange={(e) => toggle(d.scope, e.target.checked)}
                      aria-describedby={`${id}-grants`}
                    />
                    <label htmlFor={id} className="flex flex-col gap-0.5">
                      <span className="text-sm text-text">
                        <span className="font-identifier">{d.scope}</span>
                        <span className="text-text-muted"> · {d.area}</span>
                        {redundant.includes(d.scope) && (
                          <span className="text-text-faint"> · already included</span>
                        )}
                      </span>
                      <span id={`${id}-grants`} className="text-xs text-text-muted">
                        {d.grants}
                      </span>
                    </label>
                  </li>
                );
              })}
            </ul>
            <p className="text-xs text-text-muted" aria-live="polite">
              {scopes.length === 0
                ? "No scopes: the token could do nothing."
                : scopes.includes("admin:write")
                  ? "A full administrator’s token: it can do everything, including mint more tokens."
                  : `The token will hold ${scopes.join(", ")}.`}
            </p>
            {fieldError("scopes") && (
              <p role="alert" className="text-xs text-danger">
                {fieldError("scopes")}
              </p>
            )}
          </fieldset>

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
              {create.isPending ? "Minting…" : "Mint token"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
