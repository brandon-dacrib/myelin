import type { UseMutationResult } from "@tanstack/react-query";
import type { BridgeInstance, BridgeInstanceFiles, BridgeOffering } from "@/api/bridges";
import { classifyError } from "@/api/problem";
import { CopyBlock } from "@/components/CopyBlock";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { ErrorState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { instanceOwnerLabel } from "@/lib/bridge-offerings";

/**
 * An instance's files (`POST .../files`), to run it somewhere other than this cluster: how an
 * administrator runs someone's iMessage bridge on their Mac. They carry the instance's own
 * tokens, so they are fetched when asked for and dropped when the dialog closes.
 */
export function InstanceFilesDialog({
  offering,
  instance,
  files,
  onOpenChange,
  onRetry,
}: {
  offering: BridgeOffering;
  instance: BridgeInstance | null;
  files: UseMutationResult<BridgeInstanceFiles, Error, { type: string; userId: string }, unknown>;
  onOpenChange: (open: boolean) => void;
  onRetry: () => void;
}) {
  const owner = instance ? instanceOwnerLabel(instance) : "";
  const name = offering.name ?? offering.type;
  const id = instance?.appservice_id ?? offering.type;
  const elsewhere = offering.runtime === "elsewhere";
  const data = files.data;

  return (
    <Dialog open={instance !== null} onOpenChange={onOpenChange}>
      <DialogContent
        size="form"
        title={
          instance?.user_id ? `Files for ${owner}'s ${name} bridge` : `Files for the ${name} bridge`
        }
        description={
          elsewhere
            ? `Run it on a machine that can${offering.type === "mautrix-imessage" ? " (their Mac, signed in to Messages)" : ""}: put config.yaml and registration.yaml in ./${id}/ beside the Compose file and start it. It turns ready here when it answers this server.`
            : "This server already runs it. These are the same files, to run it somewhere else instead or to see what it runs with."
        }
        footer={
          <DialogClose asChild>
            <Button variant="secondary">Done</Button>
          </DialogClose>
        }
      >
        {files.isPending && <SkeletonText lines={6} />}
        {files.isError && (
          <ErrorState
            title="Couldn't render the files"
            problem={{
              detail:
                classifyError(files.error).problem?.detail ?? "The server did not render them.",
            }}
            onRetry={onRetry}
          />
        )}
        {data && (
          <div className="flex flex-col gap-4">
            <p className="rounded-md border border-warning-border bg-warning-bg px-3 py-2 text-sm text-warning">
              These carry the bridge&apos;s own tokens. Anyone with them can act as its bot and its
              people&apos;s ghosts; hand them over the way you would a password.
            </p>
            {data.config_yaml && (
              <CopyBlock label="config.yaml" content={data.config_yaml} filename="config.yaml" />
            )}
            <CopyBlock
              label="registration.yaml"
              content={data.registration_yaml ?? ""}
              filename="registration.yaml"
            />
            {data.compose_yaml && (
              <CopyBlock
                label="docker-compose.yaml"
                content={data.compose_yaml}
                filename={`${id}-compose.yaml`}
              />
            )}
            {data.manifest_yaml && (
              <CopyBlock
                label="Kubernetes manifest (Secret and Bridge)"
                content={data.manifest_yaml}
                filename={`${id}-bridge.yaml`}
              />
            )}
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
