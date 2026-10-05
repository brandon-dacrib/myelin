import type { BridgeType } from "@/api/bridges";

interface Spec {
  id: string;
  name: string;
  category: "messaging" | "social" | "irc" | "integrations";
  description: string;
  needs: [key: string, description: string];
  steps: string[];
  notes?: string;
  docs?: string;
  image?: string;
  project?: string;
  bot?: string;
  prefix?: string;
  mautrix?: boolean;
  port?: number;
  /** RFC 0017: `per_user` unless the bridge serves a network rather than a person. */
  mode?: "per_user" | "shared";
  /** RFC 0017: whether the server can run it from a rendered config alone. */
  deployable?: boolean;
}

const PORTS: Record<string, number> = {
  "mautrix-whatsapp": 29318,
  "mautrix-telegram": 29317,
  "mautrix-signal": 29328,
  "mautrix-gmessages": 29336,
  "mautrix-gvoice": 29338,
  "mautrix-meta": 29319,
  "mautrix-discord": 29334,
  "mautrix-slack": 29335,
  "mautrix-twitter": 29327,
  "mautrix-linkedin": 29341,
  "mautrix-bluesky": 29340,
  heisenbridge: 9898,
  "matrix-appservice-irc": 9999,
  "matrix-hookshot": 9993,
  "mautrix-imessage": 29337,
};

/**
 * Each mautrix type's default command prefix, as `bridge_types.rs`'s `command_prefix`: what a
 * command needs anywhere but the person's management room.
 */
const COMMAND_PREFIXES: Record<string, string> = {
  "mautrix-whatsapp": "!wa",
  "mautrix-telegram": "!tg",
  "mautrix-gmessages": "!gm",
  "mautrix-gvoice": "!gv",
  "mautrix-meta": "!fb",
  "mautrix-discord": "!discord",
  "mautrix-bluesky": "!bsky",
  // No `DefaultCommandPrefix` in their connectors: mautrix-go falls back to `!` + NetworkID.
  "mautrix-signal": "!signal",
  "mautrix-slack": "!slack",
  "mautrix-twitter": "!twitter",
  "mautrix-linkedin": "!linkedin",
};

/** Each type's provisioning surface and its note, as `bridge_types.rs`'s `provisioning_of`. */
function provisioning(id: string): Pick<BridgeType, "provisioning_api" | "provisioning_note"> {
  if (id === "heisenbridge")
    return {
      provisioning_api: "none",
      provisioning_note:
        "heisenbridge has no provisioning API: its networks, and who uses them, live in its control room, so the bridge keeps that itself.",
    };
  if (id === "matrix-appservice-irc")
    return {
      provisioning_api: "irc_v1",
      provisioning_note:
        "matrix-appservice-irc's provisioning API links rooms to channels; it does not report who is connected as which nick, so the bridge keeps that itself.",
    };
  if (id === "matrix-hookshot")
    return {
      provisioning_api: "hookshot_v1",
      provisioning_note:
        "Hookshot's provisioning API manages each room's connections (webhooks, feeds, repositories); Myelin does not read accounts from it, so the bridge keeps that itself.",
    };
  if (id.startsWith("mautrix-") && id !== "mautrix-imessage")
    return {
      provisioning_api: "mautrix_v3",
      provisioning_note:
        "The bridge's provisioning API (/_matrix/provision/v3/whoami) says who has signed in and as what, with the shared secret Myelin writes into its config.yaml.",
    };
  return {
    provisioning_api: "none",
    provisioning_note: "This bridge keeps who has signed in itself.",
  };
}

/**
 * The same catalogue `crates/hs-admin/src/bridge_types.rs` serves, entry for entry, so that the
 * mock-backed wizard shows what the real one shows. Written for the mock server's own name
 * (`example.org`), as the real catalogue writes them for its.
 */
function type(spec: Spec): BridgeType {
  const net = spec.id.replace(/^mautrix-/, "");
  const short = spec.id.replace(/^mautrix-/, "").replace(/^matrix-/, "");
  const mautrix = spec.mautrix ?? spec.id.startsWith("mautrix-");
  const prefix = spec.prefix ?? `${short}_`;
  const bot = spec.bot ?? `${short}bot`;
  return {
    id: spec.id,
    name: spec.name,
    description: spec.description,
    category: spec.category,
    upstream_project: spec.project ?? `mautrix/${net}`,
    docs_url: spec.docs ?? `https://docs.mau.fi/bridges/go/${net}/index.html`,
    image: spec.image ?? `dock.mau.dev/mautrix/${net}:latest`,
    port: spec.port ?? PORTS[spec.id] ?? 29999,
    // `default_namespaces` is an untyped OpenAPI object (`Record<string, never>` once
    // generated); this is the one sanctioned cast for it, as the wizard's is for a render.
    default_namespaces: {
      users: [
        { regex: `@${prefix}.*:example\\.org`, exclusive: true },
        { regex: `@${bot}:example\\.org`, exclusive: true },
      ],
      aliases: [{ regex: `#${prefix}.*:example\\.org`, exclusive: true }],
      rooms: [],
    } as unknown as Record<string, never>,
    config_keys: [{ key: spec.needs[0], description: spec.needs[1], required: true }],
    supports_double_puppeting: mautrix,
    command_prefix: COMMAND_PREFIXES[spec.id] ?? null,
    required_features: mautrix
      ? ["de.sorunome.msc2409.push_ephemeral", "org.matrix.msc3202", "io.element.msc4190"]
      : [],
    renders_config: mautrix,
    mode: spec.mode ?? "per_user",
    deployable: spec.deployable ?? mautrix,
    ...provisioning(spec.id),
    sign_in: { steps: spec.steps, notes: spec.notes ?? null },
  };
}

/** The wizard's catalogue (flows.md flow 1 step 2). */
export const bridgeTypes: BridgeType[] = [
  type({
    id: "mautrix-whatsapp",
    name: "WhatsApp",
    category: "messaging",
    description: "Personal and group chats, linked to your phone the way WhatsApp Web is.",
    needs: ["phone", "A phone with WhatsApp, to link the bridge to by scanning a code"],
    steps: [
      "Start a direct chat with {bot} and send `login qr`, or `login phone` to get a pairing code instead.",
      "On the phone, open WhatsApp, then Linked devices, then Link a device, and scan the code (or enter the pairing code).",
      "The bot confirms the login. Your chats start appearing as rooms about a minute later.",
    ],
    notes:
      "WhatsApp unlinks the bridge if the phone stays offline for more than two weeks; the bot warns after twelve days.",
  }),
  type({
    id: "mautrix-telegram",
    name: "Telegram",
    category: "messaging",
    description: "Chats, groups and channels, through Telegram's own API.",
    needs: ["api_id", "A Telegram API ID from my.telegram.org"],
    steps: [
      "Start a direct chat with {bot} and send `login phone +15551234567`, with your own number in international format.",
      "Telegram sends a code to your other signed-in Telegram app, not by SMS. Send the code to the bot, then your two-factor password if you have one.",
    ],
  }),
  type({
    id: "mautrix-signal",
    name: "Signal",
    category: "messaging",
    description: "Signal conversations, with the bridge linked as one of your devices.",
    needs: ["phone", "A phone with Signal, to link the bridge to as a secondary device"],
    steps: [
      "Start a direct chat with {bot} and send `login`.",
      "On the phone, open Signal, then Settings, Linked devices, Link new device, and scan the code the bot sends.",
      "If backfill is on, Signal asks whether to transfer message history; either answer works.",
    ],
    notes: "The bridge links as a secondary device. It cannot register a number of its own.",
  }),
  type({
    id: "mautrix-gmessages",
    name: "Google Messages",
    category: "messaging",
    description: "SMS and RCS from an Android phone running Google Messages.",
    needs: ["phone", "An Android phone with Google Messages, to pair with"],
    steps: [
      "Start a direct chat with {bot} and send `login google`.",
      "On the phone, open Google Messages and tap the emoji the bot shows you.",
    ],
    notes: "Every message goes through the phone, which has to stay online.",
  }),
  type({
    id: "mautrix-gvoice",
    name: "Google Voice",
    category: "messaging",
    description: "Texts and voicemail from a Google Voice number.",
    needs: ["account", "A Google account with a Google Voice number"],
    steps: ["Start a direct chat with {bot} and send `login`."],
  }),
  type({
    id: "mautrix-imessage",
    name: "iMessage",
    category: "messaging",
    description: "iMessage and SMS, through a Mac signed in to Messages.",
    needs: ["mac", "A Mac signed in to iMessage, which runs the bridge"],
    // It runs beside Messages on the person's own Mac; no cluster can run it for them.
    deployable: false,
    steps: [
      "Start the bridge on the Mac with the files an administrator gave you; it signs in with the Mac's own Messages account.",
      "Start a direct chat with {bot} and send `help`. Your iMessage chats appear as rooms as messages arrive.",
    ],
    notes: "The Mac has to stay on and signed in to Messages for the bridge to work.",
  }),
  type({
    id: "mautrix-meta",
    name: "Messenger and Instagram",
    category: "messaging",
    description: "Facebook Messenger and Instagram direct messages.",
    needs: ["account", "A Facebook or Instagram account to sign in with from the bridge"],
    steps: [
      "Start a direct chat with {bot} and send `login messenger`, `login facebook` or `login instagram`.",
    ],
  }),
  type({
    id: "mautrix-discord",
    name: "Discord",
    category: "social",
    description: "Servers and DMs, as yourself or as a Discord bot.",
    needs: ["account", "A Discord account to sign in with from the bridge"],
    steps: [
      "Start a direct chat with {bot} and send `login-qr`, then scan the code with the Discord app on your phone and approve it.",
      "For a bot account instead: `login-token bot <token>`.",
    ],
    notes:
      "Discord may flag accounts that look automated. A bot account carries none of that risk.",
  }),
  type({
    id: "mautrix-slack",
    name: "Slack",
    category: "social",
    description: "Workspaces and DMs, as yourself or through a Slack app.",
    needs: ["account", "A Slack account to sign in with from the bridge"],
    steps: ["Start a direct chat with {bot} and send `login token <xoxc-…> <xoxd-…>`."],
  }),
  type({
    id: "mautrix-twitter",
    name: "X (Twitter)",
    category: "social",
    description: "Direct messages on X.",
    needs: ["account", "An X account to sign in with from the bridge"],
    steps: ["Start a direct chat with {bot} and send `login`."],
  }),
  type({
    id: "mautrix-linkedin",
    name: "LinkedIn",
    category: "social",
    description: "LinkedIn messaging.",
    needs: ["account", "A LinkedIn account to sign in with from the bridge"],
    steps: ["Start a direct chat with {bot} and send `login`."],
  }),
  type({
    id: "mautrix-bluesky",
    name: "Bluesky",
    category: "social",
    description: "Bluesky direct messages.",
    needs: ["account", "A Bluesky account and an app password"],
    steps: [
      "Start a direct chat with {bot} and send `login`, then give it your handle and an app password (Bluesky Settings, App Passwords), not your main password.",
    ],
  }),
  type({
    id: "heisenbridge",
    mode: "shared",
    deployable: true,
    name: "IRC (heisenbridge)",
    category: "irc",
    description: "An IRC bouncer: one person's networks, channels and queries, in Matrix.",
    needs: ["owner", "The Matrix user who drives the bridge; it opens a control room for them"],
    project: "hifi/heisenbridge",
    docs: "https://github.com/hifi/heisenbridge",
    image: "hif1/heisenbridge:latest",
    bot: "heisenbridge",
    prefix: "irc_",
    steps: [
      "The bridge invites its owner to a control room when it starts; accept it. `HELP` lists every command.",
      "`ADDNETWORK libera`, then `ADDSERVER libera irc.libera.chat 6697 --tls`, then `OPEN libera` opens a room for that network.",
      "In the network room, `CONNECT`, then `JOIN #channel`. Each channel and query becomes a room.",
    ],
    notes: "One owner drives the bridge: the Matrix user named on its command line.",
  }),
  type({
    id: "matrix-appservice-irc",
    mode: "shared",
    deployable: false,
    name: "IRC (matrix-appservice-irc)",
    category: "irc",
    description: "Whole IRC channels as Matrix rooms, for many users at once.",
    needs: ["network", "The IRC network to connect to, in the bridge's config"],
    project: "matrix-org/matrix-appservice-irc",
    docs: "https://matrix-org.github.io/matrix-appservice-irc/latest/",
    image: "ghcr.io/matrix-org/matrix-appservice-irc:release-3.0.0",
    bot: "ircbot",
    prefix: "irc_",
    steps: [
      "Join a channel by its alias, `#irc_#channel:example.org` by default; the prefix and the networks are in the bridge's config file.",
      "Start a direct chat with {bot} and send `!nick <name>` to choose your IRC nick, or `!join #channel` to join one.",
    ],
  }),
  type({
    id: "matrix-hookshot",
    mode: "shared",
    deployable: false,
    name: "Hookshot",
    category: "integrations",
    description: "Webhooks, GitHub, GitLab, Jira and RSS feeds, posted into rooms.",
    needs: ["public_url", "A URL the services it listens to can reach it on, for webhooks"],
    project: "matrix-org/matrix-hookshot",
    docs: "https://matrix-org.github.io/matrix-hookshot/latest/",
    image: "halfshot/matrix-hookshot:latest",
    bot: "hookshot",
    prefix: "hookshot_",
    steps: [
      "Invite {bot} to a room and send `!hookshot help`.",
      "`!hookshot webhook <name>` gives the room a webhook URL; GitHub, GitLab, Jira and RSS are connected per room the same way, or through the widget.",
    ],
  }),
];
