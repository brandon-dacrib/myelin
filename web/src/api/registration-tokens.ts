/**
 * Registration tokens (`registration_tokens.*` in `crates/hs-admin/openapi/openapi.yaml`): list,
 * create, change and delete the tokens that let somebody register while open registration is
 * off. Settings, Registration tokens, and the Users page's "Invite by link".
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import type { components } from "./schema";
import type { RegistrationTokenView } from "@/lib/registration-tokens";

export type RegistrationToken = components["schemas"]["RegistrationToken"];

/**
 * Fills in what the contract leaves optional, so the pages read one shape. A token the server
 * sent without a `token` is not one anybody can use; it is dropped by the list.
 */
export function toTokenView(t: RegistrationToken): RegistrationTokenView {
  return {
    token: t.token ?? "",
    valid: t.valid ?? false,
    usesAllowed: t.uses_allowed ?? null,
    pending: t.pending ?? 0,
    completed: t.completed ?? 0,
    expiresAt: t.expires_at ?? null,
    createdAt: t.created_at ?? null,
  };
}

export function useRegistrationTokens(cursor?: string) {
  return useQuery({
    queryKey: ["registration-tokens", cursor ?? null],
    queryFn: async () => {
      const page = unwrap(
        await api.GET("/registration-tokens", { params: { query: { cursor, limit: 50 } } }),
      );
      return {
        items: page.items.map(toTokenView).filter((t) => t.token),
        nextCursor: page.next_cursor ?? null,
      };
    },
    refetchInterval: 30_000,
  });
}

/** What the create dialog asks for. `token` absent means "generate one of `length`". */
export interface CreateTokenInput {
  token?: string;
  length: number;
  usesAllowed: number | null;
  expiresAt: string | null;
}

export function useCreateRegistrationToken() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: CreateTokenInput) => {
      const created = unwrap(
        await api.POST("/registration-tokens", {
          params: { header: { "Idempotency-Key": newIdempotencyKey() } },
          body: {
            token: input.token,
            length: input.length,
            uses_allowed: input.usesAllowed,
            expires_at: input.expiresAt,
          },
        }),
      );
      return toTokenView(created);
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ["registration-tokens"] }),
  });
}

/** A change to a token: only the fields present are sent, and `null` clears a limit. */
export interface UpdateTokenInput {
  token: string;
  usesAllowed?: number | null;
  expiresAt?: string | null;
}

export function useUpdateRegistrationToken() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ token, usesAllowed, expiresAt }: UpdateTokenInput) => {
      const body: components["schemas"]["RegistrationTokenUpdate"] = {};
      if (usesAllowed !== undefined) body.uses_allowed = usesAllowed;
      if (expiresAt !== undefined) body.expires_at = expiresAt;
      const updated = unwrap(
        await api.PATCH("/registration-tokens/{token}", {
          params: { path: { token } },
          body,
        }),
      );
      return toTokenView(updated);
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ["registration-tokens"] }),
  });
}

export function useDeleteRegistrationToken() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (token: string) => {
      unwrap(
        await api.DELETE("/registration-tokens/{token}", {
          params: { path: { token } },
        }),
      );
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ["registration-tokens"] }),
  });
}
