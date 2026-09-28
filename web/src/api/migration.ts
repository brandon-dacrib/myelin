/**
 * The migration from Synapse (`migration.*` in `crates/hs-admin/openapi/openapi.yaml`), and the
 * `migration` configuration section that says where Synapse's database is.
 *
 * The connection string is never in the migration API: `POST /migration/start` names a
 * configuration pointer (`/migration/synapse` by default) and the server reads the source from
 * there. So pointing at a Synapse database is a `config.update` of the `migration` section,
 * whose password comes back only as `{"$secret": true}`. Starting, pausing, resuming and
 * aborting answer the new status at once; verifying and cutting over answer `202` with a task,
 * and the status says how it went. The status polls every 1.5s while something runs.
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import type { JsonValue } from "./config-schema";
import { unwrap } from "./problem";
import type { components } from "./schema";
import { hasScope } from "@/lib/auth";

export type MigrationStatus = components["schemas"]["MigrationStatus"];
export type MigrationPhase = NonNullable<MigrationStatus["status"]>;
export type MigrationStream = NonNullable<MigrationStatus["streams"]>[number];
export type MigrationLogEntry = components["schemas"]["MigrationLogEntry"];

const FAST_POLL_MS = 1_500;
const SLOW_POLL_MS = 15_000;

/** The phases in which the server is working on the migration. */
export function migrationIsRunning(status: Pick<MigrationStatus, "status"> | undefined): boolean {
  return (
    status?.status === "copying" ||
    status?.status === "verifying" ||
    status?.status === "cutting_over"
  );
}

export function useMigration(options?: { enabled?: boolean }) {
  return useQuery({
    queryKey: ["migration"],
    enabled: hasScope("admin:read") && (options?.enabled ?? true),
    queryFn: async () => unwrap(await api.GET("/migration")),
    refetchInterval: (query) =>
      migrationIsRunning(query.state.data) ? FAST_POLL_MS : SLOW_POLL_MS,
  });
}

export function useMigrationLog(limit: number) {
  const status = useMigration().data;
  const running = migrationIsRunning(status);
  return useQuery({
    // Read again whenever the status moves, so what ended a step is in the log at once.
    queryKey: ["migration-log", limit, status?.status, status?.task_id],
    enabled: hasScope("admin:read"),
    placeholderData: (previous) => previous,
    queryFn: async () => {
      // Oldest first on the server; the page shows the most recent `limit`, newest first.
      const first = unwrap(
        await api.GET("/migration/log", { params: { query: { limit: 1, include_total: true } } }),
      );
      const total = first.total ?? 0;
      const offset = Math.max(0, total - limit);
      const page = unwrap(
        await api.GET("/migration/log", {
          params: { query: { limit, cursor: offset > 0 ? String(offset) : undefined } },
        }),
      );
      return { total, items: [...page.items].reverse() };
    },
    refetchInterval: running ? FAST_POLL_MS : SLOW_POLL_MS,
  });
}

/** The Synapse source as the `migration` configuration section holds it. */
export interface SynapseSource {
  host: string;
  port: number;
  database: string;
  user: string;
  /** Whether a password is stored (it is never sent back). */
  passwordSet: boolean;
  mediaStorePath: string | null;
  batchSize: number;
}

function asSource(values: Record<string, JsonValue> | undefined): SynapseSource | null {
  const synapse = values?.synapse as Record<string, JsonValue> | null | undefined;
  if (!synapse || typeof synapse !== "object") return null;
  const database = (synapse.database ?? {}) as Record<string, JsonValue>;
  return {
    host: String(database.host ?? ""),
    port: Number(database.port ?? 5432),
    database: String(database.database ?? ""),
    user: String(database.user ?? ""),
    passwordSet: database.password != null,
    mediaStorePath: typeof synapse.media_store_path === "string" ? synapse.media_store_path : null,
    batchSize: Number(synapse.batch_size ?? 500),
  };
}

export function useMigrationSource() {
  return useQuery({
    queryKey: ["config-section", "migration"],
    enabled: hasScope("admin:read"),
    queryFn: async () => {
      const result = await api.GET("/config/{section}", {
        params: { path: { section: "migration" } },
      });
      const data = unwrap(result) as { values?: Record<string, JsonValue> };
      return asSource(data.values);
    },
  });
}

export interface SourceInput {
  host: string;
  port: number;
  database: string;
  user: string;
  /** `undefined` keeps the stored password. */
  password?: string;
  mediaStorePath: string | null;
  batchSize: number;
}

export function useSetMigrationSource() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: SourceInput) => {
      const database: Record<string, JsonValue> = {
        host: input.host,
        port: input.port,
        database: input.database,
        user: input.user,
      };
      if (input.password !== undefined) database.password = input.password;
      const patch = {
        synapse: {
          database,
          media_store_path: input.mediaStorePath,
          batch_size: input.batchSize,
        },
      };
      return unwrap(
        await api.PATCH("/config/{section}", {
          params: { path: { section: "migration" } },
          body: patch as unknown as Record<string, never>,
        }),
      );
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["config-section", "migration"] });
      qc.invalidateQueries({ queryKey: ["migration"] });
    },
  });
}

type Control = "start" | "pause" | "resume" | "abort";

export function useMigrationControl(control: Control) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async () => {
      const params = { header: { "Idempotency-Key": newIdempotencyKey() } };
      switch (control) {
        case "start":
          return unwrap(await api.POST("/migration/start", { params, body: {} }));
        case "pause":
          return unwrap(await api.POST("/migration/pause", { params }));
        case "resume":
          return unwrap(await api.POST("/migration/resume", { params }));
        case "abort":
          return unwrap(await api.POST("/migration/abort", { params }));
      }
    },
    onSuccess: (status) => {
      qc.setQueryData(["migration"], status);
      qc.invalidateQueries({ queryKey: ["migration-log"] });
    },
  });
}

export function useMigrationTask(step: "verify" | "cutover") {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async () => {
      const params = { header: { "Idempotency-Key": newIdempotencyKey() } };
      return step === "verify"
        ? unwrap(await api.POST("/migration/verify", { params }))
        : unwrap(await api.POST("/migration/cutover", { params }));
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["migration"] });
      qc.invalidateQueries({ queryKey: ["migration-log"] });
      qc.invalidateQueries({ queryKey: ["tasks"] });
    },
  });
}
