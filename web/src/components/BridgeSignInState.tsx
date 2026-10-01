import { useState, type FormEvent } from "react";
import { useAppserviceLogins, type BridgeLogin } from "@/api/bridges";
import { ApiProblemError } from "@/api/problem";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Field, Input } from "@/components/ui/input/Input";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";

/** The bridge's state word (`bad_credentials`) as words (`bad credentials`). */
function stateWords(state: string): string {
  return state.replaceAll("_", " ");
}

function Since({ at }: { at: string | null | undefined }) {
  if (!at) return null;
  const date = new Date(at);
  return (
    <>
      {" since "}
      <time dateTime={at} title={date.toISOString()} className="tabular-nums">
        {date.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" })}
      </time>
    </>
  );
}

function LoginLine({ login }: { login: BridgeLogin }) {
  const connected = login.state === "connected";
  return (
    <li className="flex flex-wrap items-center gap-2 text-sm text-text">
      <span>
        Signed in as <span className="font-identifier">{login.remote_name ?? login.remote_id}</span>
        <Since at={login.since} />
      </span>
      {!connected && <Badge status="warning">{stateWords(login.state)}</Badge>}
      {!connected && login.state_reason && (
        <span className="text-xs text-text-muted">{login.state_reason}</span>
      )}
    </li>
  );
}

/**
 * Who has signed in to this bridge, and as what, as the bridge itself says through its
 * provisioning API (`GET /appservices/{id}/logins`; a mautrix bridge's `whoami`). A bridge
 * type with no such API keeps that state itself, and this says so rather than guessing. A
 * per-user instance is asked about its owner; a shared bridge needs to be told whom to ask
 * about, so this asks the operator for a Matrix ID (their own, to start with).
 */
export function BridgeSignInState({
  appserviceId,
  defaultUserId,
}: {
  appserviceId: string;
  /** What the "whose sign-in" box starts with: the operator's own Matrix ID, usually. */
  defaultUserId?: string;
}) {
  const [userId, setUserId] = useState<string | undefined>(undefined);
  const [draft, setDraft] = useState(defaultUserId ?? "");
  const { data, error, isLoading, isFetching } = useAppserviceLogins(appserviceId, userId);

  const problem = error instanceof ApiProblemError ? error.problem : undefined;
  const userError = problem?.errors?.find((e) => e.pointer === "/user_id");
  const askForUser = Boolean(userError) || Boolean(data?.supported && userId);

  const check = (event: FormEvent) => {
    event.preventDefault();
    const value = draft.trim();
    if (value) setUserId(value);
  };

  let body;
  if (isLoading) {
    body = <SkeletonText lines={1} />;
  } else if (data && !data.supported) {
    body = (
      <>
        <p className="text-sm text-text">This bridge keeps who has signed in itself.</p>
        {data.reason && <p className="mt-1 text-sm text-text-muted">{data.reason}</p>}
      </>
    );
  } else if (data?.error) {
    body = (
      <p
        role="alert"
        className="rounded-md border border-warning-border bg-warning-bg px-3 py-2 text-sm text-warning"
      >
        Could not ask the bridge: {data.error.detail}
      </p>
    );
  } else if (data && data.signed_in) {
    body = (
      <ul className="flex flex-col gap-2" aria-label={`Sign-ins of ${data.user_id ?? "the user"}`}>
        {data.logins.map((login) => (
          <LoginLine key={`${login.user_id}/${login.remote_id}`} login={login} />
        ))}
      </ul>
    );
  } else if (data) {
    body = (
      <p className="text-sm text-text">
        <span className="font-identifier">{data.user_id}</span> is not signed in.
      </p>
    );
  } else if (userError) {
    body = (
      <p className="text-sm text-text-muted">
        Many people can use this bridge. Name one to ask it whether they have signed in.
      </p>
    );
  } else {
    body = (
      <p className="text-sm text-text-muted">
        Could not ask who has signed in: {problem?.detail ?? problem?.title ?? "the request failed"}
        .
      </p>
    );
  }

  return (
    <section aria-labelledby="bridge-sign-in-state-heading" className="mb-6">
      <h2 id="bridge-sign-in-state-heading" className="text-md font-medium text-text">
        Who has signed in
      </h2>
      <div className="mt-2" aria-live="polite" aria-busy={isFetching}>
        {body}
      </div>
      {askForUser && (
        <form onSubmit={check} className="mt-3 flex max-w-xl items-end gap-2">
          <div className="flex-1">
            <Field label="Matrix user" error={userId ? undefined : userError?.detail}>
              {(props) => (
                <Input
                  {...props}
                  value={draft}
                  placeholder="@someone:example.org"
                  onChange={(e) => setDraft(e.target.value)}
                />
              )}
            </Field>
          </div>
          <Button type="submit" variant="secondary" disabled={!draft.trim()}>
            Check
          </Button>
        </form>
      )}
      {data?.checked_at && (
        <p className="mt-2 text-xs text-text-faint">
          The bridge's answer from{" "}
          <time dateTime={data.checked_at}>{new Date(data.checked_at).toLocaleTimeString()}</time>
          {data.cached ? ", kept for up to 30 seconds" : ""}.
        </p>
      )}
    </section>
  );
}
