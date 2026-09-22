//! The bridge catalogue behind `GET /bridge-types` and `POST /bridge-types/{type}/render`: what
//! the interface's "Add bridge" wizard offers, and what it renders a choice into.
//!
//! Every entry is a bridge somebody actually runs, with the image its project publishes, the
//! port its own config generator writes into `appservice.address`, the namespaces it expects,
//! and the appservice features it needs. The render turns a wizard's choices into the four
//! things an operator needs: a registration this server will accept (`appservices.create` takes
//! it as it is), the same as the YAML file the bridge reads, a Compose service for running it
//! beside the server, and a `Bridge` resource for the Kubernetes operator that is still to be
//! written. Nothing here is created: the wizard shows the result and then creates.
//!
//! The tokens are minted here, once per render, and the registration carries them; a render
//! is what a fresh registration file is.

use rand::RngCore;
use serde_json::{Map, Value, json};

use crate::model::{BridgeType, BridgeTypeConfigKey, BridgeTypeRenderResult};

/// How a bridge is run, for the artifacts that say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runtime {
    /// A mautrix bridge: one container, a config file it generates itself, a database of its
    /// own (SQLite unless told otherwise).
    Mautrix,
    /// heisenbridge: a single Python process configured on its command line, whose state lives
    /// in its bot's account data on this server.
    Heisenbridge,
    /// matrix-appservice-irc: Node, a YAML config, its own database.
    AppserviceIrc,
    /// matrix-hookshot: Node, a YAML config, listens for webhooks as well as for this server.
    Hookshot,
}

struct Entry {
    id: &'static str,
    name: &'static str,
    upstream_project: &'static str,
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
}

const MSC2409: &str = "de.sorunome.msc2409.push_ephemeral";
const MSC3202: &str = "org.matrix.msc3202";
const MSC4190: &str = "io.element.msc4190";

const MAUTRIX_FEATURES: &[&str] = &[MSC2409, MSC3202, MSC4190];

macro_rules! mautrix {
    ($id:literal, $name:literal, $net:literal, $port:literal, $prefix:literal, $bot:literal, [$(($k:literal, $d:literal, $r:literal)),* $(,)?]) => {
        Entry {
            id: $id,
            name: $name,
            upstream_project: concat!("mautrix/", $net),
            image: concat!("dock.mau.dev/mautrix/", $net, ":latest"),
            port: $port,
            ghost_prefix: $prefix,
            bot: $bot,
            needs: &[$(($k, $d, $r)),*],
            double_puppeting: true,
            features: MAUTRIX_FEATURES,
            runtime: Runtime::Mautrix,
        }
    };
}

const CATALOGUE: &[Entry] = &[
    mautrix!(
        "mautrix-whatsapp",
        "WhatsApp",
        "whatsapp",
        29318,
        "whatsapp_",
        "whatsappbot",
        [(
            "phone",
            "A phone with WhatsApp, to link the bridge to by scanning a code",
            true
        ),]
    ),
    mautrix!(
        "mautrix-telegram",
        "Telegram",
        "telegram",
        29317,
        "telegram_",
        "telegrambot",
        [
            ("api_id", "A Telegram API ID from my.telegram.org", true),
            ("api_hash", "The matching API hash", true),
        ]
    ),
    mautrix!(
        "mautrix-signal",
        "Signal",
        "signal",
        29328,
        "signal_",
        "signalbot",
        [(
            "phone",
            "A phone with Signal, to link the bridge to as a secondary device",
            true
        ),]
    ),
    mautrix!(
        "mautrix-discord",
        "Discord",
        "discord",
        29334,
        "discord_",
        "discordbot",
        [(
            "account",
            "A Discord account to sign in with from the bridge",
            true
        ),]
    ),
    mautrix!(
        "mautrix-slack",
        "Slack",
        "slack",
        29335,
        "slack_",
        "slackbot",
        [(
            "account",
            "A Slack account to sign in with from the bridge",
            true
        ),]
    ),
    mautrix!(
        "mautrix-gmessages",
        "Google Messages",
        "gmessages",
        29336,
        "gmessages_",
        "gmessagesbot",
        [(
            "phone",
            "An Android phone with Google Messages, to pair with",
            true
        ),]
    ),
    mautrix!(
        "mautrix-meta",
        "Messenger and Instagram",
        "meta",
        29319,
        "meta_",
        "metabot",
        [(
            "account",
            "A Facebook or Instagram account to sign in with from the bridge",
            true
        ),]
    ),
    mautrix!(
        "mautrix-twitter",
        "X (Twitter)",
        "twitter",
        29327,
        "twitter_",
        "twitterbot",
        [(
            "account",
            "An X account to sign in with from the bridge",
            true
        ),]
    ),
    mautrix!(
        "mautrix-linkedin",
        "LinkedIn",
        "linkedin",
        29325,
        "linkedin_",
        "linkedinbot",
        [(
            "account",
            "A LinkedIn account to sign in with from the bridge",
            true
        ),]
    ),
    mautrix!(
        "mautrix-gvoice",
        "Google Voice",
        "gvoice",
        29338,
        "gvoice_",
        "gvoicebot",
        [(
            "account",
            "A Google account with a Google Voice number",
            true
        ),]
    ),
    mautrix!(
        "mautrix-bluesky",
        "Bluesky",
        "bluesky",
        29340,
        "bluesky_",
        "blueskybot",
        [("account", "A Bluesky account and an app password", true),]
    ),
    Entry {
        id: "heisenbridge",
        name: "IRC (heisenbridge)",
        upstream_project: "hifi/heisenbridge",
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
    },
    Entry {
        id: "matrix-appservice-irc",
        name: "IRC (matrix-appservice-irc)",
        upstream_project: "matrix-org/matrix-appservice-irc",
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
    },
    Entry {
        id: "matrix-hookshot",
        name: "Hookshot (webhooks, GitHub, GitLab, Jira, RSS)",
        upstream_project: "matrix-org/matrix-hookshot",
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
        upstream_project: entry.upstream_project.to_owned(),
        image: entry.image.to_owned(),
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
        kubernetes: values.get("deployment").and_then(Value::as_str) == Some("kubernetes"),
        k8s_namespace: string(values, "namespace").unwrap_or_else(|| "bridges".to_owned()),
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
    let url = if c.kubernetes {
        format!("http://{}.{}.svc:{}", c.id, c.k8s_namespace, entry.port)
    } else {
        // Compose puts the bridge on the same network as the server, under its service name.
        format!("http://{}:{}", c.id, entry.port)
    };

    let mut registration = Map::new();
    registration.insert("id".into(), json!(c.id));
    registration.insert("url".into(), json!(url));
    registration.insert("as_token".into(), json!(token()));
    registration.insert("hs_token".into(), json!(token()));
    registration.insert("sender_localpart".into(), json!(c.sender_localpart));
    registration.insert("rate_limited".into(), json!(!c.rate_limit_exempt));
    let mut users = vec![
        json!({"regex": c.user_namespace, "exclusive": true}),
        json!({"regex": format!("@{}:{server}", c.sender_localpart), "exclusive": true}),
    ];
    if c.double_puppeting && entry.double_puppeting {
        // Double puppeting is the bridge sending as real users; that needs a non-exclusive
        // claim on everyone, which reserves nothing.
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
    let registration = Value::Object(registration);
    let registration_yaml = serde_yaml_ng::to_string(&registration).unwrap_or_default();

    let compose_yaml = compose(entry, &c, &image, server_name);
    let bridge_resource_yaml = bridge_resource(entry, &c, &image);

    Some(BridgeTypeRenderResult {
        registration,
        registration_yaml,
        compose_yaml,
        bridge_resource_yaml,
    })
}

fn compose(entry: &Entry, c: &Choices, image: &str, server_name: &str) -> String {
    let mut out = String::new();
    out.push_str("# Run beside the server, on the same Compose network, so that `url` in the\n");
    out.push_str("# registration resolves to this service by name and the bridge reaches the\n");
    out.push_str("# server as `http://myelin:8008`.\n");
    out.push_str("services:\n");
    out.push_str(&format!("  {}:\n", c.id));
    out.push_str(&format!("    image: {image}\n"));
    out.push_str("    restart: unless-stopped\n");
    match entry.runtime {
        Runtime::Mautrix => {
            out.push_str(&format!("    volumes:\n      - ./{}:/data\n", c.id));
            out.push_str(&format!(
                "    # First run: `docker compose run --rm {} && edit ./{}/config.yaml` --\n",
                c.id, c.id
            ));
            out.push_str(
                "    # set homeserver.address to http://myelin:8008 and homeserver.domain to\n",
            );
            out.push_str(&format!(
                "    # {server_name}, put the two tokens from the registration under appservice,\n"
            ));
            out.push_str(&format!(
                "    # and leave appservice.port at {} -- the registration's url expects it.\n",
                entry.port
            ));
        }
        Runtime::Heisenbridge => {
            out.push_str(&format!("    volumes:\n      - ./{}:/data\n", c.id));
            out.push_str("    # Save the registration YAML as ./");
            out.push_str(&c.id);
            out.push_str("/registration.yaml; the owner is who the bridge answers to.\n");
            out.push_str(&format!(
                "    command: [\"-c\", \"/data/registration.yaml\", \"-l\", \"0.0.0.0\", \"-p\", \"{}\", \"-o\", \"@OWNER:{server_name}\", \"http://myelin:8008\"]\n",
                entry.port
            ));
        }
        Runtime::AppserviceIrc => {
            out.push_str(&format!("    volumes:\n      - ./{}:/data\n", c.id));
            out.push_str("    # Needs ./");
            out.push_str(&c.id);
            out.push_str("/config.yaml (the project's config.sample.yaml, with homeserver.url\n");
            out.push_str(
                "    # http://myelin:8008 and the IRC network) and the registration as\n    # ./",
            );
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
            out.push_str(
                "/config.yml (the project's sample, with bridge.url http://myelin:8008)\n",
            );
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
        assert!(result.registration_yaml.contains("sender_localpart: tgbot"));
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
    }

    #[test]
    fn heisenbridge_renders_the_way_it_is_actually_run() {
        let result = render("heisenbridge", "test.local", &json!({"id": "irc"})).unwrap();
        let r = &result.registration;
        assert_eq!(r["url"], "http://irc:9898");
        assert_eq!(r["sender_localpart"], "heisenbridge");
        assert_eq!(r["namespaces"]["users"][0]["regex"], "@irc_.*:test\\.local");
        assert!(r.get(MSC3202).is_none(), "{r}");
        assert!(result.compose_yaml.contains("hif1/heisenbridge:latest"));
        assert!(
            result
                .compose_yaml
                .contains("\"-o\", \"@OWNER:test.local\"")
        );
    }
}
