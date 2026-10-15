import type {
  AppService,
  AppServiceHealth,
  AppServiceBacklogEntry,
  BridgeLogins,
  BridgeType,
} from "@/api/bridges";

const now = Date.now();
const iso = (msAgo: number) => new Date(now - msAgo).toISOString();

/** Each `queue` agrees with the bridge's `appserviceBacklog` entry below. */
export const appservices: AppService[] = [
  {
    id: "whatsapp",
    sender_localpart: "whatsappbot",
    url: "http://mautrix-whatsapp.bridges.svc:29318",
    namespaces: {},
    rate_limited: false,
    protocols: ["whatsapp"],
    bridge_type: "mautrix-whatsapp",
    paused: false,
    health: "healthy",
    created_at: iso(30 * 24 * 3_600_000),
    double_puppet_for: null,
    links: { login_url: null },
    queue: { pending: 0, dead_lettered: 0, oldest_pending_age_ms: null },
  },
  {
    id: "telegram",
    sender_localpart: "telegrambot",
    url: "http://mautrix-telegram.bridges.svc:29317",
    namespaces: {},
    rate_limited: false,
    protocols: ["telegram"],
    bridge_type: "mautrix-telegram",
    paused: false,
    health: "degraded",
    created_at: iso(20 * 24 * 3_600_000),
    double_puppet_for: null,
    links: { login_url: null },
    queue: { pending: 1, dead_lettered: 0, oldest_pending_age_ms: 6 * 60_000 },
  },
  {
    id: "signal",
    sender_localpart: "signalbot",
    url: "http://mautrix-signal.bridges.svc:29328",
    namespaces: {},
    rate_limited: false,
    protocols: ["signal"],
    bridge_type: "mautrix-signal",
    paused: false,
    health: "down",
    created_at: iso(10 * 24 * 3_600_000),
    double_puppet_for: null,
    links: { login_url: null },
    queue: { pending: 0, dead_lettered: 2, oldest_pending_age_ms: null },
  },
  {
    id: "discord",
    sender_localpart: "discordbot",
    url: "http://localhost:29334",
    namespaces: {},
    rate_limited: false,
    protocols: ["discord"],
    bridge_type: "mautrix-discord",
    paused: true,
    health: "paused",
    created_at: iso(60 * 24 * 3_600_000),
    double_puppet_for: null,
    links: { login_url: null },
    queue: { pending: 0, dead_lettered: 0, oldest_pending_age_ms: null },
  },
  {
    id: "slack",
    sender_localpart: "slackbot",
    url: "http://localhost:29335",
    namespaces: {},
    rate_limited: false,
    protocols: ["slack"],
    bridge_type: "mautrix-slack",
    paused: false,
    health: "unknown",
    created_at: iso(2 * 3_600_000),
    double_puppet_for: null,
    links: { login_url: null },
    queue: { pending: 0, dead_lettered: 0, oldest_pending_age_ms: null },
  },
  {
    // Brandon's Slack bridge from the Slack offering, which this server deploys: its volume
    // filled and the bridge stopped (the demo's, 2026-10-15). `volume` is the instance's, added
    // when the row is answered (`handlers.ts`).
    id: "slack-brandon",
    sender_localpart: "slackbot_brandon",
    url: "http://bridge-slack-brandon.myelin.svc:29335",
    namespaces: {},
    rate_limited: false,
    protocols: ["slack"],
    bridge_type: "mautrix-slack",
    paused: false,
    health: "degraded",
    created_at: iso(30 * 24 * 3_600_000),
    double_puppet_for: null,
    links: { login_url: null },
    queue: { pending: 0, dead_lettered: 0, oldest_pending_age_ms: null },
  },
  {
    // Slack's double puppeting (decision 0046): no url, a shared claim on its owner, and
    // `io.myelin.double_puppet_for`. `GET /appservices?fold_double_puppets=true` leaves it out and
    // Slack's row names it in `double_puppet`.
    id: "slack-doublepuppet",
    sender_localpart: "slack-doublepuppet",
    url: null,
    namespaces: {
      users: [{ regex: "@alice:example\\.org", exclusive: false }],
    } as unknown as AppService["namespaces"],
    rate_limited: false,
    protocols: [],
    bridge_type: null,
    paused: false,
    health: "unknown",
    created_at: iso(2 * 3_600_000),
    double_puppet_for: "slack",
    links: { login_url: null },
    queue: { pending: 0, dead_lettered: 0, oldest_pending_age_ms: null },
  },
];

/** The registration key a double-puppeting registration names its bridge under (decision 0046). */
export const DOUBLE_PUPPET_FOR_KEY = "io.myelin.double_puppet_for";

/**
 * `appservice` as the server answers it: `double_puppet` is the registration whose
 * `double_puppet_for` names it (the first by id, should two), read from the whole registry
 * (`AdminAppservice::link_double_puppets`); `health_reason` is its health's `reason`, and `queue`
 * is counted from its backlog.
 */
export function linkedAppservice(appservice: AppService): AppService {
  const named = appservices
    .filter((o) => o.id !== appservice.id && o.double_puppet_for === appservice.id)
    .map((o) => o.id ?? "")
    .sort();
  const id = appservice.id ?? "";
  return {
    ...appservice,
    double_puppet: named[0] ?? null,
    health_reason: appserviceHealth[id]?.reason ?? null,
    queue: queueOf(id, appservice.queue),
  };
}

/**
 * Whether `GET /appservices?fold_double_puppets=true` leaves `appservice` out: it is a
 * double-puppeting registration whose bridge is registered and names it. One whose bridge is gone
 * is listed (`AdminAppservice::folded_double_puppets`).
 */
export function isFoldedDoublePuppet(appservice: AppService): boolean {
  const bridgeId = appservice.double_puppet_for;
  if (!bridgeId || bridgeId === appservice.id) return false;
  const bridge = findAppservice(bridgeId);
  return bridge !== undefined && linkedAppservice(bridge).double_puppet === appservice.id;
}

/**
 * `reason` is the server's own wording (`hs_appservice::registry::status_reason`): null when
 * healthy, otherwise why the status is what it is, ending with the last error when there is one.
 */
export const appserviceHealth: Record<string, AppServiceHealth> = {
  whatsapp: { status: "healthy", reason: null, last_ping_at: iso(60_000), last_error: null },
  telegram: {
    status: "degraded",
    reason:
      "The last 3 transactions to it failed; it is marked down at 20. It is healthy again once a transaction is delivered or a ping is answered. The last error: Rate limited by Telegram (FLOOD_WAIT 60)",
    last_ping_at: iso(6 * 60_000),
    last_error: "Rate limited by Telegram (FLOOD_WAIT 60)",
  },
  signal: {
    status: "down",
    reason:
      "The last 24 transactions to it failed, 20 or more in a row; nothing is getting through. It is healthy again once a transaction is delivered or a ping is answered. The last error: Connection refused: signal daemon not responding on :29328",
    last_ping_at: iso(3 * 3_600_000),
    last_error: "Connection refused: signal daemon not responding on :29328",
  },
  discord: {
    status: "paused",
    reason: "Paused by an operator: transactions are queued and not sent until it is resumed.",
    last_ping_at: iso(20 * 3_600_000),
    last_error: null,
  },
  slack: {
    status: "unknown",
    reason: "Nothing has been delivered to it or pinged yet, so there is nothing to judge.",
    last_ping_at: null,
    last_error: null,
  },
  "slack-brandon": {
    status: "degraded",
    reason:
      "The last 6 transactions to it failed; it is marked down at 20. It is healthy again once a transaction is delivered or a ping is answered. The last error: The bridge did not answer: its pod is not ready (Init:CrashLoopBackOff).",
    last_ping_at: iso(26 * 3_600_000),
    last_error: "The bridge did not answer: its pod is not ready (Init:CrashLoopBackOff).",
  },
  "slack-doublepuppet": {
    status: "unknown",
    reason:
      "It has no URL, so the server never sends it anything (a bridge's double puppeting registration, say).",
    last_ping_at: null,
    last_error: null,
  },
};

/** A backlog entry queued `msAgo` ago, with what it carries (OpenAPI 0.1.18). */
function entry(
  transaction_id: string,
  msAgo: number,
  rest: Omit<AppServiceBacklogEntry, "transaction_id" | "age_ms" | "enqueued_at">,
): AppServiceBacklogEntry {
  return { transaction_id, age_ms: msAgo, enqueued_at: iso(msAgo), ...rest };
}

/**
 * Each bridge's queue, oldest first as the server pages it. Signal's two dead letters are the
 * demo's case: one from an incident three days ago (encrypted events in rooms the bridge does
 * not bridge, which it refused ten times), and one from this afternoon's outage.
 */
function seedBacklog(): Record<string, AppServiceBacklogEntry[]> {
  return {
    whatsapp: [],
    telegram: [
      entry("tx_88a", 6 * 60_000, {
        attempts: 3,
        last_error: "FLOOD_WAIT_60",
        dead_lettered: false,
        events: 2,
        ephemeral: 1,
        to_device: 0,
        room_ids: ["!family:example.org"],
      }),
    ],
    signal: [
      entry("tx_700", 3 * 24 * 3_600_000, {
        attempts: 10,
        last_error: "the bridge answered 400 M_UNKNOWN: unsupported event type",
        dead_lettered: true,
        events: 48,
        ephemeral: 0,
        to_device: 0,
        room_ids: [
          "!OGEhHVWSdvArJzumhm:matrix.org",
          "!cJGsTSfRphIleQlbVM:example.org",
          "!vjbuWnLMDWdXUwqDjq:matrix.org",
          "!hqgHbCbDUNtzgqzqyA:element.io",
          "!AbCdEfGhIjKlMnOpQr:example.org",
        ],
      }),
      entry("tx_701", 3 * 3_600_000, {
        attempts: 8,
        last_error: "connect: connection refused",
        dead_lettered: true,
        events: 3,
        ephemeral: 2,
        to_device: 1,
        room_ids: ["!signalnotes:example.org"],
      }),
    ],
    discord: [],
    slack: [],
    "slack-brandon": [],
    "slack-doublepuppet": [],
  };
}

export const appserviceBacklog: Record<string, AppServiceBacklogEntry[]> = seedBacklog();

/**
 * Puts every bridge's queue back as it started (Vitest calls this between tests): a replay or a
 * discard changes it.
 */
export function resetAppserviceBacklog(): void {
  for (const key of Object.keys(appserviceBacklog)) delete appserviceBacklog[key];
  Object.assign(appserviceBacklog, seedBacklog());
}

/**
 * `AppService.queue` as the server reads it from the bridge's delivery queue: counted from
 * {@link appserviceBacklog}, so a replay or a discard shows on the list too.
 */
function queueOf(id: string, fallback: AppService["queue"]): AppService["queue"] {
  const entries = appserviceBacklog[id];
  if (!entries) return fallback;
  const pending = entries.filter((e) => !e.dead_lettered);
  return {
    pending: pending.length,
    dead_lettered: entries.length - pending.length,
    oldest_pending_age_ms:
      pending.length === 0 ? null : Math.max(...pending.map((e) => e.age_ms ?? 0)),
  };
}

/**
 * `POST /appservices/{id}/discard` in the mock: deletes the dead letters that match (every one
 * with neither filter; `before` keeps those queued at or after it) and never a waiting entry.
 * Undefined for an unknown appservice.
 */
export function discardDeadLetters(
  id: string,
  request: { transaction_ids?: string[]; before?: string },
): number | undefined {
  const entries = appserviceBacklog[id];
  if (!entries) return undefined;
  const before = request.before === undefined ? undefined : Date.parse(request.before);
  const ids = request.transaction_ids === undefined ? undefined : new Set(request.transaction_ids);
  const matches = (e: AppServiceBacklogEntry) =>
    Boolean(e.dead_lettered) &&
    (ids === undefined || ids.has(e.transaction_id ?? "")) &&
    (before === undefined || Date.parse(e.enqueued_at ?? "") < before);
  const kept = entries.filter((e) => !matches(e));
  appserviceBacklog[id] = kept;
  return entries.length - kept.length;
}

/**
 * `POST /appservices/{id}/replay` in the mock: every dead letter is sent again and, the mock's
 * bridges being well, delivered; waiting entries are left to the queue. Undefined for an unknown
 * appservice.
 */
export function replayDeadLetters(id: string): number | undefined {
  const entries = appserviceBacklog[id];
  if (!entries) return undefined;
  const kept = entries.filter((e) => !e.dead_lettered);
  appserviceBacklog[id] = kept;
  return entries.length - kept.length;
}

export const appserviceRegistration: Record<string, Record<string, unknown>> = Object.fromEntries(
  appservices.map((a) => [
    a.id,
    {
      id: a.id,
      url: a.url,
      as_token: `as_token_${a.id}_${(a.id ?? "").length}f2a`,
      hs_token: `hs_token_${a.id}_${(a.id ?? "").length}c11`,
      sender_localpart: a.sender_localpart,
      namespaces: a.namespaces,
      rate_limited: a.rate_limited,
      ...(a.double_puppet_for ? { [DOUBLE_PUPPET_FOR_KEY]: a.double_puppet_for } : {}),
    },
  ]),
);

export function findAppservice(id: string): AppService | undefined {
  return appservices.find((a) => a.id === id);
}

/** The registration key a bridge's provisioning secret is kept under (`hs_admin::bridge_types`). */
export const PROVISIONING_SECRET_KEY = "io.myelin.provisioning_secret";

/**
 * Registrations made before the server kept a bridge's provisioning secret (2026-10-01): the
 * Telegram bridge, so the Sign in tab's "add the secret" action can be seen. A `PATCH` that sets
 * {@link PROVISIONING_SECRET_KEY} takes it off this list.
 */
const WITHOUT_SECRET = new Set<string>();

function seedWithoutSecret(): void {
  WITHOUT_SECRET.clear();
  WITHOUT_SECRET.add("telegram");
}
seedWithoutSecret();

/** Puts the registrations' secrets back as they started (Vitest calls this between tests). */
export function resetAppserviceSecrets(): void {
  seedWithoutSecret();
}

/**
 * `PATCH /appservices/{id}` in the mock: an RFC 7396 merge patch of the registration. Only the
 * provisioning secret and the plain fields the interface sends are kept; `null` removes the
 * secret.
 */
export function patchAppservice(
  id: string,
  patch: Record<string, unknown>,
): AppService | undefined {
  const appservice = findAppservice(id);
  if (!appservice) return undefined;
  if (PROVISIONING_SECRET_KEY in patch) {
    const secret = patch[PROVISIONING_SECRET_KEY];
    if (typeof secret === "string" && secret !== "") WITHOUT_SECRET.delete(id);
    else WITHOUT_SECRET.add(id);
  }
  if (typeof patch.url === "string" || patch.url === null) appservice.url = patch.url;
  if (typeof patch.rate_limited === "boolean") appservice.rate_limited = patch.rate_limited;
  if (patch.namespaces && typeof patch.namespaces === "object")
    appservice.namespaces = patch.namespaces as Record<string, never>;
  return appservice;
}

/**
 * What a ping does in the mock: a bridge whose url points at localhost or a `.svc` host answers,
 * except `signal`, which stays down as its health fixture says; a bridge with no url cannot be
 * pinged at all.
 */
export function pingAppservice(id: string): AppService | undefined {
  const appservice = findAppservice(id);
  if (!appservice) return undefined;
  const now = new Date().toISOString();
  if (appservice.paused) {
    appserviceHealth[id] = {
      status: "paused",
      reason: "Paused by an operator: transactions are queued and not sent until it is resumed.",
      last_ping_at: now,
      last_error: null,
    };
  } else if (!appservice.url) {
    appserviceHealth[id] = {
      status: "down",
      reason:
        "Its last ping failed, and nothing has been delivered to it since. The last error: the registration has no url, so there is nothing to ping",
      last_ping_at: now,
      last_error: "the registration has no url, so there is nothing to ping",
    };
  } else if (id === "signal") {
    appserviceHealth[id] = {
      ...appserviceHealth[id],
      status: "down",
      last_ping_at: now,
      last_error: "Connection refused: signal daemon not responding on :29328",
    };
  } else {
    // A ping answered after failures marks the bridge healthy again.
    appserviceHealth[id] = { status: "healthy", reason: null, last_ping_at: now, last_error: null };
  }
  appservice.health = appserviceHealth[id]!.status;
  return linkedAppservice(appservice);
}

/** A bridge instance of an offering, as `appserviceLogins` needs it (`./bridge-offerings`). */
export interface LoginsSubject {
  bridge_type: string | null;
  health: string | null;
  /** The person a per-user instance belongs to; asked about when no user is named. */
  owner: string | null;
}

/**
 * What `GET /appservices/{id}/logins` answers in the mock, the way the real server does for each
 * case: a mautrix bridge asked about `@alice:example.org` (signed in to WhatsApp), anyone else
 * (not signed in), the Signal bridge (down, so the answer carries the error), a type without a
 * provisioning API (`supported: false`), and a shared bridge not told whom to ask about (`400`).
 */
export function appserviceLogins(
  id: string,
  requestedUser: string | null,
  provisioning: Pick<BridgeType, "provisioning_api" | "provisioning_note"> | undefined,
  instance?: LoginsSubject,
): { status: number; body: unknown } {
  const registered = findAppservice(id);
  const appservice: LoginsSubject | undefined = registered
    ? {
        bridge_type: registered.bridge_type ?? null,
        health: registered.health ?? null,
        // A person's bridge from an offering registered here too is asked about its owner.
        owner: instance?.owner ?? null,
      }
    : instance;
  if (!appservice)
    return {
      status: 404,
      body: { type: "urn:hs:problem:not-found", title: "Not found", status: 404 },
    };
  // A per-user instance is asked about its owner when nobody is named.
  const userId = requestedUser ?? appservice.owner;
  const api = provisioning?.provisioning_api ?? "none";
  const base = {
    appservice_id: id,
    bridge_type: appservice.bridge_type ?? null,
    provisioning_api: api,
    user_id: userId,
    logins: [] as BridgeLogins["logins"],
    cached: false,
  };
  if (api !== "mautrix_v3")
    return {
      status: 200,
      body: {
        ...base,
        supported: false,
        reason:
          provisioning?.provisioning_note ??
          "This appservice was not added from the bridge catalogue, so the server does not know whether it has a provisioning API; the bridge keeps who has signed in itself.",
      },
    };
  if (WITHOUT_SECRET.has(id))
    return {
      status: 200,
      body: {
        ...base,
        supported: false,
        reason: `The registration carries no provisioning secret: it was made before the server kept one. Put the bridge's provisioning.shared_secret (from its config.yaml) in the registration's ${PROVISIONING_SECRET_KEY} key with a merge patch, and the server can ask it.`,
      },
    };
  if (!userId) {
    const detail =
      "user_id is required: this bridge is shared, so name the Matrix user to ask about";
    return {
      status: 400,
      body: {
        type: "urn:hs:problem:validation-failed",
        title: "Validation failed",
        status: 400,
        detail,
        errors: [{ pointer: "/user_id", detail }],
      },
    };
  }
  if (appservice.health === "down")
    return {
      status: 200,
      body: {
        ...base,
        supported: true,
        error: {
          status: 502,
          reason: "unreachable",
          detail: "the server could not reach the bridge: connection refused",
        },
      },
    };
  const checked_at = new Date().toISOString();
  if (userId === "@alice:example.org" && appservice.bridge_type === "mautrix-whatsapp")
    return {
      status: 200,
      body: {
        ...base,
        supported: true,
        signed_in: true,
        checked_at,
        logins: [
          {
            user_id: userId,
            remote_id: "15551234567",
            remote_name: "+1 555-123-4567",
            state: "connected",
            since: iso(3 * 24 * 3_600_000),
          },
        ],
      },
    };
  return { status: 200, body: { ...base, supported: true, signed_in: false, checked_at } };
}
