export const WIZARD_STEPS = ["kind", "identity", "deployment", "options", "review"] as const;
export type WizardStep = (typeof WIZARD_STEPS)[number];

/**
 * The wizard's own local state. Sent as-is (camelCase, whatever shape this
 * track chose) as the free-form `values` body of `POST
 * /bridge-types/{type}/render` (`BridgeTypeRenderRequest` is
 * `additionalProperties: true` — the real API does not prescribe field
 * names here). `deployment`/`namespace`/`imageTag`/`databaseMode` are
 * presentational choices about which of the render result's artifacts to
 * show and emphasise on the Created page; the admin API itself has no
 * "deployment" concept (see api/bridges.ts's doc comment).
 */
export interface WizardFormState {
  kind: string;
  name: string;
  id: string;
  senderLocalpart: string;
  userNamespace: string;
  aliasNamespace: string;
  roomNamespace: string;
  deployment: "kubernetes" | "self-managed";
  namespace: string;
  imageTag: string;
  databaseMode: "own" | "shared";
  doublePuppeting: boolean;
  encryption: boolean;
  rateLimitExempt: boolean;
}

export function defaultsForKind(kindId: string): Partial<WizardFormState> {
  const short = kindId.replace(/^mautrix-/, "").replace(/^matrix-/, "");
  return {
    kind: kindId,
    name: short.charAt(0).toUpperCase() + short.slice(1),
    id: short,
    senderLocalpart: `${short}bot`,
    userNamespace: `@${short}_.*:example.org`,
    aliasNamespace: `#${short}_.*:example.org`,
    roomNamespace: "",
    imageTag: "latest",
  };
}

export const initialWizardState: WizardFormState = {
  kind: "",
  name: "",
  id: "",
  senderLocalpart: "",
  userNamespace: "",
  aliasNamespace: "",
  roomNamespace: "",
  deployment: "self-managed",
  namespace: "bridges",
  imageTag: "latest",
  databaseMode: "own",
  doublePuppeting: true,
  encryption: true,
  rateLimitExempt: true,
};
