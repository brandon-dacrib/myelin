import type {
  BridgeDeploymentTarget,
  BridgeInstance,
  BridgeInstanceFiles,
  BridgeOffering,
  BridgeOfferingRequest,
} from "@/api/bridges";
import { bridgeTypes } from "./bridge-types";

/**
 * RFC 0017's offerings and instances, as the mock server keeps them: a WhatsApp offering the
 * cluster runs (people in every state, one of them stuck on an image pull) and an iMessage
 * offering that runs elsewhere. Instances created while the mock runs walk the state machine
 * by the clock, so a page watching one sees it go from requested to ready in about ten seconds.
 */

const SERVER = "example.org";
const NAMESPACE = "myelin";

const now = Date.now();
const iso = (msAgo: number) => new Date(now - msAgo).toISOString();
const DAY = 24 * 3_600_000;

export const DEPLOYMENT_AVAILABLE: BridgeDeploymentTarget = {
  available: true,
  namespace: NAMESPACE,
  homeserver_url: `http://myelin.${NAMESPACE}.svc:8008`,
  reason: null,
};

export const DEPLOYMENT_UNAVAILABLE: BridgeDeploymentTarget = {
  available: false,
  namespace: null,
  homeserver_url: null,
  reason:
    "MYELIN_BRIDGES_NAMESPACE is not set: this server is not running in Kubernetes with the chart's bridges enabled.",
};

/** What `GET /bridge-deployment-target` answers; `setDeploymentTarget` changes it. */
export const deploymentTarget: { current: BridgeDeploymentTarget } = {
  current: DEPLOYMENT_AVAILABLE,
};

export function setDeploymentTarget(available: boolean, reason?: string): void {
  deploymentTarget.current = available
    ? DEPLOYMENT_AVAILABLE
    : { ...DEPLOYMENT_UNAVAILABLE, reason: reason ?? DEPLOYMENT_UNAVAILABLE.reason };
}

interface MockOffering {
  type: string;
  enabled: boolean;
  runtime: "cluster" | "elsewhere";
  imageTag: string;
  access: { all_local_users: boolean; users: string[] };
  options: { encryption: boolean; double_puppeting: boolean; backfill: boolean };
  created_at: string;
}

interface MockInstance extends BridgeInstance {
  /** Set on instances made while the mock runs: they move through the states by the clock. */
  startedAt?: number;
  tokens: { as: string; hs: string };
}

function catalogueEntry(type: string) {
  return bridgeTypes.find((t) => t.id === type);
}

function short(type: string): string {
  return type.replace(/^mautrix-/, "").replace(/^matrix-/, "");
}

function imageOf(offering: MockOffering): string {
  const repository = (catalogueEntry(offering.type)?.image ?? `${offering.type}:latest`).split(
    ":",
  )[0];
  return `${repository}:${offering.imageTag}`;
}

/** RFC 0017 section 3's localpart encoding: `_` and anything unusual become `=` and hex. */
function encodeLocalpart(localpart: string): string {
  return [...new TextEncoder().encode(localpart)]
    .map((b) => {
      const c = String.fromCharCode(b);
      return /[a-z0-9./-]/.test(c) ? c : `=${b.toString(16).padStart(2, "0")}`;
    })
    .join("");
}

/** A stand-in for the real `bridge-<8 hex of sha256(appservice id)>`: stable, not SHA-256. */
function resourceName(appserviceId: string): string {
  let h = 0x811c9dc5;
  for (const ch of appserviceId) h = Math.imul(h ^ ch.charCodeAt(0), 0x01000193) >>> 0;
  return `bridge-${h.toString(16).padStart(8, "0")}`;
}

function newTokens(id: string) {
  const rand = () => Math.random().toString(36).slice(2, 10);
  return { as: `as_${id}_${rand()}`, hs: `hs_${id}_${rand()}` };
}

function makeInstance(
  offering: MockOffering,
  userId: string | null,
  fields: Partial<Omit<MockInstance, "tokens">>,
): MockInstance {
  const s = short(offering.type);
  const localpart = userId ? userId.slice(1).split(":")[0] : null;
  const appserviceId = localpart ? `${s}-${encodeLocalpart(localpart)}` : s;
  return {
    type: offering.type,
    user_id: userId,
    state: "requested",
    reason: null,
    appservice_id: appserviceId,
    bot: localpart ? `@${s}bot_${encodeLocalpart(localpart)}:${SERVER}` : `@${s}bot:${SERVER}`,
    deployment: null,
    health: null,
    created_at: new Date().toISOString(),
    ready_at: null,
    tokens: newTokens(appserviceId),
    ...fields,
  };
}

function deploymentFor(
  offering: MockOffering,
  instance: Pick<BridgeInstance, "appservice_id">,
  phase: "Pending" | "Ready" | "Degraded",
  message: string | null,
) {
  const name = resourceName(instance.appservice_id ?? "");
  const port = catalogueEntry(offering.type)?.port ?? 29999;
  return {
    namespace: NAMESPACE,
    name,
    image: imageOf(offering),
    service_url: `http://${name}.${NAMESPACE}.svc:${port}`,
    phase,
    ready: phase === "Ready",
    message,
  };
}

function seed() {
  const whatsapp: MockOffering = {
    type: "mautrix-whatsapp",
    enabled: true,
    runtime: "cluster",
    imageTag: "v0.12.1",
    access: { all_local_users: true, users: [] },
    options: { encryption: true, double_puppeting: true, backfill: true },
    created_at: iso(14 * DAY),
  };
  const imessage: MockOffering = {
    type: "mautrix-imessage",
    enabled: true,
    runtime: "elsewhere",
    imageTag: "latest",
    access: { all_local_users: false, users: ["@alice:example.org"] },
    options: { encryption: true, double_puppeting: true, backfill: false },
    created_at: iso(3 * DAY),
  };

  const ready = (userId: string, age: number): MockInstance => {
    const i = makeInstance(whatsapp, userId, {
      state: "ready",
      health: "healthy",
      created_at: iso(age),
      ready_at: iso(age - 90_000),
    });
    i.deployment = deploymentFor(whatsapp, i, "Ready", null);
    return i;
  };
  const alice = ready("@alice:example.org", 12 * DAY);
  const ops = ready("@ops:example.org", 13 * DAY);
  const carol = makeInstance(whatsapp, "@carol:example.org", {
    state: "starting",
    reason: "Waiting for the bridge to answer this server's ping.",
    health: "unknown",
    created_at: iso(70_000),
  });
  carol.deployment = deploymentFor(whatsapp, carol, "Pending", "Waiting for the pod to be ready");
  const dave = makeInstance(whatsapp, "@dave:example.org", {
    state: "failed",
    reason: "The cluster could not pull the bridge's image (ImagePullBackOff).",
    health: "down",
    created_at: iso(2 * 3_600_000),
  });
  dave.deployment = deploymentFor(
    whatsapp,
    dave,
    "Degraded",
    'ImagePullBackOff: Back-off pulling image "dock.mau.dev/mautrix/whatsapp:v0.12.1": toomanyrequests: rate limit exceeded',
  );

  const aliceMac = makeInstance(imessage, "@alice:example.org", {
    state: "starting",
    reason: "Waiting for its first ping. It runs elsewhere: download its files and start it there.",
    health: "unknown",
    created_at: iso(2 * DAY),
  });

  return {
    offerings: [whatsapp, imessage],
    instances: {
      [whatsapp.type]: [alice, ops, carol, dave],
      [imessage.type]: [aliceMac],
    } as Record<string, MockInstance[]>,
  };
}

let state = seed();

/** Puts the fixtures back as they started (Vitest calls this between tests). */
export function resetBridgeOfferings(): void {
  state = seed();
  deploymentTarget.current = DEPLOYMENT_AVAILABLE;
}

export function findOffering(type: string): MockOffering | undefined {
  return state.offerings.find((o) => o.type === type);
}

/**
 * Where a clock-driven instance has got to. Cluster: requested, registered, deploying, starting,
 * ready over about ten seconds. Elsewhere: it registers and then waits for a ping that only
 * comes when someone runs it.
 */
function advance(offering: MockOffering, instance: MockInstance): MockInstance {
  if (instance.startedAt === undefined) return instance;
  const t = Date.now() - instance.startedAt;
  const base = { ...instance };
  if (t < 1_500) return { ...base, state: "requested", reason: null, deployment: null };
  if (t < 3_000) return { ...base, state: "registered", reason: null, deployment: null };
  if (offering.runtime === "elsewhere") {
    return {
      ...base,
      state: "starting",
      reason:
        "Waiting for its first ping. It runs elsewhere: download its files and start it there.",
      health: "unknown",
    };
  }
  if (t < 6_000)
    return {
      ...base,
      state: "deploying",
      reason: null,
      deployment: deploymentFor(offering, base, "Pending", "Pulling the image"),
    };
  if (t < 9_000)
    return {
      ...base,
      state: "starting",
      reason: "Waiting for the bridge to answer this server's ping.",
      health: "unknown",
      deployment: deploymentFor(offering, base, "Pending", "Waiting for the pod to be ready"),
    };
  return {
    ...base,
    state: "ready",
    reason: null,
    health: "healthy",
    ready_at: new Date(instance.startedAt + 9_000).toISOString(),
    deployment: deploymentFor(offering, base, "Ready", null),
  };
}

function publicInstance(instance: MockInstance): BridgeInstance {
  const { startedAt: _startedAt, tokens: _tokens, ...rest } = instance;
  return rest;
}

export function instancesOf(type: string): BridgeInstance[] {
  const offering = findOffering(type);
  if (!offering) return [];
  return (state.instances[type] ?? []).map((i) => publicInstance(advance(offering, i)));
}

function findInstance(type: string, userSegment: string): MockInstance | undefined {
  const userId = userSegment === "_" ? null : userSegment;
  return (state.instances[type] ?? []).find((i) => i.user_id === userId);
}

/** The instance registered under `appserviceId`, for `GET /appservices/{id}/logins`. */
export function instanceByAppservice(appserviceId: string): BridgeInstance | undefined {
  for (const [type, instances] of Object.entries(state.instances)) {
    const offering = findOffering(type);
    const instance = instances.find((i) => i.appservice_id === appserviceId);
    if (offering && instance) return publicInstance(advance(offering, instance));
  }
  return undefined;
}

export function getInstance(type: string, userSegment: string): BridgeInstance | undefined {
  const offering = findOffering(type);
  const instance = findInstance(type, userSegment);
  return offering && instance ? publicInstance(advance(offering, instance)) : undefined;
}

export function offeringView(offering: MockOffering): BridgeOffering {
  const entry = catalogueEntry(offering.type);
  const mode = entry?.mode ?? "per_user";
  const counts: Record<string, number> = {};
  for (const i of instancesOf(offering.type)) counts[i.state] = (counts[i.state] ?? 0) + 1;
  return {
    type: offering.type,
    name: entry?.name ?? offering.type,
    mode,
    enabled: offering.enabled,
    runtime: offering.runtime,
    image: imageOf(offering),
    front_door: mode === "per_user" ? `@${short(offering.type)}bot:${SERVER}` : null,
    access: offering.access,
    options: offering.options,
    instances: counts,
    created_at: offering.created_at,
  };
}

export function listOfferings(): BridgeOffering[] {
  return state.offerings.map(offeringView);
}

/** Why a `PUT` would be refused, or `null`; the same reasons the real server gives a 400 for. */
export function offeringRefusal(type: string, body: BridgeOfferingRequest): string | null {
  const entry = catalogueEntry(type);
  if (body.runtime === "cluster" && !deploymentTarget.current.available) {
    return `This server cannot deploy bridges: ${deploymentTarget.current.reason ?? "no deployment target"}`;
  }
  if (body.runtime === "cluster" && entry?.deployable === false) {
    return `${entry.name ?? type} cannot run from a rendered config alone; offer it with runtime "elsewhere".`;
  }
  return null;
}

export function putOffering(type: string, body: BridgeOfferingRequest): BridgeOffering {
  const existing = findOffering(type);
  const entry = catalogueEntry(type);
  const next: MockOffering = {
    type,
    enabled: body.enabled ?? existing?.enabled ?? true,
    runtime: body.runtime ?? existing?.runtime ?? "elsewhere",
    imageTag: body.image_tag || existing?.imageTag || "latest",
    access: {
      all_local_users: body.access?.all_local_users ?? existing?.access.all_local_users ?? true,
      users: body.access?.users ?? existing?.access.users ?? [],
    },
    options: {
      encryption: body.options?.encryption ?? existing?.options.encryption ?? false,
      double_puppeting:
        body.options?.double_puppeting ?? existing?.options.double_puppeting ?? false,
      backfill: body.options?.backfill ?? existing?.options.backfill ?? false,
    },
    created_at: existing?.created_at ?? new Date().toISOString(),
  };
  if (existing) state.offerings[state.offerings.indexOf(existing)] = next;
  else state.offerings.push(next);
  state.instances[type] ??= [];
  // A shared type has exactly one instance, owned by nobody, made with the offering.
  if ((entry?.mode ?? "per_user") === "shared" && state.instances[type].length === 0) {
    state.instances[type].push(makeInstance(next, null, { startedAt: Date.now() }));
  }
  return offeringView(next);
}

export function deleteOffering(type: string): void {
  state.offerings = state.offerings.filter((o) => o.type !== type);
  delete state.instances[type];
}

/** Creates (or, for a failed one, retries) a user's instance; idempotent otherwise. */
export function putInstance(type: string, userSegment: string): BridgeInstance | undefined {
  const offering = findOffering(type);
  if (!offering) return undefined;
  const existing = findInstance(type, userSegment);
  if (existing) {
    const current = advance(offering, existing);
    if (current.state === "failed") {
      Object.assign(existing, {
        state: "requested",
        reason: null,
        deployment: null,
        health: null,
        startedAt: Date.now(),
      });
    }
    return publicInstance(advance(offering, existing));
  }
  const instance = makeInstance(offering, userSegment === "_" ? null : userSegment, {
    startedAt: Date.now(),
  });
  state.instances[type] = [...(state.instances[type] ?? []), instance];
  return publicInstance(advance(offering, instance));
}

export function deleteInstance(type: string, userSegment: string): boolean {
  const instance = findInstance(type, userSegment);
  if (!instance) return false;
  state.instances[type] = (state.instances[type] ?? []).filter((i) => i !== instance);
  return true;
}

/** The files to run an instance elsewhere, with its own tokens, as the real render writes them. */
export function instanceFiles(type: string, userSegment: string): BridgeInstanceFiles | undefined {
  const offering = findOffering(type);
  const instance = findInstance(type, userSegment);
  if (!offering || !instance) return undefined;
  const entry = catalogueEntry(type);
  const id = instance.appservice_id ?? short(type);
  const port = entry?.port ?? 29999;
  const botLocalpart = (instance.bot ?? "").slice(1).split(":")[0];
  const ghostPrefix = botLocalpart.replace(/bot_/, "_");
  const name = resourceName(id);
  const url =
    offering.runtime === "cluster"
      ? `http://${name}.${NAMESPACE}.svc:${port}`
      : `http://${id}:${port}`;
  const registration_yaml = [
    `id: ${id}`,
    `url: ${url}`,
    `as_token: ${instance.tokens.as}`,
    `hs_token: ${instance.tokens.hs}`,
    `sender_localpart: ${botLocalpart}`,
    "rate_limited: false",
    "namespaces:",
    "  users:",
    `    - regex: '@${ghostPrefix}_.*:example\\.org'`,
    "      exclusive: true",
    `    - regex: '@${botLocalpart}:example\\.org'`,
    "      exclusive: true",
    ...(instance.user_id && offering.options.double_puppeting
      ? [`    - regex: '${instance.user_id.replaceAll(".", "\\.")}'`, "      exclusive: false"]
      : []),
    "de.sorunome.msc2409.push_ephemeral: true",
    `org.matrix.msc3202: ${offering.options.encryption}`,
    `io.myelin.bridge_type: ${type}`,
    `io.myelin.bridge_instance: ${instance.user_id ?? "_"}`,
  ].join("\n");
  const config_yaml = entry?.renders_config
    ? [
        "homeserver:",
        `  address: ${offering.runtime === "cluster" ? DEPLOYMENT_AVAILABLE.homeserver_url : "https://matrix.example.org"}`,
        `  domain: ${SERVER}`,
        "appservice:",
        `  address: ${url}`,
        "  hostname: 0.0.0.0",
        `  port: ${port}`,
        `  id: ${id}`,
        "  bot:",
        `    username: ${botLocalpart}`,
        `  as_token: ${instance.tokens.as}`,
        `  hs_token: ${instance.tokens.hs}`,
        "database:",
        "  type: sqlite3-fk-wal",
        `  uri: file:/data/${id}.db?_txlock=immediate`,
        "bridge:",
        "  permissions:",
        ...(instance.user_id ? [`    "${instance.user_id}": admin`] : [`    "${SERVER}": user`]),
        "encryption:",
        `  allow: ${offering.options.encryption}`,
        "backfill:",
        `  enabled: ${offering.options.backfill}`,
      ].join("\n")
    : null;
  const compose_yaml = [
    "services:",
    `  ${id}:`,
    `    image: ${imageOf(offering)}`,
    "    volumes:",
    `      - ./${id}:/data`,
    `    ports: ["${port}:${port}"]`,
    "    restart: unless-stopped",
  ].join("\n");
  const manifest_yaml = [
    "apiVersion: v1",
    "kind: Secret",
    "metadata:",
    `  name: ${name}-files`,
    "stringData:",
    "  registration.yaml: |",
    ...registration_yaml.split("\n").map((l) => `    ${l}`),
    ...(config_yaml ? ["  config.yaml: |", ...config_yaml.split("\n").map((l) => `    ${l}`)] : []),
    "---",
    "apiVersion: hs.matrix.org/v1alpha1",
    "kind: Bridge",
    "metadata:",
    `  name: ${name}`,
    "spec:",
    `  bridgeType: ${type}`,
    `  appserviceId: ${id}`,
    `  image: { repository: ${imageOf(offering).split(":")[0]}, tag: ${offering.imageTag} }`,
    `  port: ${port}`,
    `  filesSecret: ${name}-files`,
    "  storage: { size: 1Gi }",
  ].join("\n");
  return { config_yaml, registration_yaml, compose_yaml, manifest_yaml };
}
