import type {
  AppService,
  AppServiceHealth,
  AppServiceBacklogEntry,
  BridgeLogins,
  BridgeType,
} from "@/api/bridges";

const now = Date.now();
const iso = (msAgo: number) => new Date(now - msAgo).toISOString();

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
    links: { login_url: null },
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
    links: { login_url: null },
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
    links: { login_url: null },
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
    links: { login_url: null },
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
    links: { login_url: null },
  },
];

export const appserviceHealth: Record<string, AppServiceHealth> = {
  whatsapp: { status: "healthy", last_ping_at: iso(60_000), last_error: null },
  telegram: {
    status: "degraded",
    last_ping_at: iso(6 * 60_000),
    last_error: "Rate limited by Telegram (FLOOD_WAIT 60)",
  },
  signal: {
    status: "down",
    last_ping_at: iso(3 * 3_600_000),
    last_error: "Connection refused: signal daemon not responding on :29328",
  },
  discord: { status: "paused", last_ping_at: iso(20 * 3_600_000), last_error: null },
  slack: { status: "unknown", last_ping_at: null, last_error: null },
};

export const appserviceBacklog: Record<string, AppServiceBacklogEntry[]> = {
  whatsapp: [],
  telegram: [
    {
      transaction_id: "tx_88a",
      age_ms: 6 * 60_000,
      attempts: 3,
      last_error: "FLOOD_WAIT_60",
      dead_lettered: false,
    },
  ],
  signal: [
    {
      transaction_id: "tx_701",
      age_ms: 3 * 3_600_000,
      attempts: 8,
      last_error: "connect: connection refused",
      dead_lettered: true,
    },
    {
      transaction_id: "tx_700",
      age_ms: 3.2 * 3_600_000,
      attempts: 8,
      last_error: "connect: connection refused",
      dead_lettered: true,
    },
  ],
  discord: [],
  slack: [],
};

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
    },
  ]),
);

export function findAppservice(id: string): AppService | undefined {
  return appservices.find((a) => a.id === id);
}

/**
 * What `GET /appservices/{id}/logins` answers in the mock, the way the real server does for each
 * case: a mautrix bridge asked about `@alice:example.org` (signed in to WhatsApp), anyone else
 * (not signed in), the Signal bridge (down, so the answer carries the error), a type without a
 * provisioning API (`supported: false`), and a shared bridge not told whom to ask about (`400`).
 */
export function appserviceLogins(
  id: string,
  userId: string | null,
  provisioning: Pick<BridgeType, "provisioning_api" | "provisioning_note"> | undefined,
): { status: number; body: unknown } {
  const appservice = findAppservice(id);
  if (!appservice)
    return {
      status: 404,
      body: { type: "urn:hs:problem:not-found", title: "Not found", status: 404 },
    };
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
