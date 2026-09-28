import { useId } from "react";
import {
  useSetExperimentalFeatures,
  useUserAccountData,
  useUserExperimentalFeatures,
  useUserPushers,
} from "@/api/user-identity";
import { Switch } from "@/components/ui/switch/Switch";
import { QueryProblemState } from "@/components/QueryProblemState";
import { MutationError } from "@/components/MutationError";
import { toast } from "@/components/ui/toast/toast-store";

/** What each experimental feature is, in words; the key is Synapse's name for it. */
const FEATURES: Record<string, { title: string; detail: string }> = {
  msc3575: {
    title: "Sliding sync (MSC3575)",
    detail: "The proxy-era sliding sync API, for clients that still ask for it.",
  },
  msc3881: {
    title: "Remote push toggles (MSC3881)",
    detail: "Lets one of their devices turn push notifications on or off for another.",
  },
  msc4222: {
    title: "state_after in sync (MSC4222)",
    detail: "Sends room state as it is after the timeline, rather than before it.",
  },
};

/**
 * What their clients stored on the server (read-only: account data and pushers are the
 * clients' to change) and the experimental features switched on for them.
 */
export function UserClientDataSection({ userId, canWrite }: { userId: string; canWrite: boolean }) {
  return (
    <>
      <ExperimentalFeaturesPanel userId={userId} canWrite={canWrite} />
      <PushersPanel userId={userId} />
      <AccountDataPanel userId={userId} />
    </>
  );
}

function FeatureRow({
  name,
  enabled,
  disabled,
  onChange,
}: {
  name: string;
  enabled: boolean;
  disabled: boolean;
  onChange: (on: boolean) => void;
}) {
  const labelId = useId();
  const hintId = useId();
  const about = FEATURES[name] ?? { title: name, detail: "" };
  return (
    <li className="flex items-start justify-between gap-4 px-4 py-3">
      <div>
        <p id={labelId} className="text-sm font-medium text-text">
          {about.title}
        </p>
        {about.detail && (
          <p id={hintId} className="text-sm text-text-muted">
            {about.detail}
          </p>
        )}
      </div>
      <Switch
        checked={enabled}
        disabled={disabled}
        onCheckedChange={onChange}
        aria-labelledby={labelId}
        aria-describedby={about.detail ? hintId : undefined}
      />
    </li>
  );
}

function ExperimentalFeaturesPanel({ userId, canWrite }: { userId: string; canWrite: boolean }) {
  const { data, isError, error, refetch } = useUserExperimentalFeatures(userId);
  const set = useSetExperimentalFeatures();
  const names = Object.keys(data ?? {}).sort();

  return (
    <section aria-labelledby="features-heading">
      <h2 id="features-heading" className="mt-8 text-md font-medium text-text">
        Experimental features
      </h2>
      <p className="mt-1 text-sm text-text-muted">
        Unstable behaviour switched on for this person only. Saved as soon as you switch it.
      </p>
      {isError ? (
        <QueryProblemState
          error={error}
          resource="this user's experimental features"
          onRetry={() => refetch()}
        />
      ) : (
        <ul className="mt-3 divide-y divide-border rounded-md border border-border">
          {names.map((name) => (
            <FeatureRow
              key={name}
              name={name}
              enabled={Boolean(data?.[name])}
              disabled={!canWrite || set.isPending}
              onChange={(on) =>
                set.mutate(
                  { userId, features: { [name]: on } },
                  {
                    onSuccess: () =>
                      toast({
                        title: `${FEATURES[name]?.title ?? name} ${on ? "on" : "off"}`,
                      }),
                  },
                )
              }
            />
          ))}
        </ul>
      )}
      {set.isError && (
        <MutationError error={set.error} action="change the feature" className="mt-3" />
      )}
    </section>
  );
}

function PushersPanel({ userId }: { userId: string }) {
  const { data, isError, error, refetch } = useUserPushers(userId);
  const items = data?.items ?? [];
  return (
    <section aria-labelledby="pushers-heading">
      <h2 id="pushers-heading" className="mt-8 text-md font-medium text-text">
        Push notifications
      </h2>
      {isError ? (
        <QueryProblemState error={error} resource="this user's pushers" onRetry={() => refetch()} />
      ) : items.length === 0 ? (
        <p className="mt-3 text-sm text-text-muted">No device has asked for push notifications.</p>
      ) : (
        <ul className="mt-3 divide-y divide-border rounded-md border border-border">
          {items.map((p) => (
            <li key={`${p.app_id}:${p.pushkey}`} className="px-4 py-3">
              <p className="text-sm text-text">
                {p.device_display_name ?? p.pushkey}
                <span className="text-text-muted"> · {p.app_display_name ?? p.app_id}</span>
              </p>
              <p className="text-xs text-text-muted">
                {p.kind === "email" ? "Email" : "HTTP"}
                {p.data?.url && (
                  <>
                    {" to "}
                    <span className="font-identifier">{p.data.url}</span>
                  </>
                )}
              </p>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function AccountDataPanel({ userId }: { userId: string }) {
  const { data, isError, error, refetch } = useUserAccountData(userId);
  const types = Object.keys(data ?? {}).sort();
  return (
    <section aria-labelledby="account-data-heading">
      <h2 id="account-data-heading" className="mt-8 text-md font-medium text-text">
        Account data
      </h2>
      <p className="mt-1 text-sm text-text-muted">
        Settings their clients keep on the server. Shown as stored; only their clients change it.
      </p>
      {isError ? (
        <QueryProblemState
          error={error}
          resource="this user's account data"
          onRetry={() => refetch()}
        />
      ) : types.length === 0 ? (
        <p className="mt-3 text-sm text-text-muted">None.</p>
      ) : (
        <ul className="mt-3 divide-y divide-border rounded-md border border-border">
          {types.map((type) => (
            <li key={type} className="px-4 py-2">
              <details>
                <summary className="cursor-pointer font-identifier text-sm text-text">
                  {type}
                </summary>
                <pre className="mt-2 max-h-64 overflow-auto rounded-sm bg-surface-sunken p-3 text-xs text-text">
                  {JSON.stringify(data?.[type], null, 2)}
                </pre>
              </details>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
