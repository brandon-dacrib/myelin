//! Push rules across replicas: `hs-push`'s rule cache kept current over the cluster mesh.
//!
//! Each replica caches the push rules it reads (`hs_push::compiled`). A user's rules can be
//! written on any replica (wherever their `PUT /pushrules` lands) and are read on others: the
//! room owner evaluating events for them, the replica serving their `/sync`. So every write
//! is announced to the other live replicas on `POST /mesh/v1/peer`, route
//! [`RULES_CHANGED_ROUTE`], and a replica told drops its cached copy
//! (`CachedRulesetStore::changed_elsewhere`). What a client is shown (`/sync`'s
//! `m.push_rules`, `GET /pushrules`) is checked against the store's change-seq on every read
//! regardless, and the evaluation path re-checks an entry [`REVALIDATE_AFTER`] after it last
//! did, so a message that does not arrive (a peer restarting, a full queue) leaves a stale
//! copy in use for at most that long (`hs_push::compiled`'s "Across replicas").
//!
//! Delivery is best effort, one message per change per peer, sent in the background: rule
//! changes are a person editing their notification settings, rare enough not to need
//! batching. `hs_push_rule_cache_invalidations_total{source="peer"}` counts the copies dropped
//! on a message, `{source="stale"}` the ones a check found behind.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use hs_cluster::mesh::{Forwarder, PeerHandler, Reply};
use hs_cluster::{Ownership, ReplicaId};
use hs_push::rulesets::{CachedRulesetStore, RulesetChangeFeed, RulesetStore};
use ruma::{OwnedUserId, UserId};
use serde::{Deserialize, Serialize};

/// The mesh route a [`RulesChanged`] travels on.
pub const RULES_CHANGED_ROUTE: &str = "push.rules_changed";
/// The prefix this module's peer handler answers.
const ROUTE_PREFIX: &str = "push.";
/// How long a message may take to reach a peer before it is given up on.
const SEND_DEADLINE: Duration = Duration::from_secs(2);
/// How long a replica uses a cached ruleset for evaluation before checking its change-seq
/// against the store: the bound on a lost message's staleness.
pub const REVALIDATE_AFTER: Duration = Duration::from_secs(30);

/// One change: `user_id`'s push rules were written, at change-seq `seq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesChanged {
    /// Whose rules.
    pub user_id: OwnedUserId,
    /// The change-seq the write landed at.
    pub seq: u64,
}

/// The sending side: tells every other live replica about each change made here.
pub struct MeshRulesetChangeFeed {
    forwarder: Arc<Forwarder>,
    ownership: Arc<dyn Ownership>,
}

impl MeshRulesetChangeFeed {
    /// A feed over `forwarder`, to the replicas `ownership`'s shard map names.
    #[must_use]
    pub fn new(forwarder: Arc<Forwarder>, ownership: Arc<dyn Ownership>) -> Self {
        Self {
            forwarder,
            ownership,
        }
    }

    /// Every other replica that owns at least one shard right now.
    fn peers(&self) -> Vec<ReplicaId> {
        let me = self.ownership.me().clone();
        self.ownership
            .shard_map()
            .borrow()
            .replicas()
            .into_iter()
            .filter(|peer| *peer != me)
            .collect()
    }
}

impl RulesetChangeFeed for MeshRulesetChangeFeed {
    fn changed(&self, user_id: &UserId, seq: u64) {
        let message = RulesChanged {
            user_id: user_id.to_owned(),
            seq,
        };
        let payload = match serde_json::to_vec(&message) {
            Ok(payload) => Bytes::from(payload),
            Err(error) => {
                tracing::warn!(%error, "could not encode a push-rule change for the other replicas");
                return;
            }
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("no runtime to tell the other replicas of a push-rule change on");
            return;
        };
        for peer in self.peers() {
            let (forwarder, payload, user) =
                (self.forwarder.clone(), payload.clone(), user_id.to_owned());
            runtime.spawn(async move {
                match forwarder
                    .send_to_peer(&peer, RULES_CHANGED_ROUTE, payload, SEND_DEADLINE)
                    .await
                {
                    Ok(reply) if reply.status == 200 => {
                        tracing::debug!(%peer, %user, seq, "told a peer of a push-rule change");
                    }
                    Ok(reply) => tracing::info!(
                        %peer,
                        %user,
                        status = reply.status,
                        "a peer refused a push-rule change; its cached copy is checked within \
                         the revalidation interval"
                    ),
                    Err(error) => tracing::info!(
                        %peer,
                        %user,
                        %error,
                        "a push-rule change did not reach a peer; its cached copy is checked \
                         within the revalidation interval"
                    ),
                }
            });
        }
    }
}

/// The receiving side: answers [`RULES_CHANGED_ROUTE`] by dropping the cached copy.
pub struct RulesChangedPeerHandler<S: RulesetStore> {
    rulesets: Arc<CachedRulesetStore<S>>,
}

impl<S: RulesetStore> RulesChangedPeerHandler<S> {
    /// A handler over `rulesets`.
    #[must_use]
    pub fn new(rulesets: Arc<CachedRulesetStore<S>>) -> Self {
        Self { rulesets }
    }
}

#[async_trait::async_trait]
impl<S: RulesetStore + 'static> PeerHandler for RulesChangedPeerHandler<S> {
    async fn handle(&self, from: ReplicaId, route: &str, payload: Bytes) -> Reply {
        if route != RULES_CHANGED_ROUTE {
            return Reply {
                status: 404,
                payload: Bytes::from(format!("no such peer route: {route}")),
            };
        }
        match serde_json::from_slice::<RulesChanged>(&payload) {
            Ok(change) => {
                self.rulesets.changed_elsewhere(&change.user_id, change.seq);
                Reply::ok(Bytes::new())
            }
            Err(error) => Reply {
                status: 400,
                payload: Bytes::from(format!("bad push-rule change from {from}: {error}")),
            },
        }
    }
}

/// Makes `rulesets` cluster-aware over `handles`' mesh: installs the change feed and the
/// revalidation interval, and this replica's handler for its peers' changes. Call it before
/// [`crate::cluster::ClusterHandles::spawn_mesh`]. A no-op in single-node mode.
pub fn install<S: RulesetStore + 'static>(
    handles: &crate::cluster::ClusterHandles,
    rulesets: &Arc<CachedRulesetStore<S>>,
) {
    let Some(forwarder) = handles.forwarder.clone() else {
        return;
    };
    rulesets.install_change_feed(
        Arc::new(MeshRulesetChangeFeed::new(
            forwarder,
            handles.cluster.ownership().clone(),
        )),
        REVALIDATE_AFTER,
    );
    handles.add_peer_handler(
        ROUTE_PREFIX,
        Arc::new(RulesChangedPeerHandler::new(rulesets.clone())),
    );
    tracing::info!(
        revalidate_after_secs = REVALIDATE_AFTER.as_secs(),
        "push rules are cluster-aware: a change on one replica drops the others' cached copy"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_push::rulesets::memory::InMemoryRulesetStore;
    use ruma::user_id;

    #[tokio::test]
    async fn the_handler_drops_the_cached_copy_and_rejects_junk() {
        let rulesets = Arc::new(CachedRulesetStore::new(InMemoryRulesetStore::new()));
        let alice = user_id!("@alice:example.org");
        rulesets.effective_ruleset(alice).await.unwrap();
        assert!(rulesets.cache().peek(alice).is_some());
        let handler = RulesChangedPeerHandler::new(rulesets.clone());
        let from = ReplicaId::new("peer:1");
        let message = serde_json::to_vec(&RulesChanged {
            user_id: alice.to_owned(),
            seq: 1,
        })
        .unwrap();
        let reply = handler
            .handle(from.clone(), RULES_CHANGED_ROUTE, Bytes::from(message))
            .await;
        assert_eq!(reply.status, 200);
        assert!(
            rulesets.cache().peek(alice).is_none(),
            "the copy was dropped"
        );

        let junk = handler
            .handle(from.clone(), RULES_CHANGED_ROUTE, Bytes::from_static(b"{"))
            .await;
        assert_eq!(junk.status, 400);
        let elsewhere = handler.handle(from, "push.other", Bytes::new()).await;
        assert_eq!(elsewhere.status, 404);
    }
}
