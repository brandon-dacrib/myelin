import { useState } from "react";
import { useNavigate, useSearch, Link } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { Button } from "@/components/ui/button/Button";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import {
  useCreateAppservice,
  useRenderBridgeType,
  type BridgeTypeRenderResult,
} from "@/api/bridges";
import { useClusterStatus } from "@/api/dashboard";
import { classifyError } from "@/api/problem";
import { getSession, hasScope } from "@/lib/auth";
import { newIdempotencyKey } from "@/api/client";
import { StepRail } from "./StepRail";
import { KindStep } from "./steps/KindStep";
import { IdentityStep, type IdentityConflict } from "./steps/IdentityStep";
import { DeploymentStep } from "./steps/DeploymentStep";
import { OptionsStep } from "./steps/OptionsStep";
import { ReviewStep } from "./steps/ReviewStep";
import {
  WIZARD_STEPS,
  applyPatch,
  initialWizardState,
  defaultsForKind,
  type WizardStep,
} from "./wizard-state";
import { stashCreatedArtifacts } from "./created-artifacts-store";

/**
 * Turns a thrown `create`/`render` mutation error into the message `ReviewStep` shows.
 * `unwrap` (`api/problem.ts`) means every such error is an `ApiProblemError` carrying a real
 * RFC 9457 `Problem`, not the raw body a bare `throw error` used to produce — classify it
 * instead of casting, so a 501/503 says so honestly rather than "did not accept this bridge".
 */
function wizardErrorMessage(err: unknown, fallback: string): string {
  const { kind, problem } = classifyError(err);
  if (kind === "not-implemented") {
    return problem?.detail ?? "This isn't implemented on this server yet.";
  }
  if (kind === "unavailable") {
    return problem?.detail ?? "This isn't connected to a data source on this server yet.";
  }
  return problem?.detail ?? problem?.title ?? fallback;
}

/** `/bridges/new` — flows.md flow 1: add a bridge. */
export function AddBridgeWizardPage() {
  const search = useSearch({ from: "/bridges/new" });
  const navigate = useNavigate({ from: "/bridges/new" });
  const step = search.step ?? "kind";
  const [state, setState] = useState<typeof initialWizardState>(() => {
    // The operator adding the bridge is the natural first administrator of it; the real
    // server's principal id is their Matrix ID (hs_auth::admin_verifier), the mock's likewise.
    const subject = getSession()?.operator.subject ?? "";
    return { ...initialWizardState, adminUser: subject.startsWith("@") ? subject : "" };
  });
  const [furthest, setFurthest] = useState<WizardStep>("kind");
  const [conflict, setConflict] = useState<IdentityConflict | null>(null);
  const [idempotencyKey] = useState(() => newIdempotencyKey());
  const [renderResult, setRenderResult] = useState<BridgeTypeRenderResult | null>(null);

  const { data: cluster } = useClusterStatus();
  const singleNode = (cluster?.replica_count ?? 1) <= 1;
  const render = useRenderBridgeType();
  const create = useCreateAppservice();

  function goTo(next: WizardStep) {
    navigate({ search: { step: next } });
    const nextIndex = WIZARD_STEPS.indexOf(next);
    const furthestIndex = WIZARD_STEPS.indexOf(furthest);
    if (nextIndex > furthestIndex) setFurthest(next);
    // Render is a server call, not client-side YAML assembly (api/bridges.ts's
    // doc comment); (re-)request the preview every time Review is entered so
    // edits made via "Edit" links are reflected.
    if (next === "review" && state.kind) {
      render.mutate(
        // BridgeTypeRenderRequest is additionalProperties:true (free-form);
        // WizardFormState has no index signature of its own, hence the cast.
        { type: state.kind, values: state as unknown as Record<string, unknown> },
        { onSuccess: (result) => setRenderResult(result) },
      );
    }
  }

  function patch(p: Partial<typeof state>) {
    setState((s) => applyPatch(s, p));
  }

  const stepIndex = WIZARD_STEPS.indexOf(step);

  function handleCreate() {
    if (!renderResult) return;
    setConflict(null);
    const registrationYaml = renderResult.registration_yaml ?? "";
    create.mutate(
      {
        idempotencyKey,
        registration: renderResult.registration ?? {},
        registrationYaml,
      },
      {
        onSuccess: (appservice) => {
          const createdId = appservice?.id;
          if (!createdId) return;
          // The render result always carries every artifact; which ones the
          // operator asked to see is a presentational choice this track
          // made (api/bridges.ts's doc comment) — the admin API has no
          // "deployment" concept to filter by itself.
          stashCreatedArtifacts(createdId, {
            registrationYaml,
            configYaml: renderResult.config_yaml ?? undefined,
            composeYaml:
              state.deployment === "self-managed" ? renderResult.compose_yaml : undefined,
            bridgeResourceYaml:
              state.deployment === "kubernetes" ? renderResult.bridge_resource_yaml : undefined,
          });
          navigate({ to: "/bridges/$bridgeId/created", params: { bridgeId: createdId } });
        },
        onError: (err: unknown) => {
          const { problem } = classifyError(err);
          if (problem?.type?.includes("conflict") || problem?.status === 409) {
            // The real Problem schema has no structured "which resource
            // conflicts" field (RFC 9457's `instance` identifies the
            // request, not reliably the pre-existing conflicting resource),
            // so unlike this track's own earlier mock there is no honest
            // link to show here — just the message. Noted as API feedback
            // in docs/status/16-management-web-interface.md.
            setConflict({
              message: problem.detail ?? "This conflicts with an existing appservice.",
            });
            goTo("identity");
          }
        },
      },
    );
  }

  const createErrorMessage = create.isError
    ? wizardErrorMessage(create.error, "The server did not accept this bridge.")
    : undefined;
  const renderErrorMessage = render.isError
    ? wizardErrorMessage(render.error, "Couldn't render the registration preview.")
    : undefined;

  if (!hasScope("bridges:write")) {
    return (
      <div className="p-6">
        <ForbiddenState scope="bridges:write" />
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-5xl p-6">
      <Link
        to="/bridges"
        className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
      >
        <ChevronLeft size={14} aria-hidden="true" />
        Bridges
      </Link>
      <h1 className="mt-2 text-xl text-text">Add bridge</h1>

      <div className="mt-6 flex flex-col gap-8 lg:flex-row">
        <div className="lg:w-48 lg:shrink-0">
          <StepRail current={step} furthestAllowed={furthest} onSelect={goTo} />
        </div>

        <div className="flex-1">
          {step === "kind" && (
            <KindStep
              selected={state.kind}
              onSelect={(kindId, kind) => patch(defaultsForKind(kindId, kind))}
            />
          )}
          {step === "identity" && (
            <IdentityStep
              state={state}
              onChange={patch}
              conflict={conflict}
              onClearConflict={() => setConflict(null)}
            />
          )}
          {step === "deployment" && (
            <DeploymentStep state={state} onChange={patch} singleNode={singleNode} />
          )}
          {step === "options" && <OptionsStep state={state} onChange={patch} />}
          {step === "review" && (
            <ReviewStep
              state={state}
              onEdit={goTo}
              onCreate={handleCreate}
              isPending={create.isPending}
              isRendering={render.isPending}
              renderResult={renderResult}
              renderError={renderErrorMessage}
              createError={createErrorMessage}
            />
          )}

          {step !== "review" && (
            <div className="mt-8 flex justify-between border-t border-border pt-4">
              <Button
                variant="secondary"
                disabled={stepIndex === 0}
                onClick={() => goTo(WIZARD_STEPS[stepIndex - 1])}
              >
                Back
              </Button>
              <Button
                disabled={step === "kind" && !state.kind}
                onClick={() => goTo(WIZARD_STEPS[stepIndex + 1])}
              >
                Continue
              </Button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
