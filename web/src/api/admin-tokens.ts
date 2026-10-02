/**
 * Admin tokens (`admin_tokens.*` in `crates/hs-admin/openapi/openapi.yaml`): list, mint and
 * revoke admin API tokens narrower than a full administrator's. Settings, Admin tokens.
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import type { components } from "./schema";
import type { Scope } from "@/lib/auth";

export type AdminToken = components["schemas"]["AdminToken"];
export type AdminTokenCreated = components["schemas"]["AdminTokenCreated"];

/** A token as the page reads it: the contract's optional members filled in. */
export interface AdminTokenView {
  id: string;
  name: string;
  scopes: Scope[];
  createdAt: string | null;
  createdBy: string;
  expiresAt: string | null;
}

export function toAdminTokenView(t: AdminToken): AdminTokenView {
  return {
    id: t.id,
    name: t.name,
    scopes: t.scopes as Scope[],
    createdAt: t.created_at ?? null,
    createdBy: t.created_by,
    expiresAt: t.expires_at ?? null,
  };
}

export function useAdminTokens(cursor?: string) {
  return useQuery({
    queryKey: ["admin-tokens", cursor ?? null],
    queryFn: async () => {
      const page = unwrap(
        await api.GET("/admin-tokens", { params: { query: { cursor, limit: 50 } } }),
      );
      return {
        items: page.items.map(toAdminTokenView),
        nextCursor: page.next_cursor ?? null,
      };
    },
    refetchInterval: 30_000,
  });
}

/** What the mint dialog asks for. */
export interface CreateAdminTokenInput {
  name: string;
  scopes: Scope[];
  expiresAt: string | null;
}

/** The minted token: its record and, once, the bearer string. */
export interface MintedAdminToken extends AdminTokenView {
  token: string;
}

export function useCreateAdminToken() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: CreateAdminTokenInput): Promise<MintedAdminToken> => {
      const created = unwrap(
        await api.POST("/admin-tokens", {
          params: { header: { "Idempotency-Key": newIdempotencyKey() } },
          body: {
            name: input.name,
            scopes: input.scopes,
            expires_at: input.expiresAt,
          },
        }),
      );
      return { ...toAdminTokenView(created), token: created.token };
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ["admin-tokens"] }),
  });
}

export function useRevokeAdminToken() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      unwrap(await api.DELETE("/admin-tokens/{id}", { params: { path: { id } } }));
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ["admin-tokens"] }),
  });
}
