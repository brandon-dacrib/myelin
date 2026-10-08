import { useState, type FormEvent, type ReactNode } from "react";
import {
  useBridgeDeploymentTarget,
  usePutBridgeOffering,
  type BridgeOffering,
  type BridgeOfferingRuntime,
  type BridgeType,
} from "@/api/bridges";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { toast } from "@/components/ui/toast/toast-store";
import { accessIsValid, requestFromOffering, settingsEffects } from "@/lib/bridge-offerings";
import {
  AccessFields,
  OptionFields,
  RuntimeFields,
  SettingSwitch,
  type OptionValues,
} from "../offering-fields";
import { clusterAvailability, effectiveRuntime } from "../wizard/offer-state";

interface Draft extends OptionValues {
  enabled: boolean;
  allLocalUsers: boolean;
  users: string[];
  runtime: BridgeOfferingRuntime;
  imageTag: string;
}

function draftOf(offering: BridgeOffering): Draft {
  const r = requestFromOffering(offering);
  return {
    enabled: r.enabled,
    allLocalUsers: r.access.all_local_users ?? true,
    users: r.access.users ?? [],
    runtime: r.runtime,
    imageTag: r.image_tag,
    encryption: r.options.encryption ?? false,
    doublePuppeting: r.options.double_puppeting ?? false,
    backfill: r.options.backfill ?? false,
  };
}

/**
 * Changes an offering: the whole `BridgeOfferingRequest` again (`PUT` replaces it), starting
 * from what the offering is now. A new image tag or options reach the bridges people already
 * have: the server re-renders each one's files and registration, and restarts the ones it runs
 * once (a changed double puppeting is the registration's claim to act as its owner); a changed
 * runtime does not move them.
 */
export function OfferingSettingsDialog({
  offering,
  type,
  open,
  onOpenChange,
  serverName,
}: {
  offering: BridgeOffering;
  type: BridgeType | undefined;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  serverName?: string;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      {open && (
        <SettingsForm
          offering={offering}
          type={type}
          onDone={() => onOpenChange(false)}
          serverName={serverName}
        />
      )}
    </Dialog>
  );
}

function SettingsForm({
  offering,
  type,
  onDone,
  serverName,
}: {
  offering: BridgeOffering;
  type: BridgeType | undefined;
  onDone: () => void;
  serverName?: string;
}) {
  const put = usePutBridgeOffering();
  const { data: target } = useBridgeDeploymentTarget();
  // Seeded once, when the dialog opens: a refetch behind it must not overwrite an edit.
  const [draft, setDraft] = useState<Draft>(() => draftOf(offering));
  const [error, setError] = useState<string | null>(null);
  const cluster = clusterAvailability(target, type);
  // An offering already running in the cluster stays choosable while the target loads.
  const clusterAvailable = cluster.available || (!target && offering.runtime === "cluster");
  const name = offering.name ?? offering.type;
  const existing = Object.values(offering.instances ?? {}).reduce((a, b) => a + b, 0);
  const effects = settingsEffects(
    offering.runtime,
    effectiveRuntime(draft, clusterAvailable),
    existing,
  );

  function patch(p: Partial<Draft>) {
    setDraft((d) => ({ ...d, ...p }));
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    // The access field already says what is wrong with it.
    if (!accessIsValid(draft.allLocalUsers, draft.users)) return;
    setError(null);
    try {
      await put.mutateAsync({
        type: offering.type,
        body: {
          enabled: draft.enabled,
          runtime: effectiveRuntime(draft, clusterAvailable),
          image_tag: draft.imageTag.trim() || "latest",
          access: {
            all_local_users: draft.allLocalUsers,
            users: draft.allLocalUsers ? [] : draft.users,
          },
          options: {
            encryption: draft.encryption,
            double_puppeting: draft.doublePuppeting,
            backfill: draft.backfill,
          },
        },
      });
      toast({ title: `${name} settings saved` });
      onDone();
    } catch (err) {
      setError(
        err instanceof ApiProblemError
          ? (err.problem.detail ?? err.problem.title ?? "The server refused.")
          : "Couldn’t reach the server.",
      );
    }
  }

  return (
    <DialogContent
      size="form"
      title={`${name} settings`}
      description="Each section says what saving it does to bridges created after saving and to the ones people already have."
    >
      <form onSubmit={handleSubmit} noValidate className="flex flex-col gap-6">
        <SettingSwitch
          label="Offered"
          description="Turned off, its bot turns new people away. Bridges people already have keep running."
          checked={draft.enabled}
          onChange={(enabled) => patch({ enabled })}
        />
        <section aria-labelledby="settings-access">
          <h3 id="settings-access" className="mb-2 text-sm font-medium text-text">
            Who can have one
          </h3>
          <AccessFields
            allLocalUsers={draft.allLocalUsers}
            users={draft.users}
            onChange={patch}
            serverName={serverName}
          />
          <AppliesNote testId="applies-access">{effects.access}</AppliesNote>
        </section>
        <section aria-labelledby="settings-runtime">
          <h3 id="settings-runtime" className="mb-2 text-sm font-medium text-text">
            Runtime
          </h3>
          <RuntimeFields
            runtime={draft.runtime}
            onRuntime={(runtime) => patch({ runtime })}
            clusterAvailable={clusterAvailable}
            unavailableBecause={clusterAvailable ? null : cluster.reason}
            target={target}
            type={type}
            imageTag={draft.imageTag}
            onImageTag={(imageTag) => patch({ imageTag })}
          />
          <AppliesNote testId="applies-image">Image tag: {effects.imageAndOptions}</AppliesNote>
          {effects.runtimeChange && (
            <p
              role="status"
              className="mt-2 rounded-md border border-warning-border bg-warning-bg p-2 text-sm text-text"
              data-testid="runtime-change"
            >
              {effects.runtimeChange}
            </p>
          )}
        </section>
        <section aria-labelledby="settings-options">
          <h3 id="settings-options" className="mb-2 text-sm font-medium text-text">
            Options
          </h3>
          <OptionFields values={draft} onChange={patch} type={type} />
          <AppliesNote testId="applies-options">{effects.imageAndOptions}</AppliesNote>
          <DoublePuppetingChange
            from={offering.options?.double_puppeting ?? false}
            to={draft.doublePuppeting}
            existing={Object.values(offering.instances ?? {}).reduce((a, b) => a + b, 0)}
            runtime={offering.runtime}
          />
        </section>
        {error && (
          <p role="alert" className="text-sm text-danger">
            {error}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="secondary" onClick={onDone}>
            Cancel
          </Button>
          <Button type="submit" disabled={put.isPending}>
            {put.isPending ? "Saving..." : "Save settings"}
          </Button>
        </div>
      </form>
    </DialogContent>
  );
}

/** One "what saving this does" line under a section of the dialog. */
function AppliesNote({ children, testId }: { children: ReactNode; testId: string }) {
  return (
    <p className="mt-2 text-xs text-text-muted" data-testid={testId}>
      {children}
    </p>
  );
}

/**
 * What saving a changed double puppeting does to the bridges people already have: each one's
 * registration on this server gains or loses its claim to act as its owner, and its config the
 * matching secret, so a bridge the server runs is restarted once with them; one run elsewhere
 * needs its files downloaded again. Nothing when it is unchanged or nobody has a bridge yet.
 */
function DoublePuppetingChange({
  from,
  to,
  existing,
  runtime,
}: {
  from: boolean;
  to: boolean;
  existing: number;
  runtime: BridgeOfferingRuntime;
}) {
  if (from === to || existing === 0) return null;
  const who = existing === 1 ? "the 1 bridge people have" : `the ${existing} bridges people have`;
  return (
    <p className="mt-3 text-sm text-text-muted" data-testid="double-puppeting-change">
      {to
        ? `Saving re-registers ${who} so each may act as its owner (their messages from other apps appear as them, not as a ghost).`
        : `Saving re-registers ${who} without the claim to act as their owners: messages they send from other apps appear as ghost users.`}{" "}
      {runtime === "cluster"
        ? "Each restarts once with its new config."
        : "They run elsewhere: download each one's files again and restart it with them; its row says so until then."}
    </p>
  );
}
