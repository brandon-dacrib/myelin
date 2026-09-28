/**
 * The admin event stream (`GET /api/v1/events`, RFC 0004 section 10): what makes the Reports and
 * Tasks pages, and the sidebar's open-report count, change the moment something happens instead
 * of at their next poll.
 *
 * `EventSource` cannot send an `Authorization` header, so the stream is read with `fetch` and a
 * small incremental parser for the `text/event-stream` framing. {@link startLiveEvents} keeps one
 * connection open for the signed-in session, reconnects with backoff (resuming with
 * `Last-Event-ID`, so nothing published while it was away is missed), and turns each event into
 * the query-cache change it stands for:
 *
 * - `report.*` (`report.created`, `report.resolved`, `report.deleted`): the report lists, that
 *   report, and the Overview's counts (the sidebar's open-report count reads them) are refetched.
 * - `task.changed`: the task's own query is set to the task the event carries (progress included)
 *   and the task lists are refetched; other `task.*` events refetch that task.
 * - `media.*`: the media listing is refetched.
 * - `stream.reset`: the server could not replay what was missed, so everything above is refetched.
 *
 * Polling stays as the fallback. {@link useLiveEvents} says whether the stream is connected; the
 * query hooks in `./reports`, `./tasks` and `./dashboard` poll only while it is not.
 */
import { useSyncExternalStore } from "react";
import type { QueryClient } from "@tanstack/react-query";
import { apiBaseUrl } from "./client";
import type { components } from "./schema";
import { getAccessToken } from "@/lib/auth";

export type Task = components["schemas"]["Task"];

/** One event as the stream carries it (the frame's `data`, which repeats its id and type). */
export interface AdminEvent {
  id: string;
  type: string;
  recorded_at?: string;
  resource?: { type: string; id: string } | null;
  data?: unknown;
}

/** One `text/event-stream` frame. */
export interface SseFrame {
  id?: string;
  event?: string;
  data: string;
}

/**
 * Splits what has arrived so far into complete frames and the incomplete tail, which the next
 * chunk continues. Comment lines (`: keepalive`) are dropped; a frame with no `data` line is too.
 */
export function parseSseFrames(buffer: string): { frames: SseFrame[]; rest: string } {
  const text = buffer.replace(/\r\n?/g, "\n");
  const parts = text.split("\n\n");
  const rest = parts.pop() ?? "";
  const frames: SseFrame[] = [];
  for (const part of parts) {
    const frame: SseFrame = { data: "" };
    const data: string[] = [];
    for (const line of part.split("\n")) {
      if (line === "" || line.startsWith(":")) continue;
      const colon = line.indexOf(":");
      const field = colon === -1 ? line : line.slice(0, colon);
      let value = colon === -1 ? "" : line.slice(colon + 1);
      if (value.startsWith(" ")) value = value.slice(1);
      if (field === "data") data.push(value);
      else if (field === "id") frame.id = value;
      else if (field === "event") frame.event = value;
    }
    if (data.length === 0) continue;
    frame.data = data.join("\n");
    frames.push(frame);
  }
  return { frames, rest };
}

/** Decodes a frame's `data` into an event, or `undefined` for a body that is not one. */
export function frameToEvent(frame: SseFrame): AdminEvent | undefined {
  try {
    const parsed = JSON.parse(frame.data) as Partial<AdminEvent>;
    const type = parsed.type ?? frame.event;
    const id = parsed.id ?? frame.id;
    if (typeof type !== "string" || typeof id !== "string") return undefined;
    return { ...parsed, id, type };
  } catch {
    return undefined;
  }
}

function isTask(value: unknown): value is Task {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as Task).id === "string" &&
    typeof (value as Task).status === "string" &&
    typeof (value as Task).action === "string"
  );
}

/** Applies one event to the query cache (see the module docs for what each type does). */
export function applyAdminEvent(qc: QueryClient, event: AdminEvent): void {
  const resourceId = event.resource?.id;
  if (event.type.startsWith("report.")) {
    void qc.invalidateQueries({ queryKey: ["reports"] });
    void qc.invalidateQueries({ queryKey: ["statistics-overview"] });
    if (resourceId) void qc.invalidateQueries({ queryKey: ["report", resourceId] });
  } else if (event.type === "task.changed" && isTask(event.data)) {
    qc.setQueryData(["task", event.data.id], event.data);
    void qc.invalidateQueries({ queryKey: ["tasks"] });
  } else if (event.type.startsWith("task.")) {
    if (resourceId) void qc.invalidateQueries({ queryKey: ["task", resourceId] });
    void qc.invalidateQueries({ queryKey: ["tasks"] });
  } else if (event.type.startsWith("media.")) {
    void qc.invalidateQueries({ queryKey: ["media"] });
  } else if (event.type === "stream.reset") {
    for (const key of ["reports", "report", "statistics-overview", "tasks", "task", "media"]) {
      void qc.invalidateQueries({ queryKey: [key] });
    }
  }
}

/** The event types this interface listens for (`types` globs on `GET /events`). */
export const LISTENED_TYPES = ["report.*", "task.*", "media.*"] as const;

let live = false;
const liveListeners = new Set<() => void>();

function setLive(value: boolean) {
  if (live === value) return;
  live = value;
  for (const fn of liveListeners) fn();
}

function subscribeLive(fn: () => void): () => void {
  liveListeners.add(fn);
  return () => liveListeners.delete(fn);
}

/** Whether the event stream is connected right now. */
export function isLive(): boolean {
  return live;
}

/** Whether the event stream is connected: the query hooks poll only while it is not. */
export function useLiveEvents(): boolean {
  return useSyncExternalStore(subscribeLive, isLive, isLive);
}

/** Statuses after which reconnecting would get the same answer: stop, and keep polling. */
const PERMANENT_FAILURES = new Set([401, 403, 404, 405, 501]);

export interface LiveEventsOptions {
  /** First reconnect delay; doubles up to {@link LiveEventsOptions.maxBackoffMs}. */
  initialBackoffMs?: number;
  maxBackoffMs?: number;
  /** Called with each event after it is applied (tests). */
  onEvent?: (event: AdminEvent) => void;
}

/**
 * Opens the event stream for the signed-in session and keeps it open until the returned
 * function is called. Safe to call when the server has no stream (it stops after a permanent
 * refusal, and the pages keep polling).
 */
export function startLiveEvents(qc: QueryClient, options: LiveEventsOptions = {}): () => void {
  const initial = options.initialBackoffMs ?? 1_000;
  const max = options.maxBackoffMs ?? 30_000;
  const abort = new AbortController();
  let lastEventId: string | undefined;
  let stopped = false;
  let backoff = initial;

  const wait = (ms: number) =>
    new Promise<void>((resolve) => {
      const timer = setTimeout(resolve, ms);
      abort.signal.addEventListener("abort", () => {
        clearTimeout(timer);
        resolve();
      });
    });

  async function connectOnce(): Promise<"retry" | "stop"> {
    const token = getAccessToken();
    if (!token) return "stop";
    const url = new URL(`${apiBaseUrl()}/events`, window.location.origin);
    for (const type of LISTENED_TYPES) url.searchParams.append("types", type);
    const headers: Record<string, string> = {
      Authorization: `Bearer ${token}`,
      Accept: "text/event-stream",
    };
    if (lastEventId) headers["Last-Event-ID"] = lastEventId;
    const response = await fetch(url, { headers, signal: abort.signal });
    if (!response.ok || !response.body) {
      return PERMANENT_FAILURES.has(response.status) ? "stop" : "retry";
    }
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const { frames, rest } = parseSseFrames(buffer);
      buffer = rest;
      for (const frame of frames) {
        const event = frameToEvent(frame);
        if (!event) continue;
        if (event.type === "stream.hello") {
          setLive(true);
          backoff = initial;
          continue;
        }
        lastEventId = event.id;
        applyAdminEvent(qc, event);
        options.onEvent?.(event);
      }
    }
    return "retry";
  }

  void (async () => {
    while (!stopped) {
      let outcome: "retry" | "stop" = "retry";
      try {
        outcome = await connectOnce();
      } catch {
        outcome = "retry";
      }
      setLive(false);
      if (stopped || outcome === "stop") break;
      // Whatever changed while the stream was down is fetched once, not waited for.
      for (const key of ["reports", "statistics-overview", "tasks"]) {
        void qc.invalidateQueries({ queryKey: [key] });
      }
      await wait(backoff);
      backoff = Math.min(backoff * 2, max);
    }
  })();

  return () => {
    stopped = true;
    abort.abort();
    setLive(false);
  };
}
