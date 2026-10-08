//! Asking an appservice about a user of its namespace before it is sent an event naming them.
//!
//! Synapse asks every appservice whose user namespace covers an unknown local user (`GET
//! /_matrix/app/v1/users/{userId}`) before it queues an event that names them
//! (`ApplicationServicesHandler._check_user_exists`): it is how a bridge creates the ghost
//! someone just invited before it hears of the invite (Sytest's "Inviting an AS-hosted user asks
//! the AS server"). Until 2026-10-08 this server asked from the room pump, which is one task for
//! every room and every appservice, so a bridge that never answered held every bridge's delivery
//! for the query's ten-second timeout (decision 0030's consequence).
//!
//! It is asked here instead: by the appservice's own delivery worker ([`crate::delivery`],
//! through [`crate::scheduler::Scheduler::with_user_queries`]), just before it sends a batch,
//! and only of that appservice. An event naming a user of an appservice's namespace is always
//! one that appservice is sent (the sender or a membership's target is one of its users,
//! [`crate::pump::interested`]), so asking the appservice it is about to be sent to asks every
//! appservice Synapse would have asked, each in its own time. The bridge still hears of the user
//! only after it was asked about them; a bridge that never answers holds only its own
//! transactions, and only for the timeout, once per unknown user per
//! [`UNKNOWN_USER_RETRY_MS`].

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hs_kv::KvBackend;
use serde_json::Value;

use crate::namespace::NamespaceKind;
use crate::query::QueryService;
use crate::store::AppserviceRow;

/// Whether a local user ID is a registered account: what is looked up before an appservice is
/// asked about a user. A trait because the accounts are `hs-auth`'s; `hs-cli` implements it over
/// them.
#[async_trait]
pub trait LocalUsers: Send + Sync {
    /// True if `user_id` (a full user ID on this server) has an account.
    async fn is_registered(&self, user_id: &str) -> bool;
}

/// How long an appservice is not asked again about a user it did not provide.
pub const UNKNOWN_USER_RETRY_MS: u64 = 60_000;

/// See the module docs: who to look users up in, who to ask, and whom each appservice was asked
/// about lately without providing them.
pub struct UserQueries<B: KvBackend> {
    users: Arc<dyn LocalUsers>,
    queries: Arc<QueryService<B>>,
    /// `(appservice id, user ID) -> when that appservice last said no` (or did not answer).
    unknown: Mutex<HashMap<(String, String), u64>>,
}

impl<B: KvBackend> UserQueries<B> {
    /// Looks users up in `users` and asks appservices through `queries`.
    #[must_use]
    pub fn new(users: Arc<dyn LocalUsers>, queries: Arc<QueryService<B>>) -> Self {
        Self {
            users,
            queries,
            unknown: Mutex::new(HashMap::new()),
        }
    }

    /// The users `body`'s events name (each event's sender, and a membership event's target)
    /// that `row`'s appservice might be asked about: local, in its user namespace, and not its
    /// bot. In the order they first appear.
    fn candidates(&self, row: &AppserviceRow, body: &Value) -> Vec<String> {
        let Some(events) = body.get("events").and_then(Value::as_array) else {
            return Vec::new();
        };
        if events.is_empty() {
            return Vec::new();
        }
        let Ok(namespaces) = row.namespaces.compile() else {
            return Vec::new();
        };
        let registry = self.queries.registry();
        let server_name = registry.server_name();
        let bot = format!("@{}:{}", row.sender_localpart, server_name);
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for event in events {
            let field = |name: &str| event.get(name).and_then(Value::as_str);
            let mut named = vec![field("sender")];
            if field("type") == Some("m.room.member") {
                named.push(field("state_key"));
            }
            for user_id in named.into_iter().flatten() {
                if user_id == bot || !seen.insert(user_id) {
                    continue;
                }
                let local = ruma::UserId::parse(user_id)
                    .is_ok_and(|parsed| parsed.server_name() == server_name);
                if local && namespaces.is_interested(NamespaceKind::Users, user_id) {
                    out.push(user_id.to_owned());
                }
            }
        }
        out
    }

    fn recently_unknown(&self, appservice: &str, user_id: &str, now: u64) -> bool {
        self.unknown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(appservice.to_owned(), user_id.to_owned()))
            .is_some_and(|at| now.saturating_sub(*at) < UNKNOWN_USER_RETRY_MS)
    }

    /// Before `row`'s appservice is sent `body`: asks it about each user `body`'s events name
    /// who is in its user namespace and has no account, and waits for the answers (asked at
    /// once, so several unknown users cost one timeout, not one each). Returns how many it was
    /// asked about. Never fails: an appservice that does not answer is logged and counted by
    /// [`QueryService`], and the transaction is sent anyway, as Synapse sends it.
    pub async fn before_delivery(&self, row: &AppserviceRow, body: &Value) -> usize {
        if row.url.is_none() {
            return 0;
        }
        let now = self.queries.registry().now_ms();
        let mut to_ask = Vec::new();
        for user_id in self.candidates(row, body) {
            if self.recently_unknown(&row.id, &user_id, now)
                || self.users.is_registered(&user_id).await
            {
                continue;
            }
            to_ask.push(user_id);
        }
        if to_ask.is_empty() {
            return 0;
        }
        let answers = futures::future::join_all(to_ask.iter().map(|user_id| async move {
            (user_id, self.queries.user_exists_at(row, user_id).await)
        }))
        .await;
        let mut unknown = self
            .unknown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (user_id, provided) in answers {
            let key = (row.id.clone(), user_id.clone());
            if provided {
                unknown.remove(&key);
            } else {
                tracing::debug!(appservice = %row.id, user_id, "the appservice did not provide a user an event for it names");
                unknown.insert(key, now);
            }
        }
        // Forget what has aged out, so the map is bounded by the users named in a minute.
        unknown.retain(|_, at| now.saturating_sub(*at) < UNKNOWN_USER_RETRY_MS);
        to_ask.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registration::Registration;
    use crate::registry::Registry;
    use hs_kv::memory::MemoryBackend;
    use serde_json::json;

    /// The accounts, as a set the transport below adds to when the bridge is asked.
    #[derive(Default)]
    struct Accounts(Mutex<BTreeSet<String>>);

    #[async_trait]
    impl LocalUsers for Accounts {
        async fn is_registered(&self, user_id: &str) -> bool {
            self.0.lock().unwrap().contains(user_id)
        }
    }

    /// A bridge that provides `@irc_bob` when asked (registering him, as a real bridge does
    /// before it answers) and nobody else, recording what it was asked.
    struct ProvidesBob {
        accounts: Arc<Accounts>,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl crate::query::AppserviceQueryTransport for ProvidesBob {
        async fn get(
            &self,
            url: &str,
            _hs_token: &str,
            path_and_query: &str,
        ) -> Result<Option<Value>, String> {
            self.asked
                .lock()
                .unwrap()
                .push(format!("{url}{path_and_query}"));
            if path_and_query == "/users/%40irc_bob%3Aexample.org" {
                self.accounts
                    .0
                    .lock()
                    .unwrap()
                    .insert("@irc_bob:example.org".to_owned());
                return Ok(Some(json!({})));
            }
            Ok(None)
        }
    }

    const IRC: &str = "id: irc\nurl: 'http://irc'\nas_token: as_irc\n\
        hs_token: hs_irc\nsender_localpart: ircbot\nnamespaces:\n  users:\n    \
        - regex: '@irc_.*:example\\.org'\n      exclusive: true\n";
    const SLACK: &str = "id: slack\nurl: 'http://slack'\nas_token: as_slack\n\
        hs_token: hs_slack\nsender_localpart: slackbot\nnamespaces:\n  users:\n    \
        - regex: '@slack_.*:example\\.org'\n      exclusive: true\n";

    fn invite(target: &str) -> Value {
        json!({
            "type": "m.room.member",
            "sender": "@alice:example.org",
            "state_key": target,
            "content": {"membership": "invite"},
        })
    }

    /// Sytest's "Inviting an AS-hosted user asks the AS server", asked by the appservice's own
    /// worker: only the appservice about to be sent the batch is asked, only about the unknown
    /// local users of its namespace, once; nobody is asked about a person with an account, its
    /// bot, a user of another server, or another appservice's user.
    #[tokio::test]
    async fn only_the_appservice_being_delivered_to_is_asked_and_only_about_its_unknown_users() {
        let registry = Arc::new(
            Registry::open(MemoryBackend::new(), ruma::server_name!("example.org")).unwrap(),
        );
        let irc = registry
            .add(&Registration::parse_yaml(IRC).unwrap())
            .unwrap();
        registry
            .add(&Registration::parse_yaml(SLACK).unwrap())
            .unwrap();
        let accounts = Arc::new(Accounts::default());
        accounts
            .0
            .lock()
            .unwrap()
            .insert("@irc_known:example.org".to_owned());
        let transport = Arc::new(ProvidesBob {
            accounts: accounts.clone(),
            asked: Mutex::new(Vec::new()),
        });
        let checks = UserQueries::new(
            accounts.clone(),
            Arc::new(QueryService::new(registry.clone(), transport.clone())),
        );

        let body = json!({"events": [
            invite("@irc_bob:example.org"),
            invite("@irc_carol:example.org"),
            invite("@irc_carol:example.org"),
            invite("@irc_known:example.org"),
            invite("@ircbot:example.org"),
            invite("@irc_dave:elsewhere.org"),
            invite("@slack_erin:example.org"),
            invite("@dave:example.org"),
        ]});
        assert_eq!(checks.before_delivery(&irc, &body).await, 2);
        let mut asked = transport.asked.lock().unwrap().clone();
        asked.sort();
        assert_eq!(
            asked,
            vec![
                "http://irc/users/%40irc_bob%3Aexample.org",
                "http://irc/users/%40irc_carol%3Aexample.org",
            ]
        );
        assert!(accounts.is_registered("@irc_bob:example.org").await);

        // Bob is registered now, and carol was not provided: neither is asked about again for a
        // while.
        assert_eq!(checks.before_delivery(&irc, &body).await, 0);
        assert_eq!(transport.asked.lock().unwrap().len(), 2);
    }
}
