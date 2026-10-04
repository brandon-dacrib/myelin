//! The bridge catalogue behind `GET /bridge-types` and `POST /bridge-types/{type}/render`: what
//! the interface's "Add bridge" wizard offers, and what it renders a choice into.
//!
//! Every entry is a bridge somebody actually runs, with the image its project publishes, the
//! port its own config generator writes into `appservice.address`, the namespaces it expects,
//! the appservice features it needs, where its documentation is, and how a person signs in to
//! it once it runs -- the part of running a bridge that the mautrix documentation
//! (<https://docs.mau.fi/bridges/>) spends most of its pages on, and that an operator otherwise
//! has to go and read. The render turns a wizard's choices into everything an operator needs:
//! a registration this server will accept (`appservices.create` takes it as it is), the same as
//! the YAML file the bridge reads, for a mautrix bridge a `config.yaml` already pointed at this
//! server with the same tokens, a Compose service for running it beside the server, and a
//! `Bridge` resource for the Kubernetes operator that is still to be written. Nothing here is
//! created: the wizard shows the result and then creates.
//!
//! The tokens are minted here, once per render, and the registration carries them; a render
//! is what a fresh registration file is. The registration also carries
//! [`BRIDGE_TYPE_KEY`], so that an appservice created from the catalogue remembers which entry
//! it came from -- the registry keeps unknown keys, and so does every bridge's own parser.

use rand::RngCore;
use serde_json::{Map, Value, json};

use crate::model::{BridgeType, BridgeTypeConfigKey, BridgeTypeRenderResult, BridgeTypeSignIn};

/// The registration key that names the catalogue entry an appservice was created from
/// (`mautrix-whatsapp`, `heisenbridge`, ...). Namespaced like the MSC feature keys, kept by the
/// registry with the other unrecognised keys, ignored by the bridges themselves.
pub const BRIDGE_TYPE_KEY: &str = "io.myelin.bridge_type";

/// How a bridge is run, for the artifacts that say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runtime {
    /// A mautrix bridge on `bridgev2`: one container, a `config.yaml` this render writes the
    /// essentials of and the bridge completes on first start, a database of its own (SQLite
    /// unless told otherwise).
    Mautrix,
    /// heisenbridge: a single Python process configured on its command line, whose state lives
    /// in its bot's account data on this server.
    Heisenbridge,
    /// matrix-appservice-irc: Node, a YAML config, its own database.
    AppserviceIrc,
    /// matrix-hookshot: Node, a YAML config, listens for webhooks as well as for this server.
    Hookshot,
}

/// Where a bridge sits in the wizard's catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    /// Personal messaging tied to a phone or an account: WhatsApp, Signal, Telegram, ...
    Messaging,
    /// Communities and workplaces: Discord, Slack, X, LinkedIn, Bluesky.
    Social,
    /// IRC, in both of its bridging styles.
    Irc,
    /// Things that post into rooms rather than people who chat: webhooks, GitHub, feeds.
    Integrations,
}

impl Category {
    fn id(self) -> &'static str {
        match self {
            Self::Messaging => "messaging",
            Self::Social => "social",
            Self::Irc => "irc",
            Self::Integrations => "integrations",
        }
    }
}

struct Entry {
    id: &'static str,
    name: &'static str,
    /// One line on what it connects, in the wizard's words.
    description: &'static str,
    category: Category,
    upstream_project: &'static str,
    /// The project's own documentation, where the wizard's guidance stops.
    docs_url: &'static str,
    image: &'static str,
    /// The port the bridge listens on for this server by default -- what its own config
    /// generator writes, so that the registration and the container agree without anybody
    /// editing either.
    port: u16,
    /// The localpart prefix its ghost users carry: `whatsapp_` for `@whatsapp_123:server`.
    ghost_prefix: &'static str,
    /// The bot's localpart.
    bot: &'static str,
    /// What the operator has to have before the bridge is useful, in the wizard's words.
    needs: &'static [(&'static str, &'static str, bool)],
    double_puppeting: bool,
    /// Appservice features the registration must ask for.
    features: &'static [&'static str],
    runtime: Runtime,
    /// How a person signs in once the bridge runs, one step per line, from the bridge's own
    /// documentation. `{bot}` stands for the bridge bot's Matrix ID, which the interface knows
    /// and this catalogue does not (the operator may rename the bot).
    sign_in: &'static [&'static str],
    /// A caveat worth knowing before signing in, or nothing.
    sign_in_notes: Option<&'static str>,
}

const MSC2409: &str = "de.sorunome.msc2409.push_ephemeral";
const MSC3202: &str = "org.matrix.msc3202";
const MSC4190: &str = "io.element.msc4190";

const MAUTRIX_FEATURES: &[&str] = &[MSC2409, MSC3202, MSC4190];

macro_rules! mautrix {
    (
        $id:literal, $name:literal, $net:literal, $port:literal, $prefix:literal, $bot:literal,
        category: $category:expr,
        description: $description:literal,
        needs: [$(($k:literal, $d:literal, $r:literal)),* $(,)?],
        sign_in: [$($step:literal),* $(,)?],
        notes: $notes:expr $(,)?
    ) => {
        Entry {
            id: $id,
            name: $name,
            description: $description,
            category: $category,
            upstream_project: concat!("mautrix/", $net),
            docs_url: concat!("https://docs.mau.fi/bridges/go/", $net, "/index.html"),
            image: concat!("dock.mau.dev/mautrix/", $net, ":latest"),
            port: $port,
            ghost_prefix: $prefix,
            bot: $bot,
            needs: &[$(($k, $d, $r)),*],
            double_puppeting: true,
            features: MAUTRIX_FEATURES,
            runtime: Runtime::Mautrix,
            sign_in: &[$($step),*],
            sign_in_notes: $notes,
        }
    };
}

const CATALOGUE: &[Entry] = &[
    mautrix!(
        "mautrix-whatsapp", "WhatsApp", "whatsapp", 29318, "whatsapp_", "whatsappbot",
        category: Category::Messaging,
        description: "Personal and group chats, linked to your phone the way WhatsApp Web is.",
        needs: [("phone", "A phone with WhatsApp, to link the bridge to by scanning a code", true)],
        sign_in: [
            "Start a direct chat with {bot} and send `login qr`, or `login phone` to get a pairing code instead.",
            "On the phone, open WhatsApp, then Linked devices, then Link a device, and scan the code (or enter the pairing code).",
            "The bot confirms the login. Your chats start appearing as rooms about a minute later.",
        ],
        notes: Some("WhatsApp unlinks the bridge if the phone stays offline for more than two weeks; the bot warns after twelve days."),
    ),
    mautrix!(
        "mautrix-telegram", "Telegram", "telegram", 29317, "telegram_", "telegrambot",
        category: Category::Messaging,
        description: "Chats, groups and channels, through Telegram's own API.",
        needs: [
            ("api_id", "A Telegram API ID from my.telegram.org", true),
            ("api_hash", "The matching API hash", true),
        ],
        sign_in: [
            "Start a direct chat with {bot} and send `login phone +15551234567`, with your own number in international format.",
            "Telegram sends a code to your other signed-in Telegram app, not by SMS. Send the code to the bot, then your two-factor password if you have one.",
            "`login qr` scans a code from Telegram's Settings, Devices, Link Desktop Device instead; `login bot <token>` signs in a Telegram bot.",
        ],
        notes: Some("Telegram's official API is used. Brand-new accounts signed in from third-party apps are sometimes suspended; established ones are fine."),
    ),
    mautrix!(
        "mautrix-signal", "Signal", "signal", 29328, "signal_", "signalbot",
        category: Category::Messaging,
        description: "Signal conversations, with the bridge linked as one of your devices.",
        needs: [("phone", "A phone with Signal, to link the bridge to as a secondary device", true)],
        sign_in: [
            "Start a direct chat with {bot} and send `login`.",
            "On the phone, open Signal, then Settings, Linked devices, Link new device, and scan the code the bot sends.",
            "If backfill is on, Signal asks whether to transfer message history; either answer works.",
        ],
        notes: Some("The bridge links as a secondary device. It cannot register a number of its own."),
    ),
    mautrix!(
        "mautrix-gmessages", "Google Messages", "gmessages", 29336, "gmessages_", "gmessagesbot",
        category: Category::Messaging,
        description: "SMS and RCS from an Android phone running Google Messages.",
        needs: [("phone", "An Android phone with Google Messages, to pair with", true)],
        sign_in: [
            "Start a direct chat with {bot} and send `login google`.",
            "In a private browser window, sign in to messages.google.com/web, copy the `/web/config` request from the Network tab as cURL, and paste it to the bot.",
            "On the phone, open Google Messages and tap the emoji the bot shows you.",
        ],
        notes: Some("Every message goes through the phone, which has to stay online. QR pairing no longer works; Google removed it."),
    ),
    mautrix!(
        "mautrix-gvoice", "Google Voice", "gvoice", 29338, "gvoice_", "gvoicebot",
        category: Category::Messaging,
        description: "Texts and voicemail from a Google Voice number.",
        needs: [("account", "A Google account with a Google Voice number", true)],
        sign_in: [
            "Start a direct chat with {bot} and send `login`; the bot asks for the cookies from a browser signed in to voice.google.com.",
        ],
        notes: None,
    ),
    mautrix!(
        "mautrix-meta", "Messenger and Instagram", "meta", 29319, "meta_", "metabot",
        category: Category::Messaging,
        description: "Facebook Messenger and Instagram direct messages.",
        needs: [("account", "A Facebook or Instagram account to sign in with from the bridge", true)],
        sign_in: [
            "Start a direct chat with {bot} and send `login messenger`, `login facebook` or `login instagram`.",
            "In a private browser window, sign in to the site, filter the Network tab for `graphql`, copy any request as cURL, and paste it to the bot.",
        ],
        notes: Some("Meta sometimes asks for a checkpoint (a captcha, phone verification) after a new login. Two-factor authentication on the account makes that rarer."),
    ),
    mautrix!(
        "mautrix-discord", "Discord", "discord", 29334, "discord_", "discordbot",
        category: Category::Social,
        description: "Servers and DMs, as yourself or as a Discord bot.",
        needs: [("account", "A Discord account to sign in with from the bridge", true)],
        sign_in: [
            "Start a direct chat with {bot} and send `login-qr`, then scan the code with the Discord app on your phone and approve it.",
            "Without a phone: `login-token user <token>`, with the token from the Authorization header of any request in the browser's Network tab while signed in to Discord.",
            "For a bot account instead: `login-token bot <token>`, with the members and message-content intents enabled on the application.",
        ],
        notes: Some("Discord may flag accounts that look automated. A bot account carries none of that risk."),
    ),
    mautrix!(
        "mautrix-slack", "Slack", "slack", 29335, "slack_", "slackbot",
        category: Category::Social,
        description: "Workspaces and DMs, as yourself or through a Slack app.",
        needs: [("account", "A Slack account to sign in with from the bridge", true)],
        sign_in: [
            "Start a direct chat with {bot} and send `login token <xoxc-…> <xoxd-…>`.",
            "Both values come from a browser signed in to Slack: the `xoxc-` token from localStorage's `localConfig_v2`, the `xoxd-` value from the `d` cookie.",
            "`login app` signs in a Slack app instead, with its `xapp-` app token and `xoxb-` bot token.",
        ],
        notes: None,
    ),
    mautrix!(
        "mautrix-twitter", "X (Twitter)", "twitter", 29327, "twitter_", "twitterbot",
        category: Category::Social,
        description: "Direct messages on X.",
        needs: [("account", "An X account to sign in with from the bridge", true)],
        sign_in: [
            "Start a direct chat with {bot} and send `login`; the bot lists the ways to sign in and asks for the cookies from a browser signed in to X.",
        ],
        notes: None,
    ),
    mautrix!(
        "mautrix-linkedin", "LinkedIn", "linkedin", 29325, "linkedin_", "linkedinbot",
        category: Category::Social,
        description: "LinkedIn messaging.",
        needs: [("account", "A LinkedIn account to sign in with from the bridge", true)],
        sign_in: [
            "Start a direct chat with {bot} and send `login`; the bot asks for the cookies from a browser signed in to LinkedIn.",
        ],
        notes: None,
    ),
    mautrix!(
        "mautrix-bluesky", "Bluesky", "bluesky", 29340, "bluesky_", "blueskybot",
        category: Category::Social,
        description: "Bluesky direct messages.",
        needs: [("account", "A Bluesky account and an app password", true)],
        sign_in: [
            "Start a direct chat with {bot} and send `login`, then give it your handle and an app password (Bluesky Settings, App Passwords), not your main password.",
        ],
        notes: None,
    ),
    Entry {
        id: "heisenbridge",
        name: "IRC (heisenbridge)",
        description: "An IRC bouncer: one person's networks, channels and queries, in Matrix.",
        category: Category::Irc,
        upstream_project: "hifi/heisenbridge",
        docs_url: "https://github.com/hifi/heisenbridge",
        image: "hif1/heisenbridge:latest",
        port: 9898,
        ghost_prefix: "irc_",
        bot: "heisenbridge",
        needs: &[(
            "owner",
            "The Matrix user who drives the bridge; it opens a control room for them",
            true,
        )],
        double_puppeting: false,
        features: &[],
        runtime: Runtime::Heisenbridge,
        sign_in: &[
            "The bridge invites its owner to a control room when it starts; accept it. `HELP` lists every command.",
            "`ADDNETWORK libera`, then `ADDSERVER libera irc.libera.chat 6697 --tls`, then `OPEN libera` opens a room for that network.",
            "In the network room, `CONNECT`, then `JOIN #channel`. Each channel and query becomes a room.",
        ],
        sign_in_notes: Some(
            "One owner drives the bridge: the Matrix user named on its command line.",
        ),
    },
    Entry {
        id: "matrix-appservice-irc",
        name: "IRC (matrix-appservice-irc)",
        description: "Whole IRC channels as Matrix rooms, for many users at once.",
        category: Category::Irc,
        upstream_project: "matrix-org/matrix-appservice-irc",
        docs_url: "https://matrix-org.github.io/matrix-appservice-irc/latest/",
        image: "ghcr.io/matrix-org/matrix-appservice-irc:release-3.0.0",
        port: 9999,
        ghost_prefix: "irc_",
        bot: "ircbot",
        needs: &[(
            "network",
            "The IRC network to connect to, in the bridge's config",
            true,
        )],
        double_puppeting: false,
        features: &[],
        runtime: Runtime::AppserviceIrc,
        sign_in: &[
            "Join a channel by its alias, `#irc_#channel:{server}` by default; the prefix and the networks are in the bridge's config file.",
            "Start a direct chat with {bot} and send `!nick <name>` to choose your IRC nick, or `!join #channel` to join one.",
        ],
        sign_in_notes: None,
    },
    Entry {
        id: "matrix-hookshot",
        name: "Hookshot",
        description: "Webhooks, GitHub, GitLab, Jira and RSS feeds, posted into rooms.",
        category: Category::Integrations,
        upstream_project: "matrix-org/matrix-hookshot",
        docs_url: "https://matrix-org.github.io/matrix-hookshot/latest/",
        image: "halfshot/matrix-hookshot:latest",
        port: 9993,
        ghost_prefix: "hookshot_",
        bot: "hookshot",
        needs: &[(
            "public_url",
            "A URL the services it listens to can reach it on, for webhooks",
            false,
        )],
        double_puppeting: false,
        features: &[MSC2409],
        runtime: Runtime::Hookshot,
        sign_in: &[
            "Invite {bot} to a room and send `!hookshot help`.",
            "`!hookshot webhook <name>` gives the room a webhook URL; GitHub, GitLab, Jira and RSS are connected per room the same way, or through the widget.",
        ],
        sign_in_notes: None,
    },
];

/// `per_user` for a bridge of one person's account (every mautrix bridge), `shared` for one
/// that bridges a network or a server for everyone: heisenbridge is a bouncer any local user
/// the owner allows can drive, so one process serves the server (RFC 0017 section 2).
fn mode(entry: &Entry) -> &'static str {
    match entry.runtime {
        Runtime::Mautrix => "per_user",
        Runtime::Heisenbridge | Runtime::AppserviceIrc | Runtime::Hookshot => "shared",
    }
}

/// The registration key holding the secret a bridge's provisioning API takes
/// (`provisioning.shared_secret` in a mautrix bridge's `config.yaml`). A render mints it and
/// writes it into both, so that the server can ask the bridge who has signed in
/// (`GET /appservices/{id}/logins`, [`crate::bridge_logins`]). Kept by the registry with the
/// other unrecognised keys; a bridge's own registration parser ignores it. A registration made
/// before this key existed has none, and an administrator can add the bridge's own secret with
/// a merge patch.
pub const PROVISIONING_SECRET_KEY: &str = "io.myelin.provisioning_secret";

/// A bridge's own provisioning surface ([`BridgeType::provisioning_api`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningApi {
    /// A mautrix `bridgev2` bridge: `/_matrix/provision/v3` on its appservice listener,
    /// authenticated by `provisioning.shared_secret` and told the user with `?user_id=`.
    /// `GET /v3/whoami` lists the user's logins with their state.
    MautrixV3,
    /// matrix-appservice-irc's `/_matrix/provision` v1: links rooms to channels. It does not
    /// report who is connected as which nick.
    IrcV1,
    /// matrix-hookshot's provisioning API: a room's connections (webhooks, feeds, repositories).
    HookshotV1,
    /// None at all.
    None,
}

impl ProvisioningApi {
    /// The wire word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MautrixV3 => "mautrix_v3",
            Self::IrcV1 => "irc_v1",
            Self::HookshotV1 => "hookshot_v1",
            Self::None => "none",
        }
    }

    /// Whether this server can read who has signed in through it.
    #[must_use]
    pub fn reports_logins(self) -> bool {
        self == Self::MautrixV3
    }
}

fn provisioning_of(entry: &Entry) -> (ProvisioningApi, &'static str) {
    match entry.runtime {
        Runtime::Mautrix => (
            ProvisioningApi::MautrixV3,
            "The bridge's provisioning API (/_matrix/provision/v3/whoami) says who has signed in and as what, with the shared secret Myelin writes into its config.yaml.",
        ),
        Runtime::Heisenbridge => (
            ProvisioningApi::None,
            "heisenbridge has no provisioning API: its networks, and who uses them, live in its control room, so the bridge keeps that itself.",
        ),
        Runtime::AppserviceIrc => (
            ProvisioningApi::IrcV1,
            "matrix-appservice-irc's provisioning API links rooms to channels; it does not report who is connected as which nick, so the bridge keeps that itself.",
        ),
        Runtime::Hookshot => (
            ProvisioningApi::HookshotV1,
            "Hookshot's provisioning API manages each room's connections (webhooks, feeds, repositories); Myelin does not read accounts from it, so the bridge keeps that itself.",
        ),
    }
}

/// The provisioning surface of catalogue entry `type_id` and what it means in words; `None`
/// for a type not in the catalogue.
#[must_use]
pub fn provisioning(type_id: &str) -> Option<(ProvisioningApi, &'static str)> {
    entry(type_id).map(provisioning_of)
}

/// The prefix a command to the bot takes to be understood anywhere the bot is: a mautrix
/// bridge's `DefaultCommandPrefix` (its connector's `GetName()`; `!wa` for WhatsApp), which
/// the config this server renders leaves at the bridge's default. A bare command is taken only
/// in the person's management room, the chat they invited the bot into. `None` where the
/// bridge has no such prefix, or it is not known here.
///
/// Read from each bridge's source on 2026-10-02 (`pkg/connector/connector.go`, or the example
/// config for Discord). Signal, Slack and X set no `DefaultCommandPrefix` there, and on
/// 2026-10-04 the framework's fallback was read: mautrix-go's
/// `bridgev2/matrix/mxmain/example-config.yaml` writes
/// `command_prefix: '$<<or .DefaultCommandPrefix (printf "!%s" .NetworkID)>>'`, and
/// `bridgev2/networkinterface.go` says of `DefaultCommandPrefix` "defaults to NetworkID if
/// unset. Must include the ! prefix". So a connector without one takes `!` and its
/// `NetworkID`: `signal` (mautrix-signal `pkg/connector/connector.go`, `GetName`), `slack`
/// (mautrix-slack, the same file), `twitter` (mautrix-twitter, the same file: the
/// `NetworkID` stays `twitter` when the display name is `X`) and `linkedin` (mautrix-linkedin,
/// the same file). A bridge whose config an
/// administrator wrote by hand may say otherwise; an instance this server renders keeps the
/// default.
fn command_prefix(entry: &Entry) -> Option<&'static str> {
    match entry.id {
        "mautrix-whatsapp" => Some("!wa"),
        "mautrix-telegram" => Some("!tg"),
        "mautrix-gmessages" => Some("!gm"),
        "mautrix-gvoice" => Some("!gv"),
        "mautrix-meta" => Some("!fb"),
        "mautrix-discord" => Some("!discord"),
        "mautrix-bluesky" => Some("!bsky"),
        "mautrix-signal" => Some("!signal"),
        "mautrix-slack" => Some("!slack"),
        "mautrix-twitter" => Some("!twitter"),
        "mautrix-linkedin" => Some("!linkedin"),
        _ => None,
    }
}

/// Where a connector's config names the bridge among a person's linked or paired devices on
/// the network's side. Read from each bridge's `pkg/connector/config.go` and
/// `example-config.yaml` on 2026-10-02. The networks without one, so that nobody looks again:
/// Messenger and Instagram, Discord, Slack, X, LinkedIn, Bluesky and Google Voice sign in with
/// cookies, tokens or an app password and have no device the network lists under a name the
/// bridge chooses (Discord's identify payload claims a fixed browser; nothing in its config
/// names it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceSetting {
    /// mautrix-whatsapp: `network.os_name` (free text, the device's name) and
    /// `network.browser_name` (a whatsmeow `PlatformType`, which picks the icon and whether the
    /// name is shown at all; [`WHATSAPP_PLATFORM`]). Both go to WhatsApp in the pairing payload
    /// (whatsmeow `store/clientpayload.go`, `getRegistrationPayload`) and never afterwards.
    WhatsApp,
    /// mautrix-signal: `network.device_name`, sent encrypted when the bridge is linked as a
    /// secondary device (`signalmeow/provisioning.go`).
    Signal,
    /// mautrix-telegram: `network.device_info.device_model`, Telegram's "device model" in its
    /// Devices list, sent on every connection.
    Telegram,
    /// mautrix-gmessages: `network.device_meta.os`, "the name that shows up in the paired
    /// devices list". `browser` and `type` pick the icon and the session slot and keep the
    /// bridge's defaults.
    GoogleMessages,
}

fn device_setting(entry: &Entry) -> Option<DeviceSetting> {
    match entry.id {
        "mautrix-whatsapp" => Some(DeviceSetting::WhatsApp),
        "mautrix-signal" => Some(DeviceSetting::Signal),
        "mautrix-telegram" => Some(DeviceSetting::Telegram),
        "mautrix-gmessages" => Some(DeviceSetting::GoogleMessages),
        _ => None,
    }
}

/// The whatsmeow `PlatformType` a WhatsApp bridge links as (`network.browser_name`). The
/// phone's Linked devices list is driven by it: `UNKNOWN` (the bridge's default) shows "Other
/// device" and hides the device name; the browsers (`CHROME`, `FIREFOX`, `SAFARI`, `EDGE`,
/// `OPERA`) show "Google Chrome (name)" and that browser's icon, which is a lie for a bridge;
/// `DESKTOP` shows the name by itself with the desktop app's icon
/// (whatsmeow discussion #469 and issue #89, 2026-10-02). The QR code's client type does not
/// change with it (`pair.go`, `getQRClientType`): it stays "other web client".
pub const WHATSAPP_PLATFORM: &str = "DESKTOP";

/// What a bridge is called among a person's devices on the network's side (WhatsApp's and
/// Signal's Linked devices, Telegram's Devices, Google Messages' paired devices), the one
/// pattern for every type that has such a setting ([`DeviceSetting`]):
/// `Myelin WhatsApp bridge for alice (example.org)` for a person's instance and
/// `Myelin WhatsApp bridge (example.org)` for a shared one. The owner's localpart rather than
/// the whole Matrix ID, so that the phone's list, which cuts long names short, shows whose it is
/// before the server. `None` for a type without such a setting.
#[must_use]
pub fn device_name(type_id: &str, server_name: &str, owner: Option<&str>) -> Option<String> {
    let entry = entry(type_id)?;
    device_setting(entry)?;
    Some(device_name_for(entry.name, server_name, owner))
}

fn device_name_for(network: &str, server_name: &str, owner: Option<&str>) -> String {
    match owner.map(localpart_of) {
        Some(localpart) => format!("Myelin {network} bridge for {localpart} ({server_name})"),
        None => format!("Myelin {network} bridge ({server_name})"),
    }
}

/// A YAML double-quoted scalar.
fn yaml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `network:` section of a mautrix config that names the bridge on the network's side, or
/// `None` for a connector without such a setting. Every other key of the section keeps the
/// bridge's default: its config upgrader copies them key by key.
fn network_section(entry: &Entry, device_name: &str) -> Option<String> {
    let name = yaml_string(device_name);
    let mut out = String::from("network:\n");
    match device_setting(entry)? {
        DeviceSetting::WhatsApp => {
            out.push_str(
                "  # What the phone's Linked devices list calls this bridge, and what it\n",
            );
            out.push_str(
                "  # shows it as: DESKTOP shows the name by itself; the bridge's default,\n",
            );
            out.push_str(
                "  # unknown, shows \"Other device\" and hides the name. WhatsApp learns\n",
            );
            out.push_str(
                "  # both when the bridge is linked, so a link made before a change keeps\n",
            );
            out.push_str("  # its old name until the bridge is linked again.\n");
            out.push_str(&format!("  os_name: {name}\n"));
            out.push_str(&format!("  browser_name: {WHATSAPP_PLATFORM}\n"));
        }
        DeviceSetting::Signal => {
            out.push_str(
                "  # What the phone's Linked devices list calls this bridge. Signal learns it\n",
            );
            out.push_str(
                "  # when the bridge is linked, so a link made before a change keeps its old\n",
            );
            out.push_str("  # name until the bridge is linked again.\n");
            out.push_str(&format!("  device_name: {name}\n"));
        }
        DeviceSetting::Telegram => {
            out.push_str(
                "  # What Telegram's Devices list calls this bridge (its \"device model\").\n",
            );
            out.push_str("  device_info:\n");
            out.push_str(&format!("    device_model: {name}\n"));
        }
        DeviceSetting::GoogleMessages => {
            out.push_str(
                "  # What the phone's paired devices list calls this bridge; the icon and\n",
            );
            out.push_str("  # the session slot (browser, type) keep the bridge's defaults.\n");
            out.push_str("  device_meta:\n");
            out.push_str(&format!("    os: {name}\n"));
        }
    }
    Some(out)
}

/// Whether an instance runs from what a render writes alone. Not matrix-appservice-irc or
/// hookshot, whose configs name networks and services only an operator knows; not Telegram,
/// whose config needs the operator's own API ID and hash.
fn deployable(entry: &Entry) -> bool {
    not_deployable_reason(entry).is_none()
}

/// Why this server cannot deploy the type itself, in the words the interface shows; `None`
/// when it can.
fn not_deployable_reason(entry: &Entry) -> Option<String> {
    match entry.runtime {
        Runtime::AppserviceIrc | Runtime::Hookshot => Some(format!(
            "{}'s config names the networks or services it connects, which only an administrator can write, so it runs elsewhere.",
            entry.name
        )),
        Runtime::Mautrix | Runtime::Heisenbridge => entry
            .needs
            .iter()
            .find(|(key, _, required)| *required && *key == "api_id")
            .map(|(_, description, _)| {
                format!(
                    "{} needs settings only an administrator can provide ({description}), so it runs elsewhere.",
                    entry.name
                )
            }),
    }
}

fn entry(id: &str) -> Option<&'static Entry> {
    CATALOGUE.iter().find(|e| e.id == id)
}

fn regex_escape_server(server_name: &str) -> String {
    server_name.replace('.', "\\.")
}

fn namespaces_for(entry: &Entry, server_name: &str) -> Value {
    let server = regex_escape_server(server_name);
    json!({
        "users": [
            {"regex": format!("@{}.*:{server}", entry.ghost_prefix), "exclusive": true},
            {"regex": format!("@{}:{server}", entry.bot), "exclusive": true},
        ],
        "aliases": [
            {"regex": format!("#{}.*:{server}", entry.ghost_prefix), "exclusive": true},
        ],
        "rooms": [],
    })
}

fn bridge_type(entry: &Entry, server_name: &str) -> BridgeType {
    BridgeType {
        id: entry.id.to_owned(),
        name: entry.name.to_owned(),
        description: entry.description.to_owned(),
        category: entry.category.id().to_owned(),
        upstream_project: entry.upstream_project.to_owned(),
        docs_url: entry.docs_url.to_owned(),
        image: entry.image.to_owned(),
        port: entry.port,
        default_namespaces: namespaces_for(entry, server_name),
        config_keys: entry
            .needs
            .iter()
            .map(|(key, description, required)| BridgeTypeConfigKey {
                key: (*key).to_owned(),
                description: (*description).to_owned(),
                required: *required,
            })
            .collect(),
        supports_double_puppeting: entry.double_puppeting,
        command_prefix: command_prefix(entry).map(str::to_owned),
        required_features: entry.features.iter().map(|f| (*f).to_owned()).collect(),
        renders_config: entry.runtime == Runtime::Mautrix,
        mode: mode(entry).to_owned(),
        deployable: deployable(entry),
        not_deployable_reason: not_deployable_reason(entry),
        provisioning_api: provisioning_of(entry).0.as_str().to_owned(),
        provisioning_note: Some(provisioning_of(entry).1.to_owned()),
        sign_in: BridgeTypeSignIn {
            steps: entry
                .sign_in
                .iter()
                .map(|s| s.replace("{server}", server_name))
                .collect(),
            notes: entry.sign_in_notes.map(str::to_owned),
        },
    }
}

/// Every bridge type, for `bridge_types.list`. Namespaces are written for `server_name`.
#[must_use]
pub fn list(server_name: &str) -> Vec<BridgeType> {
    CATALOGUE
        .iter()
        .map(|entry| bridge_type(entry, server_name))
        .collect()
}

/// One bridge type, for `bridge_types.get`.
#[must_use]
pub fn get(id: &str, server_name: &str) -> Option<BridgeType> {
    entry(id).map(|entry| bridge_type(entry, server_name))
}

/// 64 hex characters from the OS's randomness: the shape every bridge's own generator writes.
fn token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn string(values: &Map<String, Value>, key: &str) -> Option<String> {
    values
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn flag(values: &Map<String, Value>, key: &str, default: bool) -> bool {
    values.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// What the wizard sends, in its own field names, taken as far as they are given; everything
/// else comes from the catalogue entry. A namespace pattern the wizard wrote for a placeholder
/// domain is written for this server's instead.
struct Choices {
    id: String,
    sender_localpart: String,
    user_namespace: String,
    alias_namespace: String,
    room_namespace: Option<String>,
    kubernetes: bool,
    k8s_namespace: String,
    image_tag: String,
    /// How the bridge reaches this server: a Compose service name, a Kubernetes service, or
    /// `host.docker.internal` for a server running outside Docker.
    homeserver_address: String,
    /// How this server reaches the bridge, when the wizard says: the registration's `url`.
    /// Absent, it is the bridge's Compose service name or Kubernetes service on its port.
    bridge_address: Option<String>,
    /// The Matrix user the bridge takes commands from as its administrator, if the wizard named
    /// one -- the operator signing in, by default.
    admin_user: Option<String>,
    double_puppeting: bool,
    encryption: bool,
    rate_limit_exempt: bool,
}

fn choices(entry: &Entry, server_name: &str, values: &Map<String, Value>) -> Choices {
    let server = regex_escape_server(server_name);
    let for_this_server = |pattern: Option<String>, default: String| -> String {
        match pattern {
            Some(p) => p
                .replace(":example\\.org", &format!(":{server}"))
                .replace(":example.org", &format!(":{server}")),
            None => default,
        }
    };
    let id = string(values, "id").unwrap_or_else(|| entry.id.to_owned());
    let kubernetes = values.get("deployment").and_then(Value::as_str) == Some("kubernetes");
    let k8s_namespace = string(values, "namespace").unwrap_or_else(|| "bridges".to_owned());
    Choices {
        sender_localpart: string(values, "senderLocalpart").unwrap_or_else(|| entry.bot.to_owned()),
        user_namespace: for_this_server(
            string(values, "userNamespace"),
            format!("@{}.*:{server}", entry.ghost_prefix),
        ),
        alias_namespace: for_this_server(
            string(values, "aliasNamespace"),
            format!("#{}.*:{server}", entry.ghost_prefix),
        ),
        room_namespace: string(values, "roomNamespace"),
        homeserver_address: string(values, "homeserverAddress").unwrap_or_else(|| {
            if kubernetes {
                format!("http://myelin.{k8s_namespace}.svc:8008")
            } else {
                "http://myelin:8008".to_owned()
            }
        }),
        bridge_address: string(values, "bridgeAddress"),
        admin_user: string(values, "adminUser").filter(|u| u.starts_with('@') && u.contains(':')),
        kubernetes,
        k8s_namespace,
        image_tag: string(values, "imageTag").unwrap_or_else(|| "latest".to_owned()),
        double_puppeting: flag(values, "doublePuppeting", entry.double_puppeting),
        encryption: flag(values, "encryption", entry.runtime == Runtime::Mautrix),
        rate_limit_exempt: flag(values, "rateLimitExempt", true),
        id,
    }
}

/// Renders `values` (the wizard's state) for bridge type `type_id` on `server_name`. `None` for
/// a type not in the catalogue.
#[must_use]
pub fn render(type_id: &str, server_name: &str, values: &Value) -> Option<BridgeTypeRenderResult> {
    let entry = entry(type_id)?;
    let empty = Map::new();
    let values = values.as_object().unwrap_or(&empty);
    let c = choices(entry, server_name, values);
    let image = entry.image.rsplit_once(':').map_or_else(
        || entry.image.to_owned(),
        |(name, _)| format!("{name}:{}", c.image_tag),
    );
    let server = regex_escape_server(server_name);
    let url = match &c.bridge_address {
        Some(address) => address.clone(),
        None if c.kubernetes => {
            format!("http://{}.{}.svc:{}", c.id, c.k8s_namespace, entry.port)
        }
        // Compose puts the bridge on the same network as the server, under its service name.
        None => format!("http://{}:{}", c.id, entry.port),
    };

    let as_token = token();
    let hs_token = token();
    let provisioning_secret = provisioning_of(entry).0.reports_logins().then(token);
    let mut registration = Map::new();
    registration.insert("id".into(), json!(c.id));
    registration.insert("url".into(), json!(url));
    registration.insert("as_token".into(), json!(as_token));
    registration.insert("hs_token".into(), json!(hs_token));
    registration.insert("sender_localpart".into(), json!(c.sender_localpart));
    registration.insert("rate_limited".into(), json!(!c.rate_limit_exempt));
    let mut users = vec![
        json!({"regex": c.user_namespace, "exclusive": true}),
        json!({"regex": format!("@{}:{server}", c.sender_localpart), "exclusive": true}),
    ];
    let double_puppeting = c.double_puppeting && entry.double_puppeting;
    if double_puppeting {
        // Double puppeting is the bridge sending as real users; that needs a non-exclusive
        // claim on everyone, which reserves nothing. The bridge's own token then signs in as
        // any local user (`m.login.application_service`), which is what the rendered config
        // tells it to do.
        users.push(json!({"regex": format!("@.*:{server}"), "exclusive": false}));
    }
    let mut rooms = Vec::new();
    if let Some(room) = &c.room_namespace {
        rooms.push(json!({"regex": room, "exclusive": true}));
    }
    registration.insert(
        "namespaces".into(),
        json!({
            "users": users,
            "aliases": [{"regex": c.alias_namespace, "exclusive": true}],
            "rooms": rooms,
        }),
    );
    for feature in entry.features {
        // Encryption in a mautrix bridge is what needs the device-list and to-device features;
        // ephemeral events it wants regardless.
        let wanted = match *feature {
            MSC3202 | MSC4190 => c.encryption,
            _ => true,
        };
        if wanted {
            registration.insert((*feature).to_owned(), json!(true));
        }
    }
    registration.insert(BRIDGE_TYPE_KEY.to_owned(), json!(entry.id));
    if let Some(secret) = &provisioning_secret {
        registration.insert(PROVISIONING_SECRET_KEY.to_owned(), json!(secret));
    }
    let registration = Value::Object(registration);
    let registration_yaml = serde_yaml_ng::to_string(&registration).unwrap_or_default();

    let config_yaml = (entry.runtime == Runtime::Mautrix).then(|| {
        let mut permissions = vec![("*".to_owned(), "relay"), (server_name.to_owned(), "user")];
        if let Some(admin) = &c.admin_user {
            permissions.push((admin.clone(), "admin"));
        }
        mautrix_config(&MautrixParams {
            name: entry.name,
            homeserver_address: &c.homeserver_address,
            server_name,
            url: &url,
            port: entry.port,
            id: &c.id,
            bot: &c.sender_localpart,
            ghost_prefix: entry.ghost_prefix,
            as_token: &as_token,
            hs_token: &hs_token,
            permissions,
            backfill: true,
            double_puppeting,
            encryption: c.encryption,
            provisioning_secret: provisioning_secret.as_deref(),
            pickle_key: None,
            // The wizard's bridge is one for the server, run by hand: the shared name.
            network: network_section(entry, &device_name_for(entry.name, server_name, None)),
        })
    });
    let compose_yaml = compose(entry, &c, &image, server_name);
    let bridge_resource_yaml = bridge_resource(entry, &c, &image);

    Some(BridgeTypeRenderResult {
        registration,
        registration_yaml,
        config_yaml,
        compose_yaml,
        bridge_resource_yaml,
    })
}

/// What a mautrix `config.yaml` says, for either a render or an instance.
struct MautrixParams<'a> {
    name: &'a str,
    homeserver_address: &'a str,
    server_name: &'a str,
    url: &'a str,
    port: u16,
    id: &'a str,
    bot: &'a str,
    /// The ghosts' localpart prefix, `whatsapp_` or `whatsapp_alice_`.
    ghost_prefix: &'a str,
    as_token: &'a str,
    hs_token: &'a str,
    /// `(who, level)`, in order.
    permissions: Vec<(String, &'static str)>,
    backfill: bool,
    double_puppeting: bool,
    encryption: bool,
    /// `provisioning.shared_secret`, when this server is to read the bridge's sign-ins.
    provisioning_secret: Option<&'a str>,
    /// `encryption.pickle_key`: the key the bridge's crypto store is pickled with. Written when
    /// whoever keeps the instance minted one, so that the rendered config is complete and can
    /// replace the bridge's own copy on a later start without the bridge generating a new key
    /// (which would make its crypto store unreadable). `None` leaves it to the bridge.
    pickle_key: Option<&'a str>,
    /// The `network:` section naming the bridge on the network's side
    /// ([`network_section`]), when the connector has such a setting.
    network: Option<String>,
}

/// The `config.yaml` a mautrix `bridgev2` bridge reads, with everything that ties it to this
/// server filled in: where the server is, what it is called, the bridge's own address and
/// port (the registration's `url`), the same two tokens, a database, who may use it, backfill,
/// double puppeting through its own token, and encryption in the appservice mode the
/// registration asked for. Every key not written here keeps the bridge's default: the bridge
/// completes and rewrites this file on its first start (its config upgrader), which is also
/// what its own `-e` generator does with an empty one. The database is SQLite in the bridge's
/// own volume, which is all one bridge process needs (RFC 0017 section 4.4).
fn mautrix_config(p: &MautrixParams<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {} bridge configuration, written by Myelin for the registration it created.\n",
        p.name
    ));
    out.push_str("# Every setting not listed here keeps the bridge's own default; the bridge\n");
    out.push_str("# fills the rest in and rewrites this file on its first start.\n");
    out.push_str("homeserver:\n");
    out.push_str(&format!("  address: {}\n", p.homeserver_address));
    out.push_str(&format!("  domain: {}\n", p.server_name));
    out.push_str("appservice:\n");
    out.push_str(&format!("  address: {}\n", p.url));
    out.push_str("  hostname: 0.0.0.0\n");
    out.push_str(&format!("  port: {}\n", p.port));
    out.push_str(&format!("  id: {}\n", p.id));
    out.push_str("  bot:\n");
    out.push_str(&format!("    username: {}\n", p.bot));
    out.push_str(&format!(
        "  username_template: \"{}{{{{.}}}}\"\n",
        p.ghost_prefix
    ));
    out.push_str("  ephemeral_events: true\n");
    out.push_str(&format!("  as_token: {}\n", p.as_token));
    out.push_str(&format!("  hs_token: {}\n", p.hs_token));
    out.push_str("database:\n");
    out.push_str("  # SQLite in the bridge's own volume. For Postgres: type: postgres and a\n");
    out.push_str("  # postgres:// URI to a database of its own.\n");
    out.push_str("  type: sqlite3-fk-wal\n");
    out.push_str(&format!(
        "  uri: file:/data/{}.db?_txlock=immediate\n",
        p.id
    ));
    out.push_str("bridge:\n");
    out.push_str("  # Who may use the bridge: relay (only through someone else's login),\n");
    out.push_str("  # user (sign in and chat), admin (bridge commands as well).\n");
    out.push_str("  permissions:\n");
    for (who, level) in &p.permissions {
        out.push_str(&format!("    \"{who}\": {level}\n"));
    }
    out.push_str("backfill:\n");
    out.push_str(&format!("  enabled: {}\n", p.backfill));
    if let Some(network) = &p.network {
        out.push_str(network);
    }
    if p.double_puppeting {
        out.push_str(
            "# The bridge sends as your users through its own token: the registration's\n",
        );
        out.push_str("# non-exclusive claim on them is what allows it.\n");
        out.push_str("double_puppet:\n");
        out.push_str("  secrets:\n");
        out.push_str(&format!(
            "    \"{}\": \"as_token:{}\"\n",
            p.server_name, p.as_token
        ));
    }
    out.push_str("encryption:\n");
    if p.encryption {
        out.push_str(
            "  # End-to-bridge encryption, with the device lists and to-device messages\n",
        );
        out.push_str(
            "  # arriving in appservice transactions (MSC3202, MSC4203) and the bridge's\n",
        );
        out.push_str("  # device made without a login (MSC4190), as the registration asks for.\n");
        out.push_str("  allow: true\n");
        out.push_str("  default: true\n");
        out.push_str("  appservice: true\n");
        out.push_str("  msc4190: true\n");
    } else {
        out.push_str("  allow: false\n");
    }
    if let Some(key) = p.pickle_key {
        out.push_str(
            "  # The key its crypto store is pickled with, minted with its tokens so that\n",
        );
        out.push_str(
            "  # this file can be written again without the bridge generating a new one.\n",
        );
        out.push_str(&format!("  pickle_key: {key}\n"));
    }
    if let Some(secret) = p.provisioning_secret {
        out.push_str(
            "# The server asks /_matrix/provision/v3/whoami with this secret, to show its\n",
        );
        out.push_str("# administrators who has signed in to the bridge and as what.\n");
        out.push_str("provisioning:\n");
        out.push_str(&format!("  shared_secret: {secret}\n"));
    }
    out
}

fn compose(entry: &Entry, c: &Choices, image: &str, server_name: &str) -> String {
    let mut out = String::new();
    out.push_str("# Run beside the server, on the same Compose network, so that `url` in the\n");
    out.push_str("# registration resolves to this service by name and the bridge reaches the\n");
    out.push_str(&format!("# server as `{}`.\n", c.homeserver_address));
    out.push_str("services:\n");
    out.push_str(&format!("  {}:\n", c.id));
    out.push_str(&format!("    image: {image}\n"));
    out.push_str("    restart: unless-stopped\n");
    match entry.runtime {
        Runtime::Mautrix => {
            out.push_str(&format!("    volumes:\n      - ./{}:/data\n", c.id));
            out.push_str(&format!(
                "    # Save config.yaml and registration.yaml from the wizard into ./{}/ first;\n",
                c.id
            ));
            out.push_str("    # with both present the bridge starts straight away and completes\n");
            out.push_str("    # config.yaml with its own defaults.\n");
        }
        Runtime::Heisenbridge => {
            out.push_str(&format!("    volumes:\n      - ./{}:/data\n", c.id));
            out.push_str("    # Save the registration YAML as ./");
            out.push_str(&c.id);
            out.push_str("/registration.yaml; the owner is who the bridge answers to.\n");
            out.push_str(&format!(
                "    command: [\"-c\", \"/data/registration.yaml\", \"-l\", \"0.0.0.0\", \"-p\", \"{}\", \"-o\", \"{}\", \"{}\"]\n",
                entry.port,
                c.admin_user
                    .clone()
                    .unwrap_or_else(|| format!("@OWNER:{server_name}")),
                c.homeserver_address
            ));
        }
        Runtime::AppserviceIrc => {
            out.push_str(&format!("    volumes:\n      - ./{}:/data\n", c.id));
            out.push_str("    # Needs ./");
            out.push_str(&c.id);
            out.push_str("/config.yaml (the project's config.sample.yaml, with homeserver.url\n");
            out.push_str(&format!(
                "    # {} and the IRC network) and the registration as\n    # ./",
                c.homeserver_address
            ));
            out.push_str(&c.id);
            out.push_str("/registration.yaml.\n");
            out.push_str(&format!(
                "    command: [\"-c\", \"/data/config.yaml\", \"-f\", \"/data/registration.yaml\", \"-p\", \"{}\"]\n",
                entry.port
            ));
        }
        Runtime::Hookshot => {
            out.push_str(&format!("    volumes:\n      - ./{}:/data\n", c.id));
            out.push_str("    # Needs ./");
            out.push_str(&c.id);
            out.push_str(&format!(
                "/config.yml (the project's sample, with bridge.url {})\n",
                c.homeserver_address
            ));
            out.push_str("    # and the registration as ./");
            out.push_str(&c.id);
            out.push_str("/registration.yml. Webhooks arrive on 9000.\n");
            out.push_str("    ports:\n      - \"9000:9000\"\n");
        }
    }
    out
}

fn bridge_resource(entry: &Entry, c: &Choices, image: &str) -> String {
    let (repository, tag) = image.rsplit_once(':').unwrap_or((image, "latest"));
    format!(
        "# For a cluster running the Myelin operator (RFC 0017). The Secret named in\n\
         # filesSecret holds config.yaml and registration.yaml, from the files above.\n\
         apiVersion: hs.matrix.org/v1alpha1\n\
         kind: Bridge\n\
         metadata:\n  name: {id}\n  namespace: {ns}\n\
         spec:\n  bridgeType: {kind}\n  appserviceId: {id}\n  image:\n    repository: {repository}\n    tag: {tag}\n  port: {port}\n  filesSecret: {id}-files\n",
        id = c.id,
        ns = c.k8s_namespace,
        kind = entry.id,
        port = entry.port,
    )
}

// ---------------------------------------------------------------------------------------------
// Instances (RFC 0017): one person's bridge, with namespaces, permissions and double puppeting
// that reach that person and nobody else.
// ---------------------------------------------------------------------------------------------

/// The registration key that marks an appservice as an instance of an offering, holding its
/// owner's Matrix ID (or `_` for a shared type's instance).
pub const BRIDGE_INSTANCE_KEY: &str = "io.myelin.bridge_instance";

/// A localpart, encoded so that it can sit between two `_`s in another localpart without one
/// user's namespace ever containing another's (RFC 0017 section 3): lowercase letters, digits,
/// `.`, `-` and `/` are kept; every other byte, `_` and `=` included, becomes `=` and two hex
/// digits.
#[must_use]
pub fn encode_localpart(localpart: &str) -> String {
    let mut out = String::with_capacity(localpart.len());
    for b in localpart.bytes() {
        match b {
            b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'/' => out.push(b as char),
            other => out.push_str(&format!("={other:02x}")),
        }
    }
    out
}

/// Everything an instance is rendered from. The tokens and addresses are the instance's own,
/// decided by whoever keeps it (the manager): a render here mints nothing.
#[derive(Clone, Copy)]
pub struct InstanceSpec<'a> {
    pub type_id: &'a str,
    pub server_name: &'a str,
    pub appservice_id: &'a str,
    /// The owner's Matrix ID; `None` for a shared type's instance.
    pub owner: Option<&'a str>,
    pub as_token: &'a str,
    pub hs_token: &'a str,
    /// Where this server reaches the bridge: the registration's `url`.
    pub url: &'a str,
    /// Where the bridge reaches this server.
    pub homeserver_address: &'a str,
    pub image_tag: &'a str,
    pub encryption: Option<bool>,
    pub double_puppeting: Option<bool>,
    pub backfill: Option<bool>,
    /// The secret its provisioning API takes, written into its config and kept in its
    /// registration ([`PROVISIONING_SECRET_KEY`]); `None` for an instance made before the
    /// manager minted one, or a type whose provisioning API this server does not read.
    pub provisioning_secret: Option<&'a str>,
    /// The key the bridge's crypto store is pickled with (`encryption.pickle_key`), minted and
    /// kept by whoever keeps the instance so that its config can be rendered again, complete;
    /// `None` for an instance made before the manager minted one, or a type without one.
    pub pickle_key: Option<&'a str>,
}

/// An instance, rendered: its registration, the files its process reads from `/data`, and how
/// to run it.
#[derive(Clone)]
pub struct InstanceRender {
    pub registration: Value,
    pub registration_yaml: String,
    pub config_yaml: Option<String>,
    /// File name in `/data` -> contents: what a deployment's Secret holds.
    pub files: std::collections::BTreeMap<String, String>,
    pub compose_yaml: String,
    pub image_repository: String,
    pub image_tag: String,
    pub port: u16,
    /// Arguments to the image's own entrypoint; empty for a bridge that reads `/data` itself.
    pub args: Vec<String>,
    /// The instance's bot's localpart.
    pub bot_localpart: String,
    /// The ghosts' localpart prefix.
    pub ghost_prefix: String,
    /// The catalogue's sign-in steps, with `{server}` filled in and `{bot}` left for the caller.
    pub sign_in: Vec<String>,
    /// What the network's own device list calls this bridge ([`device_name`]), written into the
    /// config; `None` for a type without such a setting.
    pub device_name: Option<String>,
}

impl std::fmt::Debug for InstanceRender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstanceRender(<redacted>)")
    }
}

/// The localparts an instance's bot and ghosts use: the catalogue's with the owner's encoded
/// localpart added (`whatsappbot_alice`, `whatsapp_alice_`), or the catalogue's own for a shared
/// instance. `None` for a type not in the catalogue.
#[must_use]
pub fn instance_names(type_id: &str, owner: Option<&str>) -> Option<(String, String)> {
    let entry = entry(type_id)?;
    Some(match owner.map(localpart_of) {
        Some(localpart) => {
            let key = encode_localpart(localpart);
            (
                format!("{}_{key}", entry.bot),
                format!("{}{key}_", entry.ghost_prefix),
            )
        }
        None => (entry.bot.to_owned(), entry.ghost_prefix.to_owned()),
    })
}

/// The front door's localpart for a type: the catalogue's bot name, which users already know.
#[must_use]
pub fn front_door_localpart(type_id: &str) -> Option<&'static str> {
    entry(type_id).map(|e| e.bot)
}

/// The catalogue's display name for a type.
#[must_use]
pub fn display_name(type_id: &str) -> Option<&'static str> {
    entry(type_id).map(|e| e.name)
}

fn localpart_of(user_id: &str) -> &str {
    user_id
        .strip_prefix('@')
        .and_then(|rest| rest.split_once(':'))
        .map_or(user_id, |(localpart, _)| localpart)
}

/// Renders one instance. `None` for a type not in the catalogue.
#[must_use]
pub fn render_instance(spec: &InstanceSpec<'_>) -> Option<InstanceRender> {
    let entry = entry(spec.type_id)?;
    let server = regex_escape_server(spec.server_name);
    let (bot, ghost_prefix) = instance_names(spec.type_id, spec.owner)?;
    let encryption = spec.encryption.unwrap_or(entry.runtime == Runtime::Mautrix);
    let double_puppeting =
        entry.double_puppeting && spec.owner.is_some() && spec.double_puppeting.unwrap_or(true);
    let backfill = spec.backfill.unwrap_or(true);

    let mut registration = Map::new();
    registration.insert("id".into(), json!(spec.appservice_id));
    registration.insert("url".into(), json!(spec.url));
    registration.insert("as_token".into(), json!(spec.as_token));
    registration.insert("hs_token".into(), json!(spec.hs_token));
    registration.insert("sender_localpart".into(), json!(bot));
    registration.insert("rate_limited".into(), json!(false));
    let mut users = vec![
        json!({"regex": format!("@{}.*:{server}", regex_escape(&ghost_prefix)), "exclusive": true}),
        json!({"regex": format!("@{}:{server}", regex_escape(&bot)), "exclusive": true}),
    ];
    if double_puppeting && let Some(owner) = spec.owner {
        // Double puppeting as the owner, and only the owner: a compromised instance can act as
        // one person, not as everyone on the server.
        users.push(json!({"regex": regex_escape(owner), "exclusive": false}));
    }
    registration.insert(
        "namespaces".into(),
        json!({
            "users": users,
            "aliases": [{"regex": format!("#{}.*:{server}", regex_escape(&ghost_prefix)), "exclusive": true}],
            "rooms": [],
        }),
    );
    for feature in entry.features {
        let wanted = match *feature {
            MSC3202 | MSC4190 => encryption,
            _ => true,
        };
        if wanted {
            registration.insert((*feature).to_owned(), json!(true));
        }
    }
    registration.insert(BRIDGE_TYPE_KEY.to_owned(), json!(entry.id));
    registration.insert(
        BRIDGE_INSTANCE_KEY.to_owned(),
        json!(
            spec.owner
                .unwrap_or(crate::bridge_offerings::SHARED_INSTANCE)
        ),
    );
    let provisioning_secret = spec
        .provisioning_secret
        .filter(|_| provisioning_of(entry).0.reports_logins());
    if let Some(secret) = provisioning_secret {
        registration.insert(PROVISIONING_SECRET_KEY.to_owned(), json!(secret));
    }
    let registration = Value::Object(registration);
    let registration_yaml = serde_yaml_ng::to_string(&registration).unwrap_or_default();

    let mut files = std::collections::BTreeMap::new();
    let mut args = Vec::new();
    let device_name = device_name(spec.type_id, spec.server_name, spec.owner);
    let config_yaml = match entry.runtime {
        Runtime::Mautrix => {
            let permissions = match spec.owner {
                Some(owner) => vec![(owner.to_owned(), "admin")],
                None => vec![
                    ("*".to_owned(), "relay"),
                    (spec.server_name.to_owned(), "user"),
                ],
            };
            let config = mautrix_config(&MautrixParams {
                name: entry.name,
                homeserver_address: spec.homeserver_address,
                server_name: spec.server_name,
                url: spec.url,
                port: entry.port,
                id: spec.appservice_id,
                bot: &bot,
                ghost_prefix: &ghost_prefix,
                as_token: spec.as_token,
                hs_token: spec.hs_token,
                permissions,
                backfill,
                double_puppeting,
                encryption,
                provisioning_secret,
                pickle_key: spec.pickle_key.filter(|k| !k.trim().is_empty()),
                network: device_name
                    .as_deref()
                    .and_then(|name| network_section(entry, name)),
            });
            files.insert("config.yaml".to_owned(), config.clone());
            Some(config)
        }
        Runtime::Heisenbridge => {
            args = vec![
                "-c".into(),
                "/data/registration.yaml".into(),
                "-l".into(),
                "0.0.0.0".into(),
                "-p".into(),
                entry.port.to_string(),
            ];
            // A shared instance names no owner: heisenbridge gives the bridge to the first local
            // user who talks to it, and that person allows the others.
            if let Some(owner) = spec.owner {
                args.push("-o".into());
                args.push(owner.to_owned());
            }
            args.push(spec.homeserver_address.to_owned());
            None
        }
        Runtime::AppserviceIrc | Runtime::Hookshot => None,
    };
    files.insert("registration.yaml".to_owned(), registration_yaml.clone());

    let repository = entry
        .image
        .rsplit_once(':')
        .map_or(entry.image, |(name, _)| name)
        .to_owned();
    let image_tag = if spec.image_tag.trim().is_empty() {
        "latest".to_owned()
    } else {
        spec.image_tag.trim().to_owned()
    };
    let mut compose_yaml = String::new();
    compose_yaml
        .push_str("# Save config.yaml and registration.yaml into the directory mounted at\n");
    compose_yaml
        .push_str("# /data first. The bridge has to reach this server at the address in its\n");
    compose_yaml.push_str(&format!(
        "# config ({}), and this server has to reach it at {}.\n",
        spec.homeserver_address, spec.url
    ));
    compose_yaml.push_str("services:\n");
    compose_yaml.push_str(&format!("  {}:\n", spec.appservice_id));
    compose_yaml.push_str(&format!("    image: {repository}:{image_tag}\n"));
    compose_yaml.push_str("    restart: unless-stopped\n");
    compose_yaml.push_str(&format!(
        "    volumes:\n      - ./{}:/data\n",
        spec.appservice_id
    ));
    compose_yaml.push_str(&format!(
        "    ports:\n      - \"{port}:{port}\"\n",
        port = entry.port
    ));
    if !args.is_empty() {
        let quoted: Vec<String> = args.iter().map(|a| format!("\"{a}\"")).collect();
        compose_yaml.push_str(&format!("    command: [{}]\n", quoted.join(", ")));
    }

    Some(InstanceRender {
        registration,
        registration_yaml,
        config_yaml,
        files,
        compose_yaml,
        image_repository: repository,
        image_tag,
        port: entry.port,
        args,
        bot_localpart: bot,
        ghost_prefix,
        sign_in: entry
            .sign_in
            .iter()
            .map(|s| s.replace("{server}", spec.server_name))
            .collect(),
        device_name,
    })
}

/// Escapes the regex metacharacters that can appear in a Matrix ID.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(
            ch,
            '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_writes_namespaces_for_the_server_it_is_asked_about() {
        let whatsapp = get("mautrix-whatsapp", "chat.example.net").unwrap();
        assert_eq!(
            whatsapp.default_namespaces["users"][0]["regex"],
            "@whatsapp_.*:chat\\.example\\.net"
        );
        assert_eq!(whatsapp.default_namespaces["users"][0]["exclusive"], true);
        assert_eq!(whatsapp.port, 29318);
        assert!(whatsapp.supports_double_puppeting);
        assert!(whatsapp.required_features.contains(&MSC3202.to_owned()));
        assert_eq!(list("x.org").len(), CATALOGUE.len());
        assert!(get("mautrix-fax", "x.org").is_none());
        // Every id is unique and every image names its project.
        let mut ids: Vec<&str> = CATALOGUE.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), CATALOGUE.len());
    }

    /// Every mautrix `bridgev2` bridge in the catalogue has a command prefix, the connector's
    /// own or mautrix-go's fallback of `!` and the network id (see [`command_prefix`]); the
    /// other runtimes have none claimed.
    #[test]
    fn every_mautrix_bridge_has_its_command_prefix() {
        let prefixes: Vec<(String, Option<String>)> = list("chat.example.net")
            .into_iter()
            .map(|t| (t.id, t.command_prefix))
            .collect();
        for (id, prefix) in &prefixes {
            let mautrix = entry(id).unwrap().runtime == Runtime::Mautrix;
            assert_eq!(prefix.is_some(), mautrix, "{id}: {prefix:?}");
            if let Some(prefix) = prefix {
                assert!(prefix.starts_with('!'), "{id}: {prefix}");
            }
        }
        let of = |id: &str| {
            prefixes
                .iter()
                .find(|(i, _)| i == id)
                .and_then(|(_, p)| p.clone())
        };
        assert_eq!(of("mautrix-whatsapp").as_deref(), Some("!wa"));
        assert_eq!(of("mautrix-signal").as_deref(), Some("!signal"));
        assert_eq!(of("mautrix-slack").as_deref(), Some("!slack"));
        assert_eq!(of("mautrix-twitter").as_deref(), Some("!twitter"));
        assert_eq!(of("heisenbridge"), None);
    }

    /// The wizard's guidance is complete for every entry: a description, a category the
    /// interface groups by, a documentation link, and at least one sign-in step. `{bot}` is
    /// left for the interface, which knows the bot's actual name; `{server}` is not.
    #[test]
    fn every_entry_says_what_it_is_and_how_to_sign_in() {
        for t in list("chat.example.net") {
            assert!(!t.description.is_empty(), "{}", t.id);
            assert!(
                ["messaging", "social", "irc", "integrations"].contains(&t.category.as_str()),
                "{}: {}",
                t.id,
                t.category
            );
            assert!(t.docs_url.starts_with("https://"), "{}", t.id);
            assert!(!t.sign_in.steps.is_empty(), "{}", t.id);
            for step in &t.sign_in.steps {
                assert!(!step.contains("{server}"), "{}: {step}", t.id);
            }
            assert_eq!(t.renders_config, t.id.starts_with("mautrix-"), "{}", t.id);
        }
        let irc = get("matrix-appservice-irc", "chat.example.net").unwrap();
        assert!(irc.sign_in.steps[0].contains("#irc_#channel:chat.example.net"));
        assert!(irc.sign_in.steps[1].contains("{bot}"));
    }

    /// The registration a render produces is one this server accepts: proper namespace objects,
    /// both tokens, the features the bridge needs -- not the mock's list of bare patterns.
    #[test]
    fn a_render_is_a_registration_this_server_would_accept() {
        let result = render(
            "mautrix-telegram",
            "chat.example.net",
            &json!({
                "id": "tg",
                "senderLocalpart": "tgbot",
                "userNamespace": "@tg_.*:example.org",
                "aliasNamespace": "#tg_.*:example.org",
                "deployment": "self-managed",
                "encryption": true,
                "doublePuppeting": true,
                "rateLimitExempt": true,
            }),
        )
        .unwrap();
        let r = &result.registration;
        assert_eq!(r["id"], "tg");
        assert_eq!(r["url"], "http://tg:29317");
        assert_eq!(r["sender_localpart"], "tgbot");
        assert_eq!(r["rate_limited"], false);
        assert_eq!(r["as_token"].as_str().unwrap().len(), 64);
        assert_ne!(r["as_token"], r["hs_token"]);
        // The wizard's placeholder domain became this server's.
        assert_eq!(
            r["namespaces"]["users"][0]["regex"],
            "@tg_.*:chat\\.example\\.net"
        );
        assert_eq!(r["namespaces"]["users"][0]["exclusive"], true);
        assert_eq!(
            r["namespaces"]["users"][1]["regex"],
            "@tgbot:chat\\.example\\.net"
        );
        // Double puppeting: everyone, non-exclusively.
        assert_eq!(
            r["namespaces"]["users"][2]["regex"],
            "@.*:chat\\.example\\.net"
        );
        assert_eq!(r["namespaces"]["users"][2]["exclusive"], false);
        assert_eq!(r[MSC2409], true);
        assert_eq!(r[MSC3202], true);
        assert_eq!(r[MSC4190], true);
        // It remembers where it came from.
        assert_eq!(r[BRIDGE_TYPE_KEY], "mautrix-telegram");
        assert!(result.registration_yaml.contains("sender_localpart: tgbot"));
        assert!(
            result
                .registration_yaml
                .contains("io.myelin.bridge_type: mautrix-telegram")
        );
        assert!(
            result
                .compose_yaml
                .contains("dock.mau.dev/mautrix/telegram:latest")
        );
        assert!(result.bridge_resource_yaml.contains("kind: Bridge"));

        // Two renders are two registrations: the tokens are minted each time.
        let again = render("mautrix-telegram", "chat.example.net", &json!({})).unwrap();
        assert_ne!(again.registration["as_token"], r["as_token"]);
        // And with nothing chosen, the catalogue's defaults.
        assert_eq!(again.registration["id"], "mautrix-telegram");
        assert_eq!(again.registration["sender_localpart"], "telegrambot");
    }

    /// The mautrix config is the bridge's own shape, agrees with the registration on every
    /// value they share, and parses as YAML.
    #[test]
    fn a_mautrix_render_writes_a_config_the_bridge_reads() {
        let result = render(
            "mautrix-whatsapp",
            "chat.example.net",
            &json!({
                "id": "wa",
                "adminUser": "@ops:chat.example.net",
                "homeserverAddress": "http://host.docker.internal:8008",
                "doublePuppeting": true,
                "encryption": true,
            }),
        )
        .unwrap();
        let r = &result.registration;
        let config_text = result
            .config_yaml
            .as_deref()
            .expect("mautrix renders a config");
        let config: Value = serde_yaml_ng::from_str(config_text).expect("valid YAML");
        assert_eq!(
            config["homeserver"]["address"],
            "http://host.docker.internal:8008"
        );
        assert_eq!(config["homeserver"]["domain"], "chat.example.net");
        assert_eq!(config["appservice"]["address"], r["url"]);
        assert_eq!(config["appservice"]["port"], 29318);
        assert_eq!(config["appservice"]["id"], "wa");
        assert_eq!(config["appservice"]["as_token"], r["as_token"]);
        assert_eq!(config["appservice"]["hs_token"], r["hs_token"]);
        assert_eq!(config["appservice"]["bot"]["username"], "whatsappbot");
        assert_eq!(config["appservice"]["username_template"], "whatsapp_{{.}}");
        assert_eq!(config["database"]["type"], "sqlite3-fk-wal");
        assert_eq!(
            config["database"]["uri"],
            "file:/data/wa.db?_txlock=immediate"
        );
        assert_eq!(config["bridge"]["permissions"]["*"], "relay");
        assert_eq!(config["bridge"]["permissions"]["chat.example.net"], "user");
        assert_eq!(
            config["bridge"]["permissions"]["@ops:chat.example.net"],
            "admin"
        );
        assert_eq!(
            config["double_puppet"]["secrets"]["chat.example.net"],
            format!("as_token:{}", r["as_token"].as_str().unwrap())
        );
        assert_eq!(config["encryption"]["allow"], true);
        assert_eq!(config["encryption"]["appservice"], true);
        assert_eq!(config["encryption"]["msc4190"], true);
        assert_eq!(config["backfill"]["enabled"], true);
        // A provisioning secret of its own, the same in the config and the registration, so the
        // server can ask the bridge who has signed in; and a different one each render.
        let secret = r[PROVISIONING_SECRET_KEY]
            .as_str()
            .expect("a minted secret");
        assert_eq!(secret.len(), 64);
        assert_eq!(config["provisioning"]["shared_secret"], secret);
        assert_ne!(secret, r["as_token"].as_str().unwrap());
        // The registration a bridge reads keeps it as one more unrecognised key.
        assert!(result.registration_yaml.contains(PROVISIONING_SECRET_KEY));
        // The Compose comment tells the operator what to do with the files.
        assert!(
            result
                .compose_yaml
                .contains("Save config.yaml and registration.yaml")
        );

        // Told where this server reaches the bridge (a published port on this host, say), the
        // registration and the config both say so.
        let published = render(
            "mautrix-whatsapp",
            "x.org",
            &json!({"bridgeAddress": "http://127.0.0.1:29318"}),
        )
        .unwrap();
        assert_eq!(published.registration["url"], "http://127.0.0.1:29318");
        let config: Value =
            serde_yaml_ng::from_str(published.config_yaml.as_deref().unwrap()).unwrap();
        assert_eq!(config["appservice"]["address"], "http://127.0.0.1:29318");
        assert_eq!(config["appservice"]["port"], 29318);

        // An admin that is not a Matrix ID is not written; encryption off means off.
        let plain = render(
            "mautrix-whatsapp",
            "x.org",
            &json!({"adminUser": "ops", "encryption": false, "doublePuppeting": false}),
        )
        .unwrap();
        let config: Value = serde_yaml_ng::from_str(plain.config_yaml.as_deref().unwrap()).unwrap();
        assert!(config["bridge"]["permissions"].as_object().unwrap().len() == 2);
        assert_eq!(config["encryption"]["allow"], false);
        assert!(config.get("double_puppet").is_none());
        assert_eq!(config["homeserver"]["address"], "http://myelin:8008");
    }

    #[test]
    fn encryption_off_drops_the_device_features_and_kubernetes_addresses_the_service() {
        let result = render(
            "mautrix-signal",
            "x.org",
            &json!({"encryption": false, "deployment": "kubernetes", "namespace": "chat", "imageTag": "v0.8.0"}),
        )
        .unwrap();
        let r = &result.registration;
        assert_eq!(r["url"], "http://mautrix-signal.chat.svc:29328");
        assert_eq!(r[MSC2409], true);
        assert!(r.get(MSC3202).is_none());
        assert!(r.get(MSC4190).is_none());
        assert!(
            result
                .compose_yaml
                .contains("dock.mau.dev/mautrix/signal:v0.8.0")
        );
        assert!(result.bridge_resource_yaml.contains("namespace: chat"));
        assert!(
            result
                .bridge_resource_yaml
                .contains("apiVersion: hs.matrix.org/v1alpha1")
        );
        let config: Value =
            serde_yaml_ng::from_str(result.config_yaml.as_deref().unwrap()).unwrap();
        assert_eq!(
            config["homeserver"]["address"],
            "http://myelin.chat.svc:8008"
        );
        assert_eq!(config["database"]["type"], "sqlite3-fk-wal");
    }

    #[test]
    fn heisenbridge_renders_the_way_it_is_actually_run() {
        let result = render(
            "heisenbridge",
            "test.local",
            &json!({"id": "irc", "adminUser": "@me:test.local"}),
        )
        .unwrap();
        let r = &result.registration;
        assert_eq!(r["url"], "http://irc:9898");
        assert_eq!(r["sender_localpart"], "heisenbridge");
        assert_eq!(r["namespaces"]["users"][0]["regex"], "@irc_.*:test\\.local");
        assert!(r.get(MSC3202).is_none(), "{r}");
        assert!(
            result.config_yaml.is_none(),
            "heisenbridge has no config file"
        );
        assert!(result.compose_yaml.contains("hif1/heisenbridge:latest"));
        assert!(result.compose_yaml.contains("\"-o\", \"@me:test.local\""));
        assert!(result.compose_yaml.contains("\"http://myelin:8008\""));
    }

    #[test]
    fn localparts_are_encoded_so_no_namespace_contains_another() {
        assert_eq!(encode_localpart("alice"), "alice");
        assert_eq!(encode_localpart("alice_x"), "alice=5fx");
        assert_eq!(encode_localpart("Bob=1"), "=42ob=3d1");
        assert_eq!(encode_localpart("a.b-c/d"), "a.b-c/d");
        let (bot, ghosts) = instance_names("mautrix-whatsapp", Some("@alice:x.org")).unwrap();
        assert_eq!(bot, "whatsappbot_alice");
        assert_eq!(ghosts, "whatsapp_alice_");
        // alice's ghost pattern cannot match alice_x's or alice.x's ghosts.
        let (_, other) = instance_names("mautrix-whatsapp", Some("@alice_x:x.org")).unwrap();
        assert!(!other.starts_with(&ghosts));
        let (_, dotted) = instance_names("mautrix-whatsapp", Some("@alice.x:x.org")).unwrap();
        assert!(!dotted.starts_with(&ghosts));
    }

    #[test]
    fn an_instance_reaches_its_owner_and_nobody_else() {
        let r = render_instance(&InstanceSpec {
            type_id: "mautrix-whatsapp",
            server_name: "chat.example.net",
            appservice_id: "whatsapp-alice",
            owner: Some("@alice:chat.example.net"),
            as_token: "a".repeat(64).as_str(),
            hs_token: "h".repeat(64).as_str(),
            url: "http://bridge-1234abcd.myelin.svc:29318",
            homeserver_address: "http://myelin-hs.myelin.svc:8008",
            image_tag: "v0.12.0",
            encryption: None,
            double_puppeting: None,
            backfill: None,
            provisioning_secret: Some("p".repeat(64).as_str()),
            pickle_key: Some("k".repeat(64).as_str()),
        })
        .unwrap();
        let reg = &r.registration;
        assert_eq!(reg["id"], "whatsapp-alice");
        assert_eq!(reg["sender_localpart"], "whatsappbot_alice");
        let users = reg["namespaces"]["users"].as_array().unwrap();
        assert_eq!(users[0]["regex"], "@whatsapp_alice_.*:chat\\.example\\.net");
        assert_eq!(users[1]["regex"], "@whatsappbot_alice:chat\\.example\\.net");
        assert_eq!(users[2]["regex"], "@alice:chat\\.example\\.net");
        assert_eq!(users[2]["exclusive"], false);
        assert_eq!(users.len(), 3, "no claim on anyone else");
        assert_eq!(reg[BRIDGE_INSTANCE_KEY], "@alice:chat.example.net");
        assert_eq!(reg[MSC4190], true);
        let config: Value = serde_yaml_ng::from_str(r.config_yaml.as_deref().unwrap()).unwrap();
        assert_eq!(config["appservice"]["bot"]["username"], "whatsappbot_alice");
        assert_eq!(
            config["appservice"]["username_template"],
            "whatsapp_alice_{{.}}"
        );
        assert_eq!(
            config["appservice"]["address"],
            "http://bridge-1234abcd.myelin.svc:29318"
        );
        assert_eq!(
            config["homeserver"]["address"],
            "http://myelin-hs.myelin.svc:8008"
        );
        // The provisioning API's secret is the same in the config the bridge reads and the
        // registration the server keeps, which is how the server asks who has signed in.
        assert_eq!(config["provisioning"]["shared_secret"], "p".repeat(64));
        assert_eq!(reg[PROVISIONING_SECRET_KEY], "p".repeat(64));
        let permissions = config["bridge"]["permissions"].as_object().unwrap();
        assert_eq!(permissions.len(), 1);
        assert_eq!(permissions["@alice:chat.example.net"], "admin");
        assert_eq!(
            config["database"]["uri"],
            "file:/data/whatsapp-alice.db?_txlock=immediate"
        );
        assert_eq!(r.files.len(), 2);
        assert!(r.files.contains_key("config.yaml") && r.files.contains_key("registration.yaml"));
        assert_eq!(r.image_repository, "dock.mau.dev/mautrix/whatsapp");
        assert_eq!(r.image_tag, "v0.12.0");
        assert!(r.args.is_empty());
        assert!(
            r.compose_yaml
                .contains("dock.mau.dev/mautrix/whatsapp:v0.12.0")
        );

        let heisen = render_instance(&InstanceSpec {
            type_id: "heisenbridge",
            owner: None,
            server_name: "x.org",
            appservice_id: "heisenbridge",
            url: "http://b:9898",
            homeserver_address: "http://hs:8008",
            image_tag: "",
            ..InstanceSpec {
                type_id: "",
                server_name: "",
                appservice_id: "",
                owner: None,
                as_token: "a",
                hs_token: "b",
                url: "",
                homeserver_address: "",
                image_tag: "",
                encryption: None,
                double_puppeting: None,
                backfill: None,
                provisioning_secret: Some("ignored"),
                pickle_key: None,
            }
        })
        .unwrap();
        assert!(heisen.config_yaml.is_none());
        assert!(
            heisen.registration.get(PROVISIONING_SECRET_KEY).is_none(),
            "heisenbridge has no provisioning API to give a secret to"
        );
        assert_eq!(heisen.args.last().unwrap(), "http://hs:8008");
        // Shared: nobody is named as the owner; the first local user to talk to it claims it.
        assert!(!heisen.args.contains(&"-o".to_owned()), "{:?}", heisen.args);
        assert_eq!(heisen.bot_localpart, "heisenbridge");
        assert_eq!(heisen.ghost_prefix, "irc_");
        assert_eq!(
            heisen.registration[BRIDGE_INSTANCE_KEY],
            crate::bridge_offerings::SHARED_INSTANCE
        );
        assert_eq!(heisen.image_tag, "latest");
    }

    fn instance_config(type_id: &str, owner: Option<&str>, pickle_key: Option<&str>) -> Value {
        let r = render_instance(&InstanceSpec {
            type_id,
            server_name: "chat.example.net",
            appservice_id: "id",
            owner,
            as_token: "a",
            hs_token: "h",
            url: "http://b:1",
            homeserver_address: "http://hs:8008",
            image_tag: "",
            encryption: None,
            double_puppeting: None,
            backfill: None,
            provisioning_secret: None,
            pickle_key,
        })
        .unwrap();
        serde_yaml_ng::from_str(r.config_yaml.as_deref().expect("a mautrix config")).unwrap()
    }

    #[test]
    fn an_instance_is_named_on_the_networks_side_after_its_owner_and_this_server() {
        // A person's WhatsApp bridge: the phone's Linked devices list gets the name, and the
        // platform that makes WhatsApp show it (DESKTOP; unknown shows "Other device").
        let config = instance_config(
            "mautrix-whatsapp",
            Some("@brandon:chat.example.net"),
            Some("k"),
        );
        assert_eq!(
            config["network"]["os_name"],
            "Myelin WhatsApp bridge for brandon (chat.example.net)"
        );
        assert_eq!(config["network"]["browser_name"], "DESKTOP");
        assert_eq!(WHATSAPP_PLATFORM, "DESKTOP");
        assert_eq!(
            device_name(
                "mautrix-whatsapp",
                "chat.example.net",
                Some("@brandon:chat.example.net")
            )
            .as_deref(),
            Some("Myelin WhatsApp bridge for brandon (chat.example.net)")
        );
        // The render says what it wrote, for the admin API and the interface.
        let r = render_instance(&InstanceSpec {
            type_id: "mautrix-whatsapp",
            server_name: "chat.example.net",
            appservice_id: "whatsapp-brandon",
            owner: Some("@brandon:chat.example.net"),
            as_token: "a",
            hs_token: "h",
            url: "http://b:1",
            homeserver_address: "http://hs:8008",
            image_tag: "",
            encryption: None,
            double_puppeting: None,
            backfill: None,
            provisioning_secret: None,
            pickle_key: None,
        })
        .unwrap();
        assert_eq!(
            r.device_name.as_deref(),
            Some("Myelin WhatsApp bridge for brandon (chat.example.net)")
        );

        // A shared instance names no person.
        let shared = instance_config("mautrix-whatsapp", None, None);
        assert_eq!(
            shared["network"]["os_name"],
            "Myelin WhatsApp bridge (chat.example.net)"
        );
        assert_eq!(
            device_name("mautrix-whatsapp", "chat.example.net", None).as_deref(),
            Some("Myelin WhatsApp bridge (chat.example.net)")
        );

        // The same name, in each connector's own key.
        let signal = instance_config("mautrix-signal", Some("@alice:chat.example.net"), None);
        assert_eq!(
            signal["network"]["device_name"],
            "Myelin Signal bridge for alice (chat.example.net)"
        );
        let telegram = instance_config("mautrix-telegram", Some("@alice:chat.example.net"), None);
        assert_eq!(
            telegram["network"]["device_info"]["device_model"],
            "Myelin Telegram bridge for alice (chat.example.net)"
        );
        assert!(
            telegram["network"]["device_info"]
                .get("app_version")
                .is_none(),
            "the other device_info keys keep the bridge's defaults"
        );
        let gmessages = instance_config("mautrix-gmessages", Some("@alice:chat.example.net"), None);
        assert_eq!(
            gmessages["network"]["device_meta"]["os"],
            "Myelin Google Messages bridge for alice (chat.example.net)"
        );
        assert!(gmessages["network"]["device_meta"].get("browser").is_none());

        // A network with no such setting gets no `network:` section and no name.
        for type_id in [
            "mautrix-meta",
            "mautrix-discord",
            "mautrix-slack",
            "mautrix-twitter",
            "mautrix-bluesky",
            "mautrix-linkedin",
            "mautrix-gvoice",
        ] {
            let config = instance_config(type_id, Some("@alice:chat.example.net"), None);
            assert!(config.get("network").is_none(), "{type_id}");
            assert_eq!(
                device_name(type_id, "chat.example.net", Some("@alice:chat.example.net")),
                None,
                "{type_id}"
            );
        }
        assert_eq!(device_name("heisenbridge", "x.org", None), None);

        // A name with a quote in it is still one YAML string.
        assert_eq!(yaml_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        let section = network_section(entry("mautrix-signal").unwrap(), "a \"b\" \\c").unwrap();
        let odd: Value = serde_yaml_ng::from_str(&section).unwrap();
        assert_eq!(odd["network"]["device_name"], "a \"b\" \\c");

        // The pickle key the manager minted is written, so the file is complete; without one
        // the bridge generates its own.
        assert_eq!(config["encryption"]["pickle_key"], "k");
        assert!(shared["encryption"].get("pickle_key").is_none());
        let blank = instance_config("mautrix-whatsapp", None, Some("  "));
        assert!(blank["encryption"].get("pickle_key").is_none());

        // The wizard's bridge, run by hand for the server, carries the shared name.
        let wizard = render("mautrix-whatsapp", "chat.example.net", &json!({})).unwrap();
        let config: Value =
            serde_yaml_ng::from_str(wizard.config_yaml.as_deref().unwrap()).unwrap();
        assert_eq!(
            config["network"]["os_name"],
            "Myelin WhatsApp bridge (chat.example.net)"
        );
        assert_eq!(config["network"]["browser_name"], "DESKTOP");
    }

    #[test]
    fn every_type_says_what_provisioning_api_it_has_and_only_mautrix_reports_logins() {
        for t in list("x.org") {
            let expected = match t.id.as_str() {
                id if id.starts_with("mautrix-") => "mautrix_v3",
                "heisenbridge" => "none",
                "matrix-appservice-irc" => "irc_v1",
                "matrix-hookshot" => "hookshot_v1",
                other => panic!("{other} has no expectation"),
            };
            assert_eq!(t.provisioning_api, expected, "{}", t.id);
            assert!(t.provisioning_note.is_some_and(|n| !n.is_empty()));
            let (api, _) = provisioning(&t.id).unwrap();
            assert_eq!(api.reports_logins(), expected == "mautrix_v3");
        }
        assert!(provisioning("nope").is_none());
        // A type with no API mints no secret it could never use.
        let heisen = render("heisenbridge", "x.org", &json!({})).unwrap();
        assert!(heisen.registration.get(PROVISIONING_SECRET_KEY).is_none());
        let irc = render("matrix-appservice-irc", "x.org", &json!({})).unwrap();
        assert!(irc.registration.get(PROVISIONING_SECRET_KEY).is_none());
    }

    #[test]
    fn every_type_says_whether_it_is_per_user_and_deployable() {
        let whatsapp = get("mautrix-whatsapp", "x.org").unwrap();
        assert_eq!(
            (whatsapp.mode.as_str(), whatsapp.deployable),
            ("per_user", true)
        );
        let telegram = get("mautrix-telegram", "x.org").unwrap();
        assert!(!telegram.deployable, "needs the operator's own API ID");
        let hookshot = get("matrix-hookshot", "x.org").unwrap();
        assert_eq!(
            (hookshot.mode.as_str(), hookshot.deployable),
            ("shared", false)
        );
        let heisenbridge = get("heisenbridge", "x.org").unwrap();
        assert_eq!(
            (heisenbridge.mode.as_str(), heisenbridge.deployable),
            ("shared", true)
        );
        assert!(whatsapp.not_deployable_reason.is_none());
        assert!(
            telegram
                .not_deployable_reason
                .as_deref()
                .unwrap_or_default()
                .contains("API ID"),
            "{:?}",
            telegram.not_deployable_reason
        );
        for t in list("x.org") {
            assert_eq!(t.deployable, t.not_deployable_reason.is_none(), "{}", t.id);
        }
    }
}
