//! The numbers on the management interface's Overview page: [`hs_admin::sources::OverviewSource`]
//! over this process's own stores.
//!
//! It lives here rather than in a domain crate because it is the one source that needs all of
//! them at once -- accounts and devices from `hs-auth`, rooms from `hs-room`, the shard map from
//! `hs-cluster` -- and `hs-cli` is where they are all in scope.
//!
//! Until this existed, `GET /statistics/overview` and `GET /cluster` answered an honest `501`,
//! and the first page a new administrator saw, seconds after creating their account, said "Not
//! implemented" for Users, Rooms, Daily active users and Mode.
//!
//! # What is counted, and what is left out
//!
//! - `users_count`: local accounts that are not deactivated.
//! - `daily_active_users` / `monthly_active_users`: of those, the ones any of whose devices was
//!   seen in the last 24 hours / 30 days. "Seen" is `DeviceRecord::last_seen_ms`, which every
//!   authenticated request refreshes.
//! - `rooms_count`: rooms this server has state for.
//! - Media, failing federation destinations and pending reports are **left out**, not zero:
//!   nothing here can count them yet, and the contract makes every field optional so that "not
//!   known" does not have to be dressed up as a number.
//!
//! # Cost
//!
//! Counting active users reads every account's devices, and the dashboard polls every thirty
//! seconds from every open tab. So the statistics are computed at most once per
//! [`STATISTICS_TTL`] and shared; callers that arrive during a computation wait for it rather
//! than starting their own.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hs_admin::model::{ClusterStatus, StatisticsOverview};
use hs_admin::sources::{OverviewSource, SourceError};
use hs_auth::state::AuthState;
use hs_cluster::Ownership;
use hs_kv::KvBackend;
use hs_room::registry::RoomRegistry;

/// How long a computed [`StatisticsOverview`] is served before it is counted again.
pub const STATISTICS_TTL: Duration = Duration::from_secs(60);

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// [`OverviewSource`] for a running `hs serve`.
pub struct ServerOverview<B: KvBackend> {
    auth: AuthState,
    rooms: Arc<RoomRegistry<B>>,
    single_node: bool,
    /// Set once the cluster has started, which is after the admin router is assembled.
    ownership: OnceLock<Arc<dyn Ownership>>,
    cached: tokio::sync::Mutex<Option<(Instant, StatisticsOverview)>>,
    ttl: Duration,
}

impl<B: KvBackend + 'static> ServerOverview<B> {
    /// `auth` and `rooms` are the already-open handles the rest of the server uses.
    #[must_use]
    pub fn new(auth: &AuthState, rooms: Arc<RoomRegistry<B>>, single_node: bool) -> Self {
        Self {
            auth: auth.clone(),
            rooms,
            single_node,
            ownership: OnceLock::new(),
            cached: tokio::sync::Mutex::new(None),
            ttl: STATISTICS_TTL,
        }
    }

    /// The same, recounting after `ttl` instead of [`STATISTICS_TTL`].
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Hands over the cluster's ownership view once it exists. Later calls are ignored.
    pub fn set_ownership(&self, ownership: Arc<dyn Ownership>) {
        let _ = self.ownership.set(ownership);
    }

    async fn count(&self) -> Result<StatisticsOverview, SourceError> {
        let unavailable = |e: hs_auth::store::StoreError| SourceError::Unavailable(e.to_string());
        let now = self.auth.now_ms();
        let (mut users, mut daily, mut monthly) = (0u64, 0u64, 0u64);
        for user in self.auth.store.list_users().await.map_err(unavailable)? {
            if user.deactivated {
                continue;
            }
            users += 1;
            let last_seen = self
                .auth
                .store
                .list_devices(&user.user_id)
                .await
                .map_err(unavailable)?
                .iter()
                .filter_map(|d| d.last_seen_ms)
                .max();
            if let Some(seen) = last_seen {
                let ago = now.saturating_sub(seen);
                if ago <= DAY_MS {
                    daily += 1;
                }
                if ago <= 30 * DAY_MS {
                    monthly += 1;
                }
            }
        }
        let rooms = self
            .rooms
            .list_all_room_ids()
            .map_err(|e| SourceError::Unavailable(e.to_string()))?
            .len() as u64;
        Ok(StatisticsOverview {
            users_count: Some(users),
            rooms_count: Some(rooms),
            daily_active_users: Some(daily),
            monthly_active_users: Some(monthly),
            ..StatisticsOverview::default()
        })
    }
}

#[async_trait]
impl<B: KvBackend + 'static> OverviewSource for ServerOverview<B> {
    async fn statistics(&self) -> Result<StatisticsOverview, SourceError> {
        // Held across the count on purpose: a second caller should wait for this answer, not
        // start reading every account again beside it.
        let mut cached = self.cached.lock().await;
        if let Some((at, statistics)) = cached.as_ref()
            && at.elapsed() < self.ttl
        {
            return Ok(statistics.clone());
        }
        let statistics = self.count().await?;
        *cached = Some((Instant::now(), statistics.clone()));
        Ok(statistics)
    }

    async fn cluster(&self) -> Result<ClusterStatus, SourceError> {
        if self.single_node {
            return Ok(ClusterStatus {
                mode: "single-node".to_owned(),
                epoch: None,
                replica_count: Some(1),
                shard_count: None,
            });
        }
        // The shard map is what this replica currently believes; the counts are of that belief.
        let map = self.ownership.get().map(|o| o.shard_map().borrow().clone());
        Ok(ClusterStatus {
            mode: "cluster".to_owned(),
            epoch: None,
            replica_count: map.as_ref().map(|m| m.owning_replica_count().max(1) as u64),
            shard_count: map.as_ref().map(|m| m.owned_shard_count() as u64),
        })
    }
}
