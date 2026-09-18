//! [`HomeserverIdentity`]: what the room actor needs to originate events -- this server's name
//! and its current signing key.

use hs_model::ids::OwnedServerName;
use hs_model::signing::SigningKeyPair;

/// This homeserver's identity for the purpose of building and signing events it originates.
#[derive(Clone)]
pub struct HomeserverIdentity {
    /// This server's name, embedded in every locally-originated event's `sender` and used as the
    /// `signatures` key.
    pub server_name: OwnedServerName,
    /// The key events are signed under. A real deployment rotates keys and keeps old ones around
    /// for verifying already-signed events; this crate only ever signs with the current one.
    pub signing_key: std::sync::Arc<SigningKeyPair>,
}

impl HomeserverIdentity {
    /// A throwaway identity with a freshly generated key, for tests.
    #[must_use]
    pub fn for_tests(server_name: &str) -> Self {
        Self {
            server_name: ruma::ServerName::parse(server_name)
                .expect("valid test server name")
                .to_owned(),
            signing_key: std::sync::Arc::new(SigningKeyPair::generate("1")),
        }
    }
}
