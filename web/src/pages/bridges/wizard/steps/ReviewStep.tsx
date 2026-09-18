import type { ReactNode } from "react";
import { Button } from "@/components/ui/button/Button";
import { CopyBlock } from "@/components/CopyBlock";
import { ErrorState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import type { BridgeTypeRenderResult } from "@/api/bridges";
import type { WizardFormState, WizardStep } from "../wizard-state";

export function ReviewStep({
  state,
  onEdit,
  onCreate,
  isPending,
  isRendering,
  renderResult,
  renderError,
  createError,
}: {
  state: WizardFormState;
  onEdit: (step: WizardStep) => void;
  onCreate: () => void;
  isPending: boolean;
  isRendering: boolean;
  renderResult: BridgeTypeRenderResult | null;
  renderError?: string;
  createError?: string;
}) {
  return (
    <div>
      <h2 className="text-lg text-text">Review</h2>
      <p className="mt-1 text-sm text-text-muted">
        Creating registers the appservice with the homeserver immediately (hot registration).
      </p>

      <div className="mt-6 flex flex-col gap-4">
        <ReviewGroup title="Kind" onEdit={() => onEdit("kind")}>
          <p className="text-sm text-text">{state.kind || "—"}</p>
        </ReviewGroup>
        <ReviewGroup title="Identity" onEdit={() => onEdit("identity")}>
          <dl className="grid grid-cols-2 gap-2 text-sm">
            <Row label="Name" value={state.name} />
            <Row label="ID" value={state.id} />
            <Row label="Sender localpart" value={state.senderLocalpart} />
            <Row label="User namespace" value={state.userNamespace || "—"} />
          </dl>
        </ReviewGroup>
        <ReviewGroup title="Deployment" onEdit={() => onEdit("deployment")}>
          <p className="text-sm text-text">
            {state.deployment === "kubernetes" ? `Kubernetes (${state.namespace})` : "Self-managed"}
          </p>
        </ReviewGroup>
        <ReviewGroup title="Options" onEdit={() => onEdit("options")}>
          <p className="text-sm text-text">
            {[
              state.doublePuppeting && "Double puppeting",
              state.encryption && "Encryption",
              state.rateLimitExempt && "Rate-limit exempt",
            ]
              .filter(Boolean)
              .join(", ") || "None"}
          </p>
        </ReviewGroup>
      </div>

      <div className="mt-6">
        {isRendering && <SkeletonText lines={6} />}
        {!isRendering && renderError && (
          <ErrorState
            title="Couldn't render the registration preview"
            problem={{ detail: renderError }}
          />
        )}
        {!isRendering && renderResult && (
          <CopyBlock
            label="registration.yaml (preview)"
            content={renderResult.registration_yaml ?? ""}
            filename={`${state.id || "bridge"}-registration.yaml`}
          />
        )}
      </div>

      {createError && (
        <div className="mt-4">
          <ErrorState
            title="Couldn't create the bridge"
            problem={{ detail: createError }}
            onRetry={onCreate}
          />
        </div>
      )}

      <div className="mt-6 flex justify-end">
        <Button size="lg" onClick={onCreate} disabled={isPending || isRendering || !renderResult}>
          {isPending ? "Creating..." : "Create bridge"}
        </Button>
      </div>
    </div>
  );
}

function ReviewGroup({
  title,
  onEdit,
  children,
}: {
  title: string;
  onEdit: () => void;
  children: ReactNode;
}) {
  return (
    <div className="rounded-md border border-border bg-surface p-4">
      <div className="flex items-center justify-between">
        <h3 className="text-sm font-medium text-text">{title}</h3>
        <button
          type="button"
          onClick={onEdit}
          className="text-sm text-accent hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
        >
          Edit
        </button>
      </div>
      <div className="mt-2">{children}</div>
    </div>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <dt className="text-xs text-text-muted">{label}</dt>
      <dd className="text-text">{value}</dd>
    </div>
  );
}
