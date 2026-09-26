import { useState, type ReactNode } from "react";
import { useParams, Link } from "@tanstack/react-router";
import { CheckCircle2, CircleAlert, Loader2 } from "lucide-react";
import { useAppservice, useAppserviceHealth, useBridgeTypes } from "@/api/bridges";
import { useServerInfo } from "@/api/dashboard";
import { BridgeGlyph } from "@/components/BridgeGlyph";
import { BridgeSignInGuide } from "@/components/BridgeSignInGuide";
import { CopyBlock } from "@/components/CopyBlock";
import { RelativeTime } from "@/components/RelativeTime";
import { buttonVariants } from "@/components/ui/button/Button";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { bridgeTitle, bridgeTypeOf, botMatrixId } from "@/lib/bridge-catalogue";
import { cn } from "@/lib/cn";
import { takeCreatedArtifacts, type CreatedArtifacts } from "./created-artifacts-store";

/**
 * `/bridges/:id/created` -- flows.md flow 1 step 7, as a runbook rather than a pile of files:
 * save these, start it, watch it connect (live, on this page), sign in. The artifacts are
 * shown once; the connection and sign-in steps are true on any later visit too.
 */
export function BridgeCreatedPage() {
  const { bridgeId } = useParams({ from: "/bridges/$bridgeId/created" });
  // Watching for the first ping: poll faster than a list would.
  const { data: bridge, isLoading } = useAppservice(bridgeId, { refetchInterval: 5_000 });
  const { data: health } = useAppserviceHealth(bridgeId, { refetchInterval: 5_000 });
  const { data: types } = useBridgeTypes();
  const { data: server } = useServerInfo();
  // Read once: a second render (e.g. StrictMode's double-invoke, or a
  // refresh) must not silently show stale artifacts from a *different*
  // bridge's creation, and the store is one-shot by design (see its doc
  // comment).
  const [artifacts] = useState<CreatedArtifacts | null>(() => takeCreatedArtifacts(bridgeId));

  if (isLoading || !bridge) {
    return (
      <div className="mx-auto max-w-3xl p-6">
        <SkeletonText lines={4} />
      </div>
    );
  }

  const type = bridgeTypeOf(bridge, types);
  const name = bridgeTitle(bridge, type);
  const id = bridge.id ?? bridgeId;
  const botId = botMatrixId(bridge.sender_localpart, server?.name);
  const kubernetes = Boolean(artifacts?.bridgeResourceYaml);
  const dir = `./${id}/`;

  return (
    <div className="mx-auto max-w-3xl p-6">
      <div className="flex items-center gap-3">
        <BridgeGlyph category={type?.category} size="lg" />
        <div>
          <h1 className="flex items-center gap-2 text-xl text-text">
            <CheckCircle2 size={20} aria-hidden="true" className="text-success" />
            Bridge {name} created
          </h1>
          <p className="mt-0.5 text-sm text-text-muted">
            Registered with this server as <span className="font-identifier">{id}</span>. Three
            steps to a working bridge, then everyone signs in.
          </p>
        </div>
      </div>

      <ol className="mt-8 flex flex-col gap-8">
        <Step n={1} title="Save the files">
          {artifacts ? (
            <>
              <p className="text-sm text-text-muted">
                {kubernetes
                  ? "The registration is already in this server. Apply the Bridge resource; the operator keeps the registration secret for you."
                  : `Put them in ${dir} next to your Compose file. The registration is already in this server; the bridge needs its own copy, and the config is complete enough to start with.`}
              </p>
              <p className="mt-2 rounded-md border border-warning-border bg-warning-bg px-3 py-2 text-sm text-warning">
                The tokens are shown once, here. They can be rotated later from the bridge&apos;s
                Registration tab if they are lost.
              </p>
              <div className="mt-4 flex flex-col gap-4">
                {artifacts.configYaml && (
                  <CopyBlock
                    label="config.yaml"
                    content={artifacts.configYaml}
                    filename={`${id}-config.yaml`}
                  />
                )}
                <CopyBlock
                  label="registration.yaml"
                  content={artifacts.registrationYaml}
                  filename={`${id}-registration.yaml`}
                />
                {artifacts.composeYaml && (
                  <CopyBlock
                    label="docker-compose.yaml"
                    content={artifacts.composeYaml}
                    filename={`${id}-compose.yaml`}
                  />
                )}
                {artifacts.bridgeResourceYaml && (
                  <CopyBlock
                    label="Bridge resource (Kubernetes)"
                    content={artifacts.bridgeResourceYaml}
                    filename={`${id}-bridge.yaml`}
                  />
                )}
              </div>
            </>
          ) : (
            <p className="text-sm text-text-muted">
              The files were shown once, when the bridge was created. The registration is still in
              this server: open the bridge&apos;s Registration tab (needs{" "}
              <code className="font-identifier">bridges:write</code>) to see it again, and rotate
              the tokens from there if they were lost.
            </p>
          )}
        </Step>

        <Step n={2} title="Start the bridge">
          <p className="text-sm text-text-muted">
            {kubernetes
              ? "Apply the resource and the operator does the rest."
              : type?.renders_config
                ? "With both files in place the bridge starts straight away, completes its config with its own defaults, and registers its bot with this server."
                : "The Compose file says what else this bridge needs before it runs; its own documentation has the details."}
          </p>
          <pre className="mt-3 overflow-x-auto rounded-md border border-border bg-surface-sunken p-3 font-identifier text-xs text-text">
            {kubernetes ? `kubectl apply -f ${id}-bridge.yaml` : `docker compose up -d ${id}`}
          </pre>
        </Step>

        <Step n={3} title="Watch it connect">
          <ConnectionStatus
            health={bridge.paused ? "paused" : (health?.status ?? bridge.health ?? "unknown")}
            lastPingAt={health?.last_ping_at}
            lastError={health?.last_error}
            logsHint={kubernetes ? `kubectl logs deploy/${id}` : `docker compose logs ${id}`}
          />
        </Step>

        <Step n={4} title="Sign in">
          <BridgeSignInGuide type={type} botId={botId} heading="none" />
        </Step>
      </ol>

      <div className="mt-10 flex justify-end">
        <Link
          to="/bridges/$bridgeId"
          params={{ bridgeId: id }}
          className={cn(buttonVariants({ variant: "primary", size: "md" }))}
        >
          Open bridge
        </Link>
      </div>
    </div>
  );
}

function Step({ n, title, children }: { n: number; title: string; children: ReactNode }) {
  return (
    <li className="flex gap-4">
      <span className="flex size-7 shrink-0 items-center justify-center rounded-full border border-border-strong text-sm font-medium text-text">
        {n}
      </span>
      <div className="min-w-0 flex-1">
        <h2 className="text-md font-medium text-text">{title}</h2>
        <div className="mt-2">{children}</div>
      </div>
    </li>
  );
}

/**
 * The live part of the page. `unknown` is what a bridge is until its first ping (flows.md
 * step 7: "Waiting for first ping"); the pulse says this page is still looking.
 */
function ConnectionStatus({
  health,
  lastPingAt,
  lastError,
  logsHint,
}: {
  health: string;
  lastPingAt: string | null | undefined;
  lastError: string | null | undefined;
  logsHint: string;
}) {
  if (health === "healthy") {
    return (
      <div
        role="status"
        className="flex items-start gap-3 rounded-md border border-success-border bg-success-bg px-4 py-3"
      >
        <CheckCircle2 size={18} aria-hidden="true" className="mt-0.5 shrink-0 text-success" />
        <div className="text-sm">
          <p className="font-medium text-success">Connected</p>
          <p className="mt-0.5 text-text-muted">
            The bridge answered this server&apos;s ping <RelativeTime at={lastPingAt} />.
          </p>
        </div>
      </div>
    );
  }
  if (health === "down" || health === "degraded") {
    return (
      <div
        role="status"
        className="flex items-start gap-3 rounded-md border border-danger-border bg-danger-bg px-4 py-3"
      >
        <CircleAlert size={18} aria-hidden="true" className="mt-0.5 shrink-0 text-danger" />
        <div className="text-sm">
          <p className="font-medium text-danger">
            {health === "down" ? "Not reachable" : "Reachable, with errors"}
          </p>
          {lastError && <p className="mt-0.5 font-identifier text-xs text-text">{lastError}</p>}
          <p className="mt-1 text-text-muted">
            Check the bridge&apos;s log: <code className="font-identifier">{logsHint}</code>. The
            registration&apos;s <code className="font-identifier">url</code> has to resolve from
            this server to the bridge, and the bridge&apos;s config has to reach this server.
          </p>
        </div>
      </div>
    );
  }
  if (health === "paused") {
    return (
      <div role="status" className="rounded-md border border-border bg-surface px-4 py-3 text-sm">
        <p className="font-medium text-text">Paused</p>
        <p className="mt-0.5 text-text-muted">
          Delivery is held. Resume it from the bridge&apos;s page once the bridge is running.
        </p>
      </div>
    );
  }
  return (
    <div
      role="status"
      className="flex items-start gap-3 rounded-md border border-info-border bg-info-bg px-4 py-3"
    >
      <Loader2
        size={18}
        aria-hidden="true"
        className="mt-0.5 shrink-0 animate-spin text-info motion-reduce:animate-none"
      />
      <div className="text-sm">
        <p className="font-medium text-info">Waiting for the bridge&apos;s first ping</p>
        <p className="mt-0.5 text-text-muted">
          This page checks every few seconds and turns green when the bridge answers. A bridge
          usually pings within a few seconds of starting.
        </p>
      </div>
    </div>
  );
}
