import type {
  BridgeDeploymentTarget,
  BridgeInstance,
  BridgeInstanceFiles,
  BridgeOffering,
  BridgeOfferingRequest,
} from "@/api/bridges";
import { appservices } from "./appservices";
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

/** The entry's port, as the real server takes it from the catalogue. */
function portOf(type: string): number {
  return catalogueEntry(type)?.port ?? 29999;
}

/**
 * What the instance's `reason` says while it waits to be run: for a program on a Mac, that the
 * Mac's address has to be set and the files run there; for anything else run elsewhere, the files.
 */
function waitingReason(type: string): string {
  return catalogueEntry(type)?.runs_on === "mac"
    ? "Waiting for the bridge on the Mac to answer this server. Set the Mac's address, download its files, and run it there."
    : "Waiting for its first ping. It runs elsewhere: download its files and start it there.";
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
    // The registration's url: the cluster Service, or the placeholder named after the
    // appservice until an administrator says where the bridge really listens.
    url:
      offering.runtime === "cluster"
        ? `http://${resourceName(appserviceId)}.${NAMESPACE}.svc:${portOf(offering.type)}`
        : `http://${appserviceId}:${portOf(offering.type)}`,
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
    // The catalogue's pinned build of the program, not a container tag.
    imageTag: "300ba6d0",
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
      chat_started_by: "owner",
      // What the manager writes into the bridge's config as WhatsApp's device name.
      device_name: `Myelin WhatsApp bridge for ${userId.slice(1).split(":")[0]} (${SERVER})`,
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
    reason: waitingReason(imessage.type),
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
      reason: waitingReason(offering.type),
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

/**
 * Appservices registered by hand for `type`'s network (the catalogue key, as the manager's
 * first test of `hs_bridges::overlap`), not one of its instances: the demo's shared WhatsApp
 * bridge beside the offering that replaces it.
 */
export function handRegisteredOverlaps(type: string) {
  const instanceIds = new Set(instancesOf(type).map((i) => i.appservice_id));
  const name = catalogueEntry(type)?.name ?? type;
  return appservices
    .filter((a) => a.bridge_type === type && !instanceIds.has(a.id))
    .map((a) => ({ id: a.id ?? "", sender_localpart: a.sender_localpart ?? "", name }));
}

/** `AppServiceHealth.overlaps_offering` for appservice `id`, as the server words it. */
export function overlapOfAppservice(id: string) {
  for (const offering of state.offerings) {
    const found = handRegisteredOverlaps(offering.type).find((o) => o.id === id);
    if (!found) continue;
    const mode = catalogueEntry(offering.type)?.mode ?? "per_user";
    const door = mode === "per_user" ? `@${short(offering.type)}bot:${SERVER}` : null;
    const getOne = door
      ? `people get their own ${found.name} bridge by messaging ${door}`
      : `one ${found.name} bridge is run for everyone`;
    return {
      type: offering.type,
      name: found.name,
      front_door: door,
      detail: `${found.name} is offered on this server now: ${getOne}. This bridge was registered by hand and it was created from the catalogue's ${found.name} entry, so a message for a ${found.name} ghost user is delivered to it and to the person's own instance. Pause it here to stop delivering to it now; once everyone who used it has their own instance, stop it where it runs and remove it here. docs/bridges/mautrix.md has the steps.`,
    };
  }
  return null;
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
    overlapping_appservices: handRegisteredOverlaps(offering.type).map((o) => ({
      id: o.id,
      sender_localpart: o.sender_localpart,
      detail: `${o.id} (bot @${o.sender_localpart}:${SERVER}) was registered by hand and it was created from the catalogue's ${o.name} entry, so a message for a ${o.name} ghost user is delivered to it as well as to the person's own instance. Once everyone who used it has their own instance, stop it where it runs and remove it from its page.`,
    })),
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
    // No tag asked for: the catalogue's pin, never `latest` (decision 0037).
    imageTag: body.image_tag || existing?.imageTag || entry?.image?.split(":").pop() || "latest",
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

/**
 * `BridgeInstanceRequest.address` as the real server reads it: a bare host takes the type's
 * port, `host:port` is used as given, and a full `http(s)://` URL is kept whole. Returns the
 * registration url to store, or why the address is refused (a 400 to the caller).
 */
export function instanceUrlFor(
  type: string,
  address: string,
): { url: string } | { refusal: string } {
  const value = address.trim();
  if (!value) return { refusal: "address names no host." };
  const hasScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(value);
  if (hasScheme && !/^https?:\/\//i.test(value)) {
    return { refusal: `address must be http:// or https://, not ${value.split(":")[0]}://.` };
  }
  let parsed: URL;
  try {
    parsed = new URL(hasScheme ? value : `http://${value}`);
  } catch {
    return { refusal: `address "${value}" is not a host, host:port or URL.` };
  }
  if (!parsed.hostname) return { refusal: `address "${value}" names no host.` };
  if (hasScheme) return { url: value };
  const port = parsed.port || String(portOf(type));
  return { url: `http://${parsed.hostname}:${port}` };
}

/**
 * Why a `PUT` with an address would be refused, or `null`: the real server rewrites the
 * registration only for an instance it does not deploy itself.
 */
export function addressRefusal(type: string, address: string): string | null {
  const offering = findOffering(type);
  if (!offering) return null;
  if (offering.runtime === "cluster") {
    return `${catalogueEntry(type)?.name ?? type} runs in this cluster, where this server sets each bridge's address itself; an address can only be set for a bridge run elsewhere.`;
  }
  const checked = instanceUrlFor(type, address);
  return "refusal" in checked ? checked.refusal : null;
}

/**
 * Creates (or, for a failed one, retries) a user's instance; idempotent otherwise. With an
 * `address` (checked with `addressRefusal` first), the instance's url is rewritten with it, as
 * are the files `instanceFiles` renders.
 */
export function putInstance(
  type: string,
  userSegment: string,
  address?: string | null,
): BridgeInstance | undefined {
  const offering = findOffering(type);
  if (!offering) return undefined;
  let instance = findInstance(type, userSegment);
  if (instance) {
    const current = advance(offering, instance);
    if (current.state === "failed") {
      Object.assign(instance, {
        state: "requested",
        reason: null,
        deployment: null,
        health: null,
        startedAt: Date.now(),
      });
    }
  } else {
    instance = makeInstance(offering, userSegment === "_" ? null : userSegment, {
      startedAt: Date.now(),
    });
    state.instances[type] = [...(state.instances[type] ?? []), instance];
  }
  if (address != null && offering.runtime !== "cluster") {
    const checked = instanceUrlFor(type, address);
    if ("url" in checked) instance.url = checked.url;
  }
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
  // The registration's url, as the instance carries it (the Mac's address once it is set).
  const url =
    instance.url ??
    (offering.runtime === "cluster"
      ? `http://${name}.${NAMESPACE}.svc:${port}`
      : `http://${id}:${port}`);
  const mac = entry?.runs_on === "mac";
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
    // Not its owner: double puppeting is a registration of its own with no URL (decision 0046).
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
        `  uri: file:${mac ? "" : "/data/"}${id}.db?_txlock=immediate`,
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
  // For a program on a Mac: what the real server renders beside the two YAML files, which
  // fetches the pinned build when it is not there yet, clears its quarantine flag and starts it.
  const run_sh = mac
    ? [
        "#!/bin/sh",
        `# Runs ${entry?.name ?? type} for ${instance.user_id ?? "everyone"} on this Mac, beside config.yaml and registration.yaml.`,
        "set -eu",
        'cd "$(dirname "$0")"',
        `BUILD="${entry?.download_url ?? ""}"`,
        "if [ ! -x ./mautrix-imessage ]; then",
        `  echo "Downloading ${entry?.name ?? type} (${imageOf(offering)})..."`,
        '  curl -fsSL -o bridge.zip "$BUILD"',
        "  unzip -o bridge.zip mautrix-imessage libolm.3.dylib",
        "  rm -f bridge.zip",
        "  chmod +x ./mautrix-imessage",
        "fi",
        "xattr -d com.apple.quarantine ./mautrix-imessage ./libolm.3.dylib 2>/dev/null || true",
        "exec ./mautrix-imessage -c config.yaml -r registration.yaml",
      ].join("\n")
    : null;
  return { config_yaml, registration_yaml, compose_yaml, manifest_yaml, run_sh };
}
