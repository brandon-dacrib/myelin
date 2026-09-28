/**
 * The mock's event stream (`GET /api/v1/events`), shaped like `crates/hs-admin/src/events.rs`:
 * every frame is `id:`, `event:` and a JSON `data:` repeating the id and type. The mock's tasks
 * and reports publish here as the server's do (`task.changed` on every change of a task,
 * `report.resolved`/`report.deleted`), and {@link publishMockEvent} lets a test publish anything
 * else (a `report.created`, which the mock itself never files).
 */

export interface MockEvent {
  id: string;
  type: string;
  recorded_at: string;
  resource?: { type: string; id: string };
  data: unknown;
}

let counter = 0;
const subscribers = new Set<(event: MockEvent) => void>();
const tickers = new Set<() => void>();
/** How to close each open stream's subscription and ticker. */
const closers = new Set<() => void>();

/**
 * Registers work the mock does once a second while a stream is open: the mock's clock-driven
 * task publishes its progress this way, as the server's running tasks do with each report.
 */
export function registerMockTicker(tick: () => void): void {
  tickers.add(tick);
}

const TICK_MS = 1_000;

/** Publishes an event to every open mock stream. */
export function publishMockEvent(
  type: string,
  data: unknown,
  resource?: { type: string; id: string },
): MockEvent {
  counter += 1;
  const event: MockEvent = {
    id: `01MOCKEVENT${String(counter).padStart(15, "0")}`,
    type,
    recorded_at: new Date().toISOString(),
    resource,
    data,
  };
  for (const fn of subscribers) fn(event);
  return event;
}

/** How many streams are open (tests wait for the interface to connect). */
export function openMockStreams(): number {
  return subscribers.size;
}

function frame(event: MockEvent): string {
  return `id: ${event.id}\nevent: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`;
}

/** Whether `type` passes a `types` filter (exact, or a `prefix.*` glob). */
function matches(filters: string[], type: string): boolean {
  if (filters.length === 0) return true;
  return filters.some((f) => (f.endsWith(".*") ? type.startsWith(f.slice(0, -1)) : f === type));
}

/** The body of one `GET /events` answer: a hello, then every event published while it is open. */
export function mockEventStream(types: string[], signal?: AbortSignal): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  let unsubscribe = () => {};
  return new ReadableStream<Uint8Array>({
    start(controller) {
      const send = (event: MockEvent) => {
        if (!matches(types, event.type)) return;
        try {
          controller.enqueue(encoder.encode(frame(event)));
        } catch {
          unsubscribe();
        }
      };
      subscribers.add(send);
      const timer = setInterval(() => {
        for (const tick of tickers) tick();
      }, TICK_MS);
      unsubscribe = () => {
        clearInterval(timer);
        subscribers.delete(send);
        closers.delete(unsubscribe);
      };
      closers.add(unsubscribe);
      signal?.addEventListener("abort", () => {
        unsubscribe();
        try {
          controller.close();
        } catch {
          /* already closed */
        }
      });
      counter += 1;
      controller.enqueue(
        encoder.encode(
          frame({
            id: `01MOCKEVENT${String(counter).padStart(15, "0")}`,
            type: "stream.hello",
            recorded_at: new Date().toISOString(),
            data: { server: "example.org", replica: "single" },
          }),
        ),
      );
    },
    cancel() {
      unsubscribe();
    },
  });
}

/** Closes every mock stream's subscription (Vitest runs this after every test). */
export function resetMockEvents(): void {
  for (const close of [...closers]) close();
  subscribers.clear();
}
