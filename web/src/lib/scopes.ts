import { ALL_SCOPES, type Scope } from "./auth";

/**
 * What each scope lets a token's holder do, in a sentence, for the admin token picker
 * (`src/pages/settings/CreateAdminTokenDialog.tsx`). The sentences follow the `OAuth2` scheme in
 * `crates/hs-admin/openapi/openapi.yaml` and RFC 0004 section 8.2; the implication rules are in
 * `hasScope` (`src/lib/auth.ts`) and `Scope::satisfies` on the server.
 */
export interface ScopeDescription {
  scope: Scope;
  /** The part of the interface the scope opens, as the sidebar names it. */
  area: string;
  /** What the holder can do with it. */
  grants: string;
}

export const SCOPE_DESCRIPTIONS: readonly ScopeDescription[] = [
  {
    scope: "admin:read",
    area: "Everything, read-only",
    grants:
      "Read every page: users, rooms, media, bridges, federation, the audit log, configuration (secrets hidden), cluster and migration status.",
  },
  {
    scope: "admin:write",
    area: "Everything",
    grants:
      "Do anything an administrator can, including changing configuration, creating users and minting admin tokens. Implies every other scope.",
  },
  {
    scope: "bridges:read",
    area: "Bridges, read-only",
    grants: "See the bridges, their registrations, health, backlog and bridge tasks.",
  },
  {
    scope: "bridges:write",
    area: "Bridges",
    grants:
      "Add, change, pause, resume, replay and remove bridges, rotate their tokens and export their registrations.",
  },
  {
    scope: "moderation:read",
    area: "Moderation, read-only",
    grants:
      "See users, rooms, reports and media, without session addresses, account data or message content.",
  },
  {
    scope: "moderation:write",
    area: "Moderation",
    grants:
      "Suspend, lock, shadow-ban and sign out users, reset passwords, block and purge rooms, quarantine media, resolve reports and send server notices.",
  },
];

/** `scopes` in catalog order, without duplicates: how the server stores and lists them. */
export function normalizeScopes(scopes: readonly Scope[]): Scope[] {
  return ALL_SCOPES.filter((s) => scopes.includes(s));
}

/**
 * The scopes a token effectively holds, with the ones implied by another dropped as redundant:
 * `admin:write` alone is a full administrator's token, and `bridges:write` already reads bridges.
 */
export function redundantScopes(scopes: readonly Scope[]): Scope[] {
  if (scopes.includes("admin:write")) return scopes.filter((s) => s !== "admin:write");
  return scopes.filter(
    (s) =>
      (s === "bridges:read" && scopes.includes("bridges:write")) ||
      (s === "moderation:read" && scopes.includes("moderation:write")) ||
      (s.endsWith(":read") && s !== "admin:read" && scopes.includes("admin:read")),
  );
}

/** A short label for a scope list: "Full administrator" or the scopes joined. */
export function describeScopes(scopes: readonly Scope[]): string {
  if (scopes.includes("admin:write")) return "Full administrator";
  return normalizeScopes(scopes).join(", ");
}
