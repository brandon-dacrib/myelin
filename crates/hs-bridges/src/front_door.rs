//! The front doors and the manager bot (RFC 0017 section 4.2): the manager's appservice API,
//! served on this server's own client listener under [`ROUTE_PREFIX`], and what its bots say.
//!
//! A local user who invites `@whatsappbot` is answered at once: it joins, says it is setting up
//! their bridge, and asks the manager for one; when the instance is ready the manager invites
//! them to it and says so here. `@bridges` takes commands.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use hs_admin::bridge_types;
use hs_kv::KvBackend;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manager::{BridgeManager, MANAGER_BOT, ROUTE_PREFIX, owner_of, short_name};
use crate::store::{InstanceState, OfferingRow};

/// The manager's appservice routes, to be merged into the client listener's router.
pub fn router<B: KvBackend + 'static>(manager: Arc<BridgeManager<B>>) -> Router {
    Router::new()
        .route(
            &format!("{ROUTE_PREFIX}/_matrix/app/v1/transactions/{{txn}}"),
            put(transaction::<B>),
        )
        .route(
            &format!("{ROUTE_PREFIX}/_matrix/app/v1/ping"),
            post(ping::<B>),
        )
        .route(
            &format!("{ROUTE_PREFIX}/_matrix/app/v1/users/{{user}}"),
            get(user_query::<B>),
        )
        .with_state(manager)
}

#[derive(Deserialize)]
struct TokenQuery {
    access_token: Option<String>,
}

fn authorized<B: KvBackend + 'static>(
    manager: &BridgeManager<B>,
    headers: &HeaderMap,
    query: &TokenQuery,
) -> bool {
    let Ok(tokens) = manager.tokens() else {
        return false;
    };
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    bearer.or(query.access_token.as_deref()) == Some(tokens.hs_token.as_str())
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({"errcode": "M_FORBIDDEN", "error": "bad hs_token"})),
    )
        .into_response()
}

async fn ping<B: KvBackend + 'static>(
    State(manager): State<Arc<BridgeManager<B>>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Response {
    if !authorized(&manager, &headers, &query) {
        return forbidden();
    }
    Json(json!({})).into_response()
}

async fn user_query<B: KvBackend + 'static>(
    State(manager): State<Arc<BridgeManager<B>>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    Path(user): Path<String>,
) -> Response {
    if !authorized(&manager, &headers, &query) {
        return forbidden();
    }
    let localpart = user
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or_default();
    if manager.is_manager_bot(localpart) {
        Json(json!({})).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"errcode": "M_NOT_FOUND"})),
        )
            .into_response()
    }
}

async fn transaction<B: KvBackend + 'static>(
    State(manager): State<Arc<BridgeManager<B>>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    Path(_txn): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if !authorized(&manager, &headers, &query) {
        return forbidden();
    }
    let events = body["events"].as_array().cloned().unwrap_or_default();
    // Answer the server at once: what the bots say goes back through its client API, and the
    // delivery that brought this transaction should not wait on that.
    tokio::spawn(async move {
        for event in events {
            if let Err(e) = handle(&manager, &event).await {
                tracing::warn!(error = %e, "the bridge manager could not handle an event");
            }
        }
    });
    Json(json!({})).into_response()
}

async fn handle<B: KvBackend + 'static>(
    manager: &BridgeManager<B>,
    event: &Value,
) -> Result<(), String> {
    let kind = event["type"].as_str().unwrap_or_default();
    let sender = event["sender"].as_str().unwrap_or_default();
    let room_id = event["room_id"].as_str().unwrap_or_default();
    let sender_localpart = sender
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or_default();
    if room_id.is_empty() || !manager.is_local(sender) || manager.is_manager_bot(sender_localpart) {
        return Ok(());
    }
    match kind {
        "m.room.member" => {
            let target = event["state_key"].as_str().unwrap_or_default();
            if event["content"]["membership"] != "invite"
                || !target.ends_with(&format!(":{}", manager.server_name))
            {
                return Ok(());
            }
            let bot = target
                .trim_start_matches('@')
                .split(':')
                .next()
                .unwrap_or_default();
            if !manager.is_manager_bot(bot) {
                return Ok(());
            }
            let (client, token) = speaker(manager)?;
            client
                .join(&token, target, room_id)
                .await
                .map_err(|e| e.to_string())?;
            manager
                .store
                .put_room(room_id, bot)
                .map_err(|e| e.to_string())?;
            if bot == MANAGER_BOT {
                say(manager, bot, room_id, &help(manager)).await;
            } else if let Some(bridge_type) = manager.type_for_front_door(bot) {
                // Being invited is asking: no need to say anything first.
                front_door(manager, &bridge_type, sender, room_id).await;
            }
        }
        "m.room.message" => {
            let Some(bot) = manager.store.room(room_id).map_err(|e| e.to_string())? else {
                return Ok(());
            };
            let body = event["content"]["body"].as_str().unwrap_or_default().trim();
            if bot == MANAGER_BOT {
                command(manager, sender, room_id, body).await;
            } else if let Some(bridge_type) = manager.type_for_front_door(&bot) {
                front_door(manager, &bridge_type, sender, room_id).await;
            }
        }
        _ => {}
    }
    Ok(())
}

fn speaker<B: KvBackend + 'static>(
    manager: &BridgeManager<B>,
) -> Result<(crate::matrix::MatrixClient, String), String> {
    let client = manager.client.get().ok_or("not started")?.clone();
    let token = manager.tokens().map_err(|e| e.to_string())?.as_token;
    Ok((client, token))
}

/// Says `text` (with `backticks` for code) in `room_id` as the manager's bot `localpart`.
async fn say<B: KvBackend + 'static>(
    manager: &BridgeManager<B>,
    localpart: &str,
    room_id: &str,
    text: &str,
) {
    let Ok((client, token)) = speaker(manager) else {
        return;
    };
    let html = text
        .split('\n')
        .map(inline_html)
        .collect::<Vec<_>>()
        .join("<br>");
    if let Err(e) = client
        .notice(&token, &manager.mxid(localpart), room_id, text, &html)
        .await
    {
        tracing::warn!(error = %e, room_id, "the bridge manager could not send a message");
    }
}

/// Escapes `text` for HTML and turns `backticks` into `<code>`.
#[must_use]
pub fn inline_html(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let mut out = String::new();
    for (i, part) in escaped.split('`').enumerate() {
        if i % 2 == 1 {
            out.push_str(&format!("<code>{part}</code>"));
        } else {
            out.push_str(part);
        }
    }
    out
}

fn name_of(bridge_type: &str) -> String {
    bridge_types::display_name(bridge_type)
        .unwrap_or(bridge_type)
        .to_owned()
}

/// What `@whatsappbot` does when `user` asks it for a bridge in `room_id`.
async fn front_door<B: KvBackend + 'static>(
    manager: &BridgeManager<B>,
    bridge_type: &str,
    user: &str,
    room_id: &str,
) {
    let door = bridge_types::front_door_localpart(bridge_type).unwrap_or(MANAGER_BOT);
    // Someone the offering is not open to is told so once, politely, and then left alone: a bot
    // that answers every message with the same refusal is a bot people mute.
    if let Ok(Some(offering)) = manager.offering_row(bridge_type)
        && offering.enabled
        && !manager.allowed(&offering, user)
        && !manager.store.record_refusal(room_id, user).unwrap_or(true)
    {
        return;
    }
    let text = match request(manager, bridge_type, user, Some(room_id)).await {
        Ok(text) | Err(text) => text,
    };
    say(manager, door, room_id, &text).await;
}

/// Asks for `user`'s instance of `bridge_type`, and says how that went, in words for them.
async fn request<B: KvBackend + 'static>(
    manager: &BridgeManager<B>,
    bridge_type: &str,
    user: &str,
    room_id: Option<&str>,
) -> Result<String, String> {
    let name = name_of(bridge_type);
    let offering: OfferingRow = match manager.offering_row(bridge_type) {
        Ok(Some(o)) if o.enabled => o,
        Ok(_) => {
            return Err(format!(
                "{name} bridges aren't offered on this server at the moment."
            ));
        }
        Err(_) => return Err("Something went wrong on my side; try again in a minute.".into()),
    };
    if !manager.allowed(&offering, user) {
        return Err(format!(
            "The {name} bridge on this server isn't available to your account. An administrator can change that."
        ));
    }
    let existing = manager
        .instance_row(bridge_type, user)
        .map_err(|_| "Something went wrong on my side; try again in a minute.".to_owned())?;
    let (row, created) = manager
        .request(bridge_type, user, room_id)
        .map_err(|_| "Something went wrong on my side; try again in a minute.".to_owned())?;
    let elsewhere = offering.runtime != "cluster";
    if created {
        return Ok(if elsewhere {
            format!(
                "An administrator runs {name} bridges on this server by hand. I've asked for one for you, and I'll invite you to it as soon as it's running."
            )
        } else {
            format!(
                "Setting up your {name} bridge. This takes a minute or two; I'll invite you to it as soon as it's running."
            )
        });
    }
    let bot =
        bridge_types::instance_names(bridge_type, owner_of(&row)).map(|(b, _)| manager.mxid(&b));
    Ok(match (existing.map(|e| e.state), row.state) {
        (Some(InstanceState::Failed), _) => format!(
            "Trying again to set up your {name} bridge. I'll invite you as soon as it's running."
        ),
        (_, InstanceState::Ready) => {
            if let (Some(dm), Some(bot), Ok((client, _))) = (&row.dm_room, &bot, speaker(manager))
                && let Some(token) = &row.as_token
            {
                let _ = client.invite(token, bot, dm, user).await;
            }
            format!(
                "Your {name} bridge is running. Talk to it in your chat with {}; I've sent the invite again in case you need it.",
                bot.unwrap_or_default()
            )
        }
        (_, state) => format!(
            "Your {name} bridge is still being set up ({}). I'll invite you as soon as it's running.",
            state.as_str()
        ),
    })
}

fn help<B: KvBackend + 'static>(manager: &BridgeManager<B>) -> String {
    let mut text = String::from(
        "I set up bridges to other chat networks. Each one is yours alone.\n\
         `list`: the bridges you can have\n\
         `start <bridge>`: set one up for you (`start whatsapp`)\n\
         `stop <bridge>`: remove yours, and your sign-in with it\n\
         `status`: how yours are doing",
    );
    let doors: Vec<String> = manager
        .offering_rows()
        .unwrap_or_default()
        .iter()
        .filter(|o| o.enabled)
        .filter_map(|o| bridge_types::front_door_localpart(&o.bridge_type))
        .map(|l| manager.mxid(l))
        .collect();
    if !doors.is_empty() {
        text.push_str(&format!(
            "\nYou can also message a bridge's own bot to get it: {}.",
            doors.join(", ")
        ));
    }
    text
}

/// Finds the offering a user means by `word`: its id, its short name, or its display name.
fn find_type<B: KvBackend + 'static>(manager: &BridgeManager<B>, word: &str) -> Option<String> {
    let word = word.trim().to_lowercase();
    manager.offering_rows().ok()?.into_iter().find_map(|o| {
        let t = o.bridge_type.as_str();
        let matches = t == word || short_name(t) == word || name_of(t).to_lowercase() == word;
        matches.then(|| t.to_owned())
    })
}

async fn command<B: KvBackend + 'static>(
    manager: &BridgeManager<B>,
    user: &str,
    room_id: &str,
    body: &str,
) {
    let mut words = body.split_whitespace();
    let verb = words.next().unwrap_or_default().to_lowercase();
    let rest: Vec<&str> = words.collect();
    let reply = match verb.as_str() {
        "list" => {
            let offerings: Vec<OfferingRow> = manager
                .offering_rows()
                .unwrap_or_default()
                .into_iter()
                .filter(|o| o.enabled && manager.allowed(o, user))
                .collect();
            if offerings.is_empty() {
                "There are no bridges you can set up on this server yet.".to_owned()
            } else {
                let mut text = String::from("Bridges you can have:");
                for o in offerings {
                    let mine = manager
                        .instance_row(&o.bridge_type, user)
                        .ok()
                        .flatten()
                        .map(|r| format!(" (yours: {})", r.state.as_str()))
                        .unwrap_or_default();
                    text.push_str(&format!(
                        "\n- {}: `start {}`{mine}",
                        name_of(&o.bridge_type),
                        short_name(&o.bridge_type)
                    ));
                }
                text
            }
        }
        "start" => match rest.first().and_then(|w| find_type(manager, w)) {
            Some(bridge_type) => match request(manager, &bridge_type, user, Some(room_id)).await {
                Ok(text) | Err(text) => text,
            },
            None => "Which one? `list` shows the bridges you can have.".to_owned(),
        },
        "stop" => match rest.first().and_then(|w| find_type(manager, w)) {
            Some(bridge_type) => {
                let name = name_of(&bridge_type);
                let mine = manager.instance_row(&bridge_type, user).ok().flatten();
                if mine.is_none() {
                    format!("You don't have a {name} bridge.")
                } else if rest.get(1).map(|w| w.eq_ignore_ascii_case("confirm")) != Some(true) {
                    format!(
                        "This removes your {name} bridge and signs it out; your bridged chats stop updating. Send `stop {} confirm` to go ahead.",
                        short_name(&bridge_type)
                    )
                } else {
                    match manager.stop(&bridge_type, user).await {
                        Ok(_) => format!("Your {name} bridge is removed."),
                        Err(e) => format!("I couldn't remove it: {e}"),
                    }
                }
            }
            None => "Which one? `status` shows yours.".to_owned(),
        },
        "status" => {
            let mut lines = Vec::new();
            for o in manager.offering_rows().unwrap_or_default() {
                if let Ok(Some(r)) = manager.instance_row(&o.bridge_type, user) {
                    let reason = r.reason.map(|r| format!(": {r}")).unwrap_or_default();
                    lines.push(format!(
                        "- {}: {}{reason}",
                        name_of(&o.bridge_type),
                        r.state.as_str()
                    ));
                }
            }
            if lines.is_empty() {
                "You don't have any bridges yet. `list` shows the ones you can have.".to_owned()
            } else {
                format!("Your bridges:\n{}", lines.join("\n"))
            }
        }
        _ => help(manager),
    };
    say(manager, MANAGER_BOT, room_id, &reply).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backticks_become_code_and_html_is_escaped() {
        assert_eq!(
            inline_html("send `login qr` <now>"),
            "send <code>login qr</code> &lt;now&gt;"
        );
    }
}
