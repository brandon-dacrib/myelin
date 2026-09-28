//! The statistics operations beyond the Overview's counts: the largest rooms
//! (`statistics.rooms`), per-user media usage (`statistics.users_media`) and a metric's time
//! series (`statistics.timeseries`).
//!
//! `statistics.rooms` needs nothing new: it reads the [`crate::sources::RoomDirectory`] the Rooms
//! page already uses. The other two read a [`StatisticsSource`], which `hs-cli` implements over
//! the server's own stores (it is the one crate that can see accounts, media and reports at
//! once, as with the Overview).
//!
//! # The metric vocabulary
//!
//! [`METRICS`] is the whole of it, and the contract's `metric` enum lists the same names. Two
//! kinds:
//!
//! - **Counters** (`users.registered`, `media.uploaded`, `media.uploaded_bytes`,
//!   `reports.received`): how many things happened in each step, counted from the timestamps
//!   the records themselves carry. They have history from the first record, and a step in which
//!   nothing happened is a real `0`.
//! - **Gauges** (the [`crate::model::StatisticsOverview`] field names: `users_count`,
//!   `daily_active_users`, ...): the Overview's own numbers, sampled periodically by the server
//!   and kept. A gauge has history only from when the server started sampling, and a step with
//!   no sample has no point (not a `0`: nothing was measured).

use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use hs_http::{Problem, ValidationError};
use serde::{Deserialize, Serialize};

use crate::handler_kit::{authorize, unwired};
use crate::model::{Page, Scope};
use crate::router::AdminState;
use crate::sources::{RoomFilter, SourceError};

/// The OpenAPI `RoomStatistic` schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomStatistic {
    /// The room.
    pub room_id: String,
    /// Its name, if it has one.
    pub name: Option<String>,
    /// Members currently joined.
    pub joined_members_count: u64,
    /// Entries in its current state.
    pub state_events_count: u64,
}

/// The OpenAPI `UserMediaStatistic` schema: one local user's uploads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMediaStatistic {
    /// The uploader.
    pub user_id: String,
    /// How many files they have uploaded that this server still holds.
    pub media_count: u64,
    /// Their total size in bytes.
    pub media_bytes: u64,
}

/// The OpenAPI `TimeseriesPoint` schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeseriesPoint {
    /// The start of the step (RFC 3339).
    pub at: String,
    /// The value for that step.
    pub value: f64,
}

/// The OpenAPI `Timeseries` schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timeseries {
    /// The metric's name.
    pub metric: String,
    /// The step, in milliseconds.
    pub step_ms: u64,
    /// One point per step with a value, oldest first.
    pub points: Vec<TimeseriesPoint>,
}

/// Whether a metric counts happenings per step or samples a level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// Things that happened in each step, summed; an empty step is `0`.
    Counter,
    /// A level, sampled; the last sample in each step, and no point for a step without one.
    Gauge,
}

/// One metric [`StatisticsSource::timeseries`] can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MetricInfo {
    /// The name `?metric=` takes.
    pub name: &'static str,
    /// Counter or gauge.
    pub kind: MetricKind,
}

/// Every metric this server can chart. See the module docs.
pub const METRICS: &[MetricInfo] = &[
    MetricInfo {
        name: "users.registered",
        kind: MetricKind::Counter,
    },
    MetricInfo {
        name: "media.uploaded",
        kind: MetricKind::Counter,
    },
    MetricInfo {
        name: "media.uploaded_bytes",
        kind: MetricKind::Counter,
    },
    MetricInfo {
        name: "reports.received",
        kind: MetricKind::Counter,
    },
    MetricInfo {
        name: "users_count",
        kind: MetricKind::Gauge,
    },
    MetricInfo {
        name: "rooms_count",
        kind: MetricKind::Gauge,
    },
    MetricInfo {
        name: "daily_active_users",
        kind: MetricKind::Gauge,
    },
    MetricInfo {
        name: "monthly_active_users",
        kind: MetricKind::Gauge,
    },
    MetricInfo {
        name: "media_count",
        kind: MetricKind::Gauge,
    },
    MetricInfo {
        name: "media_bytes",
        kind: MetricKind::Gauge,
    },
    MetricInfo {
        name: "pending_reports_count",
        kind: MetricKind::Gauge,
    },
    MetricInfo {
        name: "federation_destinations_failing_count",
        kind: MetricKind::Gauge,
    },
];

/// The metric named `name`, if there is one.
#[must_use]
pub fn metric(name: &str) -> Option<MetricInfo> {
    METRICS.iter().copied().find(|m| m.name == name)
}

/// A time range cut into steps: `[from_ms, until_ms)`, `step_ms` wide, aligned to `from_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Inclusive start, milliseconds since the epoch.
    pub from_ms: i64,
    /// Exclusive end.
    pub until_ms: i64,
    /// Step width, at least one second.
    pub step_ms: i64,
}

impl Window {
    fn buckets(&self) -> usize {
        usize::try_from((self.until_ms - self.from_ms + self.step_ms - 1) / self.step_ms)
            .unwrap_or(0)
    }

    fn bucket_of(&self, at_ms: i64) -> Option<usize> {
        if at_ms < self.from_ms || at_ms >= self.until_ms {
            return None;
        }
        usize::try_from((at_ms - self.from_ms) / self.step_ms).ok()
    }

    fn at(&self, bucket: usize) -> String {
        hs_http::time::rfc3339_from_millis(self.from_ms + self.step_ms * bucket as i64)
    }

    /// A counter's points: each `(at_ms, amount)` summed into its step, every step present.
    #[must_use]
    pub fn sum(&self, happenings: impl IntoIterator<Item = (i64, f64)>) -> Vec<TimeseriesPoint> {
        let mut sums = vec![0.0; self.buckets()];
        for (at, amount) in happenings {
            if let Some(bucket) = self.bucket_of(at)
                && let Some(slot) = sums.get_mut(bucket)
            {
                *slot += amount;
            }
        }
        sums.into_iter()
            .enumerate()
            .map(|(bucket, value)| TimeseriesPoint {
                at: self.at(bucket),
                value,
            })
            .collect()
    }

    /// A gauge's points: the latest `(at_ms, value)` sample in each step; steps without one are
    /// left out.
    #[must_use]
    pub fn last(&self, samples: impl IntoIterator<Item = (i64, f64)>) -> Vec<TimeseriesPoint> {
        let mut latest: Vec<Option<(i64, f64)>> = vec![None; self.buckets()];
        for (at, value) in samples {
            if let Some(bucket) = self.bucket_of(at)
                && let Some(slot) = latest.get_mut(bucket)
                && slot.is_none_or(|(seen, _)| seen <= at)
            {
                *slot = Some((at, value));
            }
        }
        latest
            .into_iter()
            .enumerate()
            .filter_map(|(bucket, sample)| {
                sample.map(|(_, value)| TimeseriesPoint {
                    at: self.at(bucket),
                    value,
                })
            })
            .collect()
    }
}

/// What `statistics.users_media` and `statistics.timeseries` read.
#[async_trait]
pub trait StatisticsSource: Send + Sync + 'static {
    /// Every local user who has uploaded media this server still holds, with their totals.
    async fn users_media(&self) -> Result<Vec<UserMediaStatistic>, SourceError>;
    /// `metric`'s points over `window`. The handler has already checked `metric` is one of
    /// [`METRICS`]; an implementation that cannot answer one says so with
    /// [`SourceError::Unavailable`].
    async fn timeseries(
        &self,
        metric: MetricInfo,
        window: Window,
    ) -> Result<Vec<TimeseriesPoint>, SourceError>;
}

// -------------------------------------------------------------------------------------------
// Handlers.
// -------------------------------------------------------------------------------------------

/// The paginated statistics operations' query string.
#[derive(Debug, Deserialize)]
pub(crate) struct StatisticsPageQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
    sort: Option<String>,
}

fn invalid(pointer: &str, detail: String, instance: &str) -> Response {
    Problem::validation_failed()
        .with_detail(detail.clone())
        .with_errors(vec![ValidationError::new(pointer, detail)])
        .with_instance(instance.to_owned())
        .into_response()
}

/// `GET /api/v1/statistics/rooms` (`admin:read`): the largest rooms, by joined members unless
/// `sort` says `state_events_count` (either may be prefixed `-` for descending, the default).
pub(crate) async fn statistics_rooms(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<StatisticsPageQuery>,
) -> Response {
    let instance = "/api/v1/statistics/rooms";
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, instance).await {
        return response;
    }
    let Some(rooms) = &state.rooms else {
        return unwired("room directory", instance);
    };
    let sort = query.sort.as_deref().unwrap_or("-joined_members_count");
    let (field, descending) = match sort.strip_prefix('-') {
        Some(field) => (field, true),
        None => (sort, false),
    };
    if !matches!(field, "joined_members_count" | "state_events_count") {
        return invalid(
            "/sort",
            format!(
                "room statistics sort by joined_members_count or state_events_count, not {sort:?}"
            ),
            instance,
        );
    }
    let mut items: Vec<RoomStatistic> = match rooms.list_rooms(&RoomFilter::default()).await {
        Ok(rooms) => rooms
            .into_iter()
            .map(|r| RoomStatistic {
                room_id: r.room_id,
                name: r.name,
                joined_members_count: r.joined_members_count,
                state_events_count: r.state_events_count,
            })
            .collect(),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let key = |r: &RoomStatistic| match field {
        "state_events_count" => r.state_events_count,
        _ => r.joined_members_count,
    };
    // Ties broken by room id, so pages are stable.
    if descending {
        items.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| a.room_id.cmp(&b.room_id)));
    } else {
        items.sort_by(|a, b| key(a).cmp(&key(b)).then_with(|| a.room_id.cmp(&b.room_id)));
    }
    axum::Json(Page::paginate(
        items,
        query.cursor.as_deref(),
        query.limit,
        query.include_total.unwrap_or(false),
    ))
    .into_response()
}

/// `GET /api/v1/statistics/users/media` (`admin:read`): local users by media stored, most bytes
/// first unless `sort` says otherwise (`media_bytes` or `media_count`, `-` for descending).
pub(crate) async fn statistics_users_media(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<StatisticsPageQuery>,
) -> Response {
    let instance = "/api/v1/statistics/users/media";
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, instance).await {
        return response;
    }
    let Some(statistics) = &state.statistics else {
        return unwired("statistics", instance);
    };
    let sort = query.sort.as_deref().unwrap_or("-media_bytes");
    let (field, descending) = match sort.strip_prefix('-') {
        Some(field) => (field, true),
        None => (sort, false),
    };
    if !matches!(field, "media_bytes" | "media_count") {
        return invalid(
            "/sort",
            format!("media usage sorts by media_bytes or media_count, not {sort:?}"),
            instance,
        );
    }
    let mut items = match statistics.users_media().await {
        Ok(items) => items,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let key = |u: &UserMediaStatistic| match field {
        "media_count" => u.media_count,
        _ => u.media_bytes,
    };
    if descending {
        items.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| a.user_id.cmp(&b.user_id)));
    } else {
        items.sort_by(|a, b| key(a).cmp(&key(b)).then_with(|| a.user_id.cmp(&b.user_id)));
    }
    axum::Json(Page::paginate(
        items,
        query.cursor.as_deref(),
        query.limit,
        query.include_total.unwrap_or(false),
    ))
    .into_response()
}

/// `GET /statistics/timeseries`'s query string.
#[derive(Debug, Deserialize)]
pub(crate) struct TimeseriesQuery {
    metric: Option<String>,
    from: Option<String>,
    until: Option<String>,
    step: Option<String>,
}

/// The most points one series may have.
pub const MAX_POINTS: i64 = 1_000;

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;

/// Parses a step: a whole number followed by `s`, `m`, `h`, `d` or `w` (`15m`, `1h`, `1d`), or a
/// bare number of milliseconds.
#[must_use]
pub fn parse_step(step: &str) -> Option<i64> {
    let step = step.trim();
    if let Ok(ms) = step.parse::<i64>() {
        return (ms > 0).then_some(ms);
    }
    let split = step.find(|c: char| !c.is_ascii_digit())?;
    let (count, unit) = step.split_at(split);
    let count: i64 = count.parse().ok().filter(|c| *c > 0)?;
    let unit_ms = match unit {
        "s" => 1_000,
        "m" => MINUTE_MS,
        "h" => HOUR_MS,
        "d" => DAY_MS,
        "w" => 7 * DAY_MS,
        _ => return None,
    };
    count.checked_mul(unit_ms)
}

/// The step when none is asked for: about a hundred points or fewer, in a round unit.
fn default_step(range_ms: i64) -> i64 {
    [
        MINUTE_MS,
        5 * MINUTE_MS,
        15 * MINUTE_MS,
        HOUR_MS,
        6 * HOUR_MS,
        DAY_MS,
        7 * DAY_MS,
    ]
    .into_iter()
    .find(|step| range_ms / step <= 168)
    .unwrap_or(7 * DAY_MS)
}

/// Turns the query into a [`Window`]: `until` defaults to now, `from` to seven days before
/// `until`, both widened to whole steps aligned to the epoch.
#[allow(clippy::result_large_err)]
fn window_of(query: &TimeseriesQuery, now_ms: i64, instance: &str) -> Result<Window, Response> {
    let parse_time = |field: &str, raw: &str| -> Result<i64, Response> {
        hs_http::time::parse_rfc3339(raw)
            .map(|t| i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX))
            .map_err(|_| {
                invalid(
                    &format!("/{field}"),
                    format!("{field} must be an RFC 3339 date-time, not {raw:?}"),
                    instance,
                )
            })
    };
    let until = match query.until.as_deref().filter(|s| !s.is_empty()) {
        Some(raw) => parse_time("until", raw)?,
        None => now_ms,
    };
    let from = match query.from.as_deref().filter(|s| !s.is_empty()) {
        Some(raw) => parse_time("from", raw)?,
        None => until - 7 * DAY_MS,
    };
    if from >= until {
        return Err(invalid(
            "/from",
            "from must be before until".to_owned(),
            instance,
        ));
    }
    let step = match query.step.as_deref().filter(|s| !s.is_empty()) {
        Some(raw) => match parse_step(raw) {
            Some(step) if step >= 1_000 => step,
            _ => {
                return Err(invalid(
                    "/step",
                    format!(
                        "step must be a whole number of s, m, h, d or w (15m, 1h, 1d), at least \
                         one second, not {raw:?}"
                    ),
                    instance,
                ));
            }
        },
        None => default_step(until - from),
    };
    // Steps sit on multiples of the step since the epoch (a day step starts at midnight UTC), so
    // two charts of the same metric line up. The first step is the one `from` falls in, the last
    // the one `until` falls in: a series ending "now" includes what happened now.
    let first = from.div_euclid(step) * step;
    let last = until.div_euclid(step) * step;
    let steps = (last - first) / step + 1;
    if steps > MAX_POINTS {
        return Err(invalid(
            "/step",
            format!("that is {steps} points; at most {MAX_POINTS} are served, so widen the step"),
            instance,
        ));
    }
    Ok(Window {
        from_ms: first,
        until_ms: last + step,
        step_ms: step,
    })
}

/// `GET /api/v1/statistics/timeseries` (`admin:read`): one metric's points. See the module docs
/// for the vocabulary.
pub(crate) async fn statistics_timeseries(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<TimeseriesQuery>,
) -> Response {
    let instance = "/api/v1/statistics/timeseries";
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, instance).await {
        return response;
    }
    let known = || {
        METRICS
            .iter()
            .map(|m| m.name)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let Some(metric) = query.metric.as_deref().and_then(self::metric) else {
        return invalid(
            "/metric",
            format!(
                "metric must be one of {}; got {:?}",
                known(),
                query.metric.as_deref().unwrap_or("")
            ),
            instance,
        );
    };
    let Some(statistics) = &state.statistics else {
        return unwired("statistics", instance);
    };
    let now_ms = i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .unwrap_or(i64::MAX);
    let window = match window_of(&query, now_ms, instance) {
        Ok(window) => window,
        Err(response) => return response,
    };
    match statistics.timeseries(metric, window).await {
        Ok(points) => axum::Json(Timeseries {
            metric: metric.name.to_owned(),
            step_ms: u64::try_from(window.step_ms).unwrap_or(0),
            points,
        })
        .into_response(),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_parse_in_round_units_or_milliseconds() {
        assert_eq!(parse_step("15m"), Some(15 * MINUTE_MS));
        assert_eq!(parse_step("1h"), Some(HOUR_MS));
        assert_eq!(parse_step("2d"), Some(2 * DAY_MS));
        assert_eq!(parse_step("1w"), Some(7 * DAY_MS));
        assert_eq!(parse_step("30s"), Some(30_000));
        assert_eq!(parse_step("5000"), Some(5_000));
        for bad in ["", "h", "0h", "-1h", "1y", "1.5h", "PT1H"] {
            assert_eq!(parse_step(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_default_step_keeps_a_series_short() {
        assert_eq!(default_step(DAY_MS), 15 * MINUTE_MS);
        assert_eq!(default_step(7 * DAY_MS), HOUR_MS);
        assert_eq!(default_step(90 * DAY_MS), DAY_MS);
    }

    #[test]
    fn a_counter_has_every_step_and_a_gauge_only_measured_ones() {
        let window = Window {
            from_ms: 0,
            until_ms: 3 * HOUR_MS,
            step_ms: HOUR_MS,
        };
        let counted = window.sum([
            (10, 1.0),
            (20, 1.0),
            (2 * HOUR_MS + 5, 3.0),
            (5 * HOUR_MS, 9.0),
        ]);
        let values: Vec<f64> = counted.iter().map(|p| p.value).collect();
        assert_eq!(values, [2.0, 0.0, 3.0]);
        assert_eq!(counted[1].at, "1970-01-01T01:00:00.000Z");

        let sampled = window.last([(10, 5.0), (30, 7.0), (20, 6.0), (2 * HOUR_MS, 8.0)]);
        assert_eq!(
            sampled.len(),
            2,
            "no sample in the middle hour: {sampled:?}"
        );
        assert_eq!(sampled[0].value, 7.0, "the latest sample in the step");
        assert_eq!(sampled[1].value, 8.0);
    }

    #[test]
    fn the_window_ends_on_a_whole_step_that_includes_until() {
        let query = TimeseriesQuery {
            metric: None,
            from: Some("2026-09-01T00:00:00Z".to_owned()),
            until: Some("2026-09-01T02:30:00Z".to_owned()),
            step: Some("1h".to_owned()),
        };
        let window = window_of(&query, 0, "/x").unwrap();
        assert_eq!(window.step_ms, HOUR_MS);
        assert_eq!(window.until_ms - window.from_ms, 3 * HOUR_MS);

        let too_many = TimeseriesQuery {
            step: Some("1s".to_owned()),
            ..query
        };
        assert!(window_of(&too_many, 0, "/x").is_err());
    }

    struct FixedStatistics;

    #[async_trait]
    impl StatisticsSource for FixedStatistics {
        async fn users_media(&self) -> Result<Vec<UserMediaStatistic>, SourceError> {
            Ok(vec![
                UserMediaStatistic {
                    user_id: "@a:example.org".to_owned(),
                    media_count: 10,
                    media_bytes: 100,
                },
                UserMediaStatistic {
                    user_id: "@b:example.org".to_owned(),
                    media_count: 1,
                    media_bytes: 5_000,
                },
            ])
        }

        async fn timeseries(
            &self,
            metric: MetricInfo,
            window: Window,
        ) -> Result<Vec<TimeseriesPoint>, SourceError> {
            assert_eq!(metric.name, "users.registered");
            Ok(window.sum([(window.from_ms, 2.0)]))
        }
    }

    fn room(id: &str, members: u64, state: u64) -> crate::model::AdminRoom {
        crate::model::AdminRoom {
            room_id: id.to_owned(),
            name: Some(format!("room {id}")),
            topic: None,
            avatar_url: None,
            canonical_alias: None,
            joined_members_count: members,
            local_members_count: members,
            state_events_count: state,
            version: "11".to_owned(),
            creator: None,
            encrypted: false,
            join_rule: "invite".to_owned(),
            guest_access: "forbidden".to_owned(),
            history_visibility: "shared".to_owned(),
            federatable: true,
            public: false,
            room_type: None,
            blocked: false,
            blocked_reason: None,
            tombstoned: false,
            replacement_room_id: None,
            forgotten: false,
        }
    }

    #[tokio::test]
    async fn the_http_operations_answer_from_their_sources() {
        use crate::handler_kit::testing::{call, state};
        use axum::http::StatusCode;
        use std::sync::Arc;

        let (state, _) = state();
        // Not wired: 503, never an empty chart.
        let (status, _, _) = call(
            &state,
            "GET",
            "/api/v1/statistics/timeseries?metric=users.registered",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let rooms = crate::sources::InMemoryRoomDirectory::new()
            .with_room(room("!small:example.org", 2, 40))
            .with_room(room("!big:example.org", 90, 10));
        let state = state
            .with_rooms(Arc::new(rooms))
            .with_statistics(Arc::new(FixedStatistics));

        let (status, _, page) = call(
            &state,
            "GET",
            "/api/v1/statistics/rooms",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["items"][0]["room_id"], "!big:example.org");
        assert_eq!(page["items"][0]["joined_members_count"], 90);
        let (_, _, by_state) = call(
            &state,
            "GET",
            "/api/v1/statistics/rooms?sort=-state_events_count",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(by_state["items"][0]["room_id"], "!small:example.org");
        let (status, _, _) = call(
            &state,
            "GET",
            "/api/v1/statistics/rooms?sort=name",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (_, _, media) = call(
            &state,
            "GET",
            "/api/v1/statistics/users/media",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(media["items"][0]["user_id"], "@b:example.org", "{media}");
        let (_, _, by_count) = call(
            &state,
            "GET",
            "/api/v1/statistics/users/media?sort=-media_count",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(by_count["items"][0]["user_id"], "@a:example.org");

        let (status, _, series) = call(
            &state,
            "GET",
            "/api/v1/statistics/timeseries?metric=users.registered&from=2026-09-01T00:00:00Z&until=2026-09-02T00:00:00Z&step=1h",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{series}");
        assert_eq!(series["metric"], "users.registered");
        assert_eq!(series["step_ms"], HOUR_MS);
        assert_eq!(series["points"].as_array().unwrap().len(), 25);
        assert_eq!(series["points"][0]["value"], 2.0);

        let (status, _, problem) = call(
            &state,
            "GET",
            "/api/v1/statistics/timeseries?metric=vibes",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            problem["detail"]
                .as_str()
                .unwrap()
                .contains("daily_active_users"),
            "the refusal names what can be asked for: {problem}"
        );
        let (status, _, _) = call(
            &state,
            "GET",
            "/api/v1/statistics/timeseries?metric=users_count&from=yesterday",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn every_metric_name_is_unique() {
        let mut names: Vec<_> = METRICS.iter().map(|m| m.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), METRICS.len());
        assert_eq!(
            metric("users.registered").unwrap().kind,
            MetricKind::Counter
        );
        assert_eq!(metric("users_count").unwrap().kind, MetricKind::Gauge);
        assert!(metric("nope").is_none());
    }
}
