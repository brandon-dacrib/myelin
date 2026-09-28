import type {
  BridgeDeploymentTarget,
  BridgeOfferingRequest,
  BridgeOfferingRuntime,
  BridgeType,
} from "@/api/bridges";
import { defaultOfferingOptions } from "@/lib/bridge-offerings";

export const OFFER_STEPS = ["kind", "access", "runtime", "options", "review"] as const;
export type OfferStep = (typeof OFFER_STEPS)[number];

export const OFFER_STEP_LABELS: Record<OfferStep, string> = {
  kind: "Kind",
  access: "Access",
  runtime: "Runtime",
  options: "Options",
  review: "Review",
};

/** The Offer-a-bridge wizard's own state: what `PUT /bridge-offerings/{type}` will be sent. */
export interface OfferFormState {
  type: string;
  allLocalUsers: boolean;
  /** Who may, when not everyone: the Matrix IDs listed, one entry each. */
  users: string[];
  /** What the operator chose; `effectiveRuntime` says what can actually be sent. */
  runtime: BridgeOfferingRuntime;
  imageTag: string;
  encryption: boolean;
  doublePuppeting: boolean;
  backfill: boolean;
}

export const initialOfferState: OfferFormState = {
  type: "",
  allLocalUsers: true,
  users: [],
  runtime: "cluster",
  imageTag: "latest",
  encryption: true,
  doublePuppeting: true,
  backfill: true,
};

/** Choosing a kind resets what depends on it: the options start from what it can do. */
export function stateForKind(type: BridgeType): Partial<OfferFormState> {
  const options = defaultOfferingOptions(type);
  return {
    type: type.id ?? "",
    runtime: "cluster",
    imageTag: "latest",
    encryption: options.encryption,
    doublePuppeting: options.double_puppeting,
    backfill: options.backfill,
  };
}

/**
 * Whether this server can run `type` itself, and if not, why not: the server has to have a
 * deployment target (RFC 0017 4.5) and the type has to run from its rendered config alone.
 */
export function clusterAvailability(
  target: BridgeDeploymentTarget | undefined,
  type: Pick<BridgeType, "deployable" | "name" | "id"> | undefined,
): { available: boolean; reason: "no-target" | "not-deployable" | "loading" | null } {
  if (!target) return { available: false, reason: "loading" };
  if (!target.available) return { available: false, reason: "no-target" };
  if (type?.deployable === false) return { available: false, reason: "not-deployable" };
  return { available: true, reason: null };
}

/** The runtime that will be sent: `cluster` only while it is possible. */
export function effectiveRuntime(
  state: Pick<OfferFormState, "runtime">,
  clusterAvailable: boolean,
): BridgeOfferingRuntime {
  return state.runtime === "cluster" && !clusterAvailable ? "elsewhere" : state.runtime;
}

export function offerRequest(
  state: OfferFormState,
  clusterAvailable: boolean,
): BridgeOfferingRequest {
  return {
    enabled: true,
    runtime: effectiveRuntime(state, clusterAvailable),
    image_tag: state.imageTag.trim() || "latest",
    access: {
      all_local_users: state.allLocalUsers,
      users: state.allLocalUsers ? [] : state.users,
    },
    options: {
      encryption: state.encryption,
      double_puppeting: state.doublePuppeting,
      backfill: state.backfill,
    },
  };
}
