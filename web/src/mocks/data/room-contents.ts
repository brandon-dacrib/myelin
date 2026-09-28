/**
 * What the mock holds inside its rooms, for the room page's long tail: state, a timeline of
 * messages, aliases, a space's children, the media a room references, forward extremities, and
 * the tasks purge, delete and quarantine start (they run for a moment, then succeed, as the
 * server's do). Mutable, so `src/test/setup.ts` calls {@link resetRoomContents} after each test.
 */
import type { components } from "@/api/schema";
import { rooms, roomMembers } from "./rooms";
import { findMedia } from "./media";
import { putTask } from "./tasks";

type RoomEvent = components["schemas"]["RoomEvent"];
type StateEvent = components["schemas"]["StateEvent"];
type RoomAlias = components["schemas"]["RoomAlias"];
type ForwardExtremity = components["schemas"]["ForwardExtremity"];
type RoomHierarchyNode = components["schemas"]["RoomHierarchyNode"];
type Room = components["schemas"]["Room"];
type Task = components["schemas"]["Task"];

const HOUR = 60 * 60 * 1000;
/** How long a mock room task runs before it succeeds. */
export const ROOM_TASK_MS = 800;

interface Contents {
  state: StateEvent[];
  timeline: RoomEvent[];
  aliases: RoomAlias[];
  extremities: ForwardExtremity[];
  media: { server_name: string; media_id: string }[];
  children: string[];
}

function stateEvent(
  roomId: string,
  n: number,
  type: string,
  stateKey: string,
  sender: string,
  content: Record<string, unknown>,
  ts: number,
): StateEvent {
  return {
    event_id: `$state-${roomId.slice(1, 6)}-${n}`,
    type,
    state_key: stateKey,
    sender,
    content,
    origin_server_ts: ts,
  };
}

function seedRoom(room: Room, base: number): Contents {
  const roomId = room.room_id;
  const creator = room.creator ?? "@admin:example.org";
  const created = base - 40 * HOUR;
  const state: StateEvent[] = [
    stateEvent(
      roomId,
      1,
      "m.room.create",
      "",
      creator,
      { room_version: room.version, creator },
      created,
    ),
    stateEvent(
      roomId,
      2,
      "m.room.power_levels",
      "",
      creator,
      { users: { [creator]: 100 } },
      created + 1,
    ),
    stateEvent(
      roomId,
      3,
      "m.room.join_rules",
      "",
      creator,
      { join_rule: room.join_rule ?? "invite" },
      created + 2,
    ),
    stateEvent(
      roomId,
      4,
      "m.room.history_visibility",
      "",
      creator,
      { history_visibility: room.history_visibility ?? "shared" },
      created + 3,
    ),
  ];
  if (room.name)
    state.push(stateEvent(roomId, 5, "m.room.name", "", creator, { name: room.name }, created + 4));
  if (room.topic)
    state.push(
      stateEvent(roomId, 6, "m.room.topic", "", creator, { topic: room.topic }, created + 5),
    );
  if (room.canonical_alias) {
    state.push(
      stateEvent(
        roomId,
        7,
        "m.room.canonical_alias",
        "",
        creator,
        { alias: room.canonical_alias },
        created + 6,
      ),
    );
  }
  (roomMembers[roomId] ?? []).forEach((m, i) =>
    state.push(
      stateEvent(
        roomId,
        10 + i,
        "m.room.member",
        m.user_id ?? "",
        m.user_id ?? "",
        { membership: m.membership, displayname: m.display_name },
        created + 10 + i,
      ),
    ),
  );
  const senders = (roomMembers[roomId] ?? []).map((m) => m.user_id);
  const timeline: RoomEvent[] = state.map((e) => ({
    ...e,
    room_id: roomId,
    redacted: false,
    redacts: null,
  }));
  for (let i = 0; i < 30; i++) {
    timeline.push({
      event_id: `$msg-${roomId.slice(1, 6)}-${i}`,
      room_id: roomId,
      type: "m.room.message",
      sender: senders[i % Math.max(senders.length, 1)] ?? creator,
      content: { msgtype: "m.text", body: `Message ${i + 1} in ${room.name ?? roomId}` },
      origin_server_ts: created + (i + 1) * HOUR,
      state_key: null,
      redacted: false,
      redacts: null,
    });
  }
  const newest = timeline[timeline.length - 1];
  const extremities: ForwardExtremity[] = [
    {
      event_id: newest.event_id,
      type: newest.type,
      sender: newest.sender,
      depth: 60,
      origin_server_ts: newest.origin_server_ts,
      state_key: null,
    },
  ];
  if (roomId === "!general:example.org") {
    extremities.push({
      event_id: "$fork-remote-1",
      type: "m.room.message",
      sender: "@carol:remote.example",
      depth: 58,
      origin_server_ts: newest.origin_server_ts - 5 * 60_000,
      state_key: null,
    });
  }
  const aliases: RoomAlias[] = room.canonical_alias
    ? [
        {
          alias: room.canonical_alias,
          creator,
          canonical: true,
          created_at: new Date(created).toISOString(),
        },
      ]
    : [];
  if (roomId === "!general:example.org") {
    aliases.push({
      alias: "#announcements:example.org",
      creator: "@admin:example.org",
      canonical: false,
      created_at: null,
    });
  }
  const media =
    roomId === "!general:example.org"
      ? [
          { server_name: "example.org", media_id: "vacationPhotoAbc123" },
          { server_name: "example.org", media_id: "teamLogoGhi789" },
          { server_name: "matrix.org", media_id: "avatarMno345" },
        ]
      : [];
  const children =
    room.room_type === "m.space"
      ? ["!general:example.org", "!spam-central:example.org", "!lobby:remote.example"]
      : [];
  return { state, timeline, aliases, extremities, media, children };
}

let contents = new Map<string, Contents>();
const originalRooms = [...rooms];

/** Puts every room and its contents back as they were. */
export function resetRoomContents(): void {
  rooms.splice(0, rooms.length, ...originalRooms);
  contents = new Map();
}

function contentsOf(roomId: string): Contents | undefined {
  const room = rooms.find((r) => r.room_id === roomId);
  if (!room) return undefined;
  let found = contents.get(roomId);
  if (!found) {
    found = seedRoom(room, Date.now());
    contents.set(roomId, found);
  }
  return found;
}

export function roomState(roomId: string, type: string | null): StateEvent[] | undefined {
  const c = contentsOf(roomId);
  return c && (type ? c.state.filter((e) => e.type === type) : c.state);
}

/** The timeline newest first (`dir=b`) or oldest first (`dir=f`). */
export function roomTimeline(roomId: string, dir: string): RoomEvent[] | undefined {
  const c = contentsOf(roomId);
  if (!c) return undefined;
  const sorted = [...c.timeline].sort((a, b) => a.origin_server_ts - b.origin_server_ts);
  return dir === "f" ? sorted : sorted.reverse();
}

export function findRoomEvent(eventId: string, roomId?: string): RoomEvent | undefined {
  const ids = roomId ? [roomId] : rooms.map((r) => r.room_id);
  for (const id of ids) {
    const found = contentsOf(id)?.timeline.find((e) => e.event_id === eventId);
    if (found) return found;
  }
  return undefined;
}

export function eventAt(roomId: string, ts: number, dir: string): RoomEvent | undefined {
  const timeline = roomTimeline(roomId, "f") ?? [];
  return dir === "b"
    ? [...timeline].reverse().find((e) => e.origin_server_ts <= ts)
    : timeline.find((e) => e.origin_server_ts >= ts);
}

export function eventContext(roomId: string, eventId: string, limit: number) {
  const timeline = roomTimeline(roomId, "f") ?? [];
  const i = timeline.findIndex((e) => e.event_id === eventId);
  if (i < 0) return undefined;
  return {
    event: timeline[i],
    events_before: timeline.slice(Math.max(0, i - limit), i).reverse(),
    events_after: timeline.slice(i + 1, i + 1 + limit),
    state: contentsOf(roomId)?.state ?? [],
  };
}

export function roomAliases(roomId: string): RoomAlias[] | undefined {
  return contentsOf(roomId)?.aliases;
}

/** `null` when the alias is taken; `undefined` when there is no such room. */
export function addRoomAlias(roomId: string, alias: string): RoomAlias | null | undefined {
  const c = contentsOf(roomId);
  if (!c) return undefined;
  for (const other of contents.values()) {
    if (other.aliases.some((a) => a.alias === alias)) return null;
  }
  const added: RoomAlias = {
    alias,
    creator: "@ops:example.org",
    canonical: false,
    created_at: new Date().toISOString(),
  };
  c.aliases.push(added);
  return added;
}

export function removeRoomAlias(roomId: string, alias: string): boolean {
  const c = contentsOf(roomId);
  if (!c) return false;
  const before = c.aliases.length;
  c.aliases = c.aliases.filter((a) => a.alias !== alias);
  return c.aliases.length < before;
}

export function roomHierarchy(roomId: string): RoomHierarchyNode[] | undefined {
  const c = contentsOf(roomId);
  const room = rooms.find((r) => r.room_id === roomId);
  if (!c || !room) return undefined;
  const node = (r: Room, depth: number): RoomHierarchyNode => ({
    room_id: r.room_id,
    name: r.name ?? null,
    topic: r.topic ?? null,
    canonical_alias: r.canonical_alias ?? null,
    room_type: r.room_type ?? null,
    join_rule: r.join_rule ?? null,
    joined_members_count: r.joined_members_count ?? null,
    depth,
    known: true,
    children: depth === 0 ? c.children : [],
  });
  return [
    node(room, 0),
    ...c.children.map((id) => {
      const child = rooms.find((r) => r.room_id === id);
      return child
        ? node(child, 1)
        : {
            room_id: id,
            name: null,
            topic: null,
            canonical_alias: null,
            room_type: null,
            join_rule: null,
            joined_members_count: null,
            depth: 1,
            known: false,
            children: [],
          };
    }),
  ];
}

export function forwardExtremities(roomId: string): ForwardExtremity[] | undefined {
  return contentsOf(roomId)?.extremities;
}

export function pruneExtremities(roomId: string) {
  const c = contentsOf(roomId);
  if (!c) return undefined;
  const sorted = [...c.extremities].sort((a, b) => b.depth - a.depth);
  c.extremities = sorted.slice(0, 1);
  return { deleted: sorted.slice(1).map((e) => e.event_id), remaining: c.extremities };
}

export function roomMedia(roomId: string) {
  const c = contentsOf(roomId);
  return (
    c && c.media.map((m) => findMedia(m.server_name, m.media_id)).filter((m) => m !== undefined)
  );
}

/** `true` when the user was already joined. `undefined` for no such room. */
export function joinRoom(roomId: string, userId: string) {
  const c = contentsOf(roomId);
  if (!c) return undefined;
  const members = (roomMembers[roomId] ??= []);
  const existing = members.find((m) => m.user_id === userId);
  if (existing) existing.membership = "join";
  else members.push({ user_id: userId, membership: "join", display_name: null, avatar_url: null });
  return { user_id: userId, membership: "join" as const, display_name: null, avatar_url: null };
}

/** Starts a task that runs for {@link ROOM_TASK_MS} and then succeeds with `finish()`'s result. */
function startTask(
  action: string,
  roomId: string,
  total: number,
  unit: string,
  finish: () => unknown,
): Task {
  const now = new Date().toISOString();
  const id = `task_room_${Math.random().toString(36).slice(2, 10)}`;
  const base = {
    id,
    action,
    resource: { type: "room", id: roomId },
    created_at: now,
    started_at: now,
    scheduled_for: null,
    error: null,
  };
  const running = putTask({
    ...base,
    status: "running",
    finished_at: null,
    progress: { current: 0, total, unit, message: "Starting" },
    result: null,
  });
  setTimeout(() => {
    const result = finish();
    putTask({
      ...base,
      status: "succeeded",
      finished_at: new Date().toISOString(),
      progress: { current: total, total, unit },
      result,
    });
  }, ROOM_TASK_MS);
  return running;
}

export function purgeHistory(
  roomId: string,
  before: string,
  deleteLocal: boolean,
): Task | undefined {
  const c = contentsOf(roomId);
  if (!c) return undefined;
  const cutoff = Date.parse(before);
  const doomed = c.timeline.filter(
    (e) =>
      e.state_key === null &&
      e.origin_server_ts < cutoff &&
      (deleteLocal || !e.sender.endsWith(":example.org")),
  );
  const keptLocal = c.timeline.filter(
    (e) =>
      e.state_key === null &&
      e.origin_server_ts < cutoff &&
      !deleteLocal &&
      e.sender.endsWith(":example.org"),
  ).length;
  return startTask("rooms.purge_history", roomId, doomed.length, "events", () => {
    const ids = new Set(doomed.map((e) => e.event_id));
    c.timeline = c.timeline.filter((e) => !ids.has(e.event_id));
    return { purged: ids.size, kept_state: c.state.length, kept_local: keptLocal };
  });
}

export function deleteRoom(
  roomId: string,
  body: { block?: boolean; purge?: boolean; new_room?: { creator: string; name?: string } },
): Task | undefined {
  const c = contentsOf(roomId);
  const room = rooms.find((r) => r.room_id === roomId);
  if (!c || !room) return undefined;
  const members = (roomMembers[roomId] ?? [])
    .filter((m) => m.membership === "join")
    .map((m) => m.user_id);
  const purge = body.purge ?? true;
  return startTask("rooms.delete", roomId, members.length + 2, "steps", () => {
    const events = c.timeline.length;
    if (body.block) room.blocked = true;
    if (purge) {
      const i = rooms.findIndex((r) => r.room_id === roomId);
      if (i >= 0) rooms.splice(i, 1);
      contents.delete(roomId);
    }
    return {
      kicked_users: members,
      failed_to_kick_users: [],
      local_aliases: c.aliases.map((a) => a.alias),
      new_room_id: body.new_room ? "!new-room:example.org" : null,
      blocked: Boolean(body.block),
      purged: purge,
      events_deleted: purge ? events : 0,
    };
  });
}

export function quarantineRoomMedia(roomId: string): Task | undefined {
  const items = roomMedia(roomId);
  if (!items) return undefined;
  return startTask("rooms.media.quarantine", roomId, items.length, "items", () => {
    let quarantined = 0;
    let already = 0;
    let protectedCount = 0;
    for (const item of items) {
      if (item.protected) protectedCount += 1;
      else if (item.quarantined) already += 1;
      else {
        item.quarantined = true;
        quarantined += 1;
      }
    }
    return { quarantined, already_quarantined: already, protected: protectedCount };
  });
}
