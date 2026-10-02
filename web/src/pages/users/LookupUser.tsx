import { useState, type FormEvent } from "react";
import { useNavigate } from "@tanstack/react-router";
import { lookupUser, type UserLookup } from "@/api/users";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Field, Input } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";

type Kind = UserLookup["kind"];

const KINDS = [
  { value: "email", label: "Email address" },
  { value: "msisdn", label: "Phone number" },
  { value: "external", label: "Sign-in provider subject" },
];

/**
 * The exact lookup (`users.lookup`): an administrator holding the email, phone number or
 * single-sign-on subject a person signed up with is taken straight to that account, or told
 * nobody has it. The search above it matches names and IDs loosely and lists many; this asks the
 * server for the one account with exactly this identity.
 */
export function LookupUser() {
  const navigate = useNavigate();
  const [kind, setKind] = useState<Kind>("email");
  const [address, setAddress] = useState("");
  const [provider, setProvider] = useState("");
  const [externalId, setExternalId] = useState("");
  const [pending, setPending] = useState(false);
  const [outcome, setOutcome] = useState<string | null>(null);

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setOutcome(null);
    const lookup: UserLookup =
      kind === "external"
        ? { kind, provider: provider.trim(), externalId: externalId.trim() }
        : { kind, address: address.trim() };
    if (lookup.kind === "external" ? !lookup.provider || !lookup.externalId : !lookup.address) {
      setOutcome(
        kind === "external"
          ? "Give both the provider and the subject it knows the person by."
          : `Give the ${kind === "email" ? "email address" : "phone number"} to look up.`,
      );
      return;
    }
    setPending(true);
    try {
      const user = await lookupUser(lookup);
      if (!user) {
        setOutcome(
          kind === "external"
            ? `No account is linked to ${externalId.trim()} at ${provider.trim()}.`
            : `No account has ${address.trim()} as a verified ${kind === "email" ? "email address" : "phone number"}.`,
        );
        return;
      }
      await navigate({ to: "/users/$userId", params: { userId: user.user_id } });
    } catch (err) {
      const problem = err instanceof ApiProblemError ? err.problem : undefined;
      setOutcome(
        problem?.status === 503 || problem?.status === 501
          ? "This server cannot look accounts up this way yet."
          : (problem?.detail ?? problem?.title ?? "Couldn’t reach the server."),
      );
    } finally {
      setPending(false);
    }
  }

  return (
    <details className="mt-3 max-w-2xl rounded-md border border-border bg-surface">
      <summary className="cursor-pointer px-4 py-2.5 text-sm font-medium text-text">
        Find by email, phone or sign-in provider
      </summary>
      <form onSubmit={handleSubmit} noValidate className="border-t border-border p-4">
        <p className="text-sm text-text-muted">
          The search above matches names and IDs loosely. This asks the server for the one account
          that has exactly this verified email address or phone number, or that a single-sign-on
          provider knows by this subject.
        </p>
        <div className="mt-3 grid grid-cols-1 gap-3 sm:grid-cols-[12rem_1fr]">
          <Field label="What you have">
            {(fieldProps) => (
              <Select
                {...fieldProps}
                options={KINDS}
                value={kind}
                onValueChange={(v) => {
                  setKind(v as Kind);
                  setOutcome(null);
                }}
              />
            )}
          </Field>
          {kind === "external" ? (
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <Field label="Provider" hint="The provider's id in Configuration, Authentication.">
                {(fieldProps) => (
                  <Input
                    {...fieldProps}
                    spellCheck={false}
                    className="font-identifier"
                    value={provider}
                    onChange={(e) => setProvider(e.target.value)}
                  />
                )}
              </Field>
              <Field label="Subject" hint="The id the provider knows the person by.">
                {(fieldProps) => (
                  <Input
                    {...fieldProps}
                    spellCheck={false}
                    className="font-identifier"
                    value={externalId}
                    onChange={(e) => setExternalId(e.target.value)}
                  />
                )}
              </Field>
            </div>
          ) : (
            <Field
              label={kind === "email" ? "Email address" : "Phone number"}
              hint={
                kind === "email"
                  ? "As they verified it, case does not matter."
                  : "International form without the plus sign, as Matrix stores it: 15551234567."
              }
            >
              {(fieldProps) => (
                <Input
                  {...fieldProps}
                  type={kind === "email" ? "email" : "tel"}
                  autoComplete="off"
                  spellCheck={false}
                  value={address}
                  onChange={(e) => setAddress(e.target.value)}
                />
              )}
            </Field>
          )}
        </div>
        <div className="mt-3 flex flex-wrap items-center gap-3">
          <Button type="submit" variant="secondary" disabled={pending}>
            {pending ? "Looking…" : "Find the account"}
          </Button>
          {outcome && (
            <p role="status" className="text-sm text-text-muted">
              {outcome}
            </p>
          )}
        </div>
      </form>
    </details>
  );
}
