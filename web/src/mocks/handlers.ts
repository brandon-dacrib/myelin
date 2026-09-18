import { http, HttpResponse } from "msw";
import {
  appservices,
  appserviceHealth,
  appserviceBacklog,
  appserviceRegistration,
  findAppservice,
} from "./data/appservices";
import { bridgeTypes } from "./data/bridge-types";
import {
  statisticsOverview,
  serverInfo,
  clusterStatus,
  federationDestinations,
  recentAuditEntries,
} from "./data/dashboard";
import { users, userDevices, findUser } from "./data/users";
import { rooms, roomMembers, findRoom } from "./data/rooms";
import { ALL_SCOPES, type Scope } from "@/lib/auth";
import type { AppService } from "@/api/bridges";

const API = "/api/v1";

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

const registeredIds = new Set(appservices.map((a) => a.id));

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
      operator: { name: "Operator", subject: "mock-operator" },
    });
  }),

  // ---- Dashboard ----
  http.get(`${API}/statistics/overview`, () => HttpResponse.json(statisticsOverview)),
  http.get(`${API}/server`, () => HttpResponse.json(serverInfo)),
  http.get(`${API}/server/health`, () =>
    HttpResponse.json({ status: "ok", checks: { storage: "ok", federation: "ok" } }),
  ),
  http.get(`${API}/cluster`, () => HttpResponse.json(clusterStatus)),
  http.get(`${API}/federation/destinations`, ({ request }) => {
    const url = new URL(request.url);
    const { items, next_cursor, prev_cursor } = paginate(federationDestinations, url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
  }),
  http.get(`${API}/audit-log`, ({ request }) => {
    const url = new URL(request.url);
    const { items, next_cursor, prev_cursor } = paginate(recentAuditEntries, url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
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
      deployment === "kubernetes"
        ? `http://${id}.${namespace}.svc:29999`
        : "http://localhost:29999";

    const registration = {
      id,
      url,
      as_token: asToken,
      hs_token: hsToken,
      sender_localpart: senderLocalpart,
      namespaces: {
        users: values.userNamespace ? [String(values.userNamespace)] : [],
        aliases: values.aliasNamespace ? [String(values.aliasNamespace)] : [],
      },
      rate_limited: false,
    };
    const registration_yaml = [
      `id: ${id}`,
      `url: ${url}`,
      `as_token: ${asToken}`,
      `hs_token: ${hsToken}`,
      `sender_localpart: ${senderLocalpart}`,
      "namespaces:",
      `  users:${values.userNamespace ? `\n    - exclusive: true\n      regex: '${values.userNamespace}'` : " []"}`,
      `de.sorunome.msc2409.push_ephemeral: ${Boolean(values.encryption)}`,
      `org.matrix.msc3202: ${Boolean(values.encryption)}`,
    ].join("\n");
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
      compose_yaml,
      bridge_resource_yaml,
    });
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
      registration?: { id?: string; sender_localpart?: string; url?: string };
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
    const filtered = q
      ? users.filter(
          (u) =>
            u.user_id.includes(q) || (u.display_name ?? "").toLowerCase().includes(q.toLowerCase()),
        )
      : users;
    const { items, next_cursor, prev_cursor } = paginate(filtered, url);
    return HttpResponse.json({ items, next_cursor, prev_cursor });
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

  http.post(`${API}/users/:user_id/deactivate`, ({ params }) => {
    const user = findUser(decodeURIComponent(String(params.user_id)));
    if (!user)
      return HttpResponse.json(
        { type: "urn:hs:problem:not-found", title: "Not found" },
        { status: 404 },
      );
    user.deactivated = true;
    return HttpResponse.json(user);
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
];
