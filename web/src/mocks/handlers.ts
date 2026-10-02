import { http, HttpResponse } from "msw";
import {
  appservices,
  appserviceHealth,
  appserviceBacklog,
  appserviceRegistration,
  appserviceLogins,
  findAppservice,
  patchAppservice,
  pingAppservice,
} from "./data/appservices";
import { bridgeTypes } from "./data/bridge-types";
import {
  deleteInstance,
  deleteOffering,
  deploymentTarget,
  findOffering,
  getInstance,
  instanceByAppservice,
  instanceFiles,
  instancesOf,
  listOfferings,
  offeringRefusal,
  offeringView,
  putInstance,
  putOffering,
} from "./data/bridge-offerings";
import {
  beforeValues,
  configAuditEntries,
  configChangeBody,
  configEtag,
  configHistory,
  configLastReloaded,
  configRevisions,
  configSchemaDocument,
  configValues,
  environmentPinned,
  isHotSetting,
  mergePatch,
  patchPointers,
  recordConfigChange,
  recordConfigHistory,
  revertConflicts,
  revertPatch,
  sectionSource,
  restoreEchoedSecrets,
  stripEchoedSecrets,
  validateDocument,
  validateSection,
} from "./data/config";
import { statisticsOverview, serverInfo, federationDestinations } from "./data/dashboard";
import {
  clusterSummary,
  drainReplica,
  getReplica,
  listReplicas,
  listShards,
  settleCluster,
  undrainReplica,
  type ReplicaOutcome,
} from "./data/cluster";
import {
  abortMigration,
  cutoverMigration,
  migrationLog,
  migrationStatus,
  pauseMigration,
  resumeMigration,
  startMigration,
  verifyMigration,
  type MigrationOutcome,
} from "./data/migration";
import { auditEntries } from "./data/audit";
import {
  deleteReport,
  getReport,
  listReports,
  openReportCount,
  resolveReport,
} from "./data/reports";
import { cancelTask, getTask, listTasks, putTask } from "./data/tasks";
import { mockEventStream, publishMockEvent } from "./data/events";
import { cachedKeys, destinationRooms, ownKeys, startKeyRefresh } from "./data/federation";
import { roomStatistics, sortStatistics, timeseries, userMediaStatistics } from "./data/statistics";
import type { ReportResolve } from "@/api/reports";
import { succeeded } from "@/lib/audit";
import { users, userDevices, findUser, eraseUser } from "./data/users";
import {
  KNOWN_FEATURES,
  userAccountData,
  userExternalIds,
  userFeatures,
  userPushers,
  userThreepids,
} from "./data/user-identity";
import {
  MOCK_RECOVERY_TOKEN,
  mockRecoveryExpiresAt,
  mockRecoveryLinkOpen,
  recoveryAdministrators,
  useMockRecoveryLink,
} from "./data/recovery";
import { rooms, roomMembers, findRoom } from "./data/rooms";
import { roomContentHandlers } from "./room-handlers";
import {
  clearRateLimit,
  deleteUserMedia,
  getRateLimit,
  listMemberships,
  listSessions,
  mintSupportSession,
  setRateLimit,
  startRedaction,
  userMedia,
  userStatistics,
} from "./data/user-moderation";
import {
  findRegistrationToken,
  generateMockToken,
  refreshValidity,
  registrationTokens,
} from "./data/registration-tokens";
import { adminTokens, findAdminToken, mintMockAdminToken } from "./data/admin-tokens";
import { serverNotices, SERVER_NOTICES_USER } from "./data/server-notices";
import {
  findMedia,
  lastUsed,
  listMedia,
  mediaItems,
  removeMedia,
  thumbnailSvg,
} from "./data/media";
import type { MediaItem } from "@/api/media";
import { ALL_SCOPES, type Scope } from "@/lib/auth";
import type { AppService, BridgeOfferingRequest } from "@/api/bridges";
import type { JsonValue } from "@/api/config-schema";
import type { components } from "@/api/schema";

const API = "/api/v1";

const RESOLUTION_VALUES: readonly ReportResolve["resolution"][] = [
  "no_action",
  "warned",
  "redacted",
  "suspended",
  "deactivated",
  "room_blocked",
  "other",
];

const NO_STORE = { "Cache-Control": "no-store" };

/** Where a test puts the token that opens the mock's first-run setup. */
export const MOCK_SETUP_TOKEN_KEY = "hs-mock:setup-token";

function mockSetupToken(): string | null {
  try {
    return globalThis.sessionStorage?.getItem(MOCK_SETUP_TOKEN_KEY) ?? null;
  } catch {
    return null;
  }
}

function closeMockSetup(): void {
  try {
    globalThis.sessionStorage?.removeItem(MOCK_SETUP_TOKEN_KEY);
  } catch {
    /* nothing to close */
  }
}

function encodeCursor(index: number): string {
  return btoa(`offset:${index}`);
}
function decodeCursor(cursor: string | null): number {
  if (!cursor) return 0;
  try {
    const decoded = atob(cursor);
    const match = /^offset:(\d+)$/.exec(decoded);
    return match ? Number(match[1]) : 0;
  } catch {
    return 0;
  }
}

function paginate<T>(items: T[], url: URL) {
  const limit = Math.min(Number(url.searchParams.get("limit") ?? 50), 200);
  const offset = decodeCursor(url.searchParams.get("cursor"));
  const page = items.slice(offset, offset + limit);
  const nextOffset = offset + limit;
  const next_cursor = nextOffset < items.length ? encodeCursor(nextOffset) : null;
  const prev_cursor = offset > 0 ? encodeCursor(Math.max(offset - limit, 0)) : null;
  return { items: page, next_cursor, prev_cursor };
}

function filteredAuditEntries(url: URL, datesOnly = false) {
  const q = url.searchParams;
  return [...configAuditEntries, ...auditEntries]
    .filter(
      (entry) =>
        (!q.get("recorded_after") || entry.recorded_at >= q.get("recorded_after")!) &&
        (!q.get("recorded_before") || entry.recorded_at < q.get("recorded_before")!) &&
        (datesOnly ||
          ((!q.get("actor") || entry.actor.id === q.get("actor")) &&
            (!q.get("action") || entry.action === q.get("action")) &&
            (!q.get("target_type") || entry.target.type === q.get("target_type")) &&
            (!q.get("target_id") || entry.target.id === q.get("target_id")) &&
            (!q.get("outcome") || succeeded(entry) === (q.get("outcome") === "success")))),
    )
    .sort((a, b) => b.recorded_at.localeCompare(a.recorded_at));
}

const registeredIds = new Set(appservices.map((a) => a.id));

/** An RFC 9457 body from the closed catalog in RFC 0004 section 3.5. */
function problem(
  status: number,
  slug: string,
  title: string,
  extra?: { detail?: string; errors?: { pointer: string; detail: string }[]; reason?: string },
) {
  return HttpResponse.json({ type: `urn:hs:problem:${slug}`, title, status, ...extra }, { status });
}

/** The characters a Matrix username may contain (the historical user ID grammar, lowercased). */
const MATRIX_LOCALPART = /^[a-z0-9._=\-/]+$/;

/** A Matrix-style refusal (`{errcode, error}`), as the client-server API answers. */
function invalidUsername() {
  return HttpResponse.json(
    { errcode: "M_INVALID_USERNAME", error: "User ID can only contain a-z, 0-9, . _ = - /" },
    { status: 400 },
  );
}

function userInUse() {
  return HttpResponse.json(
    { errcode: "M_USER_IN_USE", error: "User ID already taken." },
    { status: 400 },
  );
}

function configNotFound(name: string) {
  return problem(404, "not-found", "Configuration section not found", {
    detail: `There is no section named "${name}".`,
  });
}

/**
 * Why the mock refuses a recovery request, or `null` to let it through. The order is the
 * server's: whether a link is open at all comes before whether this is it, so a used link
 * answers 409 whatever token is sent.
 */
function recoveryRefusal(token: string | undefined) {
  if (!mockRecoveryLinkOpen()) {
    return problem(409, "conflict", "Conflict", {
      detail: "no recovery link is open: none was issued, or it was used or expired",
    });
  }
  if (token !== MOCK_RECOVERY_TOKEN) {
    return problem(401, "unauthenticated", "Unauthenticated", {
      detail: "that is not this server's recovery link",
    });
  }
  return null;
}

function configSectionBody(name: string) {
  const meta = configSchemaDocument.sections.find((s) => s.name === name);
  return {
    name,
    reloadable: meta?.reloadable ?? false,
    source: sectionSource(name),
    last_reloaded_at: configLastReloaded[name] ?? null,
    values: configValues[name] ?? {},
  };
}

export const handlers = [
  // ---- Mock OAuth issuer (stands in for track 07 until it exists; not
  // part of the real admin API, which only verifies bearer tokens) ----
  http.post("/oauth2/token", async ({ request }) => {
    const body = (await request.json()) as { scopes?: Scope[] };
    const scopes = body.scopes && body.scopes.length > 0 ? body.scopes : [...ALL_SCOPES];
    return HttpResponse.json({
      access_token: `mock-admin-token.${crypto.randomUUID()}`,
      token_type: "Bearer",
      scopes,
      operator: { name: "Operator", subject: "@ops:example.org" },
    });
  }),

  // ---- The event stream (crates/hs-admin/src/events.rs; `./data/events`) ----
  http.get(`${API}/events`, ({ request }) => {
    const types = new URL(request.url).searchParams.getAll("types");
    return new HttpResponse(mockEventStream(types, request.signal), {
      headers: { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" },
    });
  }),

  // ---- First-run setup ----
  // Closed by default, like a server that already has its administrator, so every other flow is
  // unaffected. A test opens it by putting a token in `sessionStorage` before the app loads
  // (`MOCK_SETUP_TOKEN_KEY`); using the token closes it again, as on the real server.
  http.get(`${API}/setup`, () =>
    HttpResponse.json({ needs_setup: mockSetupToken() !== null }, { headers: NO_STORE }),
  ),
  http.post(`${API}/setup`, async ({ request }) => {
    const body = (await request.json()) as {
      setup_token?: string;
      username?: string;
      password?: string;
    };
    const offered = mockSetupToken();
    if (offered === null) {
      return problem(409, "conflict", "Conflict", {
        detail: "this server already has an administrator",
      });
    }
    if (body.setup_token !== offered) {
      return problem(401, "unauthenticated", "Unauthenticated", {
        detail: "that is not this server's setup token",
      });
    }
    const localpart = (body.username ?? "").replace(/^@/, "").split(":")[0]!.toLowerCase();
    if (!/^[a-z0-9._=\-/+]+$/.test(localpart)) {
      return problem(400, "validation-failed", "Validation failed", {
        detail: `"${localpart}" cannot be a username`,
        errors: [{ pointer: "/username", detail: `"${localpart}" cannot be a username` }],
      });
    }
    if ((body.password ?? "").length < 8) {
      return problem(400, "validation-failed", "Validation failed", {
        detail: "Password too short (minimum 8 characters)",
        errors: [{ pointer: "/password", detail: "Password too short (minimum 8 characters)" }],
      });
    }
    closeMockSetup();
    return HttpResponse.json(
      {
        user_id: `@${localpart}:example.org`,
        access_token: `mock-admin-token.${crypto.randomUUID()}`,
        device_id: "SETUP",
      },
      { status: 201, headers: NO_STORE },
    );
  }),
  // ---- Administrator recovery ----
  // Open by default (see `data/recovery.ts`): the link `hs recover` would print is
  // `/admin/recover#token=mock-recovery-token`. It lists two administrators, and a reset
  // succeeds once, after which the link is used, as on the real server.
  http.post(`${API}/recovery/inspect`, async ({ request }) => {
    const body = (await request.json()) as { recovery_token?: string };
    const refused = recoveryRefusal(body.recovery_token);
    if (refused) return refused;
    return HttpResponse.json(
      { administrators: recoveryAdministrators, expires_at_ms: mockRecoveryExpiresAt() },
      { headers: NO_STORE },
    );
  }),
  http.post(`${API}/recovery/reset`, async ({ request }) => {
    const body = (await request.json()) as {
      recovery_token?: string;
      user_id?: string;
      password?: string;
    };
    const refused = recoveryRefusal(body.recovery_token);
    if (refused) return refused;
    const userId = body.user_id ?? "";
    if (!recoveryAdministrators.some((a) => a.user_id === userId)) {
      const detail = `${userId || "(none)"} is not an active administrator on this server`;
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/user_id", detail }],
      });
    }
    if ((body.password ?? "").length < 8) {
      return problem(400, "validation-failed", "Validation failed", {
        detail: "Password too short (minimum 8 characters)",
        errors: [{ pointer: "/password", detail: "Password too short (minimum 8 characters)" }],
      });
    }
    useMockRecoveryLink();
    return HttpResponse.json(
      {
        user_id: userId,
        access_token: `mock-admin-token.${crypto.randomUUID()}`,
        device_id: "RECOVERY",
      },
      { status: 200, headers: NO_STORE },
    );
  }),
  // What `signInWithToken` verifies a token against. Any mock-issued token is a full
  // administrator's, which is also what the real `AdminTokenVerifier` grants.
  http.get(`${API}/me`, ({ request }) => {
    const auth = request.headers.get("authorization") ?? "";
    if (!auth.startsWith("Bearer mock-admin-token.")) {
      return problem(401, "unauthenticated", "Unauthenticated", {
        detail: "missing or unrecognized token",
      });
    }
    return HttpResponse.json({
      kind: "user",
      id: "@ops:example.org",
      display_name: "Operator",
      scopes: ["admin:read", "admin:write"],
    });
  }),

  // ---- Dashboard ----
  http.get(`${API}/statistics/overview`, () =>
    HttpResponse.json({ ...statisticsOverview, pending_reports_count: openReportCount() }),
  ),

  // ---- Statistics (crates/hs-admin/src/statistics.rs) ----
  http.get(`${API}/statistics/rooms`, ({ request }) => {
    const url = new URL(request.url);
    const sorted = sortStatistics(
      roomStatistics,
      url.searchParams.get("sort") ?? "-joined_members_count",
      ["joined_members_count", "state_events_count"],
    );
    if ("error" in sorted)
      return problem(400, "validation-failed", "Validation failed", {
        errors: [{ pointer: "/sort", detail: sorted.error }],
      });
    return HttpResponse.json(paginate(sorted, url));
  }),
  http.get(`${API}/statistics/users/media`, ({ request }) => {
    const url = new URL(request.url);
    const sorted = sortStatistics(
      userMediaStatistics,
      url.searchParams.get("sort") ?? "-media_bytes",
      ["media_bytes", "media_count"],
    );
    if ("error" in sorted)
      return problem(400, "validation-failed", "Validation failed", {
        errors: [{ pointer: "/sort", detail: sorted.error }],
      });
    return HttpResponse.json(paginate(sorted, url));
  }),
  http.get(`${API}/statistics/timeseries`, ({ request }) => {
    const series = timeseries(new URL(request.url).searchParams);
    if ("error" in series)
      return problem(400, "validation-failed", "Validation failed", {
        errors: [{ pointer: series.pointer, detail: series.error }],
      });
    return HttpResponse.json(series);
  }),

  // ---- Reports (crates/hs-admin/src/reports.rs) ----
  http.get(`${API}/reports`, ({ request }) => {
    const url = new URL(request.url);
    const items = listReports(url.searchParams);
    if ("error" in items)
      return problem(400, "validation-failed", "Validation failed", {
        errors: [{ pointer: "/sort", detail: items.error }],
      });
    return HttpResponse.json(paginate(items, url));
  }),
  http.get(`${API}/reports/:id`, ({ params }) => {
    const report = getReport(String(params.id));
    return report ? HttpResponse.json(report) : problem(404, "not-found", "Report not found");
  }),
  http.post(`${API}/reports/:id/resolve`, async ({ params, request }) => {
    const body = (await request.json()) as ReportResolve;
    if (!RESOLUTION_VALUES.includes(body?.resolution)) {
      return problem(400, "validation-failed", "Validation failed", {
        errors: [{ pointer: "/resolution", detail: "resolution is not one the contract allows" }],
      });
    }
    const result = resolveReport(String(params.id), body, "@ops:example.org");
    if (result === undefined) return problem(404, "not-found", "Report not found");
    if (result === "closed")
      return problem(409, "conflict", "Conflict", {
        detail: "this report is already closed",
      });
    return HttpResponse.json(result);
  }),
  http.delete(`${API}/reports/:id`, ({ params }) =>
    deleteReport(String(params.id))
      ? new HttpResponse(null, { status: 204 })
      : problem(404, "not-found", "Report not found"),
  ),

  // ---- Tasks (crates/hs-admin/src/tasks.rs) ----
  http.get(`${API}/tasks`, ({ request }) => {
    // A replica's drain is a task too; move it on before answering.
    settleCluster();
    const url = new URL(request.url);
    return HttpResponse.json(paginate(listTasks(url.searchParams), url));
  }),
  http.get(`${API}/tasks/:id`, ({ params }) => {
    settleCluster();
    const task = getTask(String(params.id));
    return task ? HttpResponse.json(task) : problem(404, "not-found", "Task not found");
  }),
  http.post(`${API}/tasks/:id/cancel`, ({ params }) => {
    const task = cancelTask(String(params.id));
    return task ? HttpResponse.json(task) : problem(404, "not-found", "Task not found");
  }),
  http.get(`${API}/server`, () => HttpResponse.json(serverInfo)),
  http.get(`${API}/server/health`, () =>
    HttpResponse.json({ status: "ok", checks: { storage: "ok", federation: "ok" } }),
  ),
  // ---- Cluster (the replicas and shards in ./data/cluster) ----
  http.get(`${API}/cluster`, () => HttpResponse.json(clusterSummary())),
  http.get(`${API}/cluster/replicas`, ({ request }) =>
    HttpResponse.json(paginate(listReplicas(), new URL(request.url))),
  ),
  http.get(`${API}/cluster/replicas/:id`, ({ params }) => {
    const replica = getReplica(String(params.id));
    return replica
      ? HttpResponse.json(replica)
      : problem(404, "not-found", "Not found", {
          detail: `no replica "${String(params.id)}" is registered`,
        });
  }),
  http.post(`${API}/cluster/replicas/:id/drain`, ({ params }) =>
    replicaAnswer(drainReplica(String(params.id))),
  ),
  http.post(`${API}/cluster/replicas/:id/undrain`, ({ params }) =>
    replicaAnswer(undrainReplica(String(params.id))),
  ),
  http.get(`${API}/cluster/shards`, ({ request }) => {
    const url = new URL(request.url);
    const all = listShards(url.searchParams.get("kind"));
    return HttpResponse.json({
      ...paginate(all, url),
      ...(url.searchParams.get("include_total") === "true" ? { total: all.length } : {}),
    });
  }),
  // ---- Migration (the clock-driven migration in ./data/migration) ----
  http.get(`${API}/migration`, () => HttpResponse.json(migrationStatus())),
  http.get(`${API}/migration/log`, ({ request }) => {
    const url = new URL(request.url);
    const all = migrationLog();
    return HttpResponse.json({
      ...paginate(all, url),
      ...(url.searchParams.get("include_total") === "true" ? { total: all.length } : {}),
    });
  }),
  http.post(`${API}/migration/start`, () => {
    const synapse = configValues.migration?.synapse as Record<string, JsonValue> | null;
    const database = synapse?.database as Record<string, JsonValue> | undefined;
    const source = database
      ? `postgresql://${String(database.user)}@${String(database.host)}:${String(database.port ?? 5432)}/${String(database.database)}`
      : null;
    return migrationAnswer(startMigration(source));
  }),
  http.post(`${API}/migration/pause`, () => migrationAnswer(pauseMigration())),
  http.post(`${API}/migration/resume`, () => migrationAnswer(resumeMigration())),
  http.post(`${API}/migration/abort`, () => migrationAnswer(abortMigration())),
  http.post(`${API}/migration/verify`, () => migrationTask(verifyMigration())),
  http.post(`${API}/migration/cutover`, () => migrationTask(cutoverMigration())),
  http.get(`${API}/federation/destinations`, ({ request }) => {
    const url = new URL(request.url);
    const { items, next_cursor, prev_cursor } = paginate(federationDestinations, url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
  }),
  http.get(`${API}/audit-log`, ({ request }) => {
    const url = new URL(request.url);
    const { items, next_cursor, prev_cursor } = paginate(filteredAuditEntries(url), url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
  }),
  http.get(`${API}/audit-log/export`, ({ request }) => {
    const entries = filteredAuditEntries(new URL(request.url), true).slice(0, 10_000);
    return new HttpResponse(entries.map((entry) => JSON.stringify(entry) + "\n").join(""), {
      headers: { "Content-Type": "application/x-ndjson" },
    });
  }),
  http.get(`${API}/audit-log/:id`, ({ params }) => {
    const entry = [...configAuditEntries, ...auditEntries].find((entry) => entry.id === params.id);
    return entry ? HttpResponse.json(entry) : problem(404, "not-found", "Audit entry not found");
  }),

  // ---- Bridge types ----
  http.get(`${API}/bridge-types`, ({ request }) => {
    const url = new URL(request.url);
    const { items, next_cursor, prev_cursor } = paginate(bridgeTypes, url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
  }),

  http.get(`${API}/bridge-types/:type`, ({ params }) => {
    const type = bridgeTypes.find((t) => t.id === params.type);
    if (!type)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Bridge type not found", status: 404 },
        { status: 404 },
      );
    return HttpResponse.json(type);
  }),

  http.post(`${API}/bridge-types/:type/render`, async ({ params, request }) => {
    const typeId = String(params.type);
    const values = (await request.json()) as Record<string, unknown>;
    const id = String(values.id ?? typeId);
    const senderLocalpart = String(values.senderLocalpart ?? `${id}bot`);
    const namespace = String(values.namespace ?? "bridges");
    const deployment = values.deployment === "kubernetes" ? "kubernetes" : "self-managed";
    const asToken = `as_token_${id}_${crypto.randomUUID().slice(0, 8)}`;
    const hsToken = `hs_token_${id}_${crypto.randomUUID().slice(0, 8)}`;
    const url =
      typeof values.bridgeAddress === "string" && values.bridgeAddress
        ? values.bridgeAddress
        : deployment === "kubernetes"
          ? `http://${id}.${namespace}.svc:29999`
          : `http://${id}:29999`;

    // The same shape the server renders: namespace *objects*, which `appservices.create`
    // parses; a bare pattern string is not a registration.
    const registration = {
      id,
      url,
      as_token: asToken,
      hs_token: hsToken,
      sender_localpart: senderLocalpart,
      namespaces: {
        users: values.userNamespace
          ? [{ regex: String(values.userNamespace), exclusive: true }]
          : [],
        aliases: values.aliasNamespace
          ? [{ regex: String(values.aliasNamespace), exclusive: true }]
          : [],
        rooms: [],
      },
      rate_limited: false,
      "de.sorunome.msc2409.push_ephemeral": true,
      ...(values.encryption ? { "org.matrix.msc3202": true, "io.element.msc4190": true } : {}),
      "io.myelin.bridge_type": typeId,
    };
    const registration_yaml = [
      `id: ${id}`,
      `url: ${url}`,
      `as_token: ${asToken}`,
      `hs_token: ${hsToken}`,
      `sender_localpart: ${senderLocalpart}`,
      "namespaces:",
      `  users:${values.userNamespace ? `\n    - exclusive: true\n      regex: '${values.userNamespace}'` : " []"}`,
      "de.sorunome.msc2409.push_ephemeral: true",
      `org.matrix.msc3202: ${Boolean(values.encryption)}`,
      `io.myelin.bridge_type: ${typeId}`,
    ].join("\n");
    // A mautrix bridge's own config.yaml, the way the real render writes it (the essentials;
    // the bridge completes the rest on first start). Other runtimes get no config file.
    const homeserverAddress = String(values.homeserverAddress ?? "http://myelin:8008");
    const config_yaml = typeId.startsWith("mautrix-")
      ? [
          "homeserver:",
          `  address: ${homeserverAddress}`,
          "  domain: example.org",
          "appservice:",
          `  address: ${url}`,
          "  hostname: 0.0.0.0",
          "  port: 29999",
          `  id: ${id}`,
          "  bot:",
          `    username: ${senderLocalpart}`,
          `  as_token: ${asToken}`,
          `  hs_token: ${hsToken}`,
          "database:",
          "  type: sqlite3-fk-wal",
          `  uri: file:/data/${id}.db?_txlock=immediate`,
          "bridge:",
          "  permissions:",
          '    "*": relay',
          '    "example.org": user',
          ...(values.adminUser ? [`    "${String(values.adminUser)}": admin`] : []),
          "encryption:",
          `  allow: ${Boolean(values.encryption)}`,
        ].join("\n")
      : null;
    const compose_yaml = [
      "services:",
      `  ${id}:`,
      `    image: dock.mau.dev/mautrix/${typeId.replace(/^mautrix-/, "")}:${values.imageTag ?? "latest"}`,
      "    volumes:",
      `      - ./${id}:/data`,
      "    restart: unless-stopped",
    ].join("\n");
    const bridge_resource_yaml = [
      "apiVersion: bridges.hs.example/v1",
      "kind: Bridge",
      "metadata:",
      `  name: ${id}`,
      `  namespace: ${namespace}`,
      "spec:",
      `  type: ${typeId}`,
      `  image: dock.mau.dev/mautrix/${typeId.replace(/^mautrix-/, "")}:${values.imageTag ?? "latest"}`,
    ].join("\n");

    return HttpResponse.json({
      registration,
      registration_yaml,
      config_yaml,
      compose_yaml,
      bridge_resource_yaml,
    });
  }),

  // ---- Bridge offerings and instances (RFC 0017) ----
  http.get(`${API}/bridge-deployment-target`, () => HttpResponse.json(deploymentTarget.current)),

  http.get(`${API}/bridge-offerings`, () =>
    HttpResponse.json({ items: listOfferings(), next_cursor: null, prev_cursor: null }),
  ),

  http.get(`${API}/bridge-offerings/:type`, ({ params }) => {
    const offering = findOffering(String(params.type));
    return offering
      ? HttpResponse.json(offeringView(offering))
      : problem(404, "not-found", "Bridge offering not found");
  }),

  http.put(`${API}/bridge-offerings/:type`, async ({ params, request }) => {
    const type = String(params.type);
    if (!bridgeTypes.some((t) => t.id === type)) {
      return problem(404, "not-found", "Bridge type not found", {
        detail: `There is no bridge type "${type}" in the catalogue.`,
      });
    }
    const body = (await request.json()) as BridgeOfferingRequest;
    const refusal = offeringRefusal(type, body);
    if (refusal) {
      return problem(400, "validation", "Validation failed", {
        detail: refusal,
        errors: [{ pointer: "/runtime", detail: refusal }],
      });
    }
    return HttpResponse.json(putOffering(type, body));
  }),

  http.delete(`${API}/bridge-offerings/:type`, ({ params, request }) => {
    const type = String(params.type);
    if (!findOffering(type)) return problem(404, "not-found", "Bridge offering not found");
    const removeInstances = new URL(request.url).searchParams.get("remove_instances") === "true";
    const remaining = instancesOf(type).length;
    if (remaining > 0 && !removeInstances) {
      return problem(409, "conflict", "Conflict", {
        detail: `${remaining} ${remaining === 1 ? "instance is" : "instances are"} still running; remove them first, or pass remove_instances=true.`,
      });
    }
    deleteOffering(type);
    return new HttpResponse(null, { status: 204 });
  }),

  http.get(`${API}/bridge-offerings/:type/instances`, ({ params }) => {
    const type = String(params.type);
    if (!findOffering(type)) return problem(404, "not-found", "Bridge offering not found");
    return HttpResponse.json({ items: instancesOf(type), next_cursor: null, prev_cursor: null });
  }),

  http.get(`${API}/bridge-offerings/:type/instances/:user_id`, ({ params }) => {
    const instance = getInstance(String(params.type), decodeURIComponent(String(params.user_id)));
    return instance
      ? HttpResponse.json(instance)
      : problem(404, "not-found", "Bridge instance not found");
  }),

  http.put(`${API}/bridge-offerings/:type/instances/:user_id`, ({ params }) => {
    const type = String(params.type);
    const user = decodeURIComponent(String(params.user_id));
    if (!findOffering(type)) return problem(404, "not-found", "Bridge offering not found");
    if (user !== "_" && !/^@[^:]+:example\.org$/.test(user)) {
      return problem(400, "validation", "Validation failed", {
        detail: `${user} is not a user on this server; bridges are for local users.`,
        errors: [{ pointer: "/user_id", detail: "not a local user" }],
      });
    }
    return HttpResponse.json(putInstance(type, user));
  }),

  http.delete(`${API}/bridge-offerings/:type/instances/:user_id`, ({ params }) =>
    deleteInstance(String(params.type), decodeURIComponent(String(params.user_id)))
      ? new HttpResponse(null, { status: 204 })
      : problem(404, "not-found", "Bridge instance not found"),
  ),

  http.post(`${API}/bridge-offerings/:type/instances/:user_id/files`, ({ params }) => {
    const files = instanceFiles(String(params.type), decodeURIComponent(String(params.user_id)));
    return files
      ? HttpResponse.json(files)
      : problem(404, "not-found", "Bridge instance not found");
  }),

  // ---- Appservices ----
  http.get(`${API}/appservices`, ({ request }) => {
    const url = new URL(request.url);
    const q = url.searchParams.get("q");
    let filtered = appservices;
    if (q) filtered = filtered.filter((a) => (a.id ?? "").includes(q));
    // attention-first default order (information-architecture.md: "Sort:
    // attention first by default")
    const severity: Record<string, number> = { down: 0, degraded: 1, unknown: 2, healthy: 3 };
    const sorted = [...filtered].sort(
      (a, b) => (severity[a.health ?? "unknown"] ?? 9) - (severity[b.health ?? "unknown"] ?? 9),
    );
    const { items, next_cursor, prev_cursor } = paginate(sorted, url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
  }),

  http.post(`${API}/appservices`, async ({ request }) => {
    const body = (await request.json()) as {
      registration?: {
        id?: string;
        sender_localpart?: string;
        url?: string;
        "io.myelin.bridge_type"?: string;
      };
    };
    const id = body.registration?.id;
    if (!id) {
      return HttpResponse.json(
        {
          type: "urn:hs:problem:validation",
          title: "Validation failed",
          status: 400,
          errors: [{ pointer: "/registration/id", detail: "required" }],
        },
        { status: 400 },
      );
    }
    if (registeredIds.has(id)) {
      return HttpResponse.json(
        {
          type: "urn:hs:problem:conflict",
          title: "Appservice already registered",
          status: 409,
          detail: `An appservice with id "${id}" is already registered.`,
          instance: `/api/v1/appservices/${id}`,
        },
        { status: 409 },
      );
    }
    const created: AppService = {
      id,
      sender_localpart: body.registration?.sender_localpart ?? `${id}bot`,
      url: body.registration?.url ?? null,
      namespaces: {},
      rate_limited: false,
      protocols: [],
      paused: false,
      health: "unknown",
      created_at: new Date().toISOString(),
      bridge_type: body.registration?.["io.myelin.bridge_type"] ?? null,
      links: { login_url: null },
    };
    appservices.unshift(created);
    registeredIds.add(id);
    appserviceHealth[id] = { status: "unknown", last_ping_at: null, last_error: null };
    appserviceBacklog[id] = [];
    appserviceRegistration[id] = body.registration ?? { id };
    return HttpResponse.json(created, { status: 201 });
  }),

  http.get(`${API}/appservices/:id`, ({ params }) => {
    const appservice = findAppservice(String(params.id));
    if (!appservice)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Appservice not found", status: 404 },
        { status: 404 },
      );
    return HttpResponse.json(appservice);
  }),

  http.patch(`${API}/appservices/:id`, async ({ params, request }) => {
    const patch = (await request.json()) as Record<string, unknown>;
    if ("url" in patch && patch.url !== null && !/^https?:\/\//.test(String(patch.url)))
      return problem(400, "validation-failed", "Validation failed", {
        errors: [{ pointer: "/url", detail: "must be an http:// or https:// URL, or null" }],
      });
    const rules = Object.values((patch.namespaces as Record<string, unknown[]>) ?? {}).flat();
    for (const rule of rules) {
      const regex = (rule as { regex?: string }).regex ?? "";
      try {
        new RegExp(regex);
      } catch {
        return problem(400, "validation-failed", "Validation failed", {
          errors: [{ pointer: "/namespaces", detail: `"${regex}" is not a valid regex` }],
        });
      }
      if (regex.startsWith("@irc_"))
        return problem(409, "conflict", "Conflict", {
          detail: `"${regex}" overlaps the exclusive users namespace of irc`,
        });
    }
    const appservice = patchAppservice(String(params.id), patch ?? {});
    if (!appservice)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Appservice not found", status: 404 },
        { status: 404 },
      );
    return HttpResponse.json(appservice);
  }),

  http.delete(`${API}/appservices/:id`, ({ params }) => {
    const id = String(params.id);
    const idx = appservices.findIndex((a) => a.id === id);
    if (idx === -1)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Appservice not found", status: 404 },
        { status: 404 },
      );
    appservices.splice(idx, 1);
    registeredIds.delete(id);
    return new HttpResponse(null, { status: 204 });
  }),

  http.get(`${API}/appservices/:id/health`, ({ params }) => {
    const id = String(params.id);
    const health = appserviceHealth[id];
    if (!health)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Appservice not found", status: 404 },
        { status: 404 },
      );
    return HttpResponse.json(health);
  }),

  http.get(`${API}/appservices/:id/logins`, ({ params, request }) => {
    const id = String(params.id);
    const userId = new URL(request.url).searchParams.get("user_id");
    // A person's bridge from an offering is registered too; ask it about its owner.
    const instance = findAppservice(id) ? undefined : instanceByAppservice(id);
    const bridgeType = findAppservice(id)?.bridge_type ?? instance?.type;
    const type = bridgeTypes.find((t) => t.id === bridgeType);
    const { status, body } = appserviceLogins(
      id,
      userId,
      type,
      instance && {
        bridge_type: instance.type,
        health: instance.health ?? null,
        owner: instance.user_id,
      },
    );
    return HttpResponse.json(body as never, { status });
  }),

  http.get(`${API}/appservices/:id/backlog`, ({ params, request }) => {
    const id = String(params.id);
    const entries = appserviceBacklog[id];
    if (!entries)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Appservice not found", status: 404 },
        { status: 404 },
      );
    const url = new URL(request.url);
    const { items, next_cursor, prev_cursor } = paginate(entries, url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
  }),

  http.get(`${API}/appservices/:id/registration`, ({ params }) => {
    const id = String(params.id);
    const registration = appserviceRegistration[id];
    if (!registration)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Appservice not found", status: 404 },
        { status: 404 },
      );
    return HttpResponse.json(registration);
  }),

  http.post(`${API}/appservices/:id/pause`, ({ params }) => {
    const appservice = findAppservice(String(params.id));
    if (!appservice)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found" },
        { status: 404 },
      );
    appservice.paused = true;
    return HttpResponse.json(appservice);
  }),

  http.post(`${API}/appservices/:id/resume`, ({ params }) => {
    const appservice = findAppservice(String(params.id));
    if (!appservice)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found" },
        { status: 404 },
      );
    appservice.paused = false;
    return HttpResponse.json(appservice);
  }),

  http.post(`${API}/appservices/:id/ping`, ({ params }) => {
    const appservice = pingAppservice(String(params.id));
    if (!appservice) return problem(404, "not-found", "Not found");
    return HttpResponse.json(appservice);
  }),

  http.post(`${API}/appservices/:id/rotate-tokens`, ({ params }) => {
    const id = String(params.id);
    const appservice = findAppservice(id);
    const registration = appserviceRegistration[id];
    if (!appservice || !registration)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found" },
        { status: 404 },
      );
    const as_token = `as_token_${id}_${crypto.randomUUID().slice(0, 8)}`;
    const hs_token = `hs_token_${id}_${crypto.randomUUID().slice(0, 8)}`;
    registration.as_token = as_token;
    registration.hs_token = hs_token;
    return HttpResponse.json({ as_token, hs_token });
  }),

  http.post(`${API}/appservices/:id/replay`, ({ params }) => {
    const id = String(params.id);
    const entries = appserviceBacklog[id];
    if (!entries)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found" },
        { status: 404 },
      );
    appserviceBacklog[id] = [];
    return HttpResponse.json(
      {
        id: `task-${crypto.randomUUID().slice(0, 8)}`,
        action: "appservice.replay",
        status: "succeeded",
        resource: { type: "appservice", id },
        created_at: new Date().toISOString(),
      },
      { status: 202 },
    );
  }),

  // ---- Appservice id availability (probed via GET /appservices/{id} 404) ----
  // No dedicated check/validate endpoint exists on the real API; see
  // api/bridges.ts's checkAppserviceIdAvailable.

  // ---- Users (flows.md flow 2) ----
  http.get(`${API}/users`, ({ request }) => {
    const url = new URL(request.url);
    const q = url.searchParams.get("q");
    const suspended = url.searchParams.get("suspended");
    const filtered = (
      q
        ? users.filter(
            (u) =>
              u.user_id.includes(q) ||
              (u.display_name ?? "").toLowerCase().includes(q.toLowerCase()),
          )
        : users
    ).filter((u) => suspended == null || Boolean(u.suspended) === (suspended === "true"));
    const { items, next_cursor, prev_cursor } = paginate(filtered, url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
  }),

  http.post(`${API}/users`, async ({ request }) => {
    const body = (await request.json()) as {
      localpart?: string;
      password?: string;
      display_name?: string;
      admin?: boolean;
    };
    const localpart = (body.localpart ?? "").replace(/^@/, "").split(":")[0]!.toLowerCase();
    if (!/^[a-z0-9._=\-/+]+$/.test(localpart)) {
      return problem(400, "validation-failed", "Validation failed", {
        detail: `"${localpart}" cannot be a username`,
        errors: [{ pointer: "/localpart", detail: `"${localpart}" cannot be a username` }],
      });
    }
    if ((body.password ?? "").length < 8) {
      return problem(400, "validation-failed", "Validation failed", {
        detail: "Password too short (minimum 8 characters)",
        errors: [{ pointer: "/password", detail: "Password too short (minimum 8 characters)" }],
      });
    }
    const user_id = `@${localpart}:example.org`;
    if (findUser(user_id)) {
      return problem(409, "conflict", "Conflict", { detail: `${user_id} already exists` });
    }
    const created = {
      ...users[0]!,
      user_id,
      display_name: body.display_name ?? null,
      admin: body.admin ?? false,
      created_at: new Date().toISOString(),
      last_seen_at: null,
      device_count: 0,
      room_count: 0,
      media_count: 0,
    };
    users.push(created);
    return HttpResponse.json(created, { status: 201 });
  }),
  http.get(`${API}/users/:user_id`, ({ params }) => {
    const user = findUser(decodeURIComponent(String(params.user_id)));
    if (!user)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "User not found", status: 404 },
        { status: 404 },
      );
    return HttpResponse.json(user);
  }),

  http.patch(`${API}/users/:user_id`, async ({ params, request }) => {
    const user = findUser(decodeURIComponent(String(params.user_id)));
    if (!user) return problem(404, "not-found", "Not found", { detail: "no such user" });
    const body = (await request.json()) as {
      display_name?: unknown;
      avatar_url?: unknown;
      admin?: unknown;
      user_type?: unknown;
    };
    const errors: { pointer: string; detail: string }[] = [];
    if ("admin" in body && typeof body.admin !== "boolean")
      errors.push({ pointer: "/admin", detail: "must be a boolean" });
    if ("display_name" in body && typeof body.display_name !== "string")
      errors.push({ pointer: "/display_name", detail: "must be a string" });
    if ("avatar_url" in body && typeof body.avatar_url !== "string")
      errors.push({ pointer: "/avatar_url", detail: "must be a string" });
    if (
      "avatar_url" in body &&
      body.avatar_url !== "" &&
      !/^mxc:\/\//.test(String(body.avatar_url))
    )
      errors.push({ pointer: "/avatar_url", detail: "must be an mxc:// URL" });
    if ("user_type" in body && body.user_type !== null && typeof body.user_type !== "string")
      errors.push({ pointer: "/user_type", detail: "must be a string or null" });
    if (errors.length > 0)
      return problem(400, "validation-failed", "Validation failed", {
        detail: "one or more fields in the request cannot be applied",
        errors,
      });
    if (typeof body.admin === "boolean") user.admin = body.admin;
    if (typeof body.display_name === "string") user.display_name = body.display_name || null;
    if (typeof body.avatar_url === "string") user.avatar_url = body.avatar_url || null;
    if ("user_type" in body) user.user_type = (body.user_type as string | null) || null;
    return HttpResponse.json(user);
  }),

  http.get(`${API}/users/:user_id/devices`, ({ params, request }) => {
    const devices = userDevices[decodeURIComponent(String(params.user_id))] ?? [];
    const url = new URL(request.url);
    const { items, next_cursor, prev_cursor } = paginate(devices, url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
  }),

  ...(
    [
      ["lock", { locked: true }],
      ["unlock", { locked: false }],
      ["suspend", { suspended: true }],
      ["unsuspend", { suspended: false }],
      ["logout", {}],
    ] as const
  ).map(([action, patch]) =>
    http.post(`${API}/users/:user_id/${action}`, ({ params }) => {
      const user = findUser(decodeURIComponent(String(params.user_id)));
      if (!user)
        return HttpResponse.json(
          { type: "urn:hs:problem:not-found", title: "Not found" },
          { status: 404 },
        );
      Object.assign(user, patch);
      return HttpResponse.json(user);
    }),
  ),

  http.delete(`${API}/users/:user_id/devices/:device_id`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    const devices = userDevices[userId];
    const index = devices?.findIndex((d) => d.device_id === String(params.device_id)) ?? -1;
    if (!devices || index < 0)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found", status: 404 },
        { status: 404 },
      );
    devices.splice(index, 1);
    return new HttpResponse(null, { status: 204 });
  }),

  // ---- a user's devices one at a time, 3PIDs, linked identities, features, client data ----

  http.get(`${API}/users/:user_id/devices/:device_id`, ({ params }) => {
    const devices = userDevices[decodeURIComponent(String(params.user_id))] ?? [];
    const device = devices.find((d) => d.device_id === String(params.device_id));
    return device ? HttpResponse.json(device) : problem(404, "not-found", "Not found");
  }),

  http.patch(`${API}/users/:user_id/devices/:device_id`, async ({ params, request }) => {
    const devices = userDevices[decodeURIComponent(String(params.user_id))] ?? [];
    const device = devices.find((d) => d.device_id === String(params.device_id));
    if (!device) return problem(404, "not-found", "Not found");
    const body = (await request.json()) as { display_name?: string | null };
    if (!("display_name" in body))
      return problem(400, "validation-failed", "Validation failed", {
        errors: [{ pointer: "/display_name", detail: "display_name is required" }],
      });
    device.display_name = body.display_name?.trim() || null;
    return HttpResponse.json(device);
  }),

  http.post(`${API}/users/:user_id/devices/bulk-delete`, async ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    const devices = userDevices[userId] ?? [];
    const { device_ids = [] } = (await request.json()) as { device_ids?: string[] };
    if (device_ids.length === 0)
      return problem(400, "validation-failed", "Validation failed", {
        errors: [{ pointer: "/device_ids", detail: "name at least one device" }],
      });
    if (!device_ids.every((id) => devices.some((d) => d.device_id === id)))
      return problem(404, "not-found", "Not found", {
        detail: `${userId} does not have every one of these devices`,
      });
    userDevices[userId] = devices.filter((d) => !device_ids.includes(d.device_id));
    return new HttpResponse(null, { status: 204 });
  }),

  http.get(`${API}/users/:user_id/threepids`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId)) return problem(404, "not-found", "Not found");
    return HttpResponse.json(userThreepids[userId] ?? []);
  }),

  http.post(`${API}/users/:user_id/threepids`, async ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId)) return problem(404, "not-found", "Not found");
    const body = (await request.json()) as { medium: string; address: string };
    const address =
      body.medium === "email" ? body.address.trim().toLowerCase() : body.address.replace(/\D/g, "");
    if (body.medium === "email" && !/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(address))
      return problem(400, "validation-failed", "Validation failed", {
        detail: `"${body.address}" is not an email address`,
        errors: [{ pointer: "/address", detail: `"${body.address}" is not an email address` }],
      });
    const owner = Object.entries(userThreepids).find(([, list]) =>
      list.some((t) => t.medium === body.medium && t.address === address),
    )?.[0];
    if (owner && owner !== userId)
      return problem(409, "conflict", "Conflict", {
        detail: `${body.medium} ${address} is bound to ${owner}`,
      });
    const added = {
      medium: body.medium as "email" | "msisdn",
      address,
      added_at: new Date().toISOString(),
    };
    if (!owner) (userThreepids[userId] ??= []).push(added);
    return HttpResponse.json(added, { status: 201 });
  }),

  http.delete(`${API}/users/:user_id/threepids/:medium/:address`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    const address = decodeURIComponent(String(params.address));
    const list = userThreepids[userId] ?? [];
    const index = list.findIndex((t) => t.medium === params.medium && t.address === address);
    if (index < 0) return problem(404, "not-found", "Not found");
    list.splice(index, 1);
    return new HttpResponse(null, { status: 204 });
  }),

  http.get(`${API}/users/:user_id/external-ids`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId)) return problem(404, "not-found", "Not found");
    return HttpResponse.json(userExternalIds[userId] ?? []);
  }),

  http.post(`${API}/users/:user_id/external-ids`, async ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId)) return problem(404, "not-found", "Not found");
    const body = (await request.json()) as { provider: string; external_id: string };
    if (!body.provider.trim())
      return problem(400, "validation-failed", "Validation failed", {
        errors: [{ pointer: "/provider", detail: "name the identity provider" }],
      });
    const owner = Object.entries(userExternalIds).find(([, list]) =>
      list.some((x) => x.provider === body.provider && x.external_id === body.external_id),
    )?.[0];
    if (owner && owner !== userId)
      return problem(409, "conflict", "Conflict", {
        detail: `${body.provider} ${body.external_id} is linked to ${owner}`,
      });
    if (!owner) (userExternalIds[userId] ??= []).push(body);
    return HttpResponse.json(body, { status: 201 });
  }),

  http.delete(`${API}/users/:user_id/external-ids/:provider/:external_id`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    const provider = decodeURIComponent(String(params.provider));
    const externalId = decodeURIComponent(String(params.external_id));
    const list = userExternalIds[userId] ?? [];
    const index = list.findIndex((x) => x.provider === provider && x.external_id === externalId);
    if (index < 0) return problem(404, "not-found", "Not found");
    list.splice(index, 1);
    return new HttpResponse(null, { status: 204 });
  }),

  http.get(`${API}/users/:user_id/experimental-features`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId)) return problem(404, "not-found", "Not found");
    const stored = userFeatures[userId] ?? {};
    return HttpResponse.json(
      Object.fromEntries(KNOWN_FEATURES.map((f) => [f, stored[f] ?? false])),
    );
  }),

  http.put(`${API}/users/:user_id/experimental-features`, async ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId)) return problem(404, "not-found", "Not found");
    const body = (await request.json()) as Record<string, boolean>;
    const unknown = Object.keys(body).find((k) => !KNOWN_FEATURES.includes(k));
    if (unknown)
      return problem(400, "validation-failed", "Validation failed", {
        detail: `"${unknown}" is not an experimental feature this server knows`,
      });
    const stored = (userFeatures[userId] = { ...(userFeatures[userId] ?? {}), ...body });
    return HttpResponse.json(
      Object.fromEntries(KNOWN_FEATURES.map((f) => [f, stored[f] ?? false])),
    );
  }),

  http.get(`${API}/users/:user_id/account-data`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId)) return problem(404, "not-found", "Not found");
    return HttpResponse.json(userAccountData[userId] ?? {});
  }),

  http.get(`${API}/users/:user_id/pushers`, ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId)) return problem(404, "not-found", "Not found");
    const { items, next_cursor, prev_cursor } = paginate(
      userPushers[userId] ?? [],
      new URL(request.url),
    );
    return HttpResponse.json({ items, next_cursor, prev_cursor });
  }),

  http.post(`${API}/users/:user_id/reset-password`, async ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId))
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found", status: 404 },
        { status: 404 },
      );
    if (findUser(userId)?.erased)
      return problem(409, "conflict", "Conflict", {
        detail: `${userId} was erased; an erased account has no password to reset`,
      });
    const body = (await request.json()) as { password?: string; logout_devices?: boolean };
    if (!body.password || body.password.length < 8)
      return HttpResponse.json(
        {
          type: "urn:hs:problem:validation",
          title: "Validation failed",
          status: 400,
          errors: [{ pointer: "/password", detail: "the password must be at least 8 characters" }],
        },
        { status: 400 },
      );
    if (body.logout_devices ?? true) userDevices[userId] = [];
    return HttpResponse.json({});
  }),

  http.post(`${API}/users/:user_id/deactivate`, async ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    const user = findUser(userId);
    if (!user)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found" },
        { status: 404 },
      );
    const body = (await request.json().catch(() => ({}))) as { erase?: boolean };
    if (body.erase) {
      // Erasing an already-erased account is a no-op; otherwise everything personal goes.
      if (!user.erased) {
        eraseUser(user);
        userDevices[userId] = [];
        userThreepids[userId] = [];
        userExternalIds[userId] = [];
      }
      return HttpResponse.json(user);
    }
    user.deactivated = true;
    return HttpResponse.json(user);
  }),

  http.post(`${API}/users/:user_id/reactivate`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    const user = findUser(userId);
    if (!user)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found" },
        { status: 404 },
      );
    if (user.erased)
      return problem(409, "conflict", "Conflict", {
        detail: `${userId} was erased; an erased account cannot be reactivated`,
      });
    user.deactivated = false;
    return HttpResponse.json(user);
  }),

  // ---- Users: moderation and activity (shadow-ban, rate limit, login-as, sessions,
  // memberships, statistics, media, redact-events; ./data/user-moderation.ts) ----
  ...(
    [
      ["shadow-ban", true],
      ["unshadow-ban", false],
    ] as const
  ).map(([action, shadowBanned]) =>
    http.post(`${API}/users/:user_id/${action}`, ({ params }) => {
      const user = findUser(decodeURIComponent(String(params.user_id)));
      if (!user) return problem(404, "not-found", "Not found", { detail: "no such user" });
      user.shadow_banned = shadowBanned;
      return HttpResponse.json(user);
    }),
  ),
  http.get(`${API}/users/:user_id/rate-limit`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId))
      return problem(404, "not-found", "Not found", { detail: "no such user" });
    return HttpResponse.json(getRateLimit(userId));
  }),
  http.put(`${API}/users/:user_id/rate-limit`, async ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId))
      return problem(404, "not-found", "Not found", { detail: "no such user" });
    const body = (await request.json().catch(() => ({}))) as {
      messages_per_second?: unknown;
      burst_count?: unknown;
    };
    const rate = body.messages_per_second;
    const burst = body.burst_count ?? 10;
    if (typeof rate !== "number" || !Number.isFinite(rate) || rate < 0) {
      const detail = "messages_per_second must be a number, 0 or more";
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/messages_per_second", detail }],
      });
    }
    if (typeof burst !== "number" || !Number.isInteger(burst) || burst < 1) {
      const detail = "burst_count must be a whole number, 1 or more";
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/burst_count", detail }],
      });
    }
    return HttpResponse.json(
      setRateLimit(userId, { messages_per_second: rate, burst_count: burst }),
    );
  }),
  http.delete(`${API}/users/:user_id/rate-limit`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId))
      return problem(404, "not-found", "Not found", { detail: "no such user" });
    clearRateLimit(userId);
    return new HttpResponse(null, { status: 204 });
  }),
  http.post(`${API}/users/:user_id/login-as`, async ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    const user = findUser(userId);
    if (!user) return problem(404, "not-found", "Not found", { detail: "no such user" });
    if (user.deactivated) {
      return problem(409, "conflict", "Conflict", {
        detail: `${userId} is deactivated; there is nobody to sign in as`,
      });
    }
    const body = (await request.json().catch(() => ({}))) as { valid_for_seconds?: number };
    const valid = body.valid_for_seconds ?? 3600;
    if (!Number.isInteger(valid) || valid < 1 || valid > 86_400) {
      const detail = "valid_for_seconds must be between 1 and 86400";
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/valid_for_seconds", detail }],
      });
    }
    return HttpResponse.json(mintSupportSession(userId, valid), { status: 201 });
  }),
  http.get(`${API}/users/:user_id/sessions`, ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId))
      return problem(404, "not-found", "Not found", { detail: "no such user" });
    return HttpResponse.json(paginate(listSessions(userId), new URL(request.url)));
  }),
  http.get(`${API}/users/:user_id/memberships`, ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId))
      return problem(404, "not-found", "Not found", { detail: "no such user" });
    const url = new URL(request.url);
    const rows = listMemberships(userId, url.searchParams.get("membership"));
    return HttpResponse.json(paginate(rows, url));
  }),
  http.get(`${API}/users/:user_id/statistics`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId))
      return problem(404, "not-found", "Not found", { detail: "no such user" });
    return HttpResponse.json(userStatistics(userId));
  }),
  http.get(`${API}/users/:user_id/media`, ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId))
      return problem(404, "not-found", "Not found", { detail: "no such user" });
    const rows = userMedia(userId);
    return HttpResponse.json({ ...paginate(rows, new URL(request.url)), total: rows.length });
  }),
  http.delete(`${API}/users/:user_id/media`, ({ params }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId))
      return problem(404, "not-found", "Not found", { detail: "no such user" });
    const task = deleteUserMedia(userId);
    return HttpResponse.json(task, {
      status: 202,
      headers: { Location: `/api/v1/tasks/${task.id}` },
    });
  }),
  http.post(`${API}/users/:user_id/redact-events`, async ({ params, request }) => {
    const userId = decodeURIComponent(String(params.user_id));
    if (!findUser(userId))
      return problem(404, "not-found", "Not found", { detail: "no such user" });
    const body = (await request.json().catch(() => ({}))) as { room_id?: string; limit?: number };
    if (body.room_id && !findRoom(body.room_id)) {
      return problem(404, "not-found", "Not found", { detail: `no room ${body.room_id}` });
    }
    const task = startRedaction(userId, body);
    return HttpResponse.json(task, {
      status: 202,
      headers: { Location: `/api/v1/tasks/${task.id}` },
    });
  }),
  // ---- end of Users: moderation and activity ----

  // ---- Registration tokens (Settings; the Users page's "Invite by link") ----
  http.get(`${API}/registration-tokens`, ({ request }) => {
    const all = registrationTokens.map((t) => refreshValidity(t));
    return HttpResponse.json(paginate(all, new URL(request.url)));
  }),

  http.post(`${API}/registration-tokens`, async ({ request }) => {
    const body = (await request.json().catch(() => ({}))) as {
      token?: string;
      uses_allowed?: number | null;
      expires_at?: string | null;
      length?: number;
    };
    if (body.token !== undefined && !/^[A-Za-z0-9._~-]{1,64}$/.test(body.token)) {
      const detail = "a token is 1 to 64 characters from A-Z a-z 0-9 . _ ~ -";
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/token", detail }],
      });
    }
    if (body.uses_allowed != null && body.uses_allowed < 0) {
      const detail = "uses_allowed cannot be negative";
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/uses_allowed", detail }],
      });
    }
    const token = body.token ?? generateMockToken(Math.min(Math.max(body.length ?? 16, 1), 64));
    if (findRegistrationToken(token)) {
      return problem(409, "conflict", "Conflict", {
        detail: `a registration token "${token}" already exists`,
      });
    }
    const created = refreshValidity({
      token,
      valid: true,
      uses_allowed: body.uses_allowed ?? null,
      pending: 0,
      completed: 0,
      expires_at: body.expires_at ?? null,
      created_at: new Date().toISOString(),
    });
    registrationTokens.unshift(created);
    return HttpResponse.json(created, { status: 201 });
  }),

  http.get(`${API}/registration-tokens/:token`, ({ params }) => {
    const found = findRegistrationToken(decodeURIComponent(String(params.token)));
    if (!found) return problem(404, "not-found", "Not found");
    return HttpResponse.json(refreshValidity(found));
  }),

  http.patch(`${API}/registration-tokens/:token`, async ({ params, request }) => {
    const found = findRegistrationToken(decodeURIComponent(String(params.token)));
    if (!found) return problem(404, "not-found", "Not found");
    const body = (await request.json().catch(() => ({}))) as {
      uses_allowed?: number | null;
      expires_at?: string | null;
    };
    if (body.uses_allowed != null && body.uses_allowed < 0) {
      const detail = "uses_allowed cannot be negative";
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/uses_allowed", detail }],
      });
    }
    if ("uses_allowed" in body) found.uses_allowed = body.uses_allowed ?? null;
    if ("expires_at" in body) found.expires_at = body.expires_at ?? null;
    return HttpResponse.json(refreshValidity(found));
  }),

  http.delete(`${API}/registration-tokens/:token`, ({ params }) => {
    const index = registrationTokens.findIndex(
      (t) => t.token === decodeURIComponent(String(params.token)),
    );
    if (index < 0) return problem(404, "not-found", "Not found");
    registrationTokens.splice(index, 1);
    return new HttpResponse(null, { status: 204 });
  }),

  // ---- Admin tokens (Settings): tokens narrower than a full administrator's ----
  http.get(`${API}/admin-tokens`, ({ request }) =>
    HttpResponse.json(paginate(adminTokens, new URL(request.url))),
  ),

  http.post(`${API}/admin-tokens`, async ({ request }) => {
    const body = (await request.json().catch(() => ({}))) as {
      name?: string;
      scopes?: string[];
      expires_at?: string | null;
    };
    const name = (body.name ?? "").trim();
    if (!name) {
      const detail = "name is required: what this token is for, so it can be told from the others";
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/name", detail }],
      });
    }
    const scopes: string[] =
      body.scopes === undefined ? ["admin:read", "admin:write"] : body.scopes;
    if (scopes.length === 0) {
      const detail = "scopes must name at least one scope; omit it for a full administrator's";
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/scopes", detail }],
      });
    }
    const unknown = scopes.find((s) => !(ALL_SCOPES as readonly string[]).includes(s));
    if (unknown) {
      const detail = `unknown scope "${unknown}"`;
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/scopes", detail }],
      });
    }
    if (body.expires_at != null && Date.parse(body.expires_at) <= Date.now()) {
      const detail = "expires_at is in the past; a new token must be usable";
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/expires_at", detail }],
      });
    }
    const { id, token } = mintMockAdminToken();
    const created = {
      id,
      name,
      scopes: ALL_SCOPES.filter((s) => scopes.includes(s)),
      created_at: new Date().toISOString(),
      created_by: "@ops:example.org",
      expires_at: body.expires_at ?? null,
    };
    adminTokens.push(created);
    return HttpResponse.json({ ...created, token }, { status: 201 });
  }),

  http.get(`${API}/admin-tokens/:id`, ({ params }) => {
    const found = findAdminToken(String(params.id));
    if (!found) return problem(404, "not-found", "Not found");
    return HttpResponse.json(found);
  }),

  http.delete(`${API}/admin-tokens/:id`, ({ params }) => {
    const index = adminTokens.findIndex((t) => t.id === String(params.id));
    if (index < 0) return problem(404, "not-found", "Not found");
    adminTokens.splice(index, 1);
    return new HttpResponse(null, { status: 204 });
  }),

  // ---- Server notices (Settings; a user's "Send notice") ----
  http.get(`${API}/server-notices`, ({ request }) =>
    HttpResponse.json(paginate(serverNotices, new URL(request.url))),
  ),

  http.post(`${API}/server-notices`, async ({ request }) => {
    const body = (await request.json().catch(() => ({}))) as {
      recipients?: string[];
      content?: Record<string, unknown>;
      type?: string;
    };
    const recipients = body.recipients ?? [];
    if (recipients.length === 0) {
      const detail = "at least one recipient is required";
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/recipients", detail }],
      });
    }
    const missing = recipients.find((r) => !findUser(r));
    if (missing) {
      // What the server says: a field error on the recipients, and nothing sent.
      const detail = `${missing} does not exist`;
      return problem(400, "validation-failed", "Validation failed", {
        detail,
        errors: [{ pointer: "/recipients", detail }],
      });
    }
    const id = `notice-${crypto.randomUUID().slice(0, 8)}`;
    const localpart = (userId: string) => userId.slice(1).split(":")[0];
    const sent = {
      id,
      sender: SERVER_NOTICES_USER,
      type: body.type ?? "m.room.message",
      content: (body.content ?? {}) as Record<string, never>,
      recipients,
      room_ids: recipients.map((r) => `!notices-${localpart(r)}:example.org`),
      event_ids: recipients.map((r) => `$${id}-${localpart(r)}`),
      sent_at: new Date().toISOString(),
    };
    serverNotices.unshift(sent);
    return HttpResponse.json(sent, { status: 201 });
  }),

  // ---- Matrix client-server registration with a token (the public /admin/register page) ----
  // Not the admin API: the three calls any Matrix client makes to register with an invite.
  http.get("/_matrix/client/v1/register/m.login.registration_token/validity", ({ request }) => {
    const token = new URL(request.url).searchParams.get("token") ?? "";
    const found = findRegistrationToken(token);
    return HttpResponse.json({ valid: found ? refreshValidity(found).valid === true : false });
  }),

  http.get("/_matrix/client/v3/register/available", ({ request }) => {
    const username = (new URL(request.url).searchParams.get("username") ?? "").toLowerCase();
    if (!MATRIX_LOCALPART.test(username)) return invalidUsername();
    if (findUser(`@${username}:example.org`)) return userInUse();
    return HttpResponse.json({ available: true });
  }),

  http.post("/_matrix/client/v3/register", async ({ request }) => {
    const body = (await request.json().catch(() => ({}))) as {
      username?: string;
      password?: string;
      auth?: { type?: string; token?: string; session?: string };
    };
    const username = (body.username ?? "").toLowerCase();
    if (!MATRIX_LOCALPART.test(username)) return invalidUsername();
    const userId = `@${username}:example.org`;
    if (findUser(userId)) return userInUse();
    if ((body.password ?? "").length < 8) {
      return HttpResponse.json(
        { errcode: "M_WEAK_PASSWORD", error: "Password too short (minimum 8 characters)." },
        { status: 400 },
      );
    }
    const flows = [{ stages: ["m.login.registration_token"] }];
    if (body.auth?.type !== "m.login.registration_token") {
      return HttpResponse.json({ session: "mock-uia", flows, params: {} }, { status: 401 });
    }
    const found = findRegistrationToken(body.auth.token ?? "");
    if (!found || !refreshValidity(found).valid) {
      return HttpResponse.json(
        {
          errcode: "M_UNAUTHORIZED",
          error: "Invalid registration token",
          session: "mock-uia",
          flows,
          params: {},
        },
        { status: 401 },
      );
    }
    found.completed = (found.completed ?? 0) + 1;
    refreshValidity(found);
    users.push({
      ...users[1]!,
      user_id: userId,
      display_name: username,
      admin: false,
      created_at: new Date().toISOString(),
      last_seen_at: null,
      device_count: 0,
      room_count: 0,
      media_count: 0,
    });
    return HttpResponse.json({ user_id: userId, home_server: "example.org" });
  }),

  // ---- Rooms (flows.md flow 3) ----
  http.get(`${API}/rooms`, ({ request }) => {
    const url = new URL(request.url);
    const q = url.searchParams.get("q");
    const filtered = q
      ? rooms.filter(
          (r) =>
            r.room_id.includes(q) ||
            (r.name ?? "").toLowerCase().includes(q.toLowerCase()) ||
            (r.canonical_alias ?? "").includes(q),
        )
      : rooms;
    const { items, next_cursor, prev_cursor } = paginate(filtered, url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
  }),

  http.get(`${API}/rooms/:room_id`, ({ params }) => {
    const room = findRoom(decodeURIComponent(String(params.room_id)));
    if (!room)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Room not found", status: 404 },
        { status: 404 },
      );
    return HttpResponse.json(room);
  }),

  http.get(`${API}/rooms/:room_id/members`, ({ params, request }) => {
    const members = roomMembers[decodeURIComponent(String(params.room_id))] ?? [];
    const url = new URL(request.url);
    const { items, next_cursor, prev_cursor } = paginate(members, url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
  }),

  http.post(`${API}/rooms/:room_id/block`, ({ params }) => {
    const room = findRoom(decodeURIComponent(String(params.room_id)));
    if (!room)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found" },
        { status: 404 },
      );
    room.blocked = true;
    return HttpResponse.json(room);
  }),

  http.post(`${API}/rooms/:room_id/unblock`, ({ params }) => {
    const room = findRoom(decodeURIComponent(String(params.room_id)));
    if (!room)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found" },
        { status: 404 },
      );
    room.blocked = false;
    return HttpResponse.json(room);
  }),

  http.post(`${API}/rooms/:room_id/make-admin`, ({ params }) => {
    const room = findRoom(decodeURIComponent(String(params.room_id)));
    if (!room)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found" },
        { status: 404 },
      );
    return HttpResponse.json(room);
  }),

  // ---- A room's contents and its long tail (./data/room-contents) ----
  ...roomContentHandlers(),

  // ---- Configuration (crates/hs-config; docs/config.md) ----
  //
  // `/config/schema` is registered before `/config/:section` because MSW
  // matches handlers in order and `:section` would otherwise swallow it.
  http.get(`${API}/config/schema`, () => HttpResponse.json(configSchemaDocument)),

  http.get(`${API}/config`, () =>
    HttpResponse.json(configSchemaDocument.sections.map((s) => configSectionBody(s.name))),
  ),

  http.get(`${API}/config/:section`, ({ params }) => {
    const name = String(params.section);
    if (!(name in configValues)) return configNotFound(name);
    return HttpResponse.json(configSectionBody(name), {
      headers: { ETag: configEtag(name) },
    });
  }),

  http.patch(`${API}/config/:section`, async ({ params, request }) => {
    const name = String(params.section);
    if (!(name in configValues)) return configNotFound(name);

    const meta = configSchemaDocument.sections.find((s) => s.name === name);
    if (meta?.bootstrap) {
      return problem(409, "conflict", "Conflict", {
        detail: `"${name}" is a bootstrap section: it is read before this server's database is open, or belongs to one process rather than to the whole server, so it cannot be stored in the database — set it on the command line, in an HS__ environment variable, or in the bootstrap file`,
      });
    }

    const ifMatch = request.headers.get("If-Match");
    if (ifMatch && ifMatch !== configEtag(name)) {
      return problem(412, "precondition-failed", "Someone else changed this section", {
        detail: `Your copy was revision ${ifMatch}; the server is now at ${configEtag(name)}. Re-read the section and reapply your changes.`,
      });
    }

    const restored = restoreEchoedSecrets(
      (await request.json()) as JsonValue,
      configValues[name],
      name,
    );
    if (restored.errors.length > 0) {
      return problem(400, "validation-failed", "Validation failed", {
        detail: "a hidden secret in this change names nowhere a secret is stored",
        errors: restored.errors,
      });
    }
    const patch = stripEchoedSecrets(restored.patch);

    const pinned = environmentPinned(name);
    const touched = patchPointers(patch);
    const pinnedTouched = touched.filter((pointer) => pinned.includes(pointer));
    if (pinnedTouched.length > 0) {
      return problem(400, "validation-failed", "Validation failed", {
        detail: "Pinned by this deployment's environment; change it where the environment is set.",
        errors: pinnedTouched.map((pointer) => ({
          pointer: `/${name}${pointer}`,
          detail: `set by an HS__ environment variable, which takes precedence over the database — change it in the deployment, not here`,
        })),
      });
    }

    const candidate = mergePatch(configValues[name], patch) as Record<string, JsonValue>;
    // Whole-configuration pointers, section first, as the real server sends them
    // (`config_validation_errors` in crates/hs-admin/src/sources.rs). Section-relative ones
    // would be ambiguous for `listeners.listeners`, whose section and setting share a name.
    const errors = validateSection(name, candidate).map((e) => ({
      ...e,
      pointer: `/${name}${e.pointer}`,
    }));
    if (errors.length > 0) {
      return problem(400, "validation-failed", "Validation failed", {
        detail: `${errors.length} setting${errors.length === 1 ? "" : "s"} could not be accepted.`,
        errors,
      });
    }

    const before = beforeValues(configValues[name], patch);
    configValues[name] = candidate;
    configRevisions[name] = (configRevisions[name] ?? 0) + 1;
    // What the real server's answer says (`ConfigSection.applied`): the hot settings in the
    // change were applied now; any other setting waits for a restart.
    const leaves = patchPointers(patch).map((pointer) => `/${name}${pointer}`);
    const hot = leaves.filter(isHotSetting);
    if (hot.length > 0) configLastReloaded[name] = new Date().toISOString();
    recordConfigChange(name, configRevisions[name]);
    recordConfigHistory(name, configRevisions[name], patch as Record<string, JsonValue>, before);

    return HttpResponse.json(
      {
        ...configSectionBody(name),
        applied: {
          reloaded_sections: hot.length > 0 ? [name] : [],
          errors: [],
          requires_restart: hot.length < leaves.length ? [name] : [],
          revision: configRevisions[name],
        },
      },
      { headers: { ETag: configEtag(name) } },
    );
  }),

  // `config.history.list`: newest first, one row per setting, paged by revision (`r<revision>`).
  http.get(`${API}/config/:section/history`, ({ params, request }) => {
    const name = String(params.section);
    if (!(name in configValues)) return configNotFound(name);
    const url = new URL(request.url);
    const limit = Math.min(Math.max(Number(url.searchParams.get("limit") ?? 20), 1), 100);
    const cursor = url.searchParams.get("cursor");
    const before = cursor ? Number(cursor.replace(/^r/, "")) : Infinity;
    if (Number.isNaN(before)) {
      return problem(400, "invalid-cursor", "Invalid cursor", {
        detail: `"${cursor}" is not a cursor this listing handed out`,
      });
    }
    const matching = configHistory
      .filter((record) => record.section === name)
      .sort((a, b) => b.revision - a.revision);
    const start = matching.findIndex((record) => record.revision < before);
    const from = start === -1 ? matching.length : start;
    const page = matching.slice(from, from + limit);
    const next_cursor =
      from + limit < matching.length ? `r${page[page.length - 1].revision}` : null;
    const newer = from > 0 ? matching[Math.max(from - limit, 0)] : undefined;
    const prev_cursor = cursor && newer ? `r${newer.revision + 1}` : null;
    return HttpResponse.json({ items: page.map(configChangeBody), next_cursor, prev_cursor });
  }),

  // `config.history.revert`: the settings a change touched go back as they were, as a new revision.
  http.post(`${API}/config/:section/history/:revision/revert`, async ({ params, request }) => {
    const name = String(params.section);
    if (!(name in configValues)) return configNotFound(name);
    const revision = Number(params.revision);
    const record = configHistory.find((r) => r.section === name && r.revision === revision);
    if (!record) {
      return problem(404, "not-found", "Not found", {
        detail: `no change to "${name}" was recorded at revision ${String(params.revision)}`,
      });
    }
    if (record.before === null) {
      return problem(409, "conflict", "Conflict", {
        detail: `revision ${revision} cannot be reverted: it was recorded before this server kept the values a change replaced`,
      });
    }
    const ifMatch = request.headers.get("If-Match");
    if (ifMatch && ifMatch !== configEtag(name)) {
      return problem(412, "precondition-failed", "Someone else changed this section", {
        detail: `Your copy was revision ${ifMatch}; the server is now at ${configEtag(name)}.`,
      });
    }
    const body = (await request.json().catch(() => ({}))) as { force?: boolean };
    const conflicts = revertConflicts(record);
    if (conflicts.length > 0 && !body.force) {
      return problem(409, "conflict", "Conflict", {
        detail: `later changes wrote some of the same settings; reverting revision ${revision} would undo them too. Send {"force": true} to revert anyway.`,
        errors: conflicts,
      });
    }
    const patch = revertPatch(record);
    const before = beforeValues(configValues[name], patch);
    configValues[name] = mergePatch(configValues[name], patch) as Record<string, JsonValue>;
    configRevisions[name] = (configRevisions[name] ?? 0) + 1;
    recordConfigHistory(name, configRevisions[name], patch, before, revision);
    const leaves = patchPointers(patch).map((pointer) => `/${name}${pointer}`);
    const hot = leaves.filter(isHotSetting);
    if (hot.length > 0) configLastReloaded[name] = new Date().toISOString();
    return HttpResponse.json(
      {
        ...configSectionBody(name),
        applied: {
          reloaded_sections: hot.length > 0 ? [name] : [],
          errors: [],
          requires_restart: hot.length < leaves.length ? [name] : [],
          revision: configRevisions[name],
        },
      },
      { headers: { ETag: configEtag(name) } },
    );
  }),

  http.post(`${API}/config/validate`, async ({ request }) => {
    const document = (await request.json()) as Record<string, JsonValue>;
    const errors = validateDocument(document);
    // Which of the sections in the candidate document the running process
    // could not adopt without being restarted: those with a setting that is not hot.
    const requires_restart = Object.keys(document).filter((name) =>
      patchPointers(document[name]).some((pointer) => !isHotSetting(`/${name}${pointer}`)),
    );
    return HttpResponse.json({ valid: errors.length === 0, errors, requires_restart });
  }),

  http.post(`${API}/config/reload`, () => {
    const reloaded = configSchemaDocument.sections.filter((s) => s.reloadable).map((s) => s.name);
    const now = new Date().toISOString();
    for (const name of reloaded) configLastReloaded[name] = now;
    return HttpResponse.json({ reloaded_sections: reloaded, errors: [], requires_restart: [] });
  }),

  // ---- Federation destination detail (list is above, under Dashboard) ----
  http.get(`${API}/federation/destinations/:server_name`, ({ params }) => {
    const destination = federationDestinations.find(
      (d) => d.server_name === decodeURIComponent(String(params.server_name)),
    );
    if (!destination)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Destination not found", status: 404 },
        { status: 404 },
      );
    return HttpResponse.json(destination);
  }),

  http.get(`${API}/federation/destinations/:server_name/rooms`, ({ params, request }) => {
    const server = decodeURIComponent(String(params.server_name));
    const rows = destinationRooms(server);
    if (!rows)
      return problem(404, "not-found", "Not found", {
        detail: `this server has never tried to reach ${server} and shares no room with it`,
      });
    const url = new URL(request.url);
    return HttpResponse.json({ ...paginate(rows, url), total: rows.length });
  }),

  http.get(`${API}/federation/keys`, () => HttpResponse.json(ownKeys)),

  http.get(`${API}/federation/keys/:server_name`, ({ params }) => {
    const server = decodeURIComponent(String(params.server_name));
    const keys = cachedKeys(server);
    return keys
      ? HttpResponse.json(keys)
      : problem(404, "not-found", "Not found", {
          detail: `this server holds no keys for ${server}`,
        });
  }),

  http.post(`${API}/federation/keys/:server_name/refresh`, ({ params }) => {
    const task = startKeyRefresh(decodeURIComponent(String(params.server_name)));
    return HttpResponse.json(task, {
      status: 202,
      headers: { Location: `/api/v1/tasks/${task.id}` },
    });
  }),

  http.post(`${API}/federation/destinations/:server_name/reset`, ({ params }) => {
    const destination = federationDestinations.find(
      (d) => d.server_name === decodeURIComponent(String(params.server_name)),
    );
    if (!destination)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found" },
        { status: 404 },
      );
    destination.failing_since = null;
    destination.retry_interval_ms = null;
    destination.retry_last_at = null;
    return HttpResponse.json(destination);
  }),

  // ---- Media (the nine media.* operations; crates/hs-admin/src/media.rs) ----
  //
  // The two bulk routes are registered before `/media/:server_name/:media_id` so a POST to
  // `/media/delete` is never read as a media id.
  http.post(`${API}/media/delete`, async ({ request }) => {
    const body = (await request.json().catch(() => ({}))) as {
      before?: string;
      min_size_bytes?: number;
    };
    if (!body.before) {
      return problem(400, "validation-failed", "Validation failed", {
        detail: "before is required: a bulk deletion names how long media must have gone unused",
        errors: [{ pointer: "/before", detail: "before is required" }],
      });
    }
    const before = body.before;
    return purgeMedia(
      "media.delete",
      (m) => m.origin === "local" && m.size_bytes >= (body.min_size_bytes ?? 0),
      before,
    );
  }),

  http.post(`${API}/media/purge-remote-cache`, async ({ request }) => {
    const body = (await request.json().catch(() => ({}))) as {
      before?: string;
      server_name?: string;
    };
    return purgeMedia(
      "media.purge_remote_cache",
      (m) => m.origin === "remote" && (!body.server_name || m.server_name === body.server_name),
      body.before ?? new Date().toISOString(),
    );
  }),

  http.get(`${API}/media`, ({ request }) => {
    const url = new URL(request.url);
    const rows = listMedia(url.searchParams);
    return HttpResponse.json({ ...paginate(rows, url), total: rows.length });
  }),

  http.get(`${API}/media/:server_name/:media_id`, ({ params }) => {
    const item = findMedia(String(params.server_name), String(params.media_id));
    return item ? HttpResponse.json(item) : mediaNotFound();
  }),

  http.delete(`${API}/media/:server_name/:media_id`, ({ params }) => {
    const item = findMedia(String(params.server_name), String(params.media_id));
    if (!item) return mediaNotFound();
    removeMedia(item);
    return new HttpResponse(null, { status: 204 });
  }),

  ...(["quarantine", "unquarantine", "protect", "unprotect"] as const).map((action) =>
    http.post(`${API}/media/:server_name/:media_id/${action}`, ({ params }) => {
      const item = findMedia(String(params.server_name), String(params.media_id));
      if (!item) return mediaNotFound();
      if (action === "quarantine" && item.protected) {
        return problem(409, "conflict", "Conflict", {
          detail: "this media is protected; unprotect it before quarantining it",
        });
      }
      if (action === "protect" && item.quarantined) {
        return problem(409, "conflict", "Conflict", {
          detail: "this media is quarantined; lift the quarantine before protecting it",
        });
      }
      if (action === "quarantine" || action === "unquarantine") {
        item.quarantined = action === "quarantine";
      } else {
        item.protected = action === "protect";
      }
      return HttpResponse.json(item);
    }),
  ),

  // The one Matrix route the Media page calls: an authenticated thumbnail, for previews.
  http.get("*/_matrix/client/v1/media/thumbnail/:server_name/:media_id", ({ params, request }) => {
    const url = new URL(request.url);
    const item = findMedia(String(params.server_name), String(params.media_id));
    if (!item || item.quarantined) {
      return HttpResponse.json({ errcode: "M_NOT_FOUND", error: "Not found" }, { status: 404 });
    }
    const svg = thumbnailSvg(
      item.media_id,
      Number(url.searchParams.get("width") ?? 96),
      Number(url.searchParams.get("height") ?? 96),
    );
    return new HttpResponse(svg, { headers: { "Content-Type": "image/svg+xml" } });
  }),
];

/** A drain or undrain's answer: the replica, or the problem the server would give. */
function replicaAnswer(outcome: ReplicaOutcome) {
  if ("replica" in outcome) return HttpResponse.json(outcome.replica);
  return outcome.problem === "not-found"
    ? problem(404, "not-found", "Not found", { detail: outcome.detail })
    : problem(409, "conflict", "Conflict", { detail: outcome.detail, reason: outcome.reason });
}

/** A migration control's answer: the new status, the status unchanged, or the refusal. */
function migrationAnswer(outcome: MigrationOutcome | null) {
  if (outcome === null) return HttpResponse.json(migrationStatus());
  if (outcome.ok) return HttpResponse.json(outcome.status);
  return outcome.status === 400
    ? problem(400, "validation-failed", "Validation failed", { detail: outcome.detail })
    : problem(409, "conflict", "Conflict", { detail: outcome.detail });
}

/** A verification or cutover: `202` with its task, or the refusal. */
function migrationTask(
  outcome: { ok: true; task: components["schemas"]["Task"] } | { ok: false; detail: string },
) {
  if (!outcome.ok) return problem(409, "conflict", "Conflict", { detail: outcome.detail });
  return HttpResponse.json(outcome.task, {
    status: 202,
    headers: { Location: `/api/v1/tasks/${outcome.task.id}` },
  });
}

function mediaNotFound() {
  return problem(404, "not-found", "Not found", { detail: "this server holds no such media" });
}

/**
 * A bulk deletion as the server runs one: everything `inScope` selects that has gone unused
 * since `before`, except protected items and quarantined remote copies, answered as a Task that
 * has already finished.
 */
/** How long the mock takes over each item of a bulk deletion, so its progress can be seen. */
let bulkStepMs = 150;

/** Sets how long the mock takes over each item of a bulk deletion; returns the old value. */
export function setMockBulkStepMs(ms: number): number {
  const old = bulkStepMs;
  bulkStepMs = ms;
  return old;
}

/**
 * A bulk media deletion as the server runs it: the items are selected now, the answer is the
 * task `running`, and the deletion goes on one item every {@link setMockBulkStepMs} (150 ms by default), recording
 * its progress on the task (a `task.changed` event each time) until it ends `succeeded` with a
 * `media.deleted` event, or stops where it is when the task is cancelled.
 */
function purgeMedia(action: string, inScope: (m: MediaItem) => boolean, before: string) {
  const selected: MediaItem[] = [];
  let skippedProtected = 0;
  let skippedQuarantined = 0;
  for (const item of mediaItems) {
    if (!inScope(item) || !(lastUsed(item) < before)) continue;
    if (item.protected) skippedProtected += 1;
    else if (item.origin === "remote" && item.quarantined) skippedQuarantined += 1;
    else selected.push(item);
  }
  const now = new Date().toISOString();
  const id = `task_${Math.random().toString(36).slice(2, 10)}`;
  const total = selected.length;
  let done = 0;
  let deleted = 0;
  let bytes = 0;
  const result = () => ({
    deleted_count: deleted,
    deleted_bytes: bytes,
    skipped_protected: skippedProtected,
    skipped_quarantined: skippedQuarantined,
    failed: [],
  });
  const task = putTask({
    id,
    action,
    status: "running",
    progress: { current: 0, total, unit: "items" },
    result: null,
    error: null,
    created_at: now,
    started_at: now,
    finished_at: null,
  });
  const step = () => {
    const current = getTask(id);
    // Cancelled (or the mock was reset under it): it stops where it is.
    if (!current || current.status !== "running") return;
    if (done < total) {
      const item = selected[done];
      done += 1;
      if (item && findMedia(item.server_name, item.media_id)) {
        removeMedia(item);
        deleted += 1;
        bytes += item.size_bytes;
      }
      putTask({ ...current, progress: { current: done, total, unit: "items" } });
      setTimeout(step, bulkStepMs);
      return;
    }
    publishMockEvent("media.deleted", result(), { type: "task", id });
    putTask({
      ...current,
      status: "succeeded",
      finished_at: new Date().toISOString(),
      result: result(),
    });
  };
  setTimeout(step, bulkStepMs);
  return HttpResponse.json(task, {
    status: 202,
    headers: { Location: `/api/v1/tasks/${id}` },
  });
}
