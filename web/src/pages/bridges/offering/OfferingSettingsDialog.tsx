import { useState, type FormEvent } from "react";
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
import { accessIsValid, parseUserList, requestFromOffering } from "@/lib/bridge-offerings";
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
  usersText: string;
  runtime: BridgeOfferingRuntime;
  imageTag: string;
}

function draftOf(offering: BridgeOffering): Draft {
  const r = requestFromOffering(offering);
  return {
    enabled: r.enabled,
    allLocalUsers: r.access.all_local_users ?? true,
    usersText: (r.access.users ?? []).join("\n"),
    runtime: r.runtime,
    imageTag: r.image_tag,
    encryption: r.options.encryption ?? false,
    doublePuppeting: r.options.double_puppeting ?? false,
    backfill: r.options.backfill ?? false,
  };
}

/**
 * Changes an offering: the whole `BridgeOfferingRequest` again (`PUT` replaces it), starting
 * from what the offering is now. A new image tag or options apply to bridges started from now
 * on; a changed runtime does not move the bridges people already have.
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

  function patch(p: Partial<Draft>) {
    setDraft((d) => ({ ...d, ...p }));
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    // The access field already says what is wrong with it.
    if (!accessIsValid(draft.allLocalUsers, draft.usersText)) return;
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
            users: draft.allLocalUsers ? [] : parseUserList(draft.usersText),
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
      description="New settings apply to bridges started from now on. Bridges people already have keep running as they are."
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
            usersText={draft.usersText}
            onChange={patch}
            serverName={serverName}
          />
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
        </section>
        <section aria-labelledby="settings-options">
          <h3 id="settings-options" className="mb-2 text-sm font-medium text-text">
            Options
          </h3>
          <OptionFields values={draft} onChange={patch} type={type} />
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
