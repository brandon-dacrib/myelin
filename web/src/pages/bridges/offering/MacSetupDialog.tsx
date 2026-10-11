import { useState, type FormEvent, type ReactNode } from "react";
import type { UseMutationResult } from "@tanstack/react-query";
import { ExternalLink } from "lucide-react";
import {
  instanceUserSegment,
  usePutBridgeInstance,
  type BridgeInstance,
  type BridgeInstanceFiles,
  type BridgeOffering,
  type BridgeType,
} from "@/api/bridges";
import { ApiProblemError, classifyError } from "@/api/problem";
import { CopyBlock } from "@/components/CopyBlock";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { ErrorState } from "@/components/ui/error-state/ErrorState";
import { Field, Input } from "@/components/ui/input/Input";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { toast } from "@/components/ui/toast/toast-store";
import {
  checkMacAddress,
  instanceAddress,
  instanceOwnerLabel,
  instanceStateBadge,
  instanceStateLabel,
} from "@/lib/bridge-offerings";

/** The commands to start the bridge once the three files are beside the unzipped program. */
const RUN_COMMANDS = ["cd ~/Downloads/imessage-bridge", "chmod +x run.sh", "./run.sh"].join("\n");

/**
 * Sets up one person's bridge on their Mac, for a type that `runs_on: "mac"` (iMessage): the
 * steps in the order they happen. Download the pinned build (`BridgeType.download_url`); tell
 * this server where it reaches the Mac (`PUT .../instances/{user_id}` with `address`, which
 * rewrites the registration's url and the files); download config.yaml, registration.yaml and
 * run.sh (`POST .../files`, rendered when the dialog opens and again after the address is saved,
 * since they carry it); run it; and watch the instance's state, which the page polls. Nothing
 * here is a container: no compose file, no manifest, no image tag.
 *
 * `instance` is the live row from the page's instance list, so the state at the bottom moves as
 * the page polls; `files` is the page's render mutation, reset when the dialog closes because
 * the files carry the instance's tokens.
 */
export function MacSetupDialog({
  offering,
  type,
  instance,
  files,
  onOpenChange,
  onRenderFiles,
}: {
  offering: BridgeOffering;
  type: BridgeType | undefined;
  instance: BridgeInstance | null;
  files: UseMutationResult<BridgeInstanceFiles, Error, { type: string; userId: string }, unknown>;
  onOpenChange: (open: boolean) => void;
  onRenderFiles: () => void;
}) {
  const name = offering.name ?? offering.type;
  const owner = instance ? instanceOwnerLabel(instance) : "";
  return (
    <Dialog open={instance !== null} onOpenChange={onOpenChange}>
      <DialogContent
        size="form"
        title={
          instance?.user_id
            ? `Set up ${owner}'s ${name} bridge on their Mac`
            : `Set up the ${name} bridge on a Mac`
        }
        description={`The bridge runs on the Mac whose Messages it carries, not on this server. Five steps, in order; this server watches for the bridge and the row turns Ready when it answers.`}
        footer={
          <DialogClose asChild>
            <Button variant="secondary">Done</Button>
          </DialogClose>
        }
      >
        {instance && (
          <ol className="flex flex-col gap-6">
            <Step n={1} title="Download the bridge">
              <DownloadStep type={type} name={name} />
            </Step>
            <Step n={2} title="Where this server reaches the Mac">
              <AddressStep
                offering={offering}
                type={type}
                instance={instance}
                onSaved={onRenderFiles}
              />
            </Step>
            <Step n={3} title="Download its files">
              <FilesStep instance={instance} files={files} onRetry={onRenderFiles} />
            </Step>
            <Step n={4} title="Run it on the Mac">
              <RunStep />
            </Step>
            <Step n={5} title="Status">
              <StatusStep instance={instance} />
            </Step>
          </ol>
        )}
      </DialogContent>
    </Dialog>
  );
}

function Step({ n, title, children }: { n: number; title: string; children: ReactNode }) {
  const id = `mac-step-${n}`;
  return (
    <li className="flex gap-3" aria-labelledby={id}>
      <span
        aria-hidden="true"
        className="mt-0.5 flex size-6 shrink-0 items-center justify-center rounded-full bg-accent-muted text-xs font-medium text-accent"
      >
        {n}
      </span>
      <section aria-labelledby={id} className="min-w-0 flex-1">
        <h3 id={id} className="text-sm font-medium text-text">
          {title}
        </h3>
        <div className="mt-2 flex flex-col gap-2 text-sm text-text">{children}</div>
      </section>
    </li>
  );
}

function DownloadStep({ type, name }: { type: BridgeType | undefined; name: string }) {
  if (!type?.download_url) {
    return (
      <p className="text-text-muted">
        The catalogue names no download for {name}
        {type?.docs_url ? (
          <>
            ; its{" "}
            <a
              href={type.docs_url}
              target="_blank"
              rel="noreferrer"
              className="text-accent underline hover:no-underline"
            >
              documentation
            </a>{" "}
            says where to get it.
          </>
        ) : (
          "."
        )}
      </p>
    );
  }
  return (
    <>
      <p>
        <a
          href={type.download_url}
          target="_blank"
          rel="noreferrer"
          className="inline-flex items-center gap-1 font-medium text-accent underline hover:no-underline"
        >
          <ExternalLink size={14} aria-hidden="true" />
          Download {name} for the Mac
        </a>
      </p>
      {type.download_note && <p className="text-text-muted">{type.download_note}</p>}
      <p className="text-text-muted">
        Unzip it into a folder of its own on the Mac; the files from step 3 go in the same folder.
      </p>
    </>
  );
}

function AddressStep({
  offering,
  type,
  instance,
  onSaved,
}: {
  offering: BridgeOffering;
  type: BridgeType | undefined;
  instance: BridgeInstance;
  onSaved: () => void;
}) {
  const put = usePutBridgeInstance();
  const current = instanceAddress(instance);
  // Prefilled with the address already set; empty while the server has only its placeholder.
  const [value, setValue] = useState(current?.set ? current.host : "");
  const [error, setError] = useState<string | null>(null);
  const port = type?.port ?? 29331;

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    const checked = checkMacAddress(value);
    if ("error" in checked) {
      setError(checked.error);
      return;
    }
    setError(null);
    try {
      const saved = await put.mutateAsync({
        type: offering.type,
        userId: instanceUserSegment(instance),
        address: checked.address,
      });
      // The server normalises it (a bare host gets the port): show what it kept.
      const kept = instanceAddress(saved);
      if (kept?.set) setValue(kept.host);
      toast({ title: `Saved the Mac's address for ${instanceOwnerLabel(instance)}` });
      onSaved();
    } catch (err) {
      setError(
        err instanceof ApiProblemError
          ? (err.problem.detail ?? err.problem.title ?? "The server refused.")
          : "Couldn't reach the server.",
      );
    }
  }

  return (
    <>
      <p className="text-text-muted">
        This server has to be able to open a connection to the Mac on the bridge&apos;s port ({port}{" "}
        unless you change it): the same network, or a Tailscale-style private network between them.
      </p>
      <form onSubmit={handleSubmit} noValidate className="flex flex-col gap-3">
        <Field
          label="The Mac's hostname or IP address"
          hint={`With a port if it is not ${port}, like brandons-mac.local:${port}; or a full http:// address.`}
          error={error ?? undefined}
        >
          {(f) => (
            <Input
              {...f}
              value={value}
              autoComplete="off"
              placeholder={`brandons-mac.local or 100.64.0.7:${port}`}
              onChange={(e) => setValue(e.target.value)}
              className="font-identifier"
            />
          )}
        </Field>
        <div className="flex flex-wrap items-center gap-3">
          <Button type="submit" size="sm" disabled={put.isPending}>
            {put.isPending ? "Saving..." : "Save address"}
          </Button>
          <span className="text-xs text-text-muted" data-testid="address-current">
            {current?.set
              ? `This server reaches the bridge at ${instance.url}.`
              : "Not set yet: the server has a placeholder address until you save one."}
          </span>
        </div>
      </form>
    </>
  );
}

function FilesStep({
  instance,
  files,
  onRetry,
}: {
  instance: BridgeInstance;
  files: UseMutationResult<BridgeInstanceFiles, Error, { type: string; userId: string }, unknown>;
  onRetry: () => void;
}) {
  const address = instanceAddress(instance);
  const data = files.data;
  return (
    <>
      <p className="text-text-muted">
        Three files, into the folder from step 1. They carry the bridge&apos;s own tokens: anyone
        with them can act as its bot and its ghosts, so hand them over the way you would a password.
        {address && !address.set && (
          <>
            {" "}
            They carry the placeholder address until step 2 is saved; they are rendered again then.
          </>
        )}
      </p>
      {files.isPending && <SkeletonText lines={4} />}
      {files.isError && (
        <ErrorState
          title="Couldn't render the files"
          problem={{
            detail: classifyError(files.error).problem?.detail ?? "The server did not render them.",
          }}
          onRetry={onRetry}
        />
      )}
      {data && (
        <div className="flex flex-col gap-3">
          {data.config_yaml && (
            <CopyBlock label="config.yaml" content={data.config_yaml} filename="config.yaml" />
          )}
          <CopyBlock
            label="registration.yaml"
            content={data.registration_yaml ?? ""}
            filename="registration.yaml"
          />
          {data.run_sh ? (
            <CopyBlock label="run.sh" content={data.run_sh} filename="run.sh" />
          ) : (
            <p className="text-text-muted" data-testid="no-run-sh">
              This server rendered no run.sh for {instance.appservice_id ?? "this bridge"}; start
              the program by hand with{" "}
              <code className="font-identifier">
                ./mautrix-imessage -c config.yaml -r registration.yaml
              </code>
              .
            </p>
          )}
        </div>
      )}
    </>
  );
}

function RunStep() {
  return (
    <>
      <ol className="flex list-decimal flex-col gap-1 pl-5">
        <li>Open Terminal on the Mac and go to the folder with the program and the three files.</li>
        <li>
          Make run.sh runnable once, then run it. It downloads the build if the program is not
          there, clears macOS&apos;s quarantine flag and starts the bridge.
        </li>
        <li>
          The first run asks for Full Disk Access, which the bridge needs to read Messages: System
          Settings, Privacy &amp; Security, Full Disk Access, switch Terminal on, then run it again.
        </li>
      </ol>
      <CopyBlock label="In Terminal" content={RUN_COMMANDS} filename="run-imessage-bridge.txt" />
      <p className="text-text-muted">
        The Mac has to stay on and awake:{" "}
        <code className="font-identifier">caffeinate -i ./run.sh</code> keeps it awake while the
        bridge runs. To keep the bridge running after logging out, or across restarts, run it from a
        launchd agent instead of a Terminal window.
      </p>
    </>
  );
}

function StatusStep({ instance }: { instance: BridgeInstance }) {
  return (
    <>
      <div className="flex flex-wrap items-center gap-2">
        <Badge status={instanceStateBadge(instance.state)}>
          {instanceStateLabel(instance.state)}
        </Badge>
        {instance.health && instance.health !== "unknown" && (
          <span className="text-text-muted">Ping: {instance.health}</span>
        )}
      </div>
      {instance.reason && <p className="text-text-muted">{instance.reason}</p>}
      {instance.last_error && (
        <p
          className="rounded-md border border-warning-border bg-warning-bg px-3 py-2 text-warning"
          data-testid="last-error"
        >
          Last error: {instance.last_error}
        </p>
      )}
      <p className="text-text-muted">
        This server pings the bridge every 30 seconds. Once the bridge on the Mac answers, this row
        turns Ready and its bot invites {instance.user_id ?? "people"} to a chat with the sign-in
        steps. You can close this and come back; the page keeps watching.
      </p>
    </>
  );
}
