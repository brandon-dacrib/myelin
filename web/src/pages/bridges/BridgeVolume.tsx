import { useState, type FormEvent } from "react";
import { useSetBridgeVolumeSize, type BridgeVolume } from "@/api/bridges";
import { ApiProblemError } from "@/api/problem";
import { RelativeTime } from "@/components/RelativeTime";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { toast } from "@/components/ui/toast/toast-store";
import {
  DEFAULT_VOLUME_SIZE,
  MAX_VOLUME_SIZE,
  checkVolumeSize,
  currentVolumeSize,
  describeVolumeUsage,
  suggestedVolumeSize,
  volumeMessage,
  volumeNeedsAttention,
  volumeStateMeta,
  volumeUsedRatio,
} from "@/lib/bridge-volume";
import { cn } from "@/lib/cn";

/**
 * A deployed bridge's volume (OpenAPI 0.1.18, `BridgeVolume`): how full it is, and the control to
 * give it a larger size. The bridge's page and the offering page's dialog show the same pieces.
 * A bridge whose volume fills stops (the demo's Slack bridge, 2026-10-15: its init container
 * could not copy its files, `No space left on device`), so a nearly full or full volume says so
 * in the server's words, and the size control says what growing it does.
 */

/** How much of the volume is used, as a bar coloured by the server's state for it. */
export function VolumeMeter({ volume, className }: { volume: BridgeVolume; className?: string }) {
  const ratio = volumeUsedRatio(volume);
  const usage = describeVolumeUsage(volume);
  if (ratio == null || usage == null) return null;
  const fill = {
    ok: "bg-success",
    nearly_full: "bg-warning",
    full: "bg-danger",
    unknown: "bg-text-faint",
  }[volume.state];
  return (
    <div
      role="meter"
      aria-label="Volume used"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(ratio * 100)}
      aria-valuetext={usage}
      className={cn("h-2 w-full overflow-hidden rounded-full bg-surface-sunken", className)}
    >
      <div className={cn("h-full rounded-full", fill)} style={{ width: `${ratio * 100}%` }} />
    </div>
  );
}

/** The volume's state as a badge; a nearly full or full one carries the server's message. */
export function VolumeStateBadge({ volume }: { volume: Pick<BridgeVolume, "state" | "message"> }) {
  const meta = volumeStateMeta[volume.state];
  return (
    <span title={volumeMessage(volume) ?? undefined}>
      <Badge status={meta.status}>{meta.label}</Badge>
    </span>
  );
}

/**
 * The volume's size, how full it was when last measured and when that was, and for a nearly full
 * or full one, what that means and what to do.
 */
export function VolumeDetails({ volume }: { volume: BridgeVolume }) {
  const usage = describeVolumeUsage(volume);
  const message = volumeNeedsAttention(volume) ? volumeMessage(volume) : null;
  return (
    <div className="flex flex-col gap-3" data-testid="volume-details">
      {message && (
        <p
          role="status"
          className={cn(
            "rounded-md border px-4 py-3 text-sm",
            volume.state === "full"
              ? "border-danger-border bg-danger-bg text-danger"
              : "border-warning-border bg-warning-bg text-warning",
          )}
        >
          <span className="font-medium">{volumeStateMeta[volume.state].label}.</span> {message}
        </p>
      )}
      <dl className="grid grid-cols-1 gap-x-8 gap-y-3 text-sm sm:grid-cols-3">
        <div>
          <dt className="text-xs text-text-muted">Size</dt>
          <dd className="mt-0.5 text-text">
            <span className="font-identifier">{currentVolumeSize(volume)}</span>
            {volume.cluster_size ? (
              <span className="block text-xs text-text-muted">
                Raised by hand in the cluster from the {volume.size} this server asks for; a volume
                never shrinks, so it keeps {volume.cluster_size}.
              </span>
            ) : (
              !volume.size_set && (
                <span className="block text-xs text-text-muted">The default for a new bridge.</span>
              )
            )}
          </dd>
        </div>
        <div>
          <dt className="text-xs text-text-muted">Used</dt>
          <dd className="mt-0.5 flex flex-col gap-1.5 text-text">
            {usage ? (
              <>
                <span className="tabular-nums">{usage}</span>
                <VolumeMeter volume={volume} />
              </>
            ) : (
              <span className="text-text-muted">
                Not measured yet: the operator reads it about once a minute while the bridge runs.
              </span>
            )}
          </dd>
        </div>
        <div>
          <dt className="text-xs text-text-muted">State</dt>
          <dd className="mt-0.5 flex flex-col items-start gap-1 text-text">
            <VolumeStateBadge volume={volume} />
            {volume.observed_at && (
              <span className="text-xs text-text-muted">
                Measured <RelativeTime at={volume.observed_at} />
              </span>
            )}
          </dd>
        </div>
      </dl>
    </div>
  );
}

/**
 * The size control: a field and a button, with what the volume holds and what growing it does
 * said beside them. The server checks the size (a whole `Mi`, `Gi` or `Ti` from 1Gi to 1Ti, no
 * smaller than the volume has) and its refusal is shown under the field.
 */
export function VolumeSizeForm({
  volume,
  type,
  userId,
  appserviceId,
  bridgeName,
  canWrite,
  onDone,
}: {
  volume: BridgeVolume;
  /** The offering's bridge type. */
  type: string;
  /** The instance's owner, or `_` for a shared instance. */
  userId: string;
  appserviceId?: string | null;
  /** What the toast calls the bridge. */
  bridgeName: string;
  canWrite: boolean;
  onDone?: () => void;
}) {
  const setSize = useSetBridgeVolumeSize();
  const [value, setValue] = useState(() => suggestedVolumeSize(volume));
  const [error, setError] = useState<string | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const current = currentVolumeSize(volume);

  async function handleSubmit(event: FormEvent) {
    event.preventDefault();
    setError(null);
    setFormError(null);
    const checked = checkVolumeSize(value);
    if ("error" in checked) {
      setError(checked.error);
      return;
    }
    try {
      await setSize.mutateAsync({ type, userId, size: checked.size, appserviceId });
      toast({
        title: `${bridgeName}'s volume is now ${checked.size}`,
        description: "It grows in place, keeping its data; the bridge restarts to use it.",
      });
      onDone?.();
    } catch (err) {
      if (err instanceof ApiProblemError) {
        const { problem } = err;
        const field = problem.errors?.find((e) => e.pointer.startsWith("/volume_size"));
        const message =
          field?.detail ?? problem.detail ?? problem.title ?? "The server refused the size.";
        if (field || problem.status === 400) setError(message);
        else if (problem.status === 403)
          setFormError(`This needs the ${problem.required_scope ?? "bridges:write"} scope.`);
        else setFormError(message);
        return;
      }
      setFormError("Couldn't reach the server.");
    }
  }

  return (
    <form className="flex flex-col gap-3" onSubmit={handleSubmit} noValidate>
      <p className="max-w-3xl text-sm text-text-muted">
        The volume holds the bridge&apos;s database and configuration: its sign-ins, its rooms and
        everything else it keeps. When it fills, the bridge stops. A volume can only grow: it grows
        in place, keeping everything on it, and the bridge restarts to use it, so it is away for a
        minute. A new bridge gets {DEFAULT_VOLUME_SIZE}.
      </p>
      <div className="flex flex-wrap items-start gap-3">
        <div className="w-48">
          <Field
            label="New size"
            hint={`A whole number of Mi, Gi or Ti, from ${current} up to ${MAX_VOLUME_SIZE}.`}
            error={error ?? undefined}
          >
            {(fieldProps) => (
              <Input
                {...fieldProps}
                className="font-identifier"
                value={value}
                disabled={!canWrite}
                onChange={(e) => {
                  setValue(e.target.value);
                  setError(null);
                }}
              />
            )}
          </Field>
        </div>
        <Button
          type="submit"
          className="mt-6"
          disabled={!canWrite || setSize.isPending}
          title={!canWrite ? "Needs bridges:write" : undefined}
        >
          {setSize.isPending ? "Saving..." : "Set size"}
        </Button>
      </div>
      {formError && (
        <p role="alert" className="text-sm text-danger">
          {formError}
        </p>
      )}
    </form>
  );
}

/** The volume and its size control in a dialog, for a row of the offering page. */
export function VolumeSizeDialog({
  open,
  onOpenChange,
  volume,
  type,
  userId,
  appserviceId,
  bridgeName,
  canWrite,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  volume: BridgeVolume | null;
  type: string;
  userId: string;
  appserviceId?: string | null;
  bridgeName: string;
  canWrite: boolean;
}) {
  return (
    <Dialog open={open && volume !== null} onOpenChange={onOpenChange}>
      {volume && (
        <DialogContent
          size="form"
          title={`${bridgeName}'s volume`}
          description="Where the bridge keeps what it needs, measured about once a minute while it runs."
        >
          <div className="flex flex-col gap-5">
            <VolumeDetails volume={volume} />
            <VolumeSizeForm
              volume={volume}
              type={type}
              userId={userId}
              appserviceId={appserviceId}
              bridgeName={bridgeName}
              canWrite={canWrite}
              onDone={() => onOpenChange(false)}
            />
          </div>
        </DialogContent>
      )}
    </Dialog>
  );
}
