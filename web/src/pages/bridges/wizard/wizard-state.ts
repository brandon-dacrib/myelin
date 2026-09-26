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
  /**
   * Where this server is, from where the bridge runs: a Compose service name, a Kubernetes
   * service, `host.docker.internal` for a server outside Docker. Written into the bridge's
   * config as `homeserver.address`.
   */
  homeserverAddress: string;
  /**
   * Where the bridge is, from this server: the registration's `url`, which this server pushes
   * transactions to. The bridge's Compose service name or Kubernetes service on the port the
   * catalogue says it listens on, unless the operator says otherwise (a published port on this
   * host, for a bridge in Docker beside a server that is not).
   */
  bridgeAddress: string;
  /** The port the chosen kind listens on, from the catalogue; what `bridgeAddress` defaults to. */
  port: number;
  /** The Matrix user the bridge takes admin commands from; the operator signing in, by default. */
  adminUser: string;
  doublePuppeting: boolean;
  encryption: boolean;
  rateLimitExempt: boolean;
}

/** What the catalogue says about a kind, as far as the defaults use it. */
export interface KindDefaults {
  name?: string;
  /** `users`/`aliases`, each a list of `{regex, exclusive}`, already written for this server. */
  default_namespaces?: Record<string, unknown>;
  port?: number;
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
    port: kind?.port ?? 0,
  };
}

/** What `homeserverAddress` starts as for a deployment, so switching deployments keeps it plausible. */
export function defaultHomeserverAddress(
  deployment: WizardFormState["deployment"],
  namespace: string,
): string {
  return deployment === "kubernetes"
    ? `http://myelin.${namespace || "bridges"}.svc:8008`
    : "http://myelin:8008";
}

/** What `bridgeAddress` starts as, so that renaming the bridge or moving it keeps it plausible. */
export function defaultBridgeAddress(
  state: Pick<WizardFormState, "deployment" | "id" | "namespace" | "port">,
): string {
  const host = state.id || "bridge";
  const port = state.port || 0;
  return state.deployment === "kubernetes"
    ? `http://${host}.${state.namespace || "bridges"}.svc:${port}`
    : `http://${host}:${port}`;
}

/**
 * Applies `patch` and moves the two addresses along with what they are derived from, unless
 * the operator has typed something else into them: a default follows its inputs, a choice
 * stays a choice.
 */
export function applyPatch(
  state: WizardFormState,
  patch: Partial<WizardFormState>,
): WizardFormState {
  const next = { ...state, ...patch };
  const homeserverWasDefault =
    state.homeserverAddress === defaultHomeserverAddress(state.deployment, state.namespace);
  const bridgeWasDefault = state.bridgeAddress === defaultBridgeAddress(state);
  if (homeserverWasDefault && patch.homeserverAddress === undefined) {
    next.homeserverAddress = defaultHomeserverAddress(next.deployment, next.namespace);
  }
  if (bridgeWasDefault && patch.bridgeAddress === undefined) {
    next.bridgeAddress = defaultBridgeAddress(next);
  }
  return next;
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
  homeserverAddress: defaultHomeserverAddress("self-managed", "bridges"),
  bridgeAddress: "http://bridge:0",
  port: 0,
  adminUser: "",
  doublePuppeting: true,
  encryption: true,
  rateLimitExempt: true,
};
