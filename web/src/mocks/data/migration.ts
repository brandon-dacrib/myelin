import type { components } from "@/api/schema";
import { configValues } from "./config";
import { putTask } from "./tasks";

type MigrationStatus = components["schemas"]["MigrationStatus"];
type MigrationLogEntry = components["schemas"]["MigrationLogEntry"];
type Task = components["schemas"]["Task"];

/**
 * The mock's migration from Synapse, shaped like `hs_compat::migration`'s answers: one migration,
 * moving on a clock so the page's polling can be seen at work. A copy takes {@link COPY_MS};
 * a verification and a cutover {@link STEP_MS}. The source is whatever the mock configuration's
 * `migration.synapse` holds (`./config`); `start` refuses without one, as the server does.
 */

const COPY_MS = 6_000;
const STEP_MS = 1_500;
const OPERATOR = "@admin:example.org";

/** Rows per stream, in the order `hs_compat::migration::Stream::ALL` copies them. */
const TOTALS: Record<string, number> = {
  users: 1_240,
  devices: 3_115,
  access_tokens: 2_890,
  refresh_tokens: 1_974,
  threepids: 1_063,
  external_ids: 212,
  account_data: 5_402,
  e2e_keys: 2_977,
  cross_signing: 1_088,
  key_backups: 612,
  to_device: 486,
  push_rules: 1_236,
  pushers: 1_301,
  filters: 4_455,
  registration_tokens: 7,
  rooms: 318,
  receipts: 22_964,
  media: 7_730,
  remote_media: 3_418,
};
const STREAM_COUNT = Object.keys(TOTALS).length;

interface State {
  status: NonNullable<MigrationStatus["status"]>;
  source: string | null;
  startedAt: string | null;
  startedBy: string | null;
  /** Fraction copied when the clock last stopped, and when it started running again. */
  copied: number;
  runningSince: number | null;
  stepEndsAt: number | null;
  stepKind: "verify" | "cutover" | null;
  stepTask: string | null;
  resumeTo: NonNullable<MigrationStatus["status"]> | null;
  verification: MigrationStatus["verification"];
  completedAt: string | null;
  cutoverBy: string | null;
  errors: string[];
  log: MigrationLogEntry[];
  updatedAt: string;
}

function fresh(): State {
  return {
    status: "idle",
    source: null,
    startedAt: null,
    startedBy: null,
    copied: 0,
    runningSince: null,
    stepEndsAt: null,
    stepKind: null,
    stepTask: null,
    resumeTo: null,
    verification: null,
    completedAt: null,
    cutoverBy: null,
    errors: [],
    log: [],
    updatedAt: new Date().toISOString(),
  };
}

let state = fresh();

/** Back to no migration and no source (between tests). */
export function resetMigration(): void {
  state = fresh();
  configValues.migration = { synapse: null };
}

function log(stream: string, message: string, level: MigrationLogEntry["level"] = "info") {
  state.log.push({ recorded_at: new Date().toISOString(), stream, message, level });
}

/** Moves the clock: the copy and any step finish when their time is up. */
function advance(now = Date.now()) {
  if (state.status === "copying" && state.runningSince != null) {
    const fraction = state.copied + (now - state.runningSince) / COPY_MS;
    if (fraction >= 1) {
      state.copied = 1;
      state.runningSince = null;
      state.status = "ready_for_cutover";
      for (const name of Object.keys(TOTALS)) {
        log(
          name,
          `done: ${TOTALS[name].toLocaleString()} copied, 0 not copied on purpose, 0 failed`,
        );
      }
      log("migration", "everything has been copied once: verify, then stop Synapse and cut over");
    }
  }
  if (state.stepEndsAt != null && now >= state.stepEndsAt) {
    const kind = state.stepKind;
    state.stepEndsAt = null;
    state.stepKind = null;
    state.verification = {
      passed: true,
      checked_at: new Date(now).toISOString(),
      streams: Object.keys(TOTALS).map((name) => ({
        name,
        source_count: TOTALS[name],
        target_count: TOTALS[name],
        skipped_count: name === "rooms" ? 4 : name === "receipts" ? 12 : 0,
        sampled: 25,
        mismatches: [],
      })),
    };
    if (kind === "cutover") {
      state.status = "completed";
      state.completedAt = new Date(now).toISOString();
      state.cutoverBy = OPERATOR;
      log("migration", `cut over by ${OPERATOR}: verification passed`);
    } else {
      state.status = state.resumeTo ?? "ready_for_cutover";
      log("migration", "verification passed");
    }
    if (state.stepTask) {
      putTask({
        id: state.stepTask,
        action: kind === "cutover" ? "migration.cutover" : "migration.verify",
        status: "succeeded",
        resource: { type: "migration", id: "synapse" },
        created_at: new Date(now - STEP_MS).toISOString(),
        started_at: new Date(now - STEP_MS).toISOString(),
        finished_at: new Date(now).toISOString(),
        scheduled_for: null,
        progress: null,
        error: null,
        result: null,
      });
    }
    state.stepTask = null;
  }
}

/** Finishes whatever is running, at once (for tests). */
export function settleMigration(): void {
  advance(Date.now() + 60 * 60_000);
}

function fraction(now = Date.now()): number {
  if (state.runningSince == null) return state.copied;
  return Math.min(1, state.copied + (now - state.runningSince) / COPY_MS);
}

export function migrationStatus(): MigrationStatus {
  advance();
  const f = fraction();
  const started = state.status !== "idle";
  return {
    status: state.status,
    source: state.source,
    streams: started
      ? Object.entries(TOTALS).map(([name, total], i) => {
          // Streams go one after another.
          const share = Math.min(1, Math.max(0, f * STREAM_COUNT - i));
          return {
            name,
            copied_count: Math.round(total * share),
            total_count: share > 0 || f > 0 ? total : null,
            rate_per_second: state.status === "copying" && share > 0 && share < 1 ? 420 : 0,
            // Rooms only invited to, and receipts of kinds this server does not keep, are left
            // out on purpose.
            skipped_count:
              name === "rooms"
                ? Math.round(4 * share)
                : name === "receipts"
                  ? Math.round(12 * share)
                  : 0,
            failed_count: 0,
            done: share >= 1,
          };
        })
      : [],
    estimated_remaining_ms: state.status === "copying" ? Math.round((1 - f) * COPY_MS) : null,
    errors: state.errors,
    task_id: state.stepTask,
    started_at: state.startedAt,
    started_by: state.startedBy,
    updated_at: state.updatedAt,
    completed_at: state.completedAt,
    cutover_by: state.cutoverBy,
    verification: state.verification,
  };
}

export function migrationLog(): MigrationLogEntry[] {
  advance();
  return state.log;
}

export type MigrationOutcome =
  { ok: true; status: MigrationStatus } | { ok: false; status: 400 | 409; detail: string };

function refuse(detail: string, status: 400 | 409 = 409): MigrationOutcome {
  return { ok: false, status, detail };
}

function ok(): MigrationOutcome {
  state.updatedAt = new Date().toISOString();
  return { ok: true, status: migrationStatus() };
}

export function startMigration(source: string | null): MigrationOutcome {
  advance();
  if (!["idle", "failed", "aborted"].includes(state.status)) {
    return refuse(`the migration cannot be started now: it is ${state.status}`);
  }
  if (!source) {
    return refuse(
      "no Synapse database is configured at /migration/synapse: set one first (the Migration page's first step, or config.update of the migration section)",
      400,
    );
  }
  state.status = "copying";
  state.source = source;
  state.startedAt ??= new Date().toISOString();
  state.startedBy = OPERATOR;
  state.runningSince = Date.now();
  state.errors = [];
  log("migration", `${OPERATOR} started copying from ${source}`);
  return ok();
}

export function pauseMigration(): MigrationOutcome | null {
  advance();
  if (state.status === "paused") return null;
  if (state.status !== "copying") {
    return refuse(`only a copy can be paused, and the migration is ${state.status}`);
  }
  state.copied = fraction();
  state.runningSince = null;
  state.status = "paused";
  log("migration", `${OPERATOR} paused the copy`);
  return ok();
}

export function resumeMigration(): MigrationOutcome | null {
  advance();
  if (state.status === "copying") return null;
  if (state.status !== "paused") {
    return refuse(`only a paused copy can be resumed, and the migration is ${state.status}`);
  }
  state.status = "copying";
  state.runningSince = Date.now();
  log("migration", `${OPERATOR} resumed the copy`);
  return ok();
}

export function abortMigration(): MigrationOutcome | null {
  advance();
  if (state.status === "aborted") return null;
  if (state.status === "completed") {
    return refuse("this server has already been cut over to; there is nothing to abort");
  }
  if (state.status === "idle") return refuse("no migration has been started");
  state.copied = fraction();
  state.runningSince = null;
  state.stepEndsAt = null;
  state.status = "aborted";
  log(
    "migration",
    `${OPERATOR} aborted the migration. Synapse was only ever read, so it is as it was; what was copied here stays`,
  );
  return ok();
}

function step(kind: "verify" | "cutover"): Task {
  const now = new Date();
  const task = putTask({
    id: `01MIGRATION${kind.toUpperCase()}${now.getTime()}`,
    action: `migration.${kind}`,
    status: "running",
    resource: { type: "migration", id: "synapse" },
    created_at: now.toISOString(),
    started_at: now.toISOString(),
    finished_at: null,
    scheduled_for: null,
    progress: null,
    error: null,
    result: null,
  });
  state.stepTask = task.id;
  state.stepKind = kind;
  state.stepEndsAt = now.getTime() + STEP_MS;
  return task;
}

export function verifyMigration(): { ok: true; task: Task } | { ok: false; detail: string } {
  advance();
  if (!["ready_for_cutover", "paused", "failed", "completed"].includes(state.status)) {
    return {
      ok: false,
      detail:
        state.status === "idle" || state.status === "aborted"
          ? "there is no copy to verify: start the migration first"
          : `the migration is ${state.status}: verify it once that has finished`,
    };
  }
  state.resumeTo = state.status;
  state.status = "verifying";
  log("migration", `${OPERATOR} started a verification`);
  return { ok: true, task: step("verify") };
}

export function cutoverMigration(): { ok: true; task: Task } | { ok: false; detail: string } {
  advance();
  if (state.status !== "ready_for_cutover") {
    return {
      ok: false,
      detail: `the migration can only be cut over once everything has been copied, and it is ${state.status}`,
    };
  }
  state.status = "cutting_over";
  log("migration", `${OPERATOR} started the cutover`);
  return { ok: true, task: step("cutover") };
}
