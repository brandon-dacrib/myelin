//! [`hs_admin::statistics::StatisticsSource`] for a running `hs serve`: per-user media usage and
//! the time series behind the Statistics page.
//!
//! Like [`crate::overview`], this lives here because it reads several crates' stores at once:
//! accounts from `hs-auth`, uploads from `hs-media`, reports from `hs-room`.
//!
//! # Counters and gauges
//!
//! See `hs_admin::statistics` for the vocabulary. Counters are computed on request from the
//! timestamps the records carry (an account's `created_at_ms`, an upload's `created_ms`, a
//! report's `received_at`), so they have history from the first record. Gauges are the
//! Overview's numbers (plus this server's media totals, which the Overview leaves out), sampled
//! every [`SAMPLE_INTERVAL`] by [`ServerStatistics::spawn_sampler`] into the `hs_admin.stats_samples`
//! keyspace, keyed by the sample's time; so a gauge's history starts when the server first ran
//! with this code, and a gap while it was down is a gap in the chart, not a made-up line.
//! Samples older than [`SAMPLE_RETENTION`] are dropped as new ones are written.
//!
//! In cluster mode every replica samples. The chart takes the latest sample in each step, so
//! that is more samples, not a different answer.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hs_admin::sources::{OverviewSource, SourceError};
use hs_admin::statistics::{
    MetricInfo, MetricKind, StatisticsSource, TimeseriesPoint, UserMediaStatistic, Window,
};
use hs_auth::state::AuthState;
use hs_kv::{KvBackend, RangeSpec, TransactConfig, transact};
use hs_room::registry::RoomRegistry;
use hs_tables::key::TupleKey;
use hs_tables::keyspace::TypedKeyspace;

/// How often the gauges are sampled.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How long a sample is kept.
pub const SAMPLE_RETENTION: Duration = Duration::from_secs(400 * 24 * 60 * 60);

fn unavailable(e: impl std::fmt::Display) -> SourceError {
    SourceError::Unavailable(e.to_string())
}

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// The statistics source for `hs serve`.
pub struct ServerStatistics<B: KvBackend> {
    backend: B,
    server_name: String,
    auth: AuthState,
    rooms: Arc<RoomRegistry<B>>,
    overview: Arc<dyn OverviewSource>,
    samples: TypedKeyspace<B::Keyspace, (u64,)>,
}

impl<B: KvBackend + 'static> ServerStatistics<B> {
    /// Opens the samples keyspace and holds the handles the rest of the server already opened.
    ///
    /// # Errors
    /// The backend's, if the keyspace cannot be opened.
    pub fn open(
        backend: B,
        server_name: impl Into<String>,
        auth: &AuthState,
        rooms: Arc<RoomRegistry<B>>,
        overview: Arc<dyn OverviewSource>,
    ) -> Result<Self, hs_kv::KvError> {
        let samples = TypedKeyspace::new(backend.keyspace("hs_admin.stats_samples")?);
        Ok(Self {
            backend,
            server_name: server_name.into(),
            auth: auth.clone(),
            rooms,
            overview,
            samples,
        })
    }

    fn uploads(&self) -> Result<Vec<hs_media::usage::Upload>, SourceError> {
        hs_media::usage::local_uploads(&self.backend, &self.server_name).map_err(unavailable)
    }

    /// Takes one sample of every gauge now, and drops samples past retention.
    ///
    /// # Errors
    /// If the Overview cannot be counted or the sample cannot be written.
    pub async fn sample(&self) -> Result<(), SourceError> {
        self.sample_at(now_ms()).await
    }

    async fn sample_at(&self, at_ms: u64) -> Result<(), SourceError> {
        let mut overview = self.overview.statistics_now().await?;
        let uploads = self.uploads()?;
        overview.media_count.get_or_insert(uploads.len() as u64);
        overview
            .media_bytes
            .get_or_insert(uploads.iter().map(|u| u.byte_length).sum());
        let value = serde_json::to_vec(&overview).map_err(unavailable)?;
        let expired_before =
            at_ms.saturating_sub(u64::try_from(SAMPLE_RETENTION.as_millis()).unwrap_or(u64::MAX));
        let snapshot = self.backend.snapshot();
        let expired: Vec<(u64,)> = self
            .samples
            .range(
                &snapshot,
                RangeSpec {
                    start: Bound::Unbounded,
                    end: Bound::Excluded(bytes::Bytes::from((expired_before,).encode())),
                    reverse: false,
                    limit: Some(1_000),
                },
            )
            .filter_map(|item| item.ok().map(|(key, _)| key))
            .collect();
        transact(&self.backend, TransactConfig::default(), |txn| {
            for key in &expired {
                self.samples
                    .delete(txn, key)
                    .map_err(hs_kv::KvError::backend)?;
            }
            self.samples
                .put(txn, &(at_ms,), &value)
                .map_err(hs_kv::KvError::backend)
        })
        .map_err(unavailable)
    }

    /// Samples now, and then every `interval`, until the returned task is aborted. A failed
    /// sample is logged and the next one tried on schedule.
    pub fn spawn_sampler(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                if let Err(error) = self.sample().await {
                    tracing::warn!(%error, "could not sample the statistics");
                }
            }
        })
    }

    fn gauge(&self, name: &str, window: Window) -> Result<Vec<TimeseriesPoint>, SourceError> {
        let from = u64::try_from(window.from_ms.max(0)).unwrap_or(0);
        let until = u64::try_from(window.until_ms.max(0)).unwrap_or(0);
        let snapshot = self.backend.snapshot();
        let spec = RangeSpec {
            start: Bound::Included(bytes::Bytes::from((from,).encode())),
            end: Bound::Excluded(bytes::Bytes::from((until,).encode())),
            reverse: false,
            limit: None,
        };
        let mut samples = Vec::new();
        for item in self.samples.range(&snapshot, spec) {
            let ((at,), value) = item.map_err(unavailable)?;
            let overview: serde_json::Value =
                serde_json::from_slice(&value).map_err(unavailable)?;
            if let Some(value) = overview.get(name).and_then(serde_json::Value::as_f64) {
                samples.push((i64::try_from(at).unwrap_or(i64::MAX), value));
            }
        }
        Ok(window.last(samples))
    }

    async fn counter(
        &self,
        name: &str,
        window: Window,
    ) -> Result<Vec<TimeseriesPoint>, SourceError> {
        let happenings: Vec<(i64, f64)> = match name {
            "users.registered" => self
                .auth
                .store
                .list_users()
                .await
                .map_err(unavailable)?
                .into_iter()
                .filter(|u| !u.is_guest)
                .map(|u| (i64::try_from(u.created_at_ms).unwrap_or(i64::MAX), 1.0))
                .collect(),
            "media.uploaded" => self
                .uploads()?
                .into_iter()
                .map(|u| (i64::try_from(u.created_ms).unwrap_or(i64::MAX), 1.0))
                .collect(),
            "media.uploaded_bytes" => self
                .uploads()?
                .into_iter()
                .map(|u| {
                    (
                        i64::try_from(u.created_ms).unwrap_or(i64::MAX),
                        u.byte_length as f64,
                    )
                })
                .collect(),
            "reports.received" => self
                .rooms
                .reports()
                .list()
                .map_err(unavailable)?
                .into_iter()
                .filter_map(|r| {
                    hs_http::time::parse_rfc3339(&r.received_at).ok().map(|t| {
                        (
                            i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(0),
                            1.0,
                        )
                    })
                })
                .collect(),
            other => {
                return Err(SourceError::Unavailable(format!(
                    "this server does not count {other}"
                )));
            }
        };
        Ok(window.sum(happenings))
    }
}

#[async_trait]
impl<B: KvBackend + 'static> StatisticsSource for ServerStatistics<B> {
    async fn users_media(&self) -> Result<Vec<UserMediaStatistic>, SourceError> {
        let mut by_user: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for upload in self.uploads()? {
            let entry = by_user.entry(upload.uploader).or_default();
            entry.0 += 1;
            entry.1 += upload.byte_length;
        }
        Ok(by_user
            .into_iter()
            .map(|(user_id, (media_count, media_bytes))| UserMediaStatistic {
                user_id,
                media_count,
                media_bytes,
            })
            .collect())
    }

    async fn timeseries(
        &self,
        metric: MetricInfo,
        window: Window,
    ) -> Result<Vec<TimeseriesPoint>, SourceError> {
        match metric.kind {
            MetricKind::Counter => self.counter(metric.name, window).await,
            MetricKind::Gauge => self.gauge(metric.name, window),
        }
    }
}

#[cfg(test)]
mod tests {
    use hs_admin::model::{ClusterStatus, StatisticsOverview};
    use hs_kv::memory::MemoryBackend;
    use hs_media::metadata::{MediaRecord, MetadataStore};

    use super::*;

    struct CountingOverview(std::sync::atomic::AtomicU64);

    #[async_trait]
    impl OverviewSource for CountingOverview {
        async fn statistics(&self) -> Result<StatisticsOverview, SourceError> {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(StatisticsOverview {
                users_count: Some(10 + n),
                ..StatisticsOverview::default()
            })
        }

        async fn cluster(&self) -> Result<ClusterStatus, SourceError> {
            Err(SourceError::NotFound)
        }
    }

    fn upload(id: &str, uploader: &str, bytes: u64, at: u64) -> MediaRecord {
        MediaRecord {
            server_name: "example.org".to_owned(),
            media_id: id.to_owned(),
            content_type: "image/png".to_owned(),
            upload_name: None,
            byte_length: Some(bytes),
            created_ms: at,
            uploader: Some(uploader.to_owned()),
            completed: true,
            expires_at_ms: None,
            quarantined_by: None,
            safe_from_quarantine: false,
            last_accessed_ms: None,
        }
    }

    fn statistics(backend: &MemoryBackend) -> ServerStatistics<MemoryBackend> {
        let rooms = Arc::new(
            RoomRegistry::open(
                backend.clone(),
                hs_room::identity::HomeserverIdentity::for_tests("example.org"),
            )
            .unwrap(),
        );
        ServerStatistics::open(
            backend.clone(),
            "example.org",
            &AuthState::in_memory(),
            rooms,
            Arc::new(CountingOverview(std::sync::atomic::AtomicU64::new(0))),
        )
        .unwrap()
    }

    const HOUR: i64 = 3_600_000;

    #[tokio::test]
    async fn media_usage_is_per_uploader_and_uploads_are_counted_per_step() {
        let backend = MemoryBackend::new();
        let media = MetadataStore::open(backend.clone()).unwrap();
        media
            .put_media(&upload("a", "@alice:example.org", 100, 10))
            .unwrap();
        media
            .put_media(&upload("b", "@alice:example.org", 50, HOUR as u64 + 1))
            .unwrap();
        media
            .put_media(&upload("c", "@bob:example.org", 7, 20))
            .unwrap();
        let stats = statistics(&backend);

        let usage = stats.users_media().await.unwrap();
        assert_eq!(
            usage,
            [
                UserMediaStatistic {
                    user_id: "@alice:example.org".to_owned(),
                    media_count: 2,
                    media_bytes: 150
                },
                UserMediaStatistic {
                    user_id: "@bob:example.org".to_owned(),
                    media_count: 1,
                    media_bytes: 7
                },
            ]
        );

        let window = Window {
            from_ms: 0,
            until_ms: 2 * HOUR,
            step_ms: HOUR,
        };
        let uploaded = stats
            .timeseries(
                hs_admin::statistics::metric("media.uploaded").unwrap(),
                window,
            )
            .await
            .unwrap();
        assert_eq!(
            uploaded.iter().map(|p| p.value).collect::<Vec<_>>(),
            [2.0, 1.0]
        );
        let bytes = stats
            .timeseries(
                hs_admin::statistics::metric("media.uploaded_bytes").unwrap(),
                window,
            )
            .await
            .unwrap();
        assert_eq!(
            bytes.iter().map(|p| p.value).collect::<Vec<_>>(),
            [107.0, 50.0]
        );
    }

    #[tokio::test]
    async fn gauges_are_the_samples_taken_and_nothing_where_none_was() {
        let backend = MemoryBackend::new();
        let stats = statistics(&backend);
        stats.sample_at(10).await.unwrap();
        stats.sample_at(20).await.unwrap();
        stats.sample_at(2 * HOUR as u64 + 5).await.unwrap();
        let window = Window {
            from_ms: 0,
            until_ms: 3 * HOUR,
            step_ms: HOUR,
        };
        let users = stats
            .timeseries(hs_admin::statistics::metric("users_count").unwrap(), window)
            .await
            .unwrap();
        assert_eq!(users.len(), 2, "no sample in the second hour: {users:?}");
        assert_eq!(
            users[0].value, 11.0,
            "the later of the two in the first hour"
        );
        assert_eq!(users[1].value, 12.0);
        // Media totals are sampled even though the Overview leaves them out.
        let media = stats
            .timeseries(hs_admin::statistics::metric("media_count").unwrap(), window)
            .await
            .unwrap();
        assert_eq!(media[0].value, 0.0);
        // A gauge nobody measured has no points at all.
        let failing = stats
            .timeseries(
                hs_admin::statistics::metric("federation_destinations_failing_count").unwrap(),
                window,
            )
            .await
            .unwrap();
        assert!(failing.is_empty());
    }

    #[tokio::test]
    async fn samples_past_retention_are_dropped_as_new_ones_arrive() {
        let backend = MemoryBackend::new();
        let stats = statistics(&backend);
        stats.sample_at(1).await.unwrap();
        let retention = u64::try_from(SAMPLE_RETENTION.as_millis()).unwrap();
        stats.sample_at(retention + 10).await.unwrap();
        let everything = Window {
            from_ms: 0,
            until_ms: i64::try_from(retention).unwrap() + HOUR,
            step_ms: HOUR,
        };
        let users = stats
            .timeseries(
                hs_admin::statistics::metric("users_count").unwrap(),
                everything,
            )
            .await
            .unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].value, 11.0, "only the newer sample is left");
    }
}
