//! A bridge registered by hand for a network this server now offers (RFC 0017 section 6: the
//! demo's shared WhatsApp registration from 2026-09-25, replaced by an offering).
//!
//! The hand-registered bridge claims the catalogue's ghost namespace (`@whatsapp_.*:server`),
//! and every instance the offering makes claims a slice of it (`@whatsapp_brandon_.*:server`).
//! The registry allows both: neither claims the other's bot, and the patterns are not
//! identical. So a message for an instance's ghost is delivered to the hand-registered bridge
//! too, and whichever of the two is signed in to that account on the network's side bridges it.
//! Nothing here stops that on its own -- the hand-registered bridge may still be the one people
//! are using, and cutting it off silently would lose their messages -- but the server says so,
//! on the appservice's health and on the offering, with what to do: pause it now (its page has
//! the control), then stop and remove it once everyone has their own instance.

use std::collections::HashSet;

use hs_admin::bridge_types;
use hs_admin::model::{AdminAppservice, AdminAppserviceOverlap, AdminOfferingOverlap};
use serde_json::Value;

/// One hand-registered appservice that overlaps an offering's instances, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    pub appservice_id: String,
    pub sender_localpart: String,
    /// Why it counts as a bridge of the offering's network, in words.
    pub why: String,
}

/// The appservices among `appservices` that are bridges of `bridge_type`'s network without
/// being one of its instances (`instance_ids`) or the manager (`manager_id`): created from the
/// same catalogue entry (`io.myelin.bridge_type`), or registered under the catalogue's bot
/// name, or with an *exclusive* users namespace rule that covers the catalogue's ghosts
/// (matched the way the registry matches, anchored at the start). A non-exclusive rule is a
/// double-puppeting claim, there to act as people rather than to own ghosts, and does not
/// count. `None` for a type not in the catalogue.
#[must_use]
pub fn overlaps(
    bridge_type: &str,
    server_name: &str,
    appservices: &[AdminAppservice],
    instance_ids: &HashSet<String>,
    manager_id: &str,
) -> Option<Vec<Overlap>> {
    let kind = bridge_types::get(bridge_type, server_name)?;
    let (bot, ghost_prefix) = bridge_types::instance_names(bridge_type, None)?;
    let sample_ghost = format!("@{ghost_prefix}sample:{server_name}");
    let mut found = Vec::new();
    for appservice in appservices {
        if appservice.id == manager_id || instance_ids.contains(&appservice.id) {
            continue;
        }
        let why = if appservice.bridge_type.as_deref() == Some(bridge_type) {
            format!("it was created from the catalogue's {} entry", kind.name)
        } else if appservice.sender_localpart == bot {
            format!(
                "its bot is @{bot}:{server_name}, the name {}'s front door takes",
                kind.name
            )
        } else if let Some(rule) = covering_rule(&appservice.namespaces, &sample_ghost) {
            format!(
                "its user namespace `{rule}` covers the ghost users of every {} instance (@{ghost_prefix}…:{server_name})",
                kind.name
            )
        } else {
            continue;
        };
        found.push(Overlap {
            appservice_id: appservice.id.clone(),
            sender_localpart: appservice.sender_localpart.clone(),
            why,
        });
    }
    Some(found)
}

/// The first exclusive `users` rule of `namespaces` whose regex matches `user_id`, as Synapse
/// and the registry match a namespace rule: anchored at the start, a prefix match. A regex
/// that does not compile is skipped: the registry refused it or will, and it is not this
/// module's job.
fn covering_rule(namespaces: &Value, user_id: &str) -> Option<String> {
    namespaces
        .get("users")?
        .as_array()?
        .iter()
        .filter(|rule| rule.get("exclusive").and_then(Value::as_bool) == Some(true))
        .filter_map(|rule| rule.get("regex").and_then(Value::as_str))
        .find(|pattern| {
            regex::Regex::new(&format!("^(?:{pattern})"))
                .map(|re| re.is_match(user_id))
                .unwrap_or(false)
        })
        .map(str::to_owned)
}

/// The health line for the hand-registered appservice: which offering it overlaps and what to
/// do. `front_door` is the offering's bot, when it has one.
#[must_use]
pub fn for_appservice(
    overlap: &Overlap,
    bridge_type: &str,
    name: &str,
    front_door: Option<&str>,
) -> AdminOfferingOverlap {
    let get_one = match front_door {
        Some(door) => format!("people get their own {name} bridge by messaging {door}"),
        None => format!("one {name} bridge is run for everyone"),
    };
    AdminOfferingOverlap {
        bridge_type: bridge_type.to_owned(),
        name: name.to_owned(),
        front_door: front_door.map(str::to_owned),
        detail: format!(
            "{name} is offered on this server now: {get_one}. This bridge was registered by hand and {why}, so a message for a {name} ghost user is delivered to it and to the person's own instance. Pause it here to stop delivering to it now; once everyone who used it has their own instance, stop it where it runs and remove it here. docs/bridges/mautrix.md has the steps.",
            why = overlap.why
        ),
    }
}

/// The offering's line for one hand-registered appservice: who it is and what to do.
#[must_use]
pub fn for_offering(overlap: &Overlap, name: &str, server_name: &str) -> AdminAppserviceOverlap {
    AdminAppserviceOverlap {
        id: overlap.appservice_id.clone(),
        sender_localpart: overlap.sender_localpart.clone(),
        detail: format!(
            "{id} (bot @{bot}:{server_name}) was registered by hand and {why}, so a message for a {name} ghost user is delivered to it as well as to the person's own instance. Once everyone who used it has their own instance, stop it where it runs and remove it from its page.",
            id = overlap.appservice_id,
            bot = overlap.sender_localpart,
            why = overlap.why
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn appservice(
        id: &str,
        sender: &str,
        users: Value,
        bridge_type: Option<&str>,
    ) -> AdminAppservice {
        AdminAppservice {
            id: id.to_owned(),
            sender_localpart: sender.to_owned(),
            url: Some("http://bridge:29318".to_owned()),
            namespaces: json!({"users": users, "aliases": [], "rooms": []}),
            rate_limited: false,
            protocols: Vec::new(),
            paused: false,
            health: "healthy".to_owned(),
            created_at: "2026-09-25T00:00:00.000Z".to_owned(),
            bridge_type: bridge_type.map(str::to_owned),
            links: Default::default(),
            queue: Default::default(),
        }
    }

    /// The demo's registration: made by the wizard on 2026-09-25, so it carries the catalogue
    /// key; the manager's own registration and the instances' are never counted; a bridge of
    /// another network is not.
    #[test]
    fn a_wizard_made_bridge_of_the_same_network_overlaps_and_the_rest_do_not() {
        let list = vec![
            appservice(
                "whatsapp",
                "whatsappbot",
                json!([{"regex": "@whatsapp_.*:example\\.org", "exclusive": true}]),
                Some("mautrix-whatsapp"),
            ),
            appservice(
                "whatsapp-brandon",
                "whatsappbot_brandon",
                json!([{"regex": "@whatsapp_brandon_.*:example\\.org", "exclusive": true}]),
                Some("mautrix-whatsapp"),
            ),
            appservice("myelin-bridges", "bridges", json!([]), None),
            appservice(
                "telegram",
                "telegrambot",
                json!([{"regex": "@telegram_.*:example\\.org", "exclusive": true}]),
                Some("mautrix-telegram"),
            ),
        ];
        let instances = HashSet::from(["whatsapp-brandon".to_owned()]);
        let found = overlaps(
            "mautrix-whatsapp",
            "example.org",
            &list,
            &instances,
            "myelin-bridges",
        )
        .unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].appservice_id, "whatsapp");
        assert_eq!(found[0].sender_localpart, "whatsappbot");
        assert!(
            found[0].why.contains("catalogue's WhatsApp entry"),
            "{}",
            found[0].why
        );
        assert_eq!(
            overlaps(
                "mautrix-telegram",
                "example.org",
                &list,
                &instances,
                "myelin-bridges"
            )
            .unwrap()
            .len(),
            1
        );
        assert!(
            overlaps(
                "mautrix-fax",
                "example.org",
                &list,
                &instances,
                "myelin-bridges"
            )
            .is_none()
        );
    }

    /// A registration file written by hand (no catalogue key, another id and bot name) is
    /// caught by its exclusive namespace, matched as the registry matches it; a non-exclusive
    /// claim on everyone (a double-puppeting registration) is not a bridge of the network.
    #[test]
    fn a_hand_written_registration_is_caught_by_its_namespace() {
        let list = vec![
            appservice(
                "wa-old",
                "wa",
                json!([{"regex": "@whatsapp_.*:example\\.org", "exclusive": true}, {"regex": "@.*:example\\.org", "exclusive": false}]),
                None,
            ),
            appservice(
                "puppet",
                "puppet",
                json!([{"regex": "@.*:example\\.org", "exclusive": false}]),
                None,
            ),
            appservice(
                "signal",
                "signalbot",
                json!([{"regex": "@signal_.*:example\\.org", "exclusive": true}, {"regex": "(broken", "exclusive": true}]),
                None,
            ),
        ];
        let found = overlaps(
            "mautrix-whatsapp",
            "example.org",
            &list,
            &HashSet::new(),
            "myelin-bridges",
        )
        .unwrap();
        let ids: Vec<&str> = found.iter().map(|o| o.appservice_id.as_str()).collect();
        assert_eq!(ids, vec!["wa-old"], "{found:?}");
        assert!(
            found[0].why.contains("`@whatsapp_.*:example\\.org`"),
            "{}",
            found[0].why
        );
        // Signal's registration is caught by its bot name first; its pattern that does not
        // compile is never reached, and WhatsApp's broad non-exclusive rule does not make it a
        // Signal bridge.
        let signal = overlaps(
            "mautrix-signal",
            "example.org",
            &list,
            &HashSet::new(),
            "myelin-bridges",
        )
        .unwrap();
        assert_eq!(signal.len(), 1, "{signal:?}");
        assert!(
            signal[0].why.contains("@signalbot:example.org"),
            "{}",
            signal[0].why
        );
        // The pattern that does not compile is skipped when it is what is looked at.
        let broken = vec![appservice(
            "odd",
            "odd",
            json!([{"regex": "(broken", "exclusive": true}, {"regex": "@signal_.*:example\\.org", "exclusive": true}]),
            None,
        )];
        let found = overlaps(
            "mautrix-signal",
            "example.org",
            &broken,
            &HashSet::new(),
            "myelin-bridges",
        )
        .unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn the_lines_say_which_offering_and_what_to_do() {
        let overlap = Overlap {
            appservice_id: "whatsapp".into(),
            sender_localpart: "whatsappbot".into(),
            why: "it was created from the catalogue's WhatsApp entry".into(),
        };
        let health = for_appservice(
            &overlap,
            "mautrix-whatsapp",
            "WhatsApp",
            Some("@whatsappbot:example.org"),
        );
        assert_eq!(health.bridge_type, "mautrix-whatsapp");
        assert_eq!(
            health.front_door.as_deref(),
            Some("@whatsappbot:example.org")
        );
        assert!(
            health.detail.contains("messaging @whatsappbot:example.org"),
            "{}",
            health.detail
        );
        assert!(
            health.detail.contains("remove it here"),
            "{}",
            health.detail
        );
        assert!(health.detail.contains("Pause it here"), "{}", health.detail);
        let shared = for_appservice(&overlap, "heisenbridge", "heisenbridge", None);
        assert!(
            shared.detail.contains("run for everyone"),
            "{}",
            shared.detail
        );
        let line = for_offering(&overlap, "WhatsApp", "example.org");
        assert_eq!(line.id, "whatsapp");
        assert!(
            line.detail
                .starts_with("whatsapp (bot @whatsappbot:example.org)"),
            "{}",
            line.detail
        );
        assert!(
            line.detail.contains("remove it from its page"),
            "{}",
            line.detail
        );
    }
}
