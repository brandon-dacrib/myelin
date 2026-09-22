import type { BridgeType } from "@/api/bridges";

function type(id: string, name: string, network: string, requiredKey: string): BridgeType {
  const short = id.replace(/^mautrix-/, "").replace(/^matrix-/, "");
  return {
    id,
    name,
    upstream_project: `mautrix-${id.replace(/^mautrix-/, "")}`,
    image: `dock.mau.dev/mautrix/${id.replace(/^mautrix-/, "")}:latest`,
    // Written for the mock server's own name, as the real catalogue writes them for its.
    // `default_namespaces` is an untyped OpenAPI object (`Record<string, never>` once
    // generated); this is the one sanctioned cast for it, as the wizard's is for a render.
    default_namespaces: {
      users: [{ regex: `@${short}_.*:example\\.org`, exclusive: true }],
      aliases: [{ regex: `#${short}_.*:example\\.org`, exclusive: true }],
      rooms: [],
    } as unknown as Record<string, never>,
    config_keys: [{ key: requiredKey, description: `${network} ${requiredKey}`, required: true }],
    supports_double_puppeting: true,
    required_features: ["de.sorunome.msc2409.push_ephemeral", "org.matrix.msc3202"],
  };
}

/** The wizard's catalogue (flows.md flow 1 step 2). */
export const bridgeTypes: BridgeType[] = [
  type("mautrix-whatsapp", "WhatsApp", "WhatsApp", "linked phone"),
  type("mautrix-telegram", "Telegram", "Telegram", "API ID and hash"),
  type("mautrix-signal", "Signal", "Signal", "linked phone"),
  type("mautrix-discord", "Discord", "Discord", "user or bot token"),
  type("mautrix-slack", "Slack", "Slack", "app credentials"),
  type("mautrix-gmessages", "Google Messages", "Google Messages", "linked phone"),
  type("mautrix-meta", "Messenger", "Meta", "Facebook account"),
  type("mautrix-instagram", "Instagram", "Meta", "Instagram account"),
  type("mautrix-twitter", "X (Twitter)", "X", "X account"),
  type("mautrix-linkedin", "LinkedIn", "LinkedIn", "LinkedIn account"),
  type("mautrix-gvoice", "Google Voice", "Google Voice", "Google Voice number"),
  type("mautrix-irc", "IRC (mautrix)", "IRC", "network address"),
  type("mautrix-zulip", "Zulip", "Zulip", "bot token"),
  type("matrix-hookshot", "Hookshot", "Webhooks", "webhook or app credentials"),
  type("heisenbridge", "Heisenbridge", "IRC", "network address"),
  type("matrix-appservice-irc", "IRC (appservice-irc)", "IRC", "network address"),
];
