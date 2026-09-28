//! Keys across servers: asking another server for its users' device keys and one-time keys when
//! a local client asks for them ([`RemoteKeys`], installed by `hs-cli` over the federation
//! client), and answering the same two questions from another server about this one's users
//! ([`federation_keys_query`], [`federation_keys_claim`]).
//!
//! # No cache
//!
//! Every `/keys/query` for a remote user asks that user's server. Synapse caches a remote user's
//! device list while it shares a room with them and keeps the cache fresh from the
//! `m.device_list_update` EDUs it receives; this server records those EDUs as device-list changes
//! (so clients are told to re-query) but keeps no copy of the keys, so the re-query is always
//! answered by the only server that knows. That costs one federation request per query and can
//! never serve a stale key -- Complement's `TestDeviceListUpdates` checks exactly that a server
//! "must not return a cached device list" after a user left and changed their keys.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

/// Reaches another server's `/user/keys/query` and `/user/keys/claim`. Implemented in `hs-cli`
/// over `hs_federation::client::FederationClient`; installed with
/// [`crate::state::E2eState::install_remote_keys`].
#[async_trait]
pub trait RemoteKeys: Send + Sync {
    /// `POST /_matrix/federation/v1/user/keys/query` to `server` with `{"device_keys":
    /// device_keys}`, returning the response body; `Err` describes why there is none.
    async fn query(&self, server: &str, device_keys: Value) -> Result<Value, String>;

    /// `POST /_matrix/federation/v1/user/keys/claim` to `server` with `{"one_time_keys":
    /// one_time_keys}`, returning the response body.
    async fn claim(&self, server: &str, one_time_keys: Value) -> Result<Value, String>;
}

/// The part of a request naming users of servers other than `own_server`, grouped by server:
/// `{server: {user_id: what was asked for them}}`.
pub(crate) fn remote_part(
    requested: &Map<String, Value>,
    own_server: &str,
) -> BTreeMap<String, Map<String, Value>> {
    let mut by_server: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for (user_id, asked) in requested {
        let Ok(parsed) = ruma::UserId::parse(user_id.as_str()) else {
            continue;
        };
        let server = parsed.server_name().as_str();
        if server != own_server {
            by_server
                .entry(server.to_owned())
                .or_default()
                .insert(user_id.clone(), asked.clone());
        }
    }
    by_server
}

/// Which remote call [`ask_servers`] makes.
#[derive(Clone, Copy)]
pub(crate) enum Ask {
    Query,
    Claim,
}

/// Asks every server in `by_server` at once and returns each one's answer (or why there is
/// none), by server.
pub(crate) async fn ask_servers(
    remote: &Arc<dyn RemoteKeys>,
    ask: Ask,
    by_server: BTreeMap<String, Map<String, Value>>,
) -> Vec<(String, Result<Value, String>)> {
    let mut tasks = tokio::task::JoinSet::new();
    for (server, users) in by_server {
        let remote = Arc::clone(remote);
        tasks.spawn(async move {
            let answer = match ask {
                Ask::Query => remote.query(&server, Value::Object(users)).await,
                Ask::Claim => remote.claim(&server, Value::Object(users)).await,
            };
            (server, answer)
        });
    }
    let mut answers = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(answer) => answers.push(answer),
            Err(error) => tracing::warn!(%error, "a remote key request task failed"),
        }
    }
    answers
}

/// Copies from `answer[field]` into `into` only the entries for users of `server` that were
/// asked about: a server answers for its own users, and nothing it says about anybody else's
/// keys is taken.
pub(crate) fn merge_for_server(
    into: &mut Map<String, Value>,
    answer: &Value,
    field: &str,
    server: &str,
    asked: &Map<String, Value>,
) {
    let Some(entries) = answer.get(field).and_then(Value::as_object) else {
        return;
    };
    for (user_id, value) in entries {
        let belongs =
            ruma::UserId::parse(user_id.as_str()).is_ok_and(|u| u.server_name().as_str() == server);
        if belongs && asked.contains_key(user_id) {
            into.insert(user_id.clone(), value.clone());
        }
    }
}

/// The `failures` entry for a server that could not be asked, in Synapse's shape.
pub(crate) fn failure(reason: &str) -> Value {
    json!({"status": 503, "message": reason})
}

/// Answers another server's `POST /user/keys/query`: device keys, master keys and self-signing
/// keys for this server's own users named in `device_keys`, and never anybody's user-signing key
/// (who a user has verified is theirs alone). Users of other servers are ignored.
///
/// # Errors
/// Returns [`crate::error::E2eError::BadRequest`] for a malformed request, or a storage error.
pub async fn federation_keys_query<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    device_keys: &Value,
) -> Result<Value, crate::error::E2eError> {
    let local = crate::routes::keys_query::local_keys_query(state, None, device_keys).await?;
    Ok(json!({
        "device_keys": local.device_keys,
        "master_keys": local.master_keys,
        "self_signing_keys": local.self_signing_keys,
    }))
}

/// Answers another server's `POST /user/keys/claim`: one key per requested device of this
/// server's own users, each claimed atomically (a one-time key if any is left, else the fallback
/// key). Users of other servers are ignored.
///
/// # Errors
/// Returns [`crate::error::E2eError::BadRequest`] for a malformed request, or a storage error.
pub async fn federation_keys_claim<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    one_time_keys: &Value,
) -> Result<Value, crate::error::E2eError> {
    let claimed = crate::routes::keys_claim::local_keys_claim(state, one_time_keys).await?;
    Ok(json!({"one_time_keys": claimed}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_users_of_other_servers_are_grouped_by_server() {
        let requested = json!({
            "@a:here.example": [],
            "@b:there.example": ["D1"],
            "@c:there.example": [],
            "@d:third.example": [],
            "not a user": [],
        });
        let grouped = remote_part(requested.as_object().unwrap(), "here.example");
        assert_eq!(grouped.len(), 2);
        assert_eq!(
            Value::Object(grouped["there.example"].clone()),
            json!({"@b:there.example": ["D1"], "@c:there.example": []})
        );
        assert!(grouped.contains_key("third.example"));
    }

    #[test]
    fn a_server_is_believed_only_about_its_own_users_that_were_asked_about() {
        let asked = json!({"@b:there.example": []});
        let answer = json!({"device_keys": {
            "@b:there.example": {"D1": {"k": 1}},
            "@z:there.example": {"D9": {"k": 9}},
            "@a:here.example": {"EVIL": {"k": 0}},
        }});
        let mut into = Map::new();
        merge_for_server(
            &mut into,
            &answer,
            "device_keys",
            "there.example",
            asked.as_object().unwrap(),
        );
        assert_eq!(
            Value::Object(into),
            json!({"@b:there.example": {"D1": {"k": 1}}})
        );
    }
}
