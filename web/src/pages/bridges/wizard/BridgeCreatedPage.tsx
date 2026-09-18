import { useState } from "react";
import { useParams, Link } from "@tanstack/react-router";
import { CheckCircle2 } from "lucide-react";
import { useAppservice, deriveDisplayName } from "@/api/bridges";
import { Button } from "@/components/ui/button/Button";
import { CopyBlock } from "@/components/CopyBlock";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { takeCreatedArtifacts, type CreatedArtifacts } from "./created-artifacts-store";

/** `/bridges/:id/created` — flows.md flow 1 step 7: artifacts shown once. */
export function BridgeCreatedPage() {
  const { bridgeId } = useParams({ from: "/bridges/$bridgeId/created" });
  const { data: bridge, isLoading } = useAppservice(bridgeId);
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

  const name = deriveDisplayName(bridge);

  return (
    <div className="mx-auto max-w-3xl p-6">
      <div className="flex items-center gap-2 text-success">
        <CheckCircle2 size={20} aria-hidden="true" />
        <h1 className="text-xl text-text">Bridge {name} created</h1>
      </div>

      <p className="mt-2 rounded-md border border-warning-border bg-warning-bg px-4 py-3 text-sm text-warning">
        Tokens are shown once, below. You can rotate them later from the bridge&apos;s Registration
        tab if you lose them.
      </p>

      {artifacts ? (
        <div className="mt-6 flex flex-col gap-4">
          <CopyBlock
            label="registration.yaml"
            content={artifacts.registrationYaml}
            filename={`${bridge.id}-registration.yaml`}
          />
          {artifacts.composeYaml && (
            <CopyBlock
              label="docker-compose.yaml"
              content={artifacts.composeYaml}
              filename={`${bridge.id}-compose.yaml`}
            />
          )}
          {artifacts.bridgeResourceYaml && (
            <CopyBlock
              label="Bridge resource (Kubernetes)"
              content={artifacts.bridgeResourceYaml}
              filename={`${bridge.id}-bridge.yaml`}
            />
          )}
        </div>
      ) : (
        <p className="mt-6 text-sm text-text-muted">
          The rendered artifacts were shown once, on the previous page. Open the bridge&apos;s
          Registration tab (needs <code className="font-identifier">bridges:write</code>) to see the
          registration again; tokens can be rotated from there if you lost them.
        </p>
      )}

      <div className="mt-8 rounded-md border border-border bg-surface p-4">
        <h2 className="text-sm font-medium text-text">What happens next</h2>
        <p className="mt-1 text-sm text-text-muted">
          The bridge shows <strong>unknown</strong> health until it pings in, then{" "}
          <strong>healthy</strong>. Start the bridge process with the artifacts above.
        </p>
      </div>

      <div className="mt-6 flex justify-end">
        <Link to="/bridges/$bridgeId" params={{ bridgeId: bridge.id ?? bridgeId }}>
          <Button>Open bridge</Button>
        </Link>
      </div>
    </div>
  );
}
