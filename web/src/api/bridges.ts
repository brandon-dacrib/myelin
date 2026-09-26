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
export type BridgeTypeSignIn = NonNullable<BridgeType["sign_in"]>;
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

/**
 * `refetchInterval` defaults to the list's cadence; the Created page, which is watching for
 * the bridge's first ping, asks for a faster one.
 */
export function useAppservice(id: string | undefined, options: { refetchInterval?: number } = {}) {
  return useQuery({
    queryKey: ["appservice", id],
    enabled: Boolean(id),
    queryFn: async () => {
      const result = await api.GET("/appservices/{id}", { params: { path: { id: id! } } });
      return unwrap(result);
    },
    refetchInterval: options.refetchInterval ?? 15_000,
  });
}

export function useAppserviceHealth(
  id: string | undefined,
  options: { refetchInterval?: number } = {},
) {
  return useQuery({
    queryKey: ["appservice-health", id],
    enabled: Boolean(id),
    queryFn: async () => {
      const result = await api.GET("/appservices/{id}/health", {
        params: { path: { id: id! } },
      });
      return unwrap(result);
    },
    refetchInterval: options.refetchInterval ?? 15_000,
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

// ---- Offerings and instances (RFC 0017: the server deploys its own bridges, one per person) ----
//
// An offering is a bridge type an administrator has switched on for this server; an instance is
// one user's bridge (or, for a `shared` type, the one bridge everyone uses). Every instance is
// also an appservice registration, so the `appservices.*` hooks above keep seeing them.

export type BridgeDeploymentTarget = components["schemas"]["BridgeDeploymentTarget"];
export type BridgeOffering = components["schemas"]["BridgeOffering"];
export type BridgeOfferingRequest = components["schemas"]["BridgeOfferingRequest"];
export type BridgeOfferingRuntime = BridgeOffering["runtime"];
export type BridgeInstance = components["schemas"]["BridgeInstance"];
export type BridgeInstanceState = BridgeInstance["state"];
export type BridgeInstanceFiles = components["schemas"]["BridgeInstanceFiles"];
export type BridgeMode = BridgeOffering["mode"];

/** Every instance state, in the order an instance moves through them. */
export const BRIDGE_INSTANCE_STATES: BridgeInstanceState[] = [
  "requested",
  "registered",
  "deploying",
  "starting",
  "ready",
  "failed",
  "removing",
];

/** `ready` and `failed` are where an instance rests; anything else is still moving. */
export function isSettledInstanceState(state: string): boolean {
  return state === "ready" || state === "failed";
}

/** The path segment for an instance: its owner, or `_` for a shared type's one instance. */
export function instanceUserSegment(instance: Pick<BridgeInstance, "user_id">): string {
  return instance.user_id ?? "_";
}

/** How fast to poll while something is still moving (RFC 0017 4.1: a pod takes a minute or two). */
export const MOVING_POLL_MS = 5_000;

export function useBridgeDeploymentTarget() {
  return useQuery({
    queryKey: ["bridge-deployment-target"],
    queryFn: async () => unwrap(await api.GET("/bridge-deployment-target")),
    staleTime: 60_000,
  });
}

/** Whether an offering's counts include any instance that has not settled yet. */
export function offeringIsMoving(offering: Pick<BridgeOffering, "instances">): boolean {
  return Object.entries(offering.instances ?? {}).some(
    ([state, count]) => count > 0 && !isSettledInstanceState(state),
  );
}

export function useBridgeOfferings() {
  return useQuery({
    queryKey: ["bridge-offerings"],
    queryFn: async () => unwrap(await api.GET("/bridge-offerings")).data,
    refetchInterval: (query) =>
      (query.state.data ?? []).some(offeringIsMoving) ? MOVING_POLL_MS : 30_000,
  });
}

export function useBridgeOffering(type: string | undefined) {
  return useQuery({
    queryKey: ["bridge-offering", type],
    enabled: Boolean(type),
    queryFn: async () =>
      unwrap(await api.GET("/bridge-offerings/{type}", { params: { path: { type: type! } } })),
    refetchInterval: (query) =>
      query.state.data && offeringIsMoving(query.state.data) ? MOVING_POLL_MS : 30_000,
  });
}

function invalidateOffering(qc: ReturnType<typeof useQueryClient>, type: string) {
  qc.invalidateQueries({ queryKey: ["bridge-offerings"] });
  qc.invalidateQueries({ queryKey: ["bridge-offering", type] });
  qc.invalidateQueries({ queryKey: ["bridge-instances", type] });
  // Each instance is a registration too.
  qc.invalidateQueries({ queryKey: ["appservices"] });
}

/** Creates or replaces an offering (`PUT`: the whole request, not a patch). */
export function usePutBridgeOffering() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ type, body }: { type: string; body: BridgeOfferingRequest }) =>
      unwrap(await api.PUT("/bridge-offerings/{type}", { params: { path: { type } }, body })),
    onSuccess: (offering, { type }) => {
      qc.setQueryData(["bridge-offering", type], offering);
      invalidateOffering(qc, type);
    },
  });
}

/** Stops offering a type. `removeInstances` removes every instance first (a 409 without it). */
export function useDeleteBridgeOffering() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ type, removeInstances }: { type: string; removeInstances?: boolean }) => {
      unwrap(
        await api.DELETE("/bridge-offerings/{type}", {
          params: {
            path: { type },
            query: removeInstances ? { remove_instances: true } : undefined,
          },
        }),
      );
    },
    onSuccess: (_data, { type }) => {
      qc.removeQueries({ queryKey: ["bridge-offering", type] });
      qc.removeQueries({ queryKey: ["bridge-instances", type] });
      qc.invalidateQueries({ queryKey: ["bridge-offerings"] });
      qc.invalidateQueries({ queryKey: ["appservices"] });
    },
  });
}

/** An offering's instances, polled every 5 seconds while any of them is still on its way. */
export function useBridgeInstances(type: string | undefined) {
  return useQuery({
    queryKey: ["bridge-instances", type],
    enabled: Boolean(type),
    queryFn: async () =>
      unwrap(
        await api.GET("/bridge-offerings/{type}/instances", {
          params: { path: { type: type! } },
        }),
      ).data,
    refetchInterval: (query) =>
      (query.state.data ?? []).some((i) => !isSettledInstanceState(i.state))
        ? MOVING_POLL_MS
        : 30_000,
  });
}

export function useBridgeInstance(type: string | undefined, userId: string | undefined) {
  return useQuery({
    queryKey: ["bridge-instance", type, userId],
    enabled: Boolean(type) && Boolean(userId),
    queryFn: async () =>
      unwrap(
        await api.GET("/bridge-offerings/{type}/instances/{user_id}", {
          params: { path: { type: type!, user_id: userId! } },
        }),
      ),
    refetchInterval: (query) =>
      query.state.data && !isSettledInstanceState(query.state.data.state) ? MOVING_POLL_MS : false,
  });
}

/**
 * Creates a user's instance, exactly as their message to the front door would. Idempotent: an
 * existing instance comes back as it is, and a failed one is retried, so this is also Retry.
 */
export function usePutBridgeInstance() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ type, userId }: { type: string; userId: string }) =>
      unwrap(
        await api.PUT("/bridge-offerings/{type}/instances/{user_id}", {
          params: { path: { type, user_id: userId } },
        }),
      ),
    onSuccess: (_instance, { type, userId }) => {
      invalidateOffering(qc, type);
      qc.invalidateQueries({ queryKey: ["bridge-instance", type, userId] });
    },
  });
}

/** Stops an instance and removes its registration, pod and volume: the user's sign-ins go too. */
export function useDeleteBridgeInstance() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ type, userId }: { type: string; userId: string }) => {
      unwrap(
        await api.DELETE("/bridge-offerings/{type}/instances/{user_id}", {
          params: { path: { type, user_id: userId } },
        }),
      );
    },
    onSuccess: (_data, { type, userId }) => {
      qc.removeQueries({ queryKey: ["bridge-instance", type, userId] });
      invalidateOffering(qc, type);
    },
  });
}

/**
 * Renders an instance's files to run it elsewhere. A `POST` because they carry its tokens and
 * need `bridges:write`; it creates nothing, so nothing is invalidated.
 */
export function useBridgeInstanceFiles() {
  return useMutation({
    mutationFn: async ({ type, userId }: { type: string; userId: string }) =>
      unwrap(
        await api.POST("/bridge-offerings/{type}/instances/{user_id}/files", {
          params: { path: { type, user_id: userId } },
        }),
      ),
  });
}
