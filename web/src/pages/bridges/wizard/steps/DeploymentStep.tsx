import { Field, Input } from "@/components/ui/input/Input";
import type { WizardFormState } from "../wizard-state";

/**
 * The two addresses that make a bridge the operator runs themselves and this server find each
 * other. They follow the id until the operator types into them (`applyPatch` in
 * wizard-state.ts), so every change here is just a patch. A bridge this server runs in its own
 * cluster is an offering (RFC 0017) and never comes through here.
 */
export function DeploymentStep({
  state,
  onChange,
}: {
  state: WizardFormState;
  onChange: (patch: Partial<WizardFormState>) => void;
}) {
  return (
    <div>
      <h2 className="text-lg text-text">Addresses</h2>
      <p className="mt-1 text-sm text-text-muted">
        You run this bridge yourself, beside the server. The last page has its config, its
        registration and a Compose service; these two addresses are how each finds the other.
      </p>

      <div className="mt-6 grid max-w-3xl grid-cols-1 gap-4 sm:grid-cols-2">
        <Field
          label="This server, as the bridge reaches it"
          hint="In Compose, the server's service name. From a bridge outside Docker, the server's URL; from Docker to a server on this machine, http://host.docker.internal:8008."
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
          hint={`In Compose, the bridge's service name on its port. For a bridge in Docker beside a server on this machine, the published port: http://127.0.0.1:${state.port || "…"}. Becomes the registration's url.`}
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
