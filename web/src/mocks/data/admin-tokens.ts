import type { AdminToken } from "@/api/admin-tokens";

/**
 * The mock's admin tokens: one of each shape the tokens table explains (a full administrator's
 * for automation, a bridge team's read-only one with an expiry, a moderators' one). Mutable
 * module state -- mint and revoke change it -- restored after every Vitest test by
 * {@link resetAdminTokens} (`src/test/setup.ts`).
 */
const DAY = 24 * 3_600_000;

function seed(now = Date.now()): AdminToken[] {
  const iso = (offsetMs: number) => new Date(now + offsetMs).toISOString();
  return [
    {
      id: "01J9ADM1N0000000000000CI00",
      name: "Deploy pipeline",
      scopes: ["admin:read", "admin:write"],
      created_at: iso(-40 * DAY),
      created_by: "@ops:example.org",
      expires_at: null,
    },
    {
      id: "01J9ADM1N0000000000000BR00",
      name: "Bridge team dashboard",
      scopes: ["bridges:read"],
      created_at: iso(-3 * DAY),
      created_by: "@ops:example.org",
      expires_at: iso(27 * DAY),
    },
    {
      id: "01J9ADM1N0000000000000MD00",
      name: "Moderators' bot",
      scopes: ["moderation:write"],
      created_at: iso(-DAY),
      created_by: "01J9ADM1N0000000000000CI00",
      expires_at: null,
    },
  ];
}

export const adminTokens: AdminToken[] = seed();

export function resetAdminTokens(): void {
  adminTokens.splice(0, adminTokens.length, ...seed());
}

export function findAdminToken(id: string): AdminToken | undefined {
  return adminTokens.find((t) => t.id === id);
}

let minted = 0;

/** A fresh id and bearer string, the shape the server mints (`hsa_` and 40 characters). */
export function mintMockAdminToken(): { id: string; token: string } {
  minted += 1;
  const suffix = String(minted).padStart(4, "0");
  return {
    id: `01J9ADM1N000000000000NEW${suffix.slice(-2)}`,
    token: `hsa_${"mock".repeat(9)}${suffix}`,
  };
}
