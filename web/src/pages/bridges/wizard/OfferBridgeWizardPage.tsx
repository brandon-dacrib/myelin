import { useState, type ReactNode } from "react";
import { useNavigate, useSearch, Link } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import {
  useBridgeDeploymentTarget,
  useBridgeOfferings,
  useBridgeTypes,
  usePutBridgeOffering,
  type BridgeType,
} from "@/api/bridges";
import { useServerInfo } from "@/api/dashboard";
import { classifyError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { ErrorState, ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { accessIsValid, runtimeMeta } from "@/lib/bridge-offerings";
import { AccessFields, OptionFields } from "../offering-fields";
import { StepRail } from "./StepRail";
import { KindStep } from "./steps/KindStep";
import { RuntimeStep } from "./steps/RuntimeStep";
import {
  OFFER_STEPS,
  OFFER_STEP_LABELS,
  clusterAvailability,
  effectiveRuntime,
  initialOfferState,
  offerRequest,
  stateForKind,
  type OfferFormState,
  type OfferStep,
} from "./offer-state";

/**
 * `/bridges/new` -- offer a bridge (RFC 0017): choose the network, who may have one, where each
 * person's bridge runs and how it behaves, then `PUT /bridge-offerings/{type}`. Nobody gets a
 * bridge yet: each person gets theirs by messaging the offering's front door, or an
 * administrator adds one for them from the offering's page, where this ends.
 */
export function OfferBridgeWizardPage() {
  const search = useSearch({ from: "/bridges/new" });
  const navigate = useNavigate({ from: "/bridges/new" });
  const step = search.step ?? "kind";
  const [state, setState] = useState<OfferFormState>(initialOfferState);
  const [furthest, setFurthest] = useState<OfferStep>("kind");

  const { data: types } = useBridgeTypes();
  const { data: offerings } = useBridgeOfferings();
  const { data: target } = useBridgeDeploymentTarget();
  const { data: server } = useServerInfo();
  const put = usePutBridgeOffering();

  const type = types?.find((t) => t.id === state.type);
  const cluster = clusterAvailability(target, type);
  const runtime = effectiveRuntime(state, cluster.available);
  const offered = new Set((offerings ?? []).map((o) => o.type));
  const stepIndex = OFFER_STEPS.indexOf(step);

  function goTo(next: OfferStep) {
    navigate({ search: { step: next } });
    if (OFFER_STEPS.indexOf(next) > OFFER_STEPS.indexOf(furthest)) setFurthest(next);
  }

  function patch(p: Partial<OfferFormState>) {
    setState((s) => ({ ...s, ...p }));
  }

  function annotate(kind: BridgeType) {
    const notes: string[] = [
      kind.mode === "shared" ? "One bridge for everyone" : "Each person gets their own",
    ];
    if (kind.deployable === false) notes.push("Runs elsewhere only");
    if (offered.has(kind.id ?? "")) notes.push("Already offered: change it from its page");
    return { disabled: offered.has(kind.id ?? ""), notes };
  }

  const canContinue =
    (step === "kind" && Boolean(state.type)) ||
    (step === "access" && accessIsValid(state.allLocalUsers, state.users)) ||
    step === "runtime" ||
    step === "options";

  function handleOffer() {
    if (!type?.id) return;
    const typeId = type.id;
    put.mutate(
      { type: typeId, body: offerRequest(state, cluster.available) },
      {
        onSuccess: (offering) => {
          toast({ title: `${offering.name ?? type.name} is offered` });
          navigate({ to: "/bridges/offerings/$type", params: { type: typeId } });
        },
      },
    );
  }

  if (!hasScope("bridges:write")) {
    return (
      <div className="p-6">
        <ForbiddenState scope="bridges:write" />
      </div>
    );
  }

  const putError = put.isError
    ? (() => {
        const { problem } = classifyError(put.error);
        return problem?.detail ?? problem?.title ?? "The server did not accept this offering.";
      })()
    : undefined;

  return (
    <div className="mx-auto max-w-5xl p-6">
      <Link
        to="/bridges"
        className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
      >
        <ChevronLeft size={14} aria-hidden="true" />
        Bridges
      </Link>
      <h1 className="mt-2 text-xl text-text">Offer a bridge</h1>
      <p className="mt-0.5 text-sm text-text-muted">
        Switch a network on for this server. Each person then gets their own bridge by messaging its
        bot.
      </p>

      <div className="mt-6 flex flex-col gap-8 lg:flex-row">
        <div className="lg:w-48 lg:shrink-0">
          <StepRail
            steps={OFFER_STEPS}
            labels={OFFER_STEP_LABELS}
            label="Offer a bridge steps"
            current={step}
            furthestAllowed={furthest}
            onSelect={goTo}
          />
        </div>

        <div className="min-w-0 flex-1">
          {step === "kind" && (
            <KindStep
              selected={state.type}
              onSelect={(_id, kind) => patch(stateForKind(kind))}
              annotate={annotate}
              intro={
                <>
                  What people on this server can connect. Choosing one fills sensible defaults for
                  every later step; each person signs in to their own bridge from a chat with its
                  bot.
                </>
              }
            />
          )}

          {step === "access" && (
            <div>
              <h2 className="text-lg text-text">Access</h2>
              <p className="mt-1 text-sm text-text-muted">
                {type?.mode === "shared"
                  ? `Who may use ${type?.name ?? "this bridge"}. It is one bridge, shared by everyone allowed.`
                  : `Who may have their own ${type?.name ?? "bridge"}. They ask for it by messaging its bot; you can also add one for anyone from the offering's page.`}
              </p>
              <div className="mt-6">
                <AccessFields
                  allLocalUsers={state.allLocalUsers}
                  users={state.users}
                  serverName={server?.name}
                  onChange={patch}
                />
              </div>
            </div>
          )}

          {step === "runtime" && <RuntimeStep state={state} onChange={patch} type={type} />}

          {step === "options" && (
            <div>
              <h2 className="text-lg text-text">Options</h2>
              <p className="mt-1 text-sm text-text-muted">
                Every {type?.name ?? "bridge"} this offering starts gets these. What{" "}
                {type?.name ?? "the bridge"} can do is on by default.
              </p>
              <div className="mt-6">
                <OptionFields values={state} onChange={patch} type={type} />
              </div>
            </div>
          )}

          {step === "review" && (
            <div>
              <h2 className="text-lg text-text">Review</h2>
              <p className="mt-1 text-sm text-text-muted">
                {type?.mode === "shared"
                  ? `Offering ${type?.name ?? "it"} sets up its one bridge now.`
                  : `Offering ${type?.name ?? "it"} registers its bot, which people message to get their own. Nobody has a bridge until they ask, or you add one for them.`}
              </p>
              <div className="mt-6 flex flex-col gap-4">
                <ReviewGroup title="Kind" onEdit={() => goTo("kind")}>
                  <p className="text-sm text-text">
                    {type?.name ?? state.type}
                    <span className="text-text-muted">
                      {" "}
                      · {type?.mode === "shared" ? "one for everyone" : "one each"}
                    </span>
                  </p>
                </ReviewGroup>
                <ReviewGroup title="Access" onEdit={() => goTo("access")}>
                  <p className="text-sm text-text">
                    {state.allLocalUsers ? "Everyone on this server" : state.users.join(", ")}
                  </p>
                </ReviewGroup>
                <ReviewGroup title="Runtime" onEdit={() => goTo("runtime")}>
                  <dl className="grid grid-cols-2 gap-2 text-sm">
                    <Row label="Runs" value={runtimeMeta[runtime].label} />
                    <Row label="Image tag" value={state.imageTag || "latest"} />
                  </dl>
                </ReviewGroup>
                <ReviewGroup title="Options" onEdit={() => goTo("options")}>
                  <p className="text-sm text-text">
                    {[
                      state.encryption && "Encryption",
                      state.doublePuppeting &&
                        type?.supports_double_puppeting !== false &&
                        "Double puppeting",
                      state.backfill && "Backfill",
                    ]
                      .filter(Boolean)
                      .join(", ") || "None"}
                  </p>
                </ReviewGroup>
              </div>

              {putError && (
                <div className="mt-4">
                  <ErrorState
                    title={`Couldn't offer ${type?.name ?? "this bridge"}`}
                    problem={{ detail: putError }}
                    onRetry={handleOffer}
                  />
                </div>
              )}

              <div className="mt-6 flex justify-end">
                <Button size="lg" onClick={handleOffer} disabled={put.isPending || !type}>
                  {put.isPending ? "Offering..." : `Offer ${type?.name ?? "bridge"}`}
                </Button>
              </div>
            </div>
          )}

          {step !== "review" && (
            <div className="mt-8 flex justify-between border-t border-border pt-4">
              <Button
                variant="secondary"
                disabled={stepIndex === 0}
                onClick={() => goTo(OFFER_STEPS[stepIndex - 1])}
              >
                Back
              </Button>
              <Button disabled={!canContinue} onClick={() => goTo(OFFER_STEPS[stepIndex + 1])}>
                Continue
              </Button>
            </div>
          )}
        </div>
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
          aria-label={`Edit ${title.toLowerCase()}`}
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
