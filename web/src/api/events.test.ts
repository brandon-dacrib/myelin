import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { waitFor } from "@testing-library/react";
import { QueryClient } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { openMockStreams, publishMockEvent } from "@/mocks/data/events";
import { signIn, signOut } from "@/lib/auth";
import { isLive, parseSseFrames, startLiveEvents, type AdminEvent } from "./events";

describe("parseSseFrames", () => {
  it("keeps an incomplete frame for the next chunk and drops keepalives", () => {
    const first = parseSseFrames(
      ': keepalive\n\nid: 1\nevent: task.changed\ndata: {"id":"1"}\n\nid: 2\nda',
    );
    expect(first.frames).toEqual([{ id: "1", event: "task.changed", data: '{"id":"1"}' }]);
    const second = parseSseFrames(`${first.rest}ta: {"id":"2"}\r\n\r\n`);
    expect(second.frames).toEqual([{ id: "2", data: '{"id":"2"}' }]);
    expect(second.rest).toBe("");
  });

  it("joins multi-line data", () => {
    const { frames } = parseSseFrames("data: a\ndata: b\n\n");
    expect(frames[0].data).toBe("a\nb");
  });
});

describe("startLiveEvents", () => {
  let stop: (() => void) | undefined;
  beforeEach(async () => {
    await signIn();
  });
  afterEach(() => {
    stop?.();
    stop = undefined;
    signOut();
  });

  it("connects, puts a changed task in the cache, and refetches reports on report.created", async () => {
    const qc = new QueryClient();
    qc.setQueryData(["reports", {}], { items: [] });
    // The sidebar's open-report count reads the Overview's counts.
    qc.setQueryData(["statistics-overview"], { pending_reports_count: 0 });
    const seen: AdminEvent[] = [];
    stop = startLiveEvents(qc, { onEvent: (e) => seen.push(e) });
    await waitFor(() => expect(isLive()).toBe(true));
    expect(openMockStreams()).toBe(1);

    const task = { id: "T1", action: "media.delete", status: "running", created_at: "x" };
    publishMockEvent("task.changed", task, { type: "task", id: "T1" });
    await waitFor(() => expect(qc.getQueryData(["task", "T1"])).toEqual(task));

    publishMockEvent("report.created", { id: "R1" }, { type: "report", id: "R1" });
    await waitFor(() => expect(qc.getQueryState(["reports", {}])?.isInvalidated).toBe(true));
    expect(qc.getQueryState(["statistics-overview"])?.isInvalidated).toBe(true);
    expect(seen.map((e) => e.type)).toEqual(["task.changed", "report.created"]);

    // Types it did not ask for never arrive.
    publishMockEvent("user.locked", {}, { type: "user", id: "@a:example.org" });
    publishMockEvent("task.cancelled", {}, { type: "task", id: "T1" });
    await waitFor(() => expect(seen).toHaveLength(3));
    expect(seen[2].type).toBe("task.cancelled");

    stop();
    stop = undefined;
    expect(isLive()).toBe(false);
  });

  it("gives up on a server without the stream and stays on polling", async () => {
    let asked = 0;
    server.use(
      http.get("/api/v1/events", () => {
        asked += 1;
        return HttpResponse.json(
          { type: "urn:hs:problem:not-implemented", title: "Not implemented", status: 501 },
          { status: 501 },
        );
      }),
    );
    stop = startLiveEvents(new QueryClient(), { initialBackoffMs: 5 });
    await waitFor(() => expect(asked).toBe(1));
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(asked).toBe(1);
    expect(isLive()).toBe(false);
  });

  it("reconnects after a dropped stream, resuming from the last event it saw", async () => {
    const lastIds: (string | null)[] = [];
    let calls = 0;
    server.use(
      http.get("/api/v1/events", ({ request }) => {
        calls += 1;
        lastIds.push(request.headers.get("last-event-id"));
        const body =
          calls === 1
            ? 'id: H\nevent: stream.hello\ndata: {"id":"H","type":"stream.hello"}\n\n' +
              'id: E1\nevent: task.changed\ndata: {"id":"E1","type":"task.cancelled"}\n\n'
            : 'id: H2\nevent: stream.hello\ndata: {"id":"H2","type":"stream.hello"}\n\n';
        return new HttpResponse(body, { headers: { "Content-Type": "text/event-stream" } });
      }),
    );
    stop = startLiveEvents(new QueryClient(), { initialBackoffMs: 5 });
    await waitFor(() => expect(calls).toBeGreaterThanOrEqual(2));
    expect(lastIds[0]).toBeNull();
    expect(lastIds[1]).toBe("E1");
  });
});
