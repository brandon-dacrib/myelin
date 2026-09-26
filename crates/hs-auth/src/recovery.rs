//! Administrator recovery: getting an operator back into a server nobody can sign in to.
//!
//! The first-run setup link ([`crate::setup`]) creates the first administrator. Once one exists
//! that offer closes for good, and every other way of administering this server goes through an
//! administrator's session -- which is exactly what an operator who has forgotten the only
//! administrator's password does not have. Before this module the way back in was to configure
//! a registration shared secret (on Kubernetes, a `helm upgrade` and a restart), register a
//! second administrator with `hs register --admin`, sign in as it, reset the first one's
//! password from the interface, and deactivate the second: four tools and a restart, found out
//! on the first real install within minutes of its first administrator being made.
//!
//! Now `hs recover`, run where the server keeps its signing key, asks the running server for a
//! one-time *recovery link*. Opening it shows the administrator accounts, takes a new password
//! for one of them, signs out every session that account had, and signs the operator in.
//!
//! # Who may ask for a link
//!
//! Whoever can sign with this server's Ed25519 signing key. That is the one secret the server
//! could not function without, it is already where an operator can reach it and nobody else can
//! (the data volume, the mounted Secret, the directory `hs serve` was given), and holding it is
//! *already* being the server: its holder can sign events and federation requests as it. A
//! recovery link adds nothing to that. So the request `hs recover` sends carries the key ID, a
//! timestamp, a random nonce and a signature over the three, and the server accepts it if the
//! signature verifies under its own key, the timestamp is within [`REQUEST_WINDOW_MS`] of its
//! clock, and the nonce has not been seen. A refusal says only that it was refused.
//!
//! Compare the alternatives. A flag or environment variable at start-up would work everywhere
//! but costs a restart, and a variable left set would re-offer a link at every start after. A
//! loopback-only endpoint with no signature would let any local process on a shared host mint
//! a link. A separate operator secret would be one more thing to keep, and in cluster mode one
//! more Secret to mount. The signing key is already there in every mode.
//!
//! # What keeps the link to one use
//!
//! The link carries a 40-character random token in its *fragment*, which browsers do not send,
//! so it cannot land in an access log or a proxy's. The token is stored with an expiry of
//! [`LINK_LIFETIME_MS`]; a newer `hs recover` replaces it. [`AdministratorRecovery::reset_password`]
//! checks the token before anything else, so a caller without it learns nothing, then consumes
//! it atomically ([`crate::store::SetupStore::consume_recovery_token`]) before changing anything, so of any
//! number of concurrent requests carrying the right token exactly one proceeds. If the reset
//! then fails, the token is put back rather than leaving the operator locked out with a spent
//! link.
//!
//! A server with no *active* administrator has nobody to recover: `hs recover` is handed the
//! setup link instead, which creates one. That is also how an operator whose only administrator
//! account was deactivated gets back in.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use hs_admin::model::{
    RecoveryAdministrator, RecoveryInspection, RecoveryLink, RecoveryLinkKind, RecoveryLinkRequest,
    RecoveryResetRequest, SetupSession,
};
use hs_admin::sources::{RecoveryError, RecoverySource, UserDirectory as _};
use ruma::OwnedUserId;

use crate::admin_directory::AuthStoreUserDirectory;
use crate::password;
use crate::session;
use crate::setup::FirstRunSetup;
use crate::state::AuthState;
use crate::store::{RecoveryTokenRecord, StoreError, tokens_match};
use crate::token;

/// How long a recovery link works for: long enough to copy from a terminal into a browser and
/// type a password twice, short enough that one left in a scrollback is not a standing way in.
pub const LINK_LIFETIME_MS: u64 = 15 * 60 * 1000;

/// How far a signed request's timestamp may be from this server's clock, either way. Five
/// minutes is Kerberos's answer to the same question and tolerates a container whose clock is
/// a little off without tolerating a request replayed the next day.
pub const REQUEST_WINDOW_MS: u64 = 5 * 60 * 1000;

/// Nonce bounds. Long enough that a random one is never repeated, short enough that the
/// nonce set cannot be used to fill memory.
const NONCE_LEN: std::ops::RangeInclusive<usize> = 8..=64;

/// The bytes `hs recover` signs and this server verifies: one place, so the two cannot drift.
/// A fixed prefix names the purpose, so a signature made for this can never be mistaken for a
/// signature over an event or a federation request, and vice versa.
#[must_use]
pub fn message_to_sign(requested_at_ms: u64, nonce: &str) -> Vec<u8> {
    format!("hs.recovery-link.v1\n{requested_at_ms}\n{nonce}\n").into_bytes()
}

/// [`RecoverySource`] over this crate's store, session machinery and the server's own verifying
/// key. One per server process.
pub struct AdministratorRecovery {
    state: AuthState,
    setup: FirstRunSetup,
    key_id: String,
    verifying_key: VerifyingKey,
    /// Where links are rooted: the public base URL, or `http://localhost:<port>` once the
    /// listeners are bound. Set by the server after binding, like the setup link's base.
    link_base: RwLock<String>,
    /// Nonces accepted recently, each with the moment it may be forgotten. Per process: in
    /// cluster mode a replay to another replica would issue a link of its own, which is the
    /// same link the same signed request already earned, to the same holder of the key.
    seen_nonces: Mutex<HashMap<String, u64>>,
}

impl AdministratorRecovery {
    /// Shares `state`'s already-open store. `key_id` and `verifying_key` are this server's
    /// current signing key, as `/_matrix/key/v2/server` publishes it. Links are rooted at
    /// `http://localhost:8008` until [`set_link_base`](Self::set_link_base) says otherwise.
    #[must_use]
    pub fn new(state: &AuthState, key_id: impl Into<String>, verifying_key: VerifyingKey) -> Self {
        Self {
            state: state.clone(),
            setup: FirstRunSetup::from_auth_state(state),
            key_id: key_id.into(),
            verifying_key,
            link_base: RwLock::new("http://localhost:8008".to_owned()),
            seen_nonces: Mutex::new(HashMap::new()),
        }
    }

    /// Roots every link issued from now on at `base` (no trailing slash needed).
    pub fn set_link_base(&self, base: impl Into<String>) {
        let base = base.into();
        *self
            .link_base
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            base.trim_end_matches('/').to_owned();
    }

    fn link(&self, path_and_token: &str) -> String {
        let base = self
            .link_base
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        format!("{base}{path_and_token}")
    }

    /// Checks a signed request, and remembers its nonce if it passes. Every refusal is the same
    /// error; the reason goes to the log, which is the operator's, not to the response, which
    /// is anybody's.
    fn verify(&self, request: &RecoveryLinkRequest) -> Result<(), RecoveryError> {
        let now = self.state.now_ms();
        let refuse = |reason: &str| {
            tracing::info!(reason, "a recovery link request did not verify");
            RecoveryError::NotSigned
        };
        if request.key_id != self.key_id {
            return Err(refuse(
                "the request names a key that is not this server's current signing key",
            ));
        }
        if now.abs_diff(request.requested_at_ms) > REQUEST_WINDOW_MS {
            return Err(refuse(
                "the request was signed more than five minutes from this server's clock; check both clocks",
            ));
        }
        if !NONCE_LEN.contains(&request.nonce.len())
            || !request.nonce.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(refuse("the nonce is not 8 to 64 ASCII letters and digits"));
        }
        let raw = STANDARD_NO_PAD
            .decode(&request.signature)
            .map_err(|_| refuse("the signature is not unpadded base64"))?;
        let bytes: [u8; 64] = raw
            .try_into()
            .map_err(|_| refuse("the signature is not 64 bytes"))?;
        let signature = Signature::from_bytes(&bytes);
        self.verifying_key
            .verify(
                &message_to_sign(request.requested_at_ms, &request.nonce),
                &signature,
            )
            .map_err(|_| refuse("the signature does not verify under this server's signing key"))?;

        // Only a request that verified spends its nonce: nothing anybody else sends can use
        // up the operator's.
        let mut seen = self
            .seen_nonces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        seen.retain(|_, forget_at| *forget_at > now);
        if seen.contains_key(&request.nonce) {
            return Err(refuse("the nonce was already used"));
        }
        seen.insert(request.nonce.clone(), now + 2 * REQUEST_WINDOW_MS);
        Ok(())
    }

    async fn active_administrators(&self) -> Result<Vec<OwnedUserId>, StoreError> {
        Ok(self
            .state
            .store
            .list_users()
            .await?
            .into_iter()
            .filter(|u| u.is_admin && !u.deactivated)
            .map(|u| u.user_id)
            .collect())
    }

    /// The outstanding token, if `presented` is it and it has not expired. `Closed` when there
    /// is none or it has expired (an expired token is withdrawn on the way), `BadToken` when it
    /// is not the one presented. Checked before anything else about a request.
    async fn open_token(&self, presented: &str) -> Result<RecoveryTokenRecord, RecoveryError> {
        let store = &self.state.store;
        let Some(record) = store.recovery_token().await.map_err(unavailable)? else {
            return Err(RecoveryError::Closed);
        };
        if record.expires_at_ms <= self.state.now_ms() {
            store.clear_recovery_token().await.map_err(unavailable)?;
            return Err(RecoveryError::Closed);
        }
        if !tokens_match(&record.token, presented) {
            return Err(RecoveryError::BadToken);
        }
        Ok(record)
    }
}

fn unavailable(e: impl std::fmt::Display) -> RecoveryError {
    RecoveryError::Unavailable(e.to_string())
}

#[async_trait]
impl RecoverySource for AdministratorRecovery {
    async fn issue_link(
        &self,
        request: RecoveryLinkRequest,
    ) -> Result<RecoveryLink, RecoveryError> {
        self.verify(&request)?;

        if self
            .active_administrators()
            .await
            .map_err(unavailable)?
            .is_empty()
        {
            // Nobody to recover: the way in is to create an administrator, and the setup link
            // is how. `offer` hands back the one already in the log, or makes one.
            let Some(setup_token) = self.setup.offer().await.map_err(unavailable)? else {
                return Err(RecoveryError::Unavailable(
                    "this server has no active administrator and could not offer setup either"
                        .to_owned(),
                ));
            };
            return Ok(RecoveryLink {
                kind: RecoveryLinkKind::Setup,
                link: self.link(&format!("/admin/setup#token={setup_token}")),
                expires_at_ms: None,
            });
        }

        let recovery_token = token::generate_setup_token();
        let expires_at_ms = self.state.now_ms() + LINK_LIFETIME_MS;
        self.state
            .store
            .set_recovery_token(&recovery_token, expires_at_ms)
            .await
            .map_err(unavailable)?;
        Ok(RecoveryLink {
            kind: RecoveryLinkKind::Recovery,
            link: self.link(&format!("/admin/recover#token={recovery_token}")),
            expires_at_ms: Some(expires_at_ms),
        })
    }

    async fn inspect(&self, recovery_token: &str) -> Result<RecoveryInspection, RecoveryError> {
        let record = self.open_token(recovery_token).await?;
        let administrators = self
            .active_administrators()
            .await
            .map_err(unavailable)?
            .into_iter()
            .map(|user_id| RecoveryAdministrator {
                user_id: user_id.to_string(),
            })
            .collect();
        Ok(RecoveryInspection {
            administrators,
            expires_at_ms: record.expires_at_ms,
        })
    }

    async fn reset_password(
        &self,
        request: RecoveryResetRequest,
    ) -> Result<SetupSession, RecoveryError> {
        let store = &self.state.store;

        // The token first, and nothing else until it is right.
        let record = self.open_token(&request.recovery_token).await?;

        let invalid_user = |detail: String| RecoveryError::Invalid {
            pointer: "/user_id",
            detail,
        };
        let user_id: OwnedUserId = request
            .user_id
            .parse()
            .map_err(|e: ruma::IdParseError| invalid_user(e.to_string()))?;
        if user_id.server_name() != self.state.server_name() {
            return Err(invalid_user(format!(
                "{user_id} is not an account on this server"
            )));
        }
        let account = store
            .get_user(&user_id)
            .await
            .map_err(unavailable)?
            .filter(|u| u.is_admin && !u.deactivated)
            .ok_or_else(|| {
                invalid_user(format!("{user_id} is not an active administrator here"))
            })?;

        self.state
            .config
            .password_policy
            .validate(&request.password)
            .map_err(|e| RecoveryError::Invalid {
                pointer: "/password",
                detail: e.message().to_owned(),
            })?;
        // Hashing is deliberately slow; do it before taking the token so the window in which
        // the token is gone and the password is not yet reset is as short as it can be.
        let password_hash = password::hash_password(&request.password).map_err(unavailable)?;

        if !store
            .consume_recovery_token(&request.recovery_token)
            .await
            .map_err(unavailable)?
        {
            // It matched a moment ago, so somebody else holding it got here first.
            return Err(RecoveryError::Closed);
        }

        // Sign out first, then set: a session that survived a failed sign-out would still be one
        // the old password had opened. Both through the same code an administrator's reset
        // uses. If either fails, put the link back: the operator is no better off and should
        // not have to run `hs recover` again for a fault that was not theirs.
        let directory = AuthStoreUserDirectory::from_auth_state(&self.state);
        let reset = async {
            directory
                .logout_everywhere(account.user_id.as_ref())
                .await
                .map_err(|e| unavailable(e.to_string()))?;
            store
                .set_password_hash(&user_id, Some(password_hash))
                .await
                .map_err(unavailable)
        };
        if let Err(e) = reset.await {
            if let Err(restore) = store
                .set_recovery_token(&record.token, record.expires_at_ms)
                .await
            {
                tracing::error!(error = %restore, "could not restore the recovery token after a failed reset; run `hs recover` again");
            }
            return Err(e);
        }

        let new_session = session::create_session(
            &self.state,
            &user_id,
            None,
            Some("Myelin admin (recovery)".to_owned()),
            false,
        )
        .await
        .map_err(|e| unavailable(e.message()))?;

        tracing::info!(%user_id, "an administrator's password was reset through a recovery link");
        Ok(SetupSession {
            user_id: user_id.to_string(),
            access_token: new_session.access_token,
            device_id: new_session.device_id.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ed25519_dalek::{Signer as _, SigningKey};
    use hs_admin::auth::TokenVerifier;
    use hs_admin::model::Scope;
    use ruma::UserId;

    use super::*;
    use crate::admin_verifier::AdminTokenVerifier;
    use crate::clock::FixedClock;
    use crate::config::{AuthConfig, PasswordPolicy};
    use crate::store::UserRecord;

    const NOW: u64 = 1_700_000_000_000;

    struct Fixture {
        state: AuthState,
        clock: Arc<FixedClock>,
        key: SigningKey,
        recovery: AdministratorRecovery,
    }

    fn fixture() -> Fixture {
        let mut state = AuthState::in_memory_with_config(AuthConfig {
            password_policy: PasswordPolicy {
                minimum_length: Some(8),
                ..PasswordPolicy::default()
            },
            ..AuthConfig::default()
        });
        let clock = Arc::new(FixedClock::new(NOW));
        state.clock = clock.clone();
        let key = SigningKey::generate(&mut rand_core::OsRng);
        let recovery = AdministratorRecovery::new(&state, "ed25519:a_test", key.verifying_key());
        recovery.set_link_base("https://matrix.example.org/");
        Fixture {
            state,
            clock,
            key,
            recovery,
        }
    }

    fn signed(key: &SigningKey, requested_at_ms: u64, nonce: &str) -> RecoveryLinkRequest {
        let signature = key.sign(&message_to_sign(requested_at_ms, nonce));
        RecoveryLinkRequest {
            key_id: "ed25519:a_test".to_owned(),
            requested_at_ms,
            nonce: nonce.to_owned(),
            signature: STANDARD_NO_PAD.encode(signature.to_bytes()),
        }
    }

    async fn add_user(state: &AuthState, localpart: &str, admin: bool, deactivated: bool) {
        let user_id = UserId::parse_with_server_name(localpart, state.server_name()).unwrap();
        let mut record = UserRecord::new(user_id, 0);
        record.is_admin = admin;
        record.deactivated = deactivated;
        record.password_hash = Some(password::hash_password("the old password").unwrap());
        state.store.create_user(record).await.unwrap();
    }

    fn token_of(link: &str) -> &str {
        link.split_once("#token=").unwrap().1
    }

    fn reset(token: &str, user_id: &str, password: &str) -> RecoveryResetRequest {
        RecoveryResetRequest {
            recovery_token: token.to_owned(),
            user_id: user_id.to_owned(),
            password: password.to_owned(),
        }
    }

    #[test]
    fn the_message_is_a_fixed_layout_the_cli_can_reproduce() {
        assert_eq!(
            message_to_sign(1234, "abcdefgh"),
            b"hs.recovery-link.v1\n1234\nabcdefgh\n"
        );
    }

    #[tokio::test]
    async fn only_a_fresh_request_signed_by_this_servers_key_gets_a_link() {
        let f = fixture();
        add_user(&f.state, "ops", true, false).await;

        // The wrong key, even with everything else right.
        let other = SigningKey::generate(&mut rand_core::OsRng);
        assert_eq!(
            f.recovery
                .issue_link(signed(&other, NOW, "nonce00001"))
                .await
                .unwrap_err(),
            RecoveryError::NotSigned
        );
        // The right key naming a key ID that is not the current one.
        let mut renamed = signed(&f.key, NOW, "nonce00002");
        renamed.key_id = "ed25519:old".to_owned();
        assert_eq!(
            f.recovery.issue_link(renamed).await.unwrap_err(),
            RecoveryError::NotSigned
        );
        // A tampered message: the signature was over a different timestamp.
        let mut tampered = signed(&f.key, NOW, "nonce00003");
        tampered.requested_at_ms += 1;
        assert_eq!(
            f.recovery.issue_link(tampered).await.unwrap_err(),
            RecoveryError::NotSigned
        );
        // Too old, too far ahead, and the boundaries.
        for at in [NOW - REQUEST_WINDOW_MS - 1, NOW + REQUEST_WINDOW_MS + 1] {
            assert_eq!(
                f.recovery
                    .issue_link(signed(&f.key, at, "nonce00004"))
                    .await
                    .unwrap_err(),
                RecoveryError::NotSigned,
                "{at}"
            );
        }
        // A nonce that is too short, or not what a nonce looks like.
        for nonce in ["short", "has a space", ""] {
            assert_eq!(
                f.recovery
                    .issue_link(signed(&f.key, NOW, nonce))
                    .await
                    .unwrap_err(),
                RecoveryError::NotSigned,
                "{nonce:?}"
            );
        }
        // Nothing above spent the nonces a real request will use.
        assert!(f.recovery.seen_nonces.lock().unwrap().is_empty());

        let link = f
            .recovery
            .issue_link(signed(&f.key, NOW - REQUEST_WINDOW_MS, "nonce00005"))
            .await
            .unwrap();
        assert_eq!(link.kind, RecoveryLinkKind::Recovery);
        assert!(
            link.link
                .starts_with("https://matrix.example.org/admin/recover#token="),
            "{}",
            link.link
        );
        assert_eq!(token_of(&link.link).len(), 40);
        assert_eq!(link.expires_at_ms, Some(NOW + LINK_LIFETIME_MS));

        // The same signed request again is a replay.
        assert_eq!(
            f.recovery
                .issue_link(signed(&f.key, NOW - REQUEST_WINDOW_MS, "nonce00005"))
                .await
                .unwrap_err(),
            RecoveryError::NotSigned
        );
        // A newer request replaces the link: the old token opens nothing.
        let newer = f
            .recovery
            .issue_link(signed(&f.key, NOW, "nonce00006"))
            .await
            .unwrap();
        assert_eq!(
            f.recovery.inspect(token_of(&link.link)).await.unwrap_err(),
            RecoveryError::BadToken
        );
        assert!(f.recovery.inspect(token_of(&newer.link)).await.is_ok());
    }

    #[tokio::test]
    async fn a_server_with_nobody_to_recover_is_handed_its_setup_link() {
        let f = fixture();
        add_user(&f.state, "alice", false, false).await;
        add_user(&f.state, "gone", true, true).await;
        let link = f
            .recovery
            .issue_link(signed(&f.key, NOW, "nonce00001"))
            .await
            .unwrap();
        assert_eq!(link.kind, RecoveryLinkKind::Setup);
        assert!(
            link.link
                .starts_with("https://matrix.example.org/admin/setup#token="),
            "{}",
            link.link
        );
        assert_eq!(link.expires_at_ms, None);
        // It is the very token the setup page accepts.
        assert_eq!(
            f.recovery.setup.offer().await.unwrap().as_deref(),
            Some(token_of(&link.link))
        );
        // And no recovery token was made.
        assert_eq!(f.state.store.recovery_token().await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_link_resets_one_administrators_password_once_and_signs_them_in() {
        let f = fixture();
        add_user(&f.state, "ops", true, false).await;
        add_user(&f.state, "sam", true, false).await;
        add_user(&f.state, "alice", false, false).await;
        add_user(&f.state, "gone", true, true).await;
        let ops = UserId::parse("@ops:example.org").unwrap();

        // A session the old password opened, which must not survive the reset.
        let old_session = session::create_session(&f.state, &ops, None, None, false)
            .await
            .unwrap();

        assert_eq!(
            f.recovery.inspect("anything").await.unwrap_err(),
            RecoveryError::Closed,
            "nothing is open before a link is issued"
        );
        let link = f
            .recovery
            .issue_link(signed(&f.key, NOW, "nonce00001"))
            .await
            .unwrap();
        let token = token_of(&link.link);

        let inspection = f.recovery.inspect(token).await.unwrap();
        assert_eq!(
            inspection
                .administrators
                .iter()
                .map(|a| a.user_id.as_str())
                .collect::<Vec<_>>(),
            ["@ops:example.org", "@sam:example.org"],
            "active administrators only, in store order"
        );
        assert_eq!(inspection.expires_at_ms, NOW + LINK_LIFETIME_MS);

        // Whatever else is wrong, a caller without the token is told only that.
        for (user_id, password) in [
            ("@alice:example.org", "correct horse battery"),
            ("@ops:example.org", "short"),
            ("@ops:example.org", "correct horse battery"),
        ] {
            for guess in ["", "wrong", &token[..39]] {
                assert_eq!(
                    f.recovery
                        .reset_password(reset(guess, user_id, password))
                        .await
                        .unwrap_err(),
                    RecoveryError::BadToken,
                    "{user_id} / {guess:?}"
                );
            }
        }
        // With the token, a request that cannot be used says which field, and spends nothing.
        for (user_id, password, pointer) in [
            ("not a user id", "correct horse battery", "/user_id"),
            ("@ops:elsewhere.org", "correct horse battery", "/user_id"),
            ("@nobody:example.org", "correct horse battery", "/user_id"),
            ("@alice:example.org", "correct horse battery", "/user_id"),
            ("@gone:example.org", "correct horse battery", "/user_id"),
            ("@ops:example.org", "short", "/password"),
        ] {
            match f
                .recovery
                .reset_password(reset(token, user_id, password))
                .await
            {
                Err(RecoveryError::Invalid { pointer: got, .. }) => {
                    assert_eq!(got, pointer, "{user_id} / {password}")
                }
                other => panic!("{user_id} / {password}: expected Invalid, got {other:?}"),
            }
        }
        assert!(
            f.recovery.inspect(token).await.is_ok(),
            "the link is still open"
        );

        let session = f
            .recovery
            .reset_password(reset(token, "@ops:example.org", "correct horse battery"))
            .await
            .unwrap();
        assert_eq!(session.user_id, "@ops:example.org");

        // The new password is the one that works, the old session is gone, the new one is an
        // administrator's, and the other administrator is untouched.
        let record = f.state.store.get_user(&ops).await.unwrap().unwrap();
        let hash = record.password_hash.expect("a password hash");
        assert!(password::verify_password("correct horse battery", &hash, "").unwrap());
        assert!(!password::verify_password("the old password", &hash, "").unwrap());
        let verifier = AdminTokenVerifier::from_auth_state(&f.state);
        assert!(
            verifier.verify(&old_session.access_token).await.is_err(),
            "the session the old password opened survived"
        );
        let principal = verifier.verify(&session.access_token).await.unwrap();
        assert_eq!(principal.id, "@ops:example.org");
        assert!(principal.has_scope(Scope::AdminWrite));
        let sam = UserId::parse("@sam:example.org").unwrap();
        let sam_hash = f
            .state
            .store
            .get_user(&sam)
            .await
            .unwrap()
            .unwrap()
            .password_hash
            .unwrap();
        assert!(password::verify_password("the old password", &sam_hash, "").unwrap());

        // And that was the only time.
        assert_eq!(
            f.recovery.inspect(token).await.unwrap_err(),
            RecoveryError::Closed
        );
        assert_eq!(
            f.recovery
                .reset_password(reset(token, "@sam:example.org", "correct horse battery"))
                .await
                .unwrap_err(),
            RecoveryError::Closed
        );
    }

    #[tokio::test]
    async fn a_link_stops_working_when_it_expires() {
        let f = fixture();
        add_user(&f.state, "ops", true, false).await;
        let link = f
            .recovery
            .issue_link(signed(&f.key, NOW, "nonce00001"))
            .await
            .unwrap();
        let token = token_of(&link.link);
        f.clock.advance(LINK_LIFETIME_MS - 1);
        assert!(f.recovery.inspect(token).await.is_ok());
        f.clock.advance(1);
        assert_eq!(
            f.recovery.inspect(token).await.unwrap_err(),
            RecoveryError::Closed
        );
        assert_eq!(
            f.recovery
                .reset_password(reset(token, "@ops:example.org", "correct horse battery"))
                .await
                .unwrap_err(),
            RecoveryError::Closed
        );
        assert_eq!(
            f.state.store.recovery_token().await.unwrap(),
            None,
            "an expired token is withdrawn, not left lying around"
        );
        // A fresh `hs recover` after the clock moved on works, with a fresh nonce.
        let again = f
            .recovery
            .issue_link(signed(&f.key, NOW + LINK_LIFETIME_MS, "nonce00002"))
            .await
            .unwrap();
        assert!(f.recovery.inspect(token_of(&again.link)).await.is_ok());
    }
}
