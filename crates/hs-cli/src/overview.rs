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
//! - `federation_destinations_failing_count` and `pending_reports_count` (open reports), once
//!   their sources are set.
//! - `media_count` / `media_bytes`: stored uploads and cached remote copies, once the media
//!   source is set. An empty repository reports zero; an absent source leaves the fields out.
//!
//! # Cost
//!
//! Counting active users reads every account's devices, and the dashboard polls every thirty
//! seconds from every open tab. So the statistics are computed at most once per
//! [`STATISTICS_TTL`] and shared; callers that arrive during a computation wait for it rather
//! than starting their own. `pending_reports_count` is the exception: it is cheap, and it is
//! the number a moderator changes by filing or deciding a report, so it is read fresh on every
//! call.

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
    /// Where the count of failing federation destinations comes from. Absent until set, and
    /// the count is then absent too rather than zero.
    federation: OnceLock<Arc<dyn hs_admin::sources::FederationSource>>,
    /// Where the count of open reports comes from. Absent until set, and the count is then
    /// absent too.
    reports: OnceLock<Arc<dyn hs_admin::reports::ReportSource>>,
    /// Where the media counts come from. Absent until set, and the counts are then absent too.
    media: OnceLock<Arc<dyn hs_admin::media::MediaSource>>,
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
            federation: OnceLock::new(),
            reports: OnceLock::new(),
            media: OnceLock::new(),
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

    /// Hands over the federation source, so the overview can count failing destinations.
    pub fn set_federation(&self, federation: Arc<dyn hs_admin::sources::FederationSource>) {
        let _ = self.federation.set(federation);
    }

    /// Hands over the media repository, so the overview can count what it holds (every upload
    /// and cached remote copy) and how many bytes that is.
    pub fn set_media(&self, media: Arc<dyn hs_admin::media::MediaSource>) {
        let _ = self.media.set(media);
    }

    /// Hands over the reports, so the overview can count the ones awaiting action.
    pub fn set_reports(&self, reports: Arc<dyn hs_admin::reports::ReportSource>) {
        let _ = self.reports.set(reports);
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
        let failing = match self.federation.get() {
            Some(federation) => Some(
                federation
                    .list_destinations()
                    .await?
                    .iter()
                    .filter(|d| d.failing_since.is_some())
                    .count() as u64,
            ),
            None => None,
        };
        let pending_reports_count = match self.reports.get() {
            Some(reports) => Some(reports.open_count().await?),
            None => None,
        };
        let (media_count, media_bytes) = match self.media.get() {
            Some(media) => {
                let items = media.list().await?;
                (
                    Some(items.len() as u64),
                    Some(items.iter().map(|m| m.size_bytes).sum()),
                )
            }
            None => (None, None),
        };
        Ok(StatisticsOverview {
            users_count: Some(users),
            rooms_count: Some(rooms),
            media_count,
            media_bytes,
            daily_active_users: Some(daily),
            monthly_active_users: Some(monthly),
            federation_destinations_failing_count: failing,
            pending_reports_count,
        })
    }
}

#[async_trait]
impl<B: KvBackend + 'static> OverviewSource for ServerOverview<B> {
    async fn statistics(&self) -> Result<StatisticsOverview, SourceError> {
        // Held across the count on purpose: a second caller should wait for this answer, not
        // start reading every account again beside it.
        let mut cached = self.cached.lock().await;
        let mut statistics = match cached.as_ref() {
            Some((at, statistics)) if at.elapsed() < self.ttl => statistics.clone(),
            _ => {
                let statistics = self.count().await?;
                *cached = Some((Instant::now(), statistics.clone()));
                statistics
            }
        };
        drop(cached);
        // The open-report count reads the reports alone, and it is what a moderator just changed:
        // filing or deciding a report has to move the Overview and the sidebar at once, not
        // up to a minute later. So it is never served from the cache.
        if let Some(reports) = self.reports.get() {
            statistics.pending_reports_count = Some(reports.open_count().await?);
        }
        Ok(statistics)
    }

    async fn statistics_now(&self) -> Result<StatisticsOverview, SourceError> {
        self.count().await
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
