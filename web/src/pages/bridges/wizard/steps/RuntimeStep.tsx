import { useBridgeDeploymentTarget, type BridgeType } from "@/api/bridges";
import { RuntimeFields } from "../../offering-fields";
import { clusterAvailability, type OfferFormState } from "../offer-state";

/**
 * Offer a bridge, step 3: where each person's bridge runs. In this cluster only when the server
 * has somewhere to deploy (`GET /bridge-deployment-target`, RFC 0017 4.5) and the type runs from
 * its config alone; otherwise elsewhere, with the reason written out rather than an option that
 * silently isn't there.
 */
export function RuntimeStep({
  state,
  onChange,
  type,
}: {
  state: OfferFormState;
  onChange: (patch: Partial<OfferFormState>) => void;
  type: BridgeType | undefined;
}) {
  const { data: target } = useBridgeDeploymentTarget();
  const { available, reason } = clusterAvailability(target, type);
  return (
    <div>
      <h2 className="text-lg text-text">Runtime</h2>
      <p className="mt-1 text-sm text-text-muted">
        Where each person&apos;s {type?.name ?? "bridge"} runs. Either way it has its own
        registration and its own bot, and signs in only its own person.
      </p>
      <div className="mt-6">
        <RuntimeFields
          runtime={state.runtime}
          onRuntime={(runtime) => onChange({ runtime })}
          clusterAvailable={available}
          unavailableBecause={reason}
          target={target}
          type={type}
          imageTag={state.imageTag}
          onImageTag={(imageTag) => onChange({ imageTag })}
        />
      </div>
    </div>
  );
}
