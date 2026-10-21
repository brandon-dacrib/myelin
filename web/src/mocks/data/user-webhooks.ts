import type { BridgeConnectionCreated } from "@/api/bridges";
import { WEBHOOK_SOURCES } from "./bridge-offerings";

/**
 * The user webhooks API's mock (`/_matrix/client/unstable/io.myelin.webhooks`, decision 0057),
 * for the integrations page: alice's rooms, each saying whether a webhook may be added there
 * the way the server decides it, and the webhooks in them. Independent of the admin mock's
 * Hookshot offering: the page is for someone who never sees the admin page.
 */
export interface MockUserRoom {
  room_id: string;
  name: string | null;
  bot_joined: boolean;
  may_add: boolean;
  reason?: string;
}

export interface MockUserWebhook {
  room_id: string;
  room_name: string | null;
  name: string;
  source: string;
  created_by: string;
  created_at: number;
}

export const MOCK_USER = "@alice:example.org";
const SERVER = "example.org";
const PUBLIC_URL = "https://hooks.example.org";

const seededRooms = (): MockUserRoom[] => [
  { room_id: "!deploys:example.org", name: "Deploys", bot_joined: false, may_add: true },
  { room_id: "!ops:example.org", name: "Ops", bot_joined: true, may_add: true },
  {
    room_id: "!team:example.org",
    name: "Team chat",
    bot_joined: false,
    may_add: false,
    reason:
      "adding a webhook there takes power level 50 (it is a state event in the room) and you have 0; ask a room admin to raise you, or use a room of your own",
  },
  {
    room_id: "!secret:example.org",
    name: "Secret",
    bot_joined: false,
    may_add: false,
    reason:
      "that room is encrypted, and the webhooks bridge does not encrypt: use a room without encryption",
  },
];

const seededWebhooks = (): MockUserWebhook[] => [
  {
    room_id: "!ops:example.org",
    room_name: "Ops",
    name: "uptime",
    source: "generic",
    created_by: MOCK_USER,
    created_at: Date.now() - 3 * 24 * 3600 * 1000,
  },
];

let rooms: MockUserRoom[] = seededRooms();
let webhooks: MockUserWebhook[] = seededWebhooks();

export function resetUserWebhooks(): void {
  rooms = seededRooms();
  webhooks = seededWebhooks();
}

export function userRooms(): MockUserRoom[] {
  return rooms.map((r) => ({ ...r }));
}

export function userWebhooks(): MockUserWebhook[] {
  return webhooks.map((w) => ({ ...w }));
}

/** A Matrix error, as the server answers one. */
export interface MatrixRefusal {
  status: number;
  errcode: string;
  error: string;
  retry_after_ms?: number;
}

export interface UserWebhookCreateBody {
  source?: string;
  name?: string;
  room?: { id?: string; create?: { name?: string; topic?: string } };
}

/** Makes a webhook the way the server does for a user, or says why not. */
export function createUserWebhook(
  body: UserWebhookCreateBody,
): { created: BridgeConnectionCreated } | { refused: MatrixRefusal } {
  const source = WEBHOOK_SOURCES.find((s) => s.id === body.source);
  if (!source) {
    return {
      refused: {
        status: 400,
        errcode: "M_INVALID_PARAM",
        error: `\`${body.source}\` is not a webhook source`,
      },
    };
  }
  const name = body.name ?? "";
  if (name.trim() !== name || name.length < 3 || name.length > 64) {
    return {
      refused: {
        status: 400,
        errcode: "M_INVALID_PARAM",
        error: "the name is 3 to 64 characters, with no space at either end (at /name)",
      },
    };
  }
  const room = body.room ?? {};
  let roomId: string;
  let roomName: string | null;
  if (room.id && !room.create) {
    const mine = rooms.find((r) => r.room_id === room.id);
    if (!mine) {
      return {
        refused: {
          status: 403,
          errcode: "M_FORBIDDEN",
          error: "you are not in that room, or there is no such room",
        },
      };
    }
    if (!mine.may_add) {
      return { refused: { status: 403, errcode: "M_FORBIDDEN", error: mine.reason ?? "no" } };
    }
    if (webhooks.some((w) => w.room_id === mine.room_id && w.name === name)) {
      return {
        refused: {
          status: 409,
          errcode: "M_UNKNOWN",
          error: `the room already has a webhook named \`${name}\``,
        },
      };
    }
    mine.bot_joined = true;
    roomId = mine.room_id;
    roomName = mine.name;
  } else if (room.create && !room.id) {
    if (!room.create.name?.trim()) {
      return {
        refused: {
          status: 400,
          errcode: "M_INVALID_PARAM",
          error: "`room.create.name`: the room needs a name",
        },
      };
    }
    roomId = `!${Math.random().toString(36).slice(2, 10)}:${SERVER}`;
    roomName = room.create.name.trim();
    rooms.push({ room_id: roomId, name: roomName, bot_joined: true, may_add: true });
  } else {
    return {
      refused: {
        status: 400,
        errcode: "M_INVALID_PARAM",
        error: "say which room: `room.id` of one you are in, or `room.create` with a name",
      },
    };
  }
  const id = crypto.randomUUID();
  const url = `${PUBLIC_URL}/webhook/${id}`;
  webhooks.push({
    room_id: roomId,
    room_name: roomName,
    name,
    source: source.id,
    created_by: MOCK_USER,
    created_at: Date.now(),
  });
  const fill = (text: string) => text.replaceAll("{{url}}", url).replaceAll("{{name}}", name);
  return {
    created: {
      room_id: roomId,
      name,
      source: source.id,
      url,
      setup: fill(source.setup),
      files: Object.fromEntries(
        (source.files ?? []).map((f) => [f, `// ${f}\nconst WEBHOOK_URL = "${url}";\n`]),
      ),
    },
  };
}

/** Removes a webhook; false when there is none by that name in that room. */
export function removeUserWebhook(roomId: string, name: string): boolean {
  const before = webhooks.length;
  webhooks = webhooks.filter((w) => !(w.room_id === roomId && w.name === name));
  return webhooks.length < before;
}
