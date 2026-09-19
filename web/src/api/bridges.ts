/**
 * Bridges/appservices, reconciled 2026-09-18 against track 15's real
 * `crates/hs-admin/openapi/openapi.yaml` (RFC 0004). See
 * docs/status/16-management-web-interface.md for what changed from this
 * track's own earlier draft (`web/mocks/openapi.yaml`, now the fallback
 * used only if the real document is absent):
 *
 * - Resources are `/appservices` (not `/bridges`) and `/bridge-types` (not
 *   `/bridges/kinds`); "bridge" was always UI framing over the generic
 *   appservice registry (11's model), which the real API makes explicit.
 * - `AppService` has no `name` or `kind` field. There is no field linking a
 *   created appservice back to the bridge-type catalog entry it came from.
 *   This module derives a display name from `id` and a "kind" label from
 *   `protocols` as a UI-only best effort (see `deriveDisplayName`/
 *   `deriveKindLabel`); flagged as feedback for 15/11 in the status file.
 * - There is no inline backlog summary on the list/summary resource, only
 *   a separate paginated `/appservices/{id}/backlog`; the bridges list can
 *   no longer show a backlog column without an N+1 fetch, so it doesn't.
 * - Replay is asynchronous (`202` + a `Task`), not synchronous.
 * - The wizard's registration/compose/Kubernetes-resource YAML is rendered
 *   *by the server* (`POST /bridge-types/{type}/render`), not assembled
 *   client-side; `pages/bridges/wizard/artifacts.ts`'s hand-rolled YAML
 *   builder is gone.
 * - There is no `state`/`kind` filter query parameter on `GET /appservices`,
 *   only `q` (free text), `limit`, `cursor`, `include_total`; the bridges
 *   list filters by health client-side over the loaded page.
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import type { components } from "./schema";

export type AppService = components["schemas"]["AppService"];
// AppService.health is optional in the schema (no `required` list on
// AppService); every appservice in practice always reports one of these,
// so callers normalise a missing value to "unknown" (see bridge-state.ts).
export type AppServiceHealthStatus = NonNullable<AppService["health"]>;
export type AppServiceHealth = components["schemas"]["AppServiceHealth"];
export type AppServiceBacklogEntry = components["schemas"]["AppServiceBacklogEntry"];
export type AppServiceTokens = components["schemas"]["AppServiceTokens"];
export type BridgeType = components["schemas"]["BridgeType"];
export type BridgeTypeRenderResult = components["schemas"]["BridgeTypeRenderResult"];
export type Task = components["schemas"]["Task"];

/** `id` humanised for display: real appservices have no display-name field. */
export function deriveDisplayName(appservice: Pick<AppService, "id">): string {
  const id = appservice.id ?? "";
  if (!id) return "(unknown)";
  return id
    .split(/[-_]/)
    .filter(Boolean)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(" ");
}

/** `protocols` read as the bridge "kind": real appservices carry no bridge-type reference. */
export function deriveKindLabel(appservice: Pick<AppService, "protocols">): string {
  return appservice.protocols && appservice.protocols.length > 0
    ? appservice.protocols.join(", ")
    : "Custom appservice";
}

export interface AppserviceListFilters {
  q?: string;
  cursor?: string;
  limit?: number;
}

export function useAppservices(filters: AppserviceListFilters) {
  return useQuery({
    queryKey: ["appservices", filters],
    queryFn: async () => {
      const result = await api.GET("/appservices", { params: { query: filters } });
      return unwrap(result);
    },
    refetchInterval: 30_000,
  });
}

export function useAppservice(id: string | undefined) {
  return useQuery({
    queryKey: ["appservice", id],
    enabled: Boolean(id),
    queryFn: async () => {
      const result = await api.GET("/appservices/{id}", { params: { path: { id: id! } } });
      return unwrap(result);
    },
    refetchInterval: 15_000,
  });
}

export function useAppserviceHealth(id: string | undefined) {
  return useQuery({
    queryKey: ["appservice-health", id],
    enabled: Boolean(id),
    queryFn: async () => {
      const result = await api.GET("/appservices/{id}/health", {
        params: { path: { id: id! } },
      });
      return unwrap(result);
    },
    refetchInterval: 15_000,
  });
}

export function useAppserviceBacklog(id: string | undefined, limit = 50) {
  return useQuery({
    queryKey: ["appservice-backlog", id],
    enabled: Boolean(id),
    queryFn: async () => {
      const result = await api.GET("/appservices/{id}/backlog", {
        params: { path: { id: id! }, query: { limit } },
      });
      return unwrap(result);
    },
    refetchInterval: 15_000,
  });
}

export function useAppserviceRegistration(id: string | undefined, enabled: boolean) {
  return useQuery({
    queryKey: ["appservice-registration", id],
    enabled: Boolean(id) && enabled,
    queryFn: async () => {
      const result = await api.GET("/appservices/{id}/registration", {
        params: { path: { id: id! } },
        headers: { Accept: "application/json" },
      });
      return unwrap(result) as Record<string, unknown>;
    },
  });
}

export function useBridgeTypes() {
  return useQuery({
    queryKey: ["bridge-types"],
    queryFn: async () => {
      const result = await api.GET("/bridge-types", { params: { query: { limit: 50 } } });
      return unwrap(result).items;
    },
    staleTime: Infinity,
  });
}

/** Server-side rendering of the wizard's chosen config into registration/compose/CRD YAML previews. */
export function useRenderBridgeType() {
  return useMutation({
    mutationFn: async ({ type, values }: { type: string; values: Record<string, unknown> }) => {
      const result = await api.POST("/bridge-types/{type}/render", {
        params: { path: { type } },
        body: values,
      });
      return unwrap(result);
    },
  });
}

export interface CreateAppserviceVariables {
  registration: Record<string, unknown>;
  registrationYaml: string;
  idempotencyKey: string;
}

export function useCreateAppservice() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      registration,
      registrationYaml,
      idempotencyKey,
    }: CreateAppserviceVariables) => {
      const result = await api.POST("/appservices", {
        params: { header: { "Idempotency-Key": idempotencyKey } },
        // `AppServiceCreate.registration` is an untyped OpenAPI `object`
        // (openapi-typescript emits `Record<string, never>` for it); the
        // real payload is whatever `POST /bridge-types/{type}/render`
        // returned, so this is the one sanctioned cast for it.
        body: {
          registration: registration as Record<string, never>,
          registration_yaml: registrationYaml,
        },
      });
      return unwrap(result);
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ["appservices"] }),
  });
}

function invalidateAfterAction(qc: ReturnType<typeof useQueryClient>, id: string) {
  qc.invalidateQueries({ queryKey: ["appservices"] });
  qc.invalidateQueries({ queryKey: ["appservice", id] });
  qc.invalidateQueries({ queryKey: ["appservice-health", id] });
}

export function usePauseAppservice() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      const result = await api.POST("/appservices/{id}/pause", {
        params: { path: { id }, header: { "Idempotency-Key": newIdempotencyKey() } },
      });
      return unwrap(result);
    },
    onSuccess: (_data, id) => invalidateAfterAction(qc, id),
  });
}

export function useResumeAppservice() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      const result = await api.POST("/appservices/{id}/resume", {
        params: { path: { id }, header: { "Idempotency-Key": newIdempotencyKey() } },
      });
      return unwrap(result);
    },
    onSuccess: (_data, id) => invalidateAfterAction(qc, id),
  });
}

export function useRotateAppserviceTokens() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      const result = await api.POST("/appservices/{id}/rotate-tokens", {
        params: { path: { id }, header: { "Idempotency-Key": newIdempotencyKey() } },
      });
      return unwrap(result);
    },
    onSuccess: (_data, id) => {
      invalidateAfterAction(qc, id);
      qc.invalidateQueries({ queryKey: ["appservice-registration", id] });
    },
  });
}

export function useReplayAppserviceBacklog() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      const result = await api.POST("/appservices/{id}/replay", {
        params: { path: { id }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body: {},
      });
      return unwrap(result);
    },
    onSuccess: (_data, id) => qc.invalidateQueries({ queryKey: ["appservice-backlog", id] }),
  });
}

export function useDeleteAppservice() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      const result = await api.DELETE("/appservices/{id}", { params: { path: { id } } });
      unwrap(result);
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ["appservices"] }),
  });
}

export async function checkAppserviceIdAvailable(id: string): Promise<boolean> {
  const { data, response } = await api.GET("/appservices/{id}", { params: { path: { id } } });
  if (data) return false;
  if (response.status === 404) return true;
  // Any other error (network, 5xx): treat as "unknown, assume available";
  // the create call itself is still the source of truth (409 on conflict).
  return true;
}
