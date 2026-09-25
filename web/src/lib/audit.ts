import type { components } from "@/api/schema";

/**
 * How the interface reads an audit entry (docs/design/information-architecture.md, Audit log:
 * "when, who, what, change summary, request ID").
 *
 * An entry's `action` is the operation id that produced it (`users.suspend`,
 * `federation.destinations.reset`) and its `target` is an open-enum resource reference. Neither
 * is what an operator scanning a log wants to read, so this turns them into a sentence and a
 * link. The raw values are always shown too: the sentence is a reading aid, never a substitute
 * for the record.
 */

export type AuditEntry = components["schemas"]["AuditEntry"];
export type AuditActor = components["schemas"]["Actor"];
export type AuditTarget = components["schemas"]["ResourceRef"];

/** The resource types the contract names for `ResourceRef.type` (an open enum, so others may appear). */
export const TARGET_TYPES = [
  "user",
  "device",
  "room",
  "event",
  "media",
  "appservice",
  "destination",
  "report",
  "registration_token",
  "task",
  "replica",
  "config_section",
  "migration",
  "server_key",
  "server_notice",
] as const;

const TARGET_LABELS: Record<string, string> = {
  user: "User",
  device: "Device",
  room: "Room",
  event: "Event",
  media: "Media",
  appservice: "Bridge",
  destination: "Destination",
  report: "Report",
  registration_token: "Registration token",
  task: "Task",
  replica: "Replica",
  config_section: "Configuration",
  migration: "Migration",
  server_key: "Server key",
  server_notice: "Server notice",
};

/** "Registration token" for `registration_token`; an unknown type is spelled out rather than hidden. */
export function targetTypeLabel(type: string): string {
  return TARGET_LABELS[type] ?? humanize(type);
}

/**
 * The page the interface has for a resource, or `null` for one it has no page for yet
 * (a device, an event, a registration token). Callers render the identifier either way; the
 * link is what changes.
 */
export function targetRoute(
  target: AuditTarget,
):
  | { to: "/users/$userId"; params: { userId: string } }
  | { to: "/rooms/$roomId"; params: { roomId: string } }
  | { to: "/bridges/$bridgeId"; params: { bridgeId: string } }
  | { to: "/federation/$serverName"; params: { serverName: string } }
  | { to: "/configuration/$section"; params: { section: string } }
  | null {
  switch (target.type) {
    case "user":
      return { to: "/users/$userId", params: { userId: target.id } };
    case "room":
      return { to: "/rooms/$roomId", params: { roomId: target.id } };
    case "appservice":
      return { to: "/bridges/$bridgeId", params: { bridgeId: target.id } };
    case "destination":
      return { to: "/federation/$serverName", params: { serverName: target.id } };
    case "config_section":
      return { to: "/configuration/$section", params: { section: target.id } };
    default:
      return null;
  }
}

/** Actions whose generic "<verb> <noun>" reading would be wrong or awkward. */
const ACTION_PHRASES: Record<string, string> = {
  "setup.create": "Created the first administrator",
  "users.reset_password": "Reset password",
  "users.logout": "Signed out everywhere",
  "users.login_as": "Signed in as",
  "users.redact_events": "Redacted messages",
  "users.devices.delete": "Signed out device",
  "users.devices.bulk_delete": "Signed out devices",
  "users.media.delete": "Deleted media",
  "users.shadow_ban": "Shadow-banned user",
  "users.unshadow_ban": "Lifted shadow ban",
  "users.rate_limit.put": "Set rate limit",
  "users.rate_limit.delete": "Cleared rate limit",
  "users.experimental_features.put": "Set experimental features",
  "rooms.make_admin": "Granted admin in room",
  "rooms.purge_history": "Purged room history",
  "rooms.media.quarantine": "Quarantined room media",
  "rooms.forward_extremities.delete": "Deleted forward extremities",
  "rooms.join": "Joined user to room",
  "appservices.rotate_tokens": "Rotated bridge tokens",
  "appservices.replay": "Replayed bridge transactions",
  "appservices.ping": "Pinged bridge",
  "config.reload": "Reloaded configuration",
  "config.update": "Changed configuration",
  "federation.destinations.reset": "Reset destination backoff",
  "federation.keys.refresh": "Refreshed server keys",
  "media.delete_bulk": "Deleted media",
  "media.delete_one": "Deleted media",
  "media.purge_remote_cache": "Purged remote media cache",
  "reports.resolve": "Resolved report",
  "server_notices.send": "Sent server notice",
  "migration.cutover": "Cut over migration",
  "migration.abort": "Aborted migration",
  "cluster.replicas.drain": "Drained replica",
  "cluster.replicas.undrain": "Undrained replica",
  "tasks.cancel": "Cancelled task",
};

const VERBS: Record<string, string> = {
  create: "Created",
  add: "Added",
  update: "Updated",
  put: "Set",
  delete: "Deleted",
  remove: "Removed",
  lock: "Locked",
  unlock: "Unlocked",
  suspend: "Suspended",
  unsuspend: "Unsuspended",
  deactivate: "Deactivated",
  reactivate: "Reactivated",
  block: "Blocked",
  unblock: "Unblocked",
  pause: "Paused",
  resume: "Resumed",
  start: "Started",
  verify: "Verified",
  quarantine: "Quarantined",
  unquarantine: "Unquarantined",
  protect: "Protected",
  unprotect: "Unprotected",
};

const NOUNS: Record<string, string> = {
  users: "user",
  "users.devices": "device",
  "users.threepids": "email or phone number",
  "users.external_ids": "external ID",
  "users.media": "media",
  rooms: "room",
  "rooms.aliases": "room alias",
  "rooms.media": "room media",
  appservices: "bridge",
  config: "configuration",
  "federation.destinations": "destination",
  "federation.keys": "server key",
  media: "media",
  reports: "report",
  registration_tokens: "registration token",
  server_notices: "server notice",
  tasks: "task",
  migration: "migration",
  "cluster.replicas": "replica",
  setup: "administrator",
};

/**
 * "Suspended user" for `users.suspend`, "Created registration token" for
 * `registration_tokens.create`. An action this cannot read is returned as it is, so a new
 * operation on the server is still legible in the log before anyone teaches the interface its
 * name.
 */
export function describeAction(action: string): string {
  const phrase = ACTION_PHRASES[action];
  if (phrase) return phrase;
  const dot = action.lastIndexOf(".");
  if (dot < 0) return action;
  const resource = action.slice(0, dot);
  const verb = VERBS[action.slice(dot + 1)];
  const noun = NOUNS[resource];
  if (!verb || !noun) return action;
  return `${verb} ${noun}`;
}

/** Whether the mutation succeeded: RFC 0004 counts 2xx and 3xx as success, as `AuditFilter` does. */
export function succeeded(entry: Pick<AuditEntry, "outcome">): boolean {
  const status = entry.outcome.status ?? 200;
  return status >= 200 && status < 400;
}

const ACTOR_KIND_LABELS: Record<AuditActor["kind"], string> = {
  user: "User",
  client: "Client",
  service_account: "Service account",
  system: "System",
};

export function actorKindLabel(kind: AuditActor["kind"]): string {
  return ACTOR_KIND_LABELS[kind] ?? humanize(kind);
}

function humanize(identifier: string): string {
  const words = identifier.replace(/[_.]+/g, " ").trim();
  return words ? words.charAt(0).toUpperCase() + words.slice(1) : identifier;
}
