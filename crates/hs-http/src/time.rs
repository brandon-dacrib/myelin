//! RFC 3339 UTC timestamp formatting, millisecond precision (RFC 0004 decision D15.2:
//! `2026-09-17T21:04:05.123Z`).

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The current time, formatted as `2026-09-17T21:04:05.123Z`.
pub fn now_rfc3339() -> String {
    format_rfc3339(OffsetDateTime::now_utc())
}

/// Formats an `OffsetDateTime` the way the admin API wants it: UTC, millisecond precision, `Z`
/// suffix (never `+00:00`).
pub fn format_rfc3339(dt: OffsetDateTime) -> String {
    let dt = dt.to_offset(time::UtcOffset::UTC);
    // `Rfc3339` renders offset UTC as `Z` and includes as many fractional digits as the value
    // carries; round explicitly to milliseconds first so the format is always `.SSS`.
    let millis = dt.millisecond();
    let truncated = dt
        .replace_nanosecond((millis as u32) * 1_000_000)
        .unwrap_or(dt);
    truncated
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000Z".to_string())
}

/// Formats milliseconds since the Unix epoch the same way. Stores that record a timestamp as an
/// integer -- the configuration store's change history, for one -- have to reach the admin API's
/// string form somehow, and doing it here keeps one definition of what that form is.
pub fn rfc3339_from_millis(ms: i64) -> String {
    let Ok(dt) = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000) else {
        return "1970-01-01T00:00:00.000Z".to_string();
    };
    format_rfc3339(dt)
}

/// Parses an RFC 3339 timestamp such as the one [`now_rfc3339`] produces.
pub fn parse_rfc3339(s: &str) -> Result<OffsetDateTime, time::error::Parse> {
    OffsetDateTime::parse(s, &Rfc3339)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let s = now_rfc3339();
        assert!(s.ends_with('Z'));
        let parsed = parse_rfc3339(&s).unwrap();
        assert_eq!(format_rfc3339(parsed), s);
    }

    #[test]
    fn epoch_millis_render_the_same_way() {
        assert_eq!(
            rfc3339_from_millis(1_789_000_000_123),
            format_rfc3339(
                OffsetDateTime::from_unix_timestamp_nanos(1_789_000_000_123_i128 * 1_000_000)
                    .unwrap()
            )
        );
        // Nonsense in, epoch out, rather than a panic in a response formatter.
        assert!(rfc3339_from_millis(i64::MAX).ends_with('Z'));
    }

    #[test]
    fn millisecond_precision() {
        let dt = time::macros::datetime!(2026-09-17 21:04:05.123456789 UTC);
        assert_eq!(format_rfc3339(dt), "2026-09-17T21:04:05.123Z");
    }
}
