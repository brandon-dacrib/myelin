//! First-run setup: giving a server that has no administrator its first one.
//!
//! Every way of administering this server goes through an account with `is_admin` set
//! ([`crate::admin_verifier`]), and a fresh server has no accounts. Until this module the way out
//! of that was to configure a registration shared secret, run `hs register --admin` against the
//! running server, log in over the client-server API for an access token, and paste the token
//! into the management interface -- four steps and two tools before the interface meant to make
//! administration pleasant could be opened at all.
//!
//! Now, for as long as no administrator exists, the server holds a one-time *setup token* and
//! writes a link containing it to its log at every start. Whoever opens the link chooses a
//! username and a password and is signed in as the first administrator.
//!
//! # Why a token, and why the log
//!
//! The alternative is what several self-hosted applications do: the first visitor to an
//! unclaimed server becomes its administrator. That is a race, and on a homeserver -- which is
//! on the public internet by design, often before its operator has finished setting it up --
//! whoever is scanning for port 8448 that afternoon is a serious contender. Reading the server's
//! log is something only its operator can do, and it is something they can already do on every
//! platform this runs on (`docker logs`, `kubectl logs`, `journalctl`), so the token costs them
//! one copy and paste and costs anybody else everything.
//!
//! The link carries the token in its *fragment* (`/admin/setup#token=...`), which browsers do
//! not send to the server: it cannot end up in an access log, a reverse proxy's log, or a
//! `Referer` header.
//!
//! # What keeps it to exactly one administrator
//!
//! [`FirstRunSetup::create_first_admin`] checks the token before anything else, so a caller
//! without it learns nothing -- not whether a username is taken, not what the password policy
//! is. It then consumes the token atomically ([`SetupStore::consume_setup_token`]) *before*
//! creating the account, so of any number of concurrent requests carrying the right token
//! exactly one proceeds. If the account cannot then be created, the token is put back rather
//! than leaving a server that needs setting up and has no way to be.
//!
//! An administrator created some other way closes the offer too: [`FirstRunSetup::offer`]
//! withdraws the token at the next start, and `create_first_admin` re-checks before it acts.

use hs_admin::model::{SetupRequest, SetupSession};
use hs_admin::sources::{SetupError, SetupSource, SourceError};
use ruma::UserId;

use crate::password;
use crate::session;
use crate::state::AuthState;
use crate::store::{StoreError, UserRecord, tokens_match};
use crate::token;

/// [`SetupSource`] over this crate's own store and session machinery.
pub struct FirstRunSetup {
    state: AuthState,
}

impl FirstRunSetup {
    /// Shares `state`'s already-open store, like [`crate::admin_verifier::AdminTokenVerifier`].
    #[must_use]
    pub fn from_auth_state(state: &AuthState) -> Self {
        Self {
            state: state.clone(),
        }
    }

    /// Decides, once at startup, whether this server is offering setup, and returns the token to
    /// put in the log if it is.
    ///
    /// A server with no administrator gets a token: the one already stored if there is one, so
    /// that every restart and every replica prints the same link, and a new one otherwise. A
    /// server *with* an administrator has any leftover token withdrawn -- one can be left over
    /// when the first administrator was made with `hs register --admin` instead.
    ///
    /// # Errors
    /// Propagates store failures. The caller should log and carry on: a server that cannot offer
    /// setup should still serve.
    pub async fn offer(&self) -> Result<Option<String>, StoreError> {
        if self.an_administrator_exists().await? {
            self.state.store.clear_setup_token().await?;
            return Ok(None);
        }
        let token = self
            .state
            .store
            .setup_token_or_insert(&token::generate_setup_token())
            .await?;
        Ok(Some(token))
    }

    /// Whether any account could administer this server. A deactivated administrator cannot sign
    /// in, so it does not count -- which also makes this the way back in for an operator whose
    /// only administrator account is gone.
    ///
    /// This reads every account, so it is only called at startup and from behind a valid setup
    /// token, never on the unauthenticated path.
    async fn an_administrator_exists(&self) -> Result<bool, StoreError> {
        Ok(self
            .state
            .store
            .list_users()
            .await?
            .iter()
            .any(|u| u.is_admin && !u.deactivated))
    }

    /// `alice`, `@alice`, and `@alice:<this server>` all mean the same account; anything naming
    /// another server does not belong here.
    fn user_id_for(&self, username: &str) -> Result<ruma::OwnedUserId, SetupError> {
        let invalid = |detail: String| SetupError::Invalid {
            pointer: "/username",
            detail,
        };
        let trimmed = username.trim();
        let localpart = match trimmed.strip_prefix('@').unwrap_or(trimmed).split_once(':') {
            None => trimmed.strip_prefix('@').unwrap_or(trimmed),
            Some((localpart, server)) if server == self.state.server_name().as_str() => localpart,
            Some((_, server)) => {
                return Err(invalid(format!(
                    "this server is {}, not {server}",
                    self.state.server_name()
                )));
            }
        };
        if localpart.is_empty() {
            return Err(invalid("choose a username".to_owned()));
        }
        // Lower-cased for the same reason `POST /register` does it: `Ops` and `ops` must not be
        // two accounts.
        let localpart = localpart.to_ascii_lowercase();
        let user_id = UserId::parse_with_server_name(localpart.as_str(), self.state.server_name())
            .map_err(|_| {
                invalid(format!(
                    "\"{localpart}\" cannot be a username: use lowercase letters, digits, and any of . _ = - /"
                ))
            })?;
        // `parse_with_server_name` accepts historical localparts that a new account should not
        // be given; hold a new administrator to the strict grammar.
        if !localpart
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._=-/+".contains(&b))
        {
            return Err(invalid(format!(
                "\"{localpart}\" cannot be a username: use lowercase letters, digits, and any of . _ = - /"
            )));
        }
        Ok(user_id)
    }
}

fn unavailable(e: impl std::fmt::Display) -> SetupError {
    SetupError::Unavailable(e.to_string())
}

#[async_trait::async_trait]
impl SetupSource for FirstRunSetup {
    async fn needs_setup(&self) -> Result<bool, SourceError> {
        self.state
            .store
            .setup_token()
            .await
            .map(|token| token.is_some())
            .map_err(|e| SourceError::Unavailable(e.to_string()))
    }

    async fn create_first_admin(&self, request: SetupRequest) -> Result<SetupSession, SetupError> {
        let store = &self.state.store;

        // The token first, and nothing else until it is right.
        let Some(offered) = store.setup_token().await.map_err(unavailable)? else {
            return Err(SetupError::Closed);
        };
        if !tokens_match(&offered, &request.setup_token) {
            return Err(SetupError::BadToken);
        }

        let user_id = self.user_id_for(&request.username)?;
        self.state
            .config
            .password_policy
            .validate(&request.password)
            .map_err(|e| SetupError::Invalid {
                pointer: "/password",
                detail: e.message().to_owned(),
            })?;
        if !store
            .is_localpart_available(user_id.localpart())
            .await
            .map_err(unavailable)?
        {
            return Err(SetupError::Invalid {
                pointer: "/username",
                detail: format!("{user_id} already exists"),
            });
        }

        if self.an_administrator_exists().await.map_err(unavailable)? {
            store.clear_setup_token().await.map_err(unavailable)?;
            return Err(SetupError::Closed);
        }

        // Hashing is deliberately slow; do it before taking the token so the window in which the
        // token is gone and the account does not exist yet is as short as it can be.
        let password_hash = password::hash_password(&request.password).map_err(unavailable)?;

        if !store
            .consume_setup_token(&request.setup_token)
            .await
            .map_err(unavailable)?
        {
            // It matched a moment ago, so somebody else holding it got here first.
            return Err(SetupError::Closed);
        }

        let mut record = UserRecord::new(user_id.clone(), self.state.now_ms());
        record.password_hash = Some(password_hash);
        record.is_admin = true;
        if let Err(e) = store.create_user(record).await {
            // Put the offer back: otherwise this server still needs setting up and has just
            // lost the only means of doing it until its next restart.
            if let Err(restore) = store.setup_token_or_insert(&offered).await {
                tracing::error!(error = %restore, "could not restore the setup token after a failed setup; restart the server to be offered a new one");
            }
            return Err(unavailable(e));
        }

        let session = session::create_session(
            &self.state,
            &user_id,
            None,
            Some("Myelin admin (first-run setup)".to_owned()),
            false,
        )
        .await
        .map_err(|e| unavailable(e.message()))?;

        tracing::info!(%user_id, "first-run setup complete: this server has its first administrator");
        Ok(SetupSession {
            user_id: user_id.to_string(),
            access_token: session.access_token,
            device_id: session.device_id.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hs_admin::auth::TokenVerifier;
    use hs_admin::model::Scope;

    use super::*;
    use crate::admin_verifier::AdminTokenVerifier;
    use crate::config::{AuthConfig, PasswordPolicy};

    fn state() -> AuthState {
        AuthState::in_memory_with_config(AuthConfig {
            password_policy: PasswordPolicy {
                minimum_length: Some(8),
                ..PasswordPolicy::default()
            },
            ..AuthConfig::default()
        })
    }

    fn request(token: &str, username: &str, password: &str) -> SetupRequest {
        SetupRequest {
            setup_token: token.to_owned(),
            username: username.to_owned(),
            password: password.to_owned(),
        }
    }

    async fn add_user(state: &AuthState, localpart: &str, admin: bool, deactivated: bool) {
        let user_id = UserId::parse_with_server_name(localpart, state.server_name()).unwrap();
        let mut record = UserRecord::new(user_id, 0);
        record.is_admin = admin;
        record.deactivated = deactivated;
        state.store.create_user(record).await.unwrap();
    }

    #[tokio::test]
    async fn a_fresh_server_offers_setup_and_offers_the_same_token_every_time() {
        let state = state();
        let setup = FirstRunSetup::from_auth_state(&state);
        assert!(
            !setup.needs_setup().await.unwrap(),
            "nothing is on offer before startup asks"
        );

        let first = setup.offer().await.unwrap().expect("a token");
        assert_eq!(first.len(), 40);
        assert!(setup.needs_setup().await.unwrap());

        // A restart, or a second replica over the same database.
        let again = FirstRunSetup::from_auth_state(&state);
        assert_eq!(
            again.offer().await.unwrap().as_deref(),
            Some(first.as_str())
        );
    }

    #[tokio::test]
    async fn ordinary_users_do_not_close_the_offer_and_an_administrator_does() {
        let state = state();
        add_user(&state, "alice", false, false).await;
        let setup = FirstRunSetup::from_auth_state(&state);
        assert!(setup.offer().await.unwrap().is_some());

        // An administrator made some other way, e.g. `hs register --admin`.
        add_user(&state, "ops", true, false).await;
        assert_eq!(setup.offer().await.unwrap(), None);
        assert!(
            !setup.needs_setup().await.unwrap(),
            "the leftover token is withdrawn"
        );
    }

    #[tokio::test]
    async fn a_deactivated_administrator_is_not_an_administrator() {
        let state = state();
        add_user(&state, "gone", true, true).await;
        let setup = FirstRunSetup::from_auth_state(&state);
        assert!(setup.offer().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn the_first_administrator_can_sign_in_everywhere_and_setup_is_then_over() {
        let state = state();
        let setup = FirstRunSetup::from_auth_state(&state);
        let token = setup.offer().await.unwrap().unwrap();

        let session = setup
            .create_first_admin(request(&token, "Ops", "correct horse battery"))
            .await
            .unwrap();
        assert_eq!(session.user_id, "@ops:example.org");

        // The account is real, is an administrator, and has the password that was typed.
        let user_id = UserId::parse(&session.user_id).unwrap();
        let record = state.store.get_user(&user_id).await.unwrap().unwrap();
        assert!(record.is_admin);
        let hash = record.password_hash.expect("a password hash");
        assert!(password::verify_password("correct horse battery", &hash, "").unwrap());
        assert!(!hash.contains("correct horse"));

        // The session it returned is one the admin API accepts, with full access.
        let principal = AdminTokenVerifier::from_auth_state(&state)
            .verify(&session.access_token)
            .await
            .expect("the admin API accepts the new session");
        assert_eq!(principal.id, "@ops:example.org");
        assert!(principal.has_scope(Scope::AdminWrite));

        // And that was the only time.
        assert!(!setup.needs_setup().await.unwrap());
        assert_eq!(
            setup
                .create_first_admin(request(&token, "mallory", "correct horse battery"))
                .await
                .unwrap_err(),
            SetupError::Closed
        );
        assert_eq!(setup.offer().await.unwrap(), None);
    }

    /// Whatever else is wrong with a request, a caller without the token is told only that, so
    /// the endpoint cannot be used to ask which usernames exist or what the password policy is.
    #[tokio::test]
    async fn without_the_token_nothing_else_is_revealed_and_nothing_is_spent() {
        let state = state();
        add_user(&state, "alice", false, false).await;
        let setup = FirstRunSetup::from_auth_state(&state);
        let token = setup.offer().await.unwrap().unwrap();

        for (username, password) in [
            ("alice", "correct horse battery"), // taken
            ("not a username", "correct horse battery"),
            ("@ops:elsewhere.org", "correct horse battery"),
            ("ops", "short"),
            ("ops", "correct horse battery"), // nothing wrong but the token
        ] {
            for guess in ["", "wrong", &token[..39], &format!("{token}x")] {
                assert_eq!(
                    setup
                        .create_first_admin(request(guess, username, password))
                        .await
                        .unwrap_err(),
                    SetupError::BadToken,
                    "{username:?} / {guess:?}"
                );
            }
        }
        // None of that burned the real token.
        assert!(
            setup
                .create_first_admin(request(&token, "ops", "correct horse battery"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_request_that_cannot_be_used_says_which_field_and_leaves_the_offer_open() {
        let state = state();
        add_user(&state, "alice", false, false).await;
        let setup = FirstRunSetup::from_auth_state(&state);
        let token = setup.offer().await.unwrap().unwrap();

        for (username, password, pointer) in [
            ("", "correct horse battery", "/username"),
            ("not a username", "correct horse battery", "/username"),
            ("@ops:elsewhere.org", "correct horse battery", "/username"),
            ("alice", "correct horse battery", "/username"),
            ("Alice", "correct horse battery", "/username"),
            ("ops", "short", "/password"),
        ] {
            match setup
                .create_first_admin(request(&token, username, password))
                .await
            {
                Err(SetupError::Invalid { pointer: got, .. }) => {
                    assert_eq!(got, pointer, "{username:?}")
                }
                other => panic!("{username:?}/{password:?}: expected Invalid, got {other:?}"),
            }
        }
        assert!(setup.needs_setup().await.unwrap());

        // Every spelling of the same local account is accepted.
        for (i, username) in ["ops", "@ops2", "@OPS3:example.org"].iter().enumerate() {
            let state = self::state();
            let setup = FirstRunSetup::from_auth_state(&state);
            let token = setup.offer().await.unwrap().unwrap();
            let session = setup
                .create_first_admin(request(&token, username, "correct horse battery"))
                .await
                .unwrap_or_else(|e| panic!("{username}: {e:?}"));
            let expected = ["@ops:example.org", "@ops2:example.org", "@ops3:example.org"][i];
            assert_eq!(session.user_id, expected);
        }
    }

    #[tokio::test]
    async fn an_administrator_appearing_after_the_offer_closes_it() {
        let state = state();
        let setup = FirstRunSetup::from_auth_state(&state);
        let token = setup.offer().await.unwrap().unwrap();
        add_user(&state, "ops", true, false).await;

        assert_eq!(
            setup
                .create_first_admin(request(&token, "second", "correct horse battery"))
                .await
                .unwrap_err(),
            SetupError::Closed
        );
        assert!(!setup.needs_setup().await.unwrap());
        let second = UserId::parse("@second:example.org").unwrap();
        assert!(state.store.get_user(&second).await.unwrap().is_none());
    }

    /// Two browser tabs, a double-click, or two people both reading the same log: however many
    /// requests carry the right token at once, the server ends up with one administrator.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn of_many_simultaneous_claims_exactly_one_wins() {
        let state = state();
        let setup = Arc::new(FirstRunSetup::from_auth_state(&state));
        let token = setup.offer().await.unwrap().unwrap();

        let mut tasks = Vec::new();
        for i in 0..8 {
            let setup = setup.clone();
            let token = token.clone();
            tasks.push(tokio::spawn(async move {
                setup
                    .create_first_admin(request(
                        &token,
                        &format!("admin{i}"),
                        "correct horse battery",
                    ))
                    .await
            }));
        }
        let mut won = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(_) => won += 1,
                Err(SetupError::Closed) => {}
                Err(other) => panic!("a losing claim should be Closed, got {other:?}"),
            }
        }
        assert_eq!(won, 1);

        let admins = state
            .store
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .filter(|u| u.is_admin)
            .count();
        assert_eq!(admins, 1);
    }
}
