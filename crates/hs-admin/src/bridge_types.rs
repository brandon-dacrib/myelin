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
        required_features: entry.features.iter().map(|f| (*f).to_owned()).collect(),
        renders_config: entry.runtime == Runtime::Mautrix,
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
    let registration = Value::Object(registration);
    let registration_yaml = serde_yaml_ng::to_string(&registration).unwrap_or_default();

    let config_yaml = (entry.runtime == Runtime::Mautrix).then(|| {
        mautrix_config(
            entry,
            &c,
            server_name,
            &url,
            &as_token,
            &hs_token,
            double_puppeting,
        )
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

/// The `config.yaml` a mautrix `bridgev2` bridge reads, with everything that ties it to this
/// server filled in: where the server is, what it is called, the bridge's own address and
/// port (the registration's `url`), the same two tokens, a database, who may use it, backfill,
/// double puppeting through its own token, and encryption in the appservice mode the
/// registration asked for. Every key not written here keeps the bridge's default: the bridge
/// completes and rewrites this file on its first start (its config upgrader), which is also
/// what its own `-e` generator does with an empty one.
fn mautrix_config(
    entry: &Entry,
    c: &Choices,
    server_name: &str,
    url: &str,
    as_token: &str,
    hs_token: &str,
    double_puppeting: bool,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {} bridge configuration, written by Myelin for the registration it created.\n",
        entry.name
    ));
    out.push_str("# Every setting not listed here keeps the bridge's own default; the bridge\n");
    out.push_str("# fills the rest in and rewrites this file on its first start.\n");
    out.push_str("homeserver:\n");
    out.push_str(&format!("  address: {}\n", c.homeserver_address));
    out.push_str(&format!("  domain: {server_name}\n"));
    out.push_str("appservice:\n");
    out.push_str(&format!("  address: {url}\n"));
    out.push_str("  hostname: 0.0.0.0\n");
    out.push_str(&format!("  port: {}\n", entry.port));
    out.push_str(&format!("  id: {}\n", c.id));
    out.push_str("  bot:\n");
    out.push_str(&format!("    username: {}\n", c.sender_localpart));
    out.push_str(&format!(
        "  username_template: \"{}{{{{.}}}}\"\n",
        entry.ghost_prefix
    ));
    out.push_str("  ephemeral_events: true\n");
    out.push_str(&format!("  as_token: {as_token}\n"));
    out.push_str(&format!("  hs_token: {hs_token}\n"));
    out.push_str("database:\n");
    if c.kubernetes {
        out.push_str(
            "  # The operator provisions this database; until it does, fill in the URI.\n",
        );
        out.push_str("  type: postgres\n");
        out.push_str(&format!(
            "  uri: postgres://{id}:PASSWORD@{id}-db.{ns}.svc/{id}?sslmode=disable\n",
            id = c.id,
            ns = c.k8s_namespace
        ));
    } else {
        out.push_str("  # SQLite in the bridge's own volume. For Postgres: type: postgres and a\n");
        out.push_str("  # postgres:// URI to a database of its own.\n");
        out.push_str("  type: sqlite3-fk-wal\n");
        out.push_str(&format!(
            "  uri: file:/data/{}.db?_txlock=immediate\n",
            c.id
        ));
    }
    out.push_str("bridge:\n");
    out.push_str("  # Who may use the bridge: relay (only through someone else's login),\n");
    out.push_str("  # user (sign in and chat), admin (bridge commands as well).\n");
    out.push_str("  permissions:\n");
    out.push_str("    \"*\": relay\n");
    out.push_str(&format!("    \"{server_name}\": user\n"));
    if let Some(admin) = &c.admin_user {
        out.push_str(&format!("    \"{admin}\": admin\n"));
    }
    out.push_str("backfill:\n");
    out.push_str("  enabled: true\n");
    if double_puppeting {
        out.push_str(
            "# The bridge sends as your users through its own token: the registration's\n",
        );
        out.push_str("# non-exclusive claim on every local user is what allows it.\n");
        out.push_str("double_puppet:\n");
        out.push_str("  secrets:\n");
        out.push_str(&format!("    \"{server_name}\": \"as_token:{as_token}\"\n"));
    }
    out.push_str("encryption:\n");
    if c.encryption {
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
    format!(
        "# For the Bridge operator, which is not written yet: what it will take to run this.\n\
         apiVersion: bridges.myelin.dev/v1alpha1\n\
         kind: Bridge\n\
         metadata:\n  name: {id}\n  namespace: {ns}\n\
         spec:\n  type: {kind}\n  image: {image}\n  port: {port}\n  registrationSecret: {id}-registration\n",
        id = c.id,
        ns = c.k8s_namespace,
        kind = entry.id,
        port = entry.port,
    )
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
        let config: Value =
            serde_yaml_ng::from_str(result.config_yaml.as_deref().unwrap()).unwrap();
        assert_eq!(
            config["homeserver"]["address"],
            "http://myelin.chat.svc:8008"
        );
        assert_eq!(config["database"]["type"], "postgres");
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
}
