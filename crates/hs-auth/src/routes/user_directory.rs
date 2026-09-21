//! `POST /user_directory/search`: finding a person to invite.
//!
//! # What this searches, and what the spec asks for
//!
//! The spec sets a *floor*, not a ceiling: a homeserver "MUST at a minimum consider users who are
//! visible to the requester based on their membership in rooms" -- users sharing a room, users in
//! publicly joinable rooms, users in world-readable rooms -- while "the homeserver may determine
//! which subset of users are searched".
//!
//! This implementation searches every account on this server. That is a superset of the floor, so
//! it satisfies the requirement, and it is the behaviour that makes the endpoint useful for what
//! clients actually call it for: finding somebody you have *not* met yet in order to invite them.
//! A shared-rooms filter is also not something this crate could apply -- room membership lives in
//! `hs-room`, which depends on this crate and not the other way round.
//!
//! It is worth being explicit that this is a policy choice with a privacy cost: on this server,
//! any logged-in user can enumerate the display names of all the others by searching. Synapse
//! makes the opposite choice by default (`user_directory.search_all_users: false`) and is
//! correspondingly unable to find a stranger to invite. If this server ever hosts users who
//! should not be able to see each other, this is the first thing to put behind a setting.
//!
//! Remote users are not searched. The spec says a server "SHOULD query remote users as part of
//! the search"; doing so means federated user-directory queries, which this server does not make.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::error::MatrixError;
use crate::requester::Requester;
use crate::state::AuthState;
use crate::store::UserRecord;
use hs_http::body::PermissiveJson;

/// The spec's documented default when the request omits `limit`.
const DEFAULT_LIMIT: usize = 10;

/// An upper bound this server imposes on `limit`, so one request cannot ask it to serialize every
/// account it has. Not in the spec -- the spec lets a server decide which subset it searches, and
/// a ceiling on the answer is part of that.
const MAX_LIMIT: usize = 100;

/// `POST /user_directory/search`.
pub async fn post_user_directory_search(
    State(state): State<AuthState>,
    requester: Requester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    let search_term = body
        .get("search_term")
        .and_then(Value::as_str)
        .ok_or_else(|| MatrixError::missing_param("Missing search_term"))?;

    let limit = match body.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(value) => {
            let n = value.as_u64().ok_or_else(|| {
                MatrixError::invalid_param("limit must be a non-negative integer")
            })?;
            usize::try_from(n).unwrap_or(MAX_LIMIT).min(MAX_LIMIT)
        }
    };

    // An empty search term matches everything, which is a directory dump rather than a search.
    // The spec does not forbid it; answering nothing is the conservative reading and costs a
    // client nothing, since no client asks for it on purpose.
    if search_term.trim().is_empty() {
        return Ok(Json(json!({"results": [], "limited": false})).into_response());
    }

    let needle = search_term.to_lowercase();
    let mut matches: Vec<(Rank, UserRecord)> = state
        .store
        .list_users()
        .await?
        .into_iter()
        .filter(|user| {
            // A deactivated account cannot be invited anywhere or log in again, so offering it as
            // somebody to talk to would be a dead end. The requester's own account is left out
            // too: every client filters it back out, and nobody searches for themselves.
            !user.deactivated && user.user_id != requester.user_id
        })
        .filter_map(|user| rank(&user, &needle).map(|rank| (rank, user)))
        .collect();

    // "Ordered by rank and then whether or not profile info is available", then by user id so
    // that two equally-ranked results do not swap places between identical requests.
    matches.sort_by(|(a_rank, a), (b_rank, b)| {
        a_rank
            .cmp(b_rank)
            .then_with(|| b.display_name.is_some().cmp(&a.display_name.is_some()))
            .then_with(|| a.user_id.cmp(&b.user_id))
    });

    let limited = matches.len() > limit;
    let results: Vec<Value> = matches
        .into_iter()
        .take(limit)
        .map(|(_, user)| {
            let mut entry = json!({"user_id": user.user_id.as_str()});
            let object = entry.as_object_mut().expect("built as an object");
            if let Some(name) = user.display_name {
                object.insert("display_name".to_owned(), Value::String(name));
            }
            if let Some(avatar) = user.avatar_url {
                object.insert("avatar_url".to_owned(), Value::String(avatar));
            }
            entry
        })
        .collect();

    Ok(Json(json!({"results": results, "limited": limited})).into_response())
}

/// How well a user matched, best first. Derived `Ord` follows declaration order, which is the
/// ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    /// The search term is the whole user ID or the whole localpart.
    Exact,
    /// A display name or localpart that starts with the term -- what somebody typing a name means.
    Prefix,
    /// The term appears somewhere inside.
    Substring,
}

/// `None` when the user does not match at all. Matching is case-insensitive on the user ID and
/// the display name, as the spec requires; `needle` is already lowercased by the caller.
fn rank(user: &UserRecord, needle: &str) -> Option<Rank> {
    let user_id = user.user_id.as_str().to_lowercase();
    let localpart = user.user_id.localpart().to_lowercase();
    let display_name = user.display_name.as_deref().map(str::to_lowercase);

    if user_id == needle || localpart == needle || display_name.as_deref() == Some(needle) {
        return Some(Rank::Exact);
    }
    if localpart.starts_with(needle)
        || user_id.starts_with(needle)
        || display_name
            .as_deref()
            .is_some_and(|name| name.starts_with(needle))
    {
        return Some(Rank::Prefix);
    }
    if user_id.contains(needle)
        || display_name
            .as_deref()
            .is_some_and(|name| name.contains(needle))
    {
        return Some(Rank::Substring);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::user_id;

    async fn state_with_users() -> AuthState {
        let state = AuthState::in_memory();
        for (id, name) in [
            ("@alice:example.org", Some("Alice Wonderland")),
            ("@bob:example.org", Some("Bob Builder")),
            ("@carol:example.org", None),
            ("@dave:example.org", Some("Alice Impostor")),
        ] {
            let mut record = UserRecord::new(ruma::UserId::parse(id).unwrap(), 0);
            record.display_name = name.map(str::to_owned);
            state.store.create_user(record).await.unwrap();
        }
        state
    }

    async fn search(state: &AuthState, requester: &str, term: &str) -> Value {
        let body = json!({"search_term": term});
        let requester = Requester::for_user(ruma::UserId::parse(requester).unwrap().to_owned());
        let response =
            post_user_directory_search(State(state.clone()), requester, PermissiveJson(body))
                .await
                .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn ids(value: &Value) -> Vec<String> {
        value["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["user_id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The case the endpoint exists for: somebody typing a name into an invite box.
    #[tokio::test]
    async fn a_display_name_is_searchable_case_insensitively() {
        let state = state_with_users().await;
        let found = search(&state, "@bob:example.org", "alice").await;
        assert_eq!(
            ids(&found),
            vec!["@alice:example.org", "@dave:example.org"],
            "an exact localpart match outranks a display name that merely contains the term"
        );
    }

    #[tokio::test]
    async fn a_full_user_id_finds_exactly_that_user() {
        let state = state_with_users().await;
        let found = search(&state, "@bob:example.org", "@carol:example.org").await;
        assert_eq!(ids(&found), vec!["@carol:example.org"]);
    }

    #[tokio::test]
    async fn a_user_with_no_display_name_is_still_findable_and_carries_no_empty_field() {
        let state = state_with_users().await;
        let found = search(&state, "@bob:example.org", "carol").await;
        assert_eq!(ids(&found), vec!["@carol:example.org"]);
        let entry = &found["results"][0];
        assert!(
            entry.get("display_name").is_none(),
            "a field that was never set is omitted, never sent as null: {entry}"
        );
    }

    #[tokio::test]
    async fn the_searcher_is_not_among_their_own_results() {
        let state = state_with_users().await;
        let found = search(&state, "@alice:example.org", "alice").await;
        assert_eq!(ids(&found), vec!["@dave:example.org"]);
    }

    #[tokio::test]
    async fn a_deactivated_account_is_not_offered_as_somebody_to_talk_to() {
        let state = state_with_users().await;
        state
            .store
            .set_deactivated(user_id!("@carol:example.org"), true)
            .await
            .unwrap();
        let found = search(&state, "@bob:example.org", "carol").await;
        assert!(ids(&found).is_empty(), "{found}");
    }

    #[tokio::test]
    async fn limit_truncates_and_says_it_did() {
        let state = state_with_users().await;
        let body = json!({"search_term": "example.org", "limit": 2});
        let requester = Requester::for_user(user_id!("@bob:example.org").to_owned());
        let response =
            post_user_directory_search(State(state.clone()), requester, PermissiveJson(body))
                .await
                .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let found: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(found["results"].as_array().unwrap().len(), 2);
        assert_eq!(found["limited"], true);
    }

    #[tokio::test]
    async fn an_empty_search_term_is_not_a_directory_dump() {
        let state = state_with_users().await;
        let found = search(&state, "@bob:example.org", "   ").await;
        assert!(ids(&found).is_empty(), "{found}");
        assert_eq!(found["limited"], false);
    }

    #[tokio::test]
    async fn a_missing_search_term_is_a_missing_param() {
        let state = state_with_users().await;
        let requester = Requester::for_user(user_id!("@bob:example.org").to_owned());
        let err = post_user_directory_search(State(state), requester, PermissiveJson(json!({})))
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_MISSING_PARAM");
    }
}
