//! `hs_auth::state::RemoteProfileSource` over the federation client: how `GET
//! /profile/{userId}` reaches the profile of a user of another server.
//!
//! `hs-auth` serves the profile routes and `hs-federation` speaks to other servers; neither
//! depends on the other, and this is where they meet (the same shape as [`crate::remote_join`]).
//! The call is `GET /_matrix/federation/v1/query/profile?user_id=..&field=..` to the user's own
//! server, and the answer is passed through as it came: the spec's response is the client-server
//! profile object.

use std::sync::Arc;

use async_trait::async_trait;
use hs_federation::client::FederationClient;
use ruma::UserId;
use serde_json::Value;

/// The `hs serve` implementation of [`hs_auth::state::RemoteProfileSource`]. See the module docs.
pub struct FederationRemoteProfile {
    client: Arc<FederationClient>,
}

impl FederationRemoteProfile {
    /// Over the federation mount's own client, so discovery, TLS trust, request signing and
    /// per-destination backoff are the ones every other outbound call uses.
    #[must_use]
    pub fn new(client: Arc<FederationClient>) -> Self {
        Self { client }
    }
}

/// The path of the profile query for `user_id`, narrowed to `field` when one is given.
fn query_path(user_id: &UserId, field: Option<&str>) -> String {
    let mut path = format!(
        "/_matrix/federation/v1/query/profile?user_id={}",
        crate::remote_join::query_encode(user_id.as_str())
    );
    if let Some(field) = field {
        path.push_str("&field=");
        path.push_str(&crate::remote_join::query_encode(field));
    }
    path
}

#[async_trait]
impl hs_auth::state::RemoteProfileSource for FederationRemoteProfile {
    async fn remote_profile(
        &self,
        user_id: &UserId,
        field: Option<&str>,
    ) -> Result<Option<Value>, String> {
        let destination = user_id.server_name().as_str();
        let response = self
            .client
            .send(destination, "GET", &query_path(user_id, field), None)
            .await
            .map_err(|error| {
                format!("could not ask {destination} for {user_id}'s profile: {error}")
            })?;
        match response.status {
            200..=299 if response.body.is_object() => Ok(Some(response.body)),
            200..=299 => Err(format!(
                "{destination} answered the profile query for {user_id} with something other \
                 than a profile: {}",
                response.body
            )),
            404 => Ok(None),
            status => Err(format!(
                "{destination} answered the profile query for {user_id} with HTTP {status}: {}",
                response.body
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_names_the_user_and_the_field_percent_encoded() {
        let user = UserId::parse("@alice:remote.example:8448").unwrap();
        assert_eq!(
            query_path(&user, Some("displayname")),
            "/_matrix/federation/v1/query/profile?user_id=%40alice%3Aremote.example%3A8448&field=displayname"
        );
        assert_eq!(
            query_path(&user, None),
            "/_matrix/federation/v1/query/profile?user_id=%40alice%3Aremote.example%3A8448"
        );
    }
}
