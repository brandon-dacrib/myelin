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
  /** Whether a render writes a config the bridge reads as it is; mautrix bridges' default. */
  rendersConfig?: boolean;
  /** Why the server cannot deploy it, when it cannot (`not_deployable_reason`). */
  notDeployableReason?: string;
  /** A program for a Mac rather than a container image: never deployed by this server. */
  runsOn?: "container" | "mac";
  /** For `runsOn: "mac"`, the pinned build to download and what it is. */
  download?: { url: string; note: string };
  /** The MSCs the bridge needs; mautrix's three unless the type says otherwise. */
  requiredFeatures?: string[];
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
  // matrimail's connector says 29319 (mautrix-meta's); the config the server renders says this.
  matrimail: 29339,
  heisenbridge: 9898,
  "matrix-appservice-irc": 9999,
  "matrix-hookshot": 9993,
  "mautrix-imessage": 29331,
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
  "mautrix-imessage": "!im",
  // No `DefaultCommandPrefix` in their connectors: mautrix-go falls back to `!` + NetworkID.
  "mautrix-signal": "!signal",
  "mautrix-slack": "!slack",
  "mautrix-twitter": "!twitter",
  "mautrix-linkedin": "!linkedin",
  matrimail: "!matrimail",
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
      provisioning_api: "none",
      provisioning_note:
        "Hookshot 7 has no provisioning API and nobody signs in to it: each room's connections (webhooks, feeds, repositories) are state events in the room, which the offering's page lists.",
    };
  if (id === "mautrix-discord")
    return {
      provisioning_api: "none",
      provisioning_note:
        "This bridge is on mautrix-go's older framework, whose provisioning API has no whoami, so the bridge keeps who has signed in itself: ask its bot `ping`.",
    };
  if ((id.startsWith("mautrix-") && id !== "mautrix-imessage") || id === "matrimail")
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
/**
 * The release each mautrix image is pinned to, as the real catalogue pins it
 * (`crates/hs-admin/src/bridge_types.rs`; decision 0037: no offering runs `latest`).
 */
const MAUTRIX_PINS: Record<string, string> = {
  whatsapp: "v0.2610.0",
  telegram: "v0.2610.0",
  signal: "v0.2610.0",
  gmessages: "v0.2610.0",
  gvoice: "v0.2605.0",
  meta: "v0.2610.0",
  discord: "v0.7.7",
  slack: "v0.2610.0",
  twitter: "v0.2609.0",
  linkedin: "v0.2609.0",
  bluesky: "v0.2510.0",
};

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
    image: spec.image ?? `dock.mau.dev/mautrix/${net}:${MAUTRIX_PINS[net] ?? "latest"}`,
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
    required_features:
      spec.requiredFeatures ??
      (mautrix
        ? ["de.sorunome.msc2409.push_ephemeral", "org.matrix.msc3202", "io.element.msc4190"]
        : []),
    renders_config: spec.rendersConfig ?? mautrix,
    mode: spec.mode ?? "per_user",
    deployable: spec.deployable ?? mautrix,
    not_deployable_reason: spec.notDeployableReason ?? null,
    runs_on: spec.runsOn ?? "container",
    download_url: spec.download?.url ?? null,
    download_note: spec.download?.note ?? null,
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
    description: "Your Mac's Messages: iMessage and SMS chats, as yourself.",
    needs: ["mac", "A Mac signed in to iMessage, which runs the bridge"],
    docs: "https://docs.mau.fi/bridges/go/imessage/mac/setup.html",
    // Not a container image: the pinned build of the program for a Mac, named like a tag.
    image: "mautrix-imessage:300ba6d0",
    // It runs beside Messages on the person's own Mac; no cluster can run it for them.
    deployable: false,
    notDeployableReason:
      "iMessage bridges run on a Mac, beside the Messages app they read, so this server cannot deploy one: each person downloads the bridge and runs it on their Mac.",
    runsOn: "mac",
    download: {
      url: "https://mau.dev/mautrix/imessage/-/jobs/95972/artifacts/download",
      note: "Universal macOS build (Apple silicon and Intel) of mautrix-imessage commit 300ba6d0, from the project's CI on 2026-05-14. The zip holds the program, libolm.3.dylib and an example config.",
    },
    // No MSC4190: the bridge's bot has an ordinary device.
    requiredFeatures: ["de.sorunome.msc2409.push_ephemeral", "org.matrix.msc3202"],
    steps: [
      "Download the bridge for your Mac from your bridge's page and unzip it.",
      "Put your bridge's config.yaml, registration.yaml and run.sh beside it, open Terminal there and run `./run.sh`. When macOS asks, give Terminal Full Disk Access (System Settings → Privacy & Security → Full Disk Access) and run it again.",
      "Keep the Mac on and awake; the bridge carries the chats of the Apple account Messages is signed in to.",
    ],
    notes:
      "The server has to reach the Mac on its port (29331 by default): the same network, or a Tailscale-style private network. A Mac behind NAT with no such path cannot be reached yet.",
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
    id: "matrimail",
    name: "Email (matrimail)",
    category: "messaging",
    description:
      "Email over IMAP, one room per thread: Gmail with an app password, or any IMAP mailbox; replies go out by SMTP. A community bridge with one maintainer and no release, unproven here beyond starting and answering its bot.",
    needs: [
      "account",
      "A mailbox the bridge can read over IMAP; for Gmail, 2-step verification and an app password",
    ],
    project: "moiri-gamboni/matrimail",
    docs: "https://github.com/moiri-gamboni/matrimail/blob/24cd0f981f803897f7b71693c1c84738e78cf9d7/README.md",
    // The image this repository builds (decision 0055), tagged with the pinned commit.
    image: "ghcr.io/brandon-dacrib/matrimail:24cd0f9",
    // Its registration claims what the bridge's own generator would: NetworkID `email`.
    prefix: "email_",
    bot: "emailbot",
    mautrix: true,
    steps: [
      "Start a direct chat with {bot} and send `login`, or in one line `login email:you@gmail.com password:<app password>`. The bot works out the IMAP and SMTP settings from the address (Gmail, Outlook, iCloud, Yahoo, FastMail; a guess for anything else).",
      "For Gmail: turn on 2-step verification in your Google account, make an app password at myaccount.google.com/apppasswords, and give the bot that, never your Google password. IMAP has to be on in Gmail's settings (See all settings, Forwarding and POP/IMAP).",
      'The bot lists your folders (Gmail: labels) and asks which to watch; `default` is the inbox and Sent. Each email thread becomes a room; a message in that room is sent as a reply, and `compose to:someone@example.org subject:"..."` in the bot\'s chat starts a new thread.',
    ],
    notes:
      "Community software: one maintainer, no release, and Myelin builds the image itself from the fork's source (AGPL-3.0). Edits and redactions are not sent as email (email cannot be unsent), and changing the watched folders means `logout` and `login` again (`config folders` says so).",
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
    image: "hif1/heisenbridge:1.15.4",
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
    image: "matrixdotorg/matrix-appservice-irc:release-4.0.0",
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
    // Deployable since 2026-10-20: its config is rendered whole from the offering's public URL.
    deployable: true,
    rendersConfig: true,
    name: "Hookshot",
    category: "integrations",
    description:
      "One bridge for the whole server, with many connections: GitHub repositories, GitLab projects, Jira, RSS feeds and plain webhooks, each posting into a room of its own.",
    needs: [
      "public_url",
      "A public URL (https://hooks.example.org) that reaches the bridge's webhooks listener, for GitHub and the other services to post to",
    ],
    project: "matrix-org/matrix-hookshot",
    docs: "https://matrix-org.github.io/matrix-hookshot/latest/",
    image: "ghcr.io/matrix-org/matrix-hookshot:7.5.0",
    bot: "hookshot",
    prefix: "hookshot_",
    steps: [
      "Nobody signs in: it is one bridge for everyone here, and each room connects what it wants. Invite {bot} to a room (it needs to be allowed to send state events, the Moderator level in most clients) and send `!hookshot help`.",
      "`!hookshot webhook <name>` answers with a URL for that room; anything that can post JSON to a URL (a GitHub or GitLab repository webhook, a monitoring alert, a script) then posts into the room. `!hookshot feed <url>` follows an RSS or Atom feed.",
      "With a GitHub App in the offering's settings, `!hookshot github repo <org>/<repo>` connects a repository properly: issues, pull requests, reviews and releases, with the people who commented shown as ghost users.",
    ],
    notes:
      "Each connection is a state event in its room (uk.half-shot.matrix-hookshot.*); the offering's page lists them. Hookshot 7 has no provisioning API, so that list is read from the rooms.",
  }),
];
