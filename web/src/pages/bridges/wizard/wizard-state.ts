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

/** What the catalogue says about a kind, as far as the defaults use it. */
export interface KindDefaults {
  name?: string;
  /** `users`/`aliases`, each a list of `{regex, exclusive}`, already written for this server. */
  default_namespaces?: Record<string, unknown>;
}

function firstPattern(namespaces: Record<string, unknown> | undefined, key: string): string | null {
  const list = namespaces?.[key];
  if (!Array.isArray(list) || list.length === 0) return null;
  const first = list[0] as { regex?: unknown };
  return typeof first?.regex === "string" ? first.regex : null;
}

/**
 * The wizard's starting values for a kind. The namespaces come from the catalogue entry, which
 * the server writes for its own name; the fallback pattern here (`example.org`) is only for a
 * catalogue that said nothing, and the server replaces it with its name when rendering.
 */
export function defaultsForKind(kindId: string, kind?: KindDefaults): Partial<WizardFormState> {
  const short = kindId.replace(/^mautrix-/, "").replace(/^matrix-/, "");
  return {
    kind: kindId,
    name: kind?.name ?? short.charAt(0).toUpperCase() + short.slice(1),
    id: short,
    senderLocalpart: `${short}bot`,
    userNamespace: firstPattern(kind?.default_namespaces, "users") ?? `@${short}_.*:example.org`,
    aliasNamespace: firstPattern(kind?.default_namespaces, "aliases") ?? `#${short}_.*:example.org`,
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
