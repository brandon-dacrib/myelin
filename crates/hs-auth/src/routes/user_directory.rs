//! `POST /user_directory/search`: finding a person to invite.
//!
//! # What this searches, and what the spec asks for
//!
//! The spec sets a *floor*, not a ceiling: a homeserver "MUST at a minimum consider users who are
//! visible to the requester based on their membership in rooms" -- users sharing a room, users in
//! publicly joinable rooms, users in world-readable rooms -- while "the homeserver may determine
//! which subset of users are searched".
//!
//! By default this implementation searches exactly that floor: the people the requester shares a
//! room with, and the members of public rooms. It used to search every account, on the reasoning
//! that the endpoint exists to find somebody you have *not* met yet -- which is true, and is why
//! the wider search is still available as `auth.user_directory_search_all_users`. But it is off
//! by default, as Synapse's `user_directory.search_all_users` is, because the cost is not
//! confined to open servers: a bridge makes a local account for every contact of every user it
//! serves, so searching everyone lets any user read other people's address books.
//!
//! Who shares a room with whom is a fact about rooms, and this crate cannot see rooms (`hs-user`
//! depends on it, not the other way round), so the scope arrives through
//! [`crate::state::UserDirectoryVisibility`], installed by `hs serve`.
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

    // Who the requester may find at all: people they share a room with, and people in public
    // rooms. Without this a search covered every account, so any account could enumerate every
    // other user's name -- and Complement's `TestRoomSpecificUsername*` noticed, by finding
    // somebody it should not have. `None` only when no room layer is installed (this crate's own
    // tests), where there are no rooms for anybody to be private in. A room layer that *fails* is
    // an error, never a reason to show everybody.
    let visible = match state.user_directory_visibility() {
        // An operator's explicit choice to let everybody find everybody.
        Some(_) if state.config.user_directory_search_all_users => None,
        Some(visibility) => Some(visibility.visible_to(&requester.user_id).await.map_err(|e| {
            tracing::error!(error = %e, "could not work out who the user directory may show");
            MatrixError::internal()
        })?),
        None => None,
    };

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
        .filter(|user| visible.as_ref().is_none_or(|v| v.contains(&user.user_id)))
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

    // ---------------------------------------------------------------------------------------
    // Who may find whom.
    // ---------------------------------------------------------------------------------------

    /// Stands in for the room layer: a fixed answer per requester, or a failure.
    struct FixedVisibility(Result<Vec<&'static str>, &'static str>);

    #[async_trait::async_trait]
    impl crate::state::UserDirectoryVisibility for FixedVisibility {
        async fn visible_to(
            &self,
            _requester: &ruma::UserId,
        ) -> Result<std::collections::BTreeSet<ruma::OwnedUserId>, String> {
            match &self.0 {
                Ok(ids) => Ok(ids
                    .iter()
                    .map(|id| ruma::UserId::parse(*id).unwrap())
                    .collect()),
                Err(e) => Err((*e).to_owned()),
            }
        }
    }

    /// With a room layer installed, a match the requester is not allowed to see is not a match.
    /// "alice" matches Alice and Dave ("Alice Impostor"); bob may only see Alice.
    #[tokio::test]
    async fn a_search_finds_only_the_people_its_caller_may_see() {
        let state = state_with_users().await;
        assert_eq!(
            ids(&search(&state, "@bob:example.org", "alice").await).len(),
            2,
            "without a room layer there are no rooms to be private in"
        );

        state.install_user_directory_visibility(std::sync::Arc::new(FixedVisibility(Ok(vec![
            "@alice:example.org",
            "@carol:example.org",
        ]))));
        let results = search(&state, "@bob:example.org", "alice").await;
        assert_eq!(ids(&results), vec!["@alice:example.org"]);
        assert_eq!(results["limited"], false);

        // Nobody visible means nobody found, however good the match.
        let nobody = AuthState::in_memory();
        for id in ["@alice:example.org", "@bob:example.org"] {
            nobody
                .store
                .create_user(UserRecord::new(ruma::UserId::parse(id).unwrap(), 0))
                .await
                .unwrap();
        }
        nobody.install_user_directory_visibility(std::sync::Arc::new(FixedVisibility(Ok(vec![]))));
        assert!(ids(&search(&nobody, "@bob:example.org", "alice").await).is_empty());
    }

    /// The operator's opt-in: everybody finds everybody, whatever the room layer would say.
    #[tokio::test]
    async fn search_all_users_is_an_explicit_setting_that_overrides_the_scope() {
        let state = AuthState::in_memory_with_config(crate::config::AuthConfig {
            user_directory_search_all_users: true,
            ..crate::config::AuthConfig::default()
        });
        for id in [
            "@alice:example.org",
            "@bob:example.org",
            "@alicia:example.org",
        ] {
            state
                .store
                .create_user(UserRecord::new(ruma::UserId::parse(id).unwrap(), 0))
                .await
                .unwrap();
        }
        state.install_user_directory_visibility(std::sync::Arc::new(FixedVisibility(Ok(vec![]))));
        assert_eq!(
            ids(&search(&state, "@bob:example.org", "ali").await),
            vec!["@alice:example.org", "@alicia:example.org"]
        );
    }

    /// A room layer that cannot answer is an error. The tempting fallback -- search everybody --
    /// is exactly the disclosure the scoping exists to prevent.
    #[tokio::test]
    async fn a_room_layer_that_fails_does_not_fall_back_to_showing_everybody() {
        let state = state_with_users().await;
        state.install_user_directory_visibility(std::sync::Arc::new(FixedVisibility(Err(
            "room store unreachable",
        ))));
        let requester =
            Requester::for_user(ruma::UserId::parse("@bob:example.org").unwrap().to_owned());
        let err = post_user_directory_search(
            State(state.clone()),
            requester,
            PermissiveJson(json!({"search_term": "alice"})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }
}
