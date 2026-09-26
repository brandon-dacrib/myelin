import type { AppService, AppServiceHealth, AppServiceBacklogEntry } from "@/api/bridges";

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
