import type { components } from "@/api/schema";

type StatisticsOverview = components["schemas"]["StatisticsOverview"];
type ServerInfo = components["schemas"]["ServerInfo"];
type ClusterStatus = components["schemas"]["ClusterStatus"];
type Destination = components["schemas"]["Destination"];
type AuditEntry = components["schemas"]["AuditEntry"];

const now = Date.now();
const iso = (msAgo: number) => new Date(now - msAgo).toISOString();

export const statisticsOverview: StatisticsOverview = {
  users_count: 642,
  rooms_count: 118,
  media_count: 4_302,
  media_bytes: 38 * 1024 * 1024 * 1024,
  daily_active_users: 214,
  monthly_active_users: 580,
  federation_destinations_failing_count: 1,
  pending_reports_count: 2,
};

export const serverInfo: ServerInfo = {
  name: "example.org",
  version: "0.1.0-dev",
  build: "dev",
  supported_room_versions: ["11", "12"],
  enabled_components: ["federation", "appservices", "media"],
  uptime_ms: (4 * 24 + 6) * 3_600_000,
  contract_version: "0.1.0-draft",
};

export const clusterStatus: ClusterStatus = {
  mode: "single-node",
  epoch: 1,
  replica_count: 1,
  shard_count: 1,
};

export const federationDestinations: Destination[] = [
  {
    server_name: "matrix.org",
    last_successful_at: iso(30_000),
    failing_since: null,
    retry_last_at: null,
    retry_interval_ms: null,
    pending_pdu_count: 0,
    pending_edu_count: 0,
  },
  {
    server_name: "element.io",
    last_successful_at: iso(60_000),
    failing_since: null,
    retry_last_at: null,
    retry_interval_ms: null,
    pending_pdu_count: 0,
    pending_edu_count: 0,
  },
  {
    server_name: "gnome.org",
    last_successful_at: iso(20 * 60_000),
    failing_since: null,
    retry_last_at: iso(5 * 60_000),
    retry_interval_ms: 60_000,
    pending_pdu_count: 4,
    pending_edu_count: 0,
  },
  {
    server_name: "mozilla.org",
    last_successful_at: iso(3 * 3_600_000),
    failing_since: iso(2 * 3_600_000),
    retry_last_at: iso(2 * 60_000),
    retry_interval_ms: 300_000,
    pending_pdu_count: 42,
    pending_edu_count: 3,
  },
];

export const recentAuditEntries: AuditEntry[] = [
  {
    id: "audit-1",
    recorded_at: iso(3 * 60_000),
    action: "appservices.pause",
    actor: { kind: "user", id: "@admin:example.org", display_name: "Operator" },
    target: { type: "appservice", id: "discord" },
    outcome: { status: 200, problem: null },
  },
  {
    id: "audit-2",
    recorded_at: iso(40 * 60_000),
    action: "users.suspend",
    actor: { kind: "user", id: "@admin:example.org", display_name: "Operator" },
    target: { type: "user", id: "@spammer42:example.org" },
    outcome: { status: 200, problem: null },
  },
  {
    id: "audit-3",
    recorded_at: iso(2 * 3_600_000),
    action: "appservices.rotate_tokens",
    actor: { kind: "user", id: "@admin:example.org", display_name: "Operator" },
    target: { type: "appservice", id: "whatsapp" },
    outcome: { status: 200, problem: null },
  },
  {
    id: "audit-4",
    recorded_at: iso(5 * 3_600_000),
    action: "rooms.block",
    actor: { kind: "user", id: "@admin:example.org", display_name: "Operator" },
    target: { type: "room", id: "!abc:example.org" },
    outcome: { status: 200, problem: null },
  },
  {
    id: "audit-5",
    recorded_at: iso(9 * 3_600_000),
    action: "registration_tokens.create",
    actor: { kind: "user", id: "@admin:example.org", display_name: "Operator" },
    target: { type: "registration_token", id: "INVITE-2026-09" },
    outcome: { status: 200, problem: null },
  },
];
