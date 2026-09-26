import { Field, Input } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { cn } from "@/lib/cn";
import type { WizardFormState } from "../wizard-state";

/**
 * flows.md flow 1 step 4, plus the two addresses that make a bridge and this server find each
 * other: they follow the deployment, the id and the namespace until the operator types into
 * them (`applyPatch` in wizard-state.ts), so every change here is just a patch.
 */
export function DeploymentStep({
  state,
  onChange,
  singleNode,
}: {
  state: WizardFormState;
  onChange: (patch: Partial<WizardFormState>) => void;
  singleNode: boolean;
}) {
  return (
    <div>
      <h2 className="text-lg text-text">Deployment</h2>
      <p className="mt-1 text-sm text-text-muted">Who runs the bridge process.</p>

      <div
        role="radiogroup"
        aria-label="Deployment"
        className="mt-6 grid grid-cols-1 gap-3 sm:grid-cols-2"
      >
        {!singleNode && (
          <Card
            selected={state.deployment === "kubernetes"}
            onSelect={() => onChange({ deployment: "kubernetes" })}
            title="Kubernetes"
            description="Create a Bridge resource; the operator deploys and manages it."
          />
        )}
        <Card
          selected={state.deployment === "self-managed"}
          onSelect={() => onChange({ deployment: "self-managed" })}
          title="Self-managed"
          description="You run the bridge yourself, beside the server. Produces the bridge's config, its registration and a Compose service."
        />
      </div>

      {state.deployment === "kubernetes" && !singleNode && (
        <div className="mt-6 grid grid-cols-1 gap-4 sm:grid-cols-2">
          <Field label="Namespace">
            {(f) => (
              <Input
                {...f}
                value={state.namespace}
                onChange={(e) => onChange({ namespace: e.target.value })}
              />
            )}
          </Field>
          <Field label="Image tag">
            {(f) => (
              <Input
                {...f}
                value={state.imageTag}
                onChange={(e) => onChange({ imageTag: e.target.value })}
              />
            )}
          </Field>
          <Field
            label="Database"
            hint="Own CloudNativePG database, or a schema in a shared cluster."
          >
            {(f) => (
              <Select
                {...f}
                value={state.databaseMode}
                onValueChange={(v) => onChange({ databaseMode: v as "own" | "shared" })}
                options={[
                  { value: "own", label: "Own database" },
                  { value: "shared", label: "Shared cluster schema" },
                ]}
              />
            )}
          </Field>
        </div>
      )}

      <div className="mt-6 grid max-w-3xl grid-cols-1 gap-4 sm:grid-cols-2">
        <Field
          label="This server, as the bridge reaches it"
          hint={
            state.deployment === "kubernetes"
              ? "The server's Kubernetes service, from inside the cluster."
              : "In Compose, the server's service name. From a bridge outside Docker, the server's URL; from Docker to a server on this machine, http://host.docker.internal:8008."
          }
        >
          {(f) => (
            <Input
              {...f}
              value={state.homeserverAddress}
              onChange={(e) => onChange({ homeserverAddress: e.target.value })}
              className="font-identifier"
            />
          )}
        </Field>
        <Field
          label="The bridge, as this server reaches it"
          hint={
            state.deployment === "kubernetes"
              ? "The bridge's Kubernetes service. Becomes the registration's url."
              : `In Compose, the bridge's service name on its port. For a bridge in Docker beside a server on this machine, the published port: http://127.0.0.1:${state.port || "…"}. Becomes the registration's url.`
          }
        >
          {(f) => (
            <Input
              {...f}
              value={state.bridgeAddress}
              onChange={(e) => onChange({ bridgeAddress: e.target.value })}
              className="font-identifier"
            />
          )}
        </Field>
      </div>
    </div>
  );
}

function Card({
  selected,
  onSelect,
  title,
  description,
}: {
  selected: boolean;
  onSelect: () => void;
  title: string;
  description: string;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      onClick={onSelect}
      className={cn(
        "flex flex-col items-start gap-1 rounded-md border p-4 text-left",
        selected
          ? "border-accent bg-accent-muted"
          : "border-border bg-surface hover:bg-surface-sunken",
        "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
      )}
    >
      <span className="text-sm font-medium text-text">{title}</span>
      <span className="text-xs text-text-muted">{description}</span>
    </button>
  );
}
