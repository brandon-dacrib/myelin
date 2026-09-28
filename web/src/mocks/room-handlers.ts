/**
 * The mock's handlers for the room page's long tail: the seventeen room operations beyond
 * list, get, block, unblock, make-admin and members, over `./data/room-contents`. Spread into
 * `./handlers.ts`'s list.
 */
import { http, HttpResponse } from "msw";
import {
  addRoomAlias,
  deleteRoom,
  eventAt,
  eventContext,
  findRoomEvent,
  forwardExtremities,
  joinRoom,
  pruneExtremities,
  purgeHistory,
  quarantineRoomMedia,
  removeRoomAlias,
  roomAliases,
  roomHierarchy,
  roomMedia,
  roomState,
  roomTimeline,
} from "./data/room-contents";

const API = "/api/v1";

function notFound(what: string) {
  return HttpResponse.json(
    { type: "urn:hs:problem:not-found", title: `${what} not found`, status: 404 },
    { status: 404 },
  );
}

function invalid(pointer: string, detail: string) {
  return HttpResponse.json(
    {
      type: "urn:hs:problem:validation-failed",
      title: "Validation failed",
      status: 400,
      detail,
      errors: [{ pointer, detail }],
    },
    { status: 400 },
  );
}

/** Offset cursors, like the rest of the mock's lists. */
function page<T>(items: T[], url: URL) {
  const limit = Math.min(Number(url.searchParams.get("limit") ?? 50), 1000);
  const offset = Number(url.searchParams.get("cursor") ?? 0) || 0;
  const next = offset + limit;
  return {
    items: items.slice(offset, next),
    next_cursor: next < items.length ? String(next) : null,
    prev_cursor: offset > 0 ? String(Math.max(offset - limit, 0)) : null,
  };
}

function roomId(params: Record<string, unknown>): string {
  return decodeURIComponent(String(params.room_id));
}

function accepted(task: { id: string }) {
  return HttpResponse.json(task, {
    status: 202,
    headers: { Location: `/api/v1/tasks/${task.id}` },
  });
}

export function roomContentHandlers() {
  return [
    http.get(`${API}/rooms/:room_id/state`, ({ params, request }) => {
      const url = new URL(request.url);
      const state = roomState(roomId(params), url.searchParams.get("type"));
      return state ? HttpResponse.json(page(state, url)) : notFound("Room");
    }),

    http.get(`${API}/rooms/:room_id/messages`, ({ params, request }) => {
      const url = new URL(request.url);
      const timeline = roomTimeline(roomId(params), url.searchParams.get("dir") ?? "b");
      return timeline ? HttpResponse.json(page(timeline, url)) : notFound("Room");
    }),

    // Before `events/:event_id`, which would otherwise take "at" as an event id.
    http.get(`${API}/rooms/:room_id/events/at`, ({ params, request }) => {
      const url = new URL(request.url);
      const ts = Number(url.searchParams.get("ts"));
      if (!Number.isFinite(ts)) return invalid("/ts", "ts must be a number of milliseconds");
      const found = eventAt(roomId(params), ts, url.searchParams.get("dir") ?? "f");
      return found ? HttpResponse.json(found) : notFound("Event");
    }),

    http.get(`${API}/rooms/:room_id/events/:event_id/context`, ({ params, request }) => {
      const url = new URL(request.url);
      const context = eventContext(
        roomId(params),
        decodeURIComponent(String(params.event_id)),
        Number(url.searchParams.get("limit") ?? 10),
      );
      return context ? HttpResponse.json(context) : notFound("Event");
    }),

    http.get(`${API}/rooms/:room_id/events/:event_id`, ({ params }) => {
      const found = findRoomEvent(decodeURIComponent(String(params.event_id)), roomId(params));
      return found ? HttpResponse.json(found) : notFound("Event");
    }),

    http.get(`${API}/events/:event_id`, ({ params }) => {
      const found = findRoomEvent(decodeURIComponent(String(params.event_id)));
      return found ? HttpResponse.json(found) : notFound("Event");
    }),

    http.get(`${API}/rooms/:room_id/aliases`, ({ params }) => {
      const aliases = roomAliases(roomId(params));
      return aliases ? HttpResponse.json(aliases) : notFound("Room");
    }),

    http.post(`${API}/rooms/:room_id/aliases`, async ({ params, request }) => {
      const body = (await request.json()) as { alias?: string };
      const alias = body.alias ?? "";
      if (!/^#[^:]+:example\.org$/.test(alias)) {
        return invalid("/alias", "an alias looks like #name:example.org, on this server");
      }
      const added = addRoomAlias(roomId(params), alias);
      if (added === undefined) return notFound("Room");
      if (added === null) {
        return HttpResponse.json(
          {
            type: "urn:hs:problem:conflict",
            title: "Conflict",
            status: 409,
            detail: `${alias} is already in use`,
          },
          { status: 409 },
        );
      }
      return HttpResponse.json(added, { status: 201 });
    }),

    http.delete(`${API}/rooms/:room_id/aliases/:alias`, ({ params }) =>
      removeRoomAlias(roomId(params), decodeURIComponent(String(params.alias)))
        ? new HttpResponse(null, { status: 204 })
        : notFound("Alias"),
    ),

    http.get(`${API}/rooms/:room_id/hierarchy`, ({ params, request }) => {
      const nodes = roomHierarchy(roomId(params));
      return nodes ? HttpResponse.json(page(nodes, new URL(request.url))) : notFound("Room");
    }),

    http.post(`${API}/rooms/:room_id/join`, async ({ params, request }) => {
      const body = (await request.json()) as { user_id?: string };
      const userId = body.user_id ?? "";
      if (!/^@[^:]+:example\.org$/.test(userId)) {
        return invalid("/user_id", "only a user on this server can be joined to a room");
      }
      const member = joinRoom(roomId(params), userId);
      return member ? HttpResponse.json(member) : notFound("Room");
    }),

    http.get(`${API}/rooms/:room_id/forward-extremities`, ({ params }) => {
      const found = forwardExtremities(roomId(params));
      return found ? HttpResponse.json(found) : notFound("Room");
    }),

    http.delete(`${API}/rooms/:room_id/forward-extremities`, ({ params }) => {
      const pruned = pruneExtremities(roomId(params));
      return pruned ? HttpResponse.json(pruned) : notFound("Room");
    }),

    http.get(`${API}/rooms/:room_id/media`, ({ params, request }) => {
      const media = roomMedia(roomId(params));
      return media ? HttpResponse.json(page(media, new URL(request.url))) : notFound("Room");
    }),

    http.post(`${API}/rooms/:room_id/media/quarantine`, ({ params }) => {
      const task = quarantineRoomMedia(roomId(params));
      return task ? accepted(task) : notFound("Room");
    }),

    http.post(`${API}/rooms/:room_id/purge-history`, async ({ params, request }) => {
      const text = await request.text();
      const body = (text ? JSON.parse(text) : {}) as {
        before?: string;
        delete_local_events?: boolean;
      };
      if (!body.before || Number.isNaN(Date.parse(body.before))) {
        return invalid("/before", "before is required: say how far back to purge");
      }
      const task = purgeHistory(roomId(params), body.before, Boolean(body.delete_local_events));
      return task ? accepted(task) : notFound("Room");
    }),

    http.post(`${API}/rooms/:room_id/delete`, async ({ params, request }) => {
      const text = await request.text();
      const body = (text ? JSON.parse(text) : {}) as {
        block?: boolean;
        purge?: boolean;
        new_room?: { creator: string; name?: string };
      };
      if (body.new_room && !/^@[^:]+:example\.org$/.test(body.new_room.creator ?? "")) {
        return invalid("/new_room/creator", "the new room's creator must be a user on this server");
      }
      const task = deleteRoom(roomId(params), body);
      return task ? accepted(task) : notFound("Room");
    }),
  ];
}
