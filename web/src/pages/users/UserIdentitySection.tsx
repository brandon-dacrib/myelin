import { useState, type FormEvent } from "react";
import {
  useAddExternalId,
  useAddThreepid,
  useRemoveExternalId,
  useRemoveThreepid,
  useUserExternalIds,
  useUserThreepids,
} from "@/api/user-identity";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogTrigger, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { QueryProblemState } from "@/components/QueryProblemState";
import { MutationError } from "@/components/MutationError";
import { RelativeTime } from "@/components/RelativeTime";
import { toast } from "@/components/ui/toast/toast-store";

const MEDIUM_LABEL: Record<string, string> = { email: "Email", msisdn: "Phone" };

/** The server's reason for refusing a field, if it named one. */
function fieldError(error: unknown, pointer: string): string | undefined {
  if (!(error instanceof ApiProblemError)) return undefined;
  const match = error.problem.errors?.find((e) => e.pointer === pointer);
  return match?.detail;
}

/**
 * How this account is identified besides its Matrix ID: the email addresses and phone numbers
 * bound to it (each signs them in and finds them in a lookup), and the accounts at upstream
 * identity providers (OIDC, SAML, LDAP) linked to it.
 */
export function UserIdentitySection({ userId, canWrite }: { userId: string; canWrite: boolean }) {
  return (
    <>
      <ThreepidsPanel userId={userId} canWrite={canWrite} />
      <ExternalIdsPanel userId={userId} canWrite={canWrite} />
    </>
  );
}

function RemoveButton({
  label,
  title,
  description,
  onConfirm,
  disabled,
}: {
  label: string;
  title: string;
  description: string;
  onConfirm: () => void;
  disabled: boolean;
}) {
  return (
    <Dialog>
      <DialogTrigger asChild>
        <Button variant="ghost" size="sm" disabled={disabled} aria-label={label}>
          Remove
        </Button>
      </DialogTrigger>
      <DialogContent
        title={title}
        description={description}
        footer={
          <>
            <DialogClose asChild>
              <Button variant="secondary">Cancel</Button>
            </DialogClose>
            <DialogClose asChild>
              <Button variant="danger" onClick={onConfirm}>
                Remove
              </Button>
            </DialogClose>
          </>
        }
      />
    </Dialog>
  );
}

function ThreepidsPanel({ userId, canWrite }: { userId: string; canWrite: boolean }) {
  const { data, isError, error, refetch } = useUserThreepids(userId);
  const add = useAddThreepid();
  const remove = useRemoveThreepid();
  const [medium, setMedium] = useState("email");
  const [address, setAddress] = useState("");

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    add.mutate(
      { userId, threepid: { medium: medium as "email" | "msisdn", address } },
      {
        onSuccess: (added) => {
          setAddress("");
          toast({ title: `${MEDIUM_LABEL[added.medium] ?? added.medium} added` });
        },
      },
    );
  }

  const addressError = fieldError(add.error, "/address") ?? fieldError(add.error, "/medium");

  return (
    <section aria-labelledby="threepids-heading">
      <h2 id="threepids-heading" className="mt-8 text-md font-medium text-text">
        Email and phone
      </h2>
      <p className="mt-1 text-sm text-text-muted">
        They can sign in with any of these instead of their user ID.
      </p>
      {isError ? (
        <QueryProblemState
          error={error}
          resource="this user's email addresses"
          onRetry={() => refetch()}
        />
      ) : (data?.length ?? 0) === 0 ? (
        <p className="mt-3 text-sm text-text-muted">None.</p>
      ) : (
        <ul className="mt-3 divide-y divide-border rounded-md border border-border">
          {data?.map((t) => (
            <li
              key={`${t.medium}:${t.address}`}
              className="flex items-center justify-between gap-3 px-4 py-3"
            >
              <div>
                <p className="text-text">{t.address}</p>
                <p className="text-xs text-text-muted">
                  {MEDIUM_LABEL[t.medium] ?? t.medium}
                  {t.added_at && (
                    <>
                      {" · added "}
                      <RelativeTime at={t.added_at} />
                    </>
                  )}
                </p>
              </div>
              <RemoveButton
                label={`Remove ${t.address}`}
                title={`Remove ${t.address}?`}
                description="They can no longer sign in with it, and it is free for another account."
                disabled={!canWrite}
                onConfirm={() =>
                  remove.mutate(
                    { userId, medium: t.medium, address: t.address },
                    { onSuccess: () => toast({ title: "Removed" }) },
                  )
                }
              />
            </li>
          ))}
        </ul>
      )}
      {remove.isError && <MutationError error={remove.error} action="remove it" className="mt-3" />}
      {canWrite && (
        <form
          onSubmit={handleSubmit}
          className="mt-3 grid grid-cols-1 items-end gap-2 sm:grid-cols-[8rem_1fr_auto]"
          noValidate
        >
          <Field label="Kind">
            {(fieldProps) => (
              <Select
                id={fieldProps.id}
                aria-label="Kind"
                value={medium}
                onValueChange={setMedium}
                options={[
                  { value: "email", label: "Email" },
                  { value: "msisdn", label: "Phone" },
                ]}
              />
            )}
          </Field>
          <Field label={medium === "email" ? "Email address" : "Phone number"} error={addressError}>
            {(fieldProps) => (
              <Input
                {...fieldProps}
                type={medium === "email" ? "email" : "tel"}
                value={address}
                placeholder={medium === "email" ? "name@example.org" : "+44 7700 900123"}
                onChange={(e) => setAddress(e.target.value)}
              />
            )}
          </Field>
          <Button type="submit" variant="secondary" disabled={add.isPending || !address.trim()}>
            Add
          </Button>
        </form>
      )}
      {add.isError && !addressError && (
        <MutationError error={add.error} action="add it" className="mt-3" />
      )}
    </section>
  );
}

function ExternalIdsPanel({ userId, canWrite }: { userId: string; canWrite: boolean }) {
  const { data, isError, error, refetch } = useUserExternalIds(userId);
  const add = useAddExternalId();
  const remove = useRemoveExternalId();
  const [provider, setProvider] = useState("");
  const [subject, setSubject] = useState("");

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    add.mutate(
      { userId, externalId: { provider: provider.trim(), external_id: subject.trim() } },
      {
        onSuccess: () => {
          setSubject("");
          toast({ title: "Identity linked" });
        },
      },
    );
  }

  const providerError = fieldError(add.error, "/provider");
  const subjectError = fieldError(add.error, "/external_id");

  return (
    <section aria-labelledby="external-ids-heading">
      <h2 id="external-ids-heading" className="mt-8 text-md font-medium text-text">
        Linked identities
      </h2>
      <p className="mt-1 text-sm text-text-muted">
        Their accounts at upstream identity providers (OIDC, SAML, LDAP). Each one belongs to one
        account on this server.
      </p>
      {isError ? (
        <QueryProblemState
          error={error}
          resource="this user's linked identities"
          onRetry={() => refetch()}
        />
      ) : (data?.length ?? 0) === 0 ? (
        <p className="mt-3 text-sm text-text-muted">None.</p>
      ) : (
        <ul className="mt-3 divide-y divide-border rounded-md border border-border">
          {data?.map((x) => (
            <li
              key={`${x.provider}:${x.external_id}`}
              className="flex items-center justify-between gap-3 px-4 py-3"
            >
              <div>
                <p className="font-identifier text-text">{x.external_id}</p>
                <p className="text-xs text-text-muted">{x.provider}</p>
              </div>
              <RemoveButton
                label={`Unlink ${x.external_id} at ${x.provider}`}
                title={`Unlink ${x.external_id}?`}
                description={`That account at ${x.provider} no longer leads here.`}
                disabled={!canWrite}
                onConfirm={() =>
                  remove.mutate(
                    { userId, provider: x.provider, externalId: x.external_id },
                    { onSuccess: () => toast({ title: "Unlinked" }) },
                  )
                }
              />
            </li>
          ))}
        </ul>
      )}
      {remove.isError && <MutationError error={remove.error} action="unlink it" className="mt-3" />}
      {canWrite && (
        <form
          onSubmit={handleSubmit}
          className="mt-3 grid grid-cols-1 items-end gap-2 sm:grid-cols-[12rem_1fr_auto]"
          noValidate
        >
          <Field label="Provider" error={providerError}>
            {(fieldProps) => (
              <Input
                {...fieldProps}
                value={provider}
                placeholder="oidc-google"
                onChange={(e) => setProvider(e.target.value)}
              />
            )}
          </Field>
          <Field label="Subject at the provider" error={subjectError}>
            {(fieldProps) => (
              <Input
                {...fieldProps}
                className="font-identifier"
                value={subject}
                spellCheck={false}
                onChange={(e) => setSubject(e.target.value)}
              />
            )}
          </Field>
          <Button
            type="submit"
            variant="secondary"
            disabled={add.isPending || !provider.trim() || !subject.trim()}
          >
            Link
          </Button>
        </form>
      )}
      {add.isError && !providerError && !subjectError && (
        <MutationError error={add.error} action="link it" className="mt-3" />
      )}
    </section>
  );
}
