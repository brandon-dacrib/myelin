//! Human-readable durations (`"30s"`, `"1h30m"`, `"7d"`, `"1w"`, `"1y"`).
//!
//! The accepted syntax is a superset of Synapse's `parse_duration`: a bare
//! integer means milliseconds (Synapse's convention, which keeps translated
//! values exact), and a string is one or more `<number><unit>` groups with the
//! units `ms`, `s`, `m`, `h`, `d`, `w` and `y` (a year is 365 days, as in
//! Synapse). Serialisation always emits the largest exact unit, so `86400000`
//! round-trips as `"1d"`.

use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// A duration parsed from a human-readable string or a millisecond count.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Duration(std::time::Duration);

/// Errors from parsing a [`Duration`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DurationParseError {
    /// The string was empty.
    #[error("empty duration")]
    Empty,
    /// A group did not have the form `<number><unit>`.
    #[error(
        "invalid duration syntax in {0:?}: expected groups like 30s, 5m, 1h, 7d, 1w, 1y or 500ms"
    )]
    Syntax(String),
    /// The unit suffix is not one of the supported ones.
    #[error("unknown duration unit {0:?}")]
    Unit(String),
    /// The value overflows.
    #[error("duration out of range")]
    Overflow,
}

impl Duration {
    /// Builds a duration from milliseconds.
    pub const fn from_millis(ms: u64) -> Self {
        Self(std::time::Duration::from_millis(ms))
    }

    /// Builds a duration from seconds.
    pub const fn from_secs(s: u64) -> Self {
        Self(std::time::Duration::from_secs(s))
    }

    /// Builds a duration from minutes.
    pub const fn from_mins(m: u64) -> Self {
        Self(std::time::Duration::from_secs(m * 60))
    }

    /// Builds a duration from hours.
    pub const fn from_hours(h: u64) -> Self {
        Self(std::time::Duration::from_secs(h * 3600))
    }

    /// Builds a duration from days.
    pub const fn from_days(d: u64) -> Self {
        Self(std::time::Duration::from_secs(d * 86_400))
    }

    /// The zero duration.
    pub const ZERO: Self = Self(std::time::Duration::ZERO);

    /// Total milliseconds, saturating at `u64::MAX`.
    pub fn as_millis(self) -> u64 {
        u64::try_from(self.0.as_millis()).unwrap_or(u64::MAX)
    }

    /// The wrapped standard-library duration.
    pub const fn as_std(self) -> std::time::Duration {
        self.0
    }

    /// True when the duration is zero.
    pub const fn is_zero(self) -> bool {
        self.0.is_zero()
    }
}

impl From<std::time::Duration> for Duration {
    fn from(d: std::time::Duration) -> Self {
        Self(d)
    }
}

impl From<Duration> for std::time::Duration {
    fn from(d: Duration) -> Self {
        d.0
    }
}

fn unit_millis(unit: &str) -> Option<u64> {
    Some(match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 7 * 86_400_000,
        "y" => 365 * 86_400_000,
        _ => return None,
    })
}

impl FromStr for Duration {
    type Err = DurationParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(DurationParseError::Empty);
        }
        if let Ok(ms) = s.parse::<u64>() {
            return Ok(Self::from_millis(ms));
        }
        if s.chars().all(|c| c.is_ascii_digit()) {
            // All-digit strings only fail the parse above by overflowing
            // u64; falling through to the unit-group parser below would
            // misreport that as a syntax error (no unit follows the
            // digits), so it is reported precisely here instead.
            return Err(DurationParseError::Overflow);
        }
        let mut total: u64 = 0;
        let mut rest = s;
        while !rest.is_empty() {
            let digits_end = rest
                .find(|c: char| !c.is_ascii_digit())
                .ok_or_else(|| DurationParseError::Syntax(s.to_owned()))?;
            if digits_end == 0 {
                return Err(DurationParseError::Syntax(s.to_owned()));
            }
            let (num, tail) = rest.split_at(digits_end);
            let unit_end = tail
                .find(|c: char| !c.is_ascii_alphabetic())
                .unwrap_or(tail.len());
            if unit_end == 0 {
                return Err(DurationParseError::Syntax(s.to_owned()));
            }
            let (unit, tail) = tail.split_at(unit_end);
            let n: u64 = num.parse().map_err(|_| DurationParseError::Overflow)?;
            let mult =
                unit_millis(unit).ok_or_else(|| DurationParseError::Unit(unit.to_owned()))?;
            total = n
                .checked_mul(mult)
                .and_then(|v| total.checked_add(v))
                .ok_or(DurationParseError::Overflow)?;
            rest = tail.trim_start();
        }
        Ok(Self::from_millis(total))
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.as_millis();
        if ms == 0 {
            return write!(f, "0s");
        }
        for (unit, mult) in [
            ("y", 365 * 86_400_000u64),
            ("w", 7 * 86_400_000),
            ("d", 86_400_000),
            ("h", 3_600_000),
            ("m", 60_000),
            ("s", 1_000),
        ] {
            if ms.is_multiple_of(mult) {
                return write!(f, "{}{unit}", ms / mult);
            }
        }
        write!(f, "{ms}ms")
    }
}

impl fmt::Debug for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Duration;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration string like \"30s\" or a millisecond count")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Duration, E> {
                Ok(Duration::from_millis(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Duration, E> {
                u64::try_from(v)
                    .map(Duration::from_millis)
                    .map_err(|_| E::custom("negative duration"))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Duration, E> {
                if v < 0.0 || !v.is_finite() {
                    return Err(E::custom("negative or non-finite duration"));
                }
                // Millisecond precision; a float only appears when YAML wrote
                // one, which the translator never does.
                Ok(Duration::from_millis(v.round() as u64))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Duration, E> {
                v.parse().map_err(E::custom)
            }
        }
        deserializer.deserialize_any(V)
    }
}

impl JsonSchema for Duration {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Duration".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": ["string", "integer"],
            "description": "A duration: a string of <number><unit> groups (ms, s, m, h, d, w, y), or an integer number of milliseconds.",
            "x-duration": true
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units_and_compounds() {
        assert_eq!(
            "500ms".parse::<Duration>().unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!("30s".parse::<Duration>().unwrap(), Duration::from_secs(30));
        assert_eq!("5m".parse::<Duration>().unwrap(), Duration::from_mins(5));
        assert_eq!("1h".parse::<Duration>().unwrap(), Duration::from_hours(1));
        assert_eq!("7d".parse::<Duration>().unwrap(), Duration::from_days(7));
        assert_eq!("1w".parse::<Duration>().unwrap(), Duration::from_days(7));
        assert_eq!("1y".parse::<Duration>().unwrap(), Duration::from_days(365));
        assert_eq!(
            "1h30m".parse::<Duration>().unwrap(),
            Duration::from_mins(90)
        );
        assert_eq!(
            "1h 30m".parse::<Duration>().unwrap(),
            Duration::from_mins(90)
        );
        assert_eq!(
            "86400000".parse::<Duration>().unwrap(),
            Duration::from_days(1)
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!("".parse::<Duration>(), Err(DurationParseError::Empty));
        assert!(matches!(
            "abc".parse::<Duration>(),
            Err(DurationParseError::Syntax(_))
        ));
        assert!(matches!(
            "5x".parse::<Duration>(),
            Err(DurationParseError::Unit(_))
        ));
        assert!(matches!(
            "5".repeat(30).parse::<Duration>(),
            Err(DurationParseError::Overflow)
        ));
    }

    #[test]
    fn display_uses_largest_exact_unit() {
        assert_eq!(Duration::from_days(7).to_string(), "1w");
        assert_eq!(Duration::from_days(8).to_string(), "8d");
        assert_eq!(Duration::from_mins(90).to_string(), "90m");
        assert_eq!(Duration::from_millis(1500).to_string(), "1500ms");
        assert_eq!(Duration::ZERO.to_string(), "0s");
    }

    #[test]
    fn serde_round_trip() {
        let d: Duration = serde_yaml_ng::from_str("10m").unwrap();
        assert_eq!(d, Duration::from_mins(10));
        let d: Duration = serde_yaml_ng::from_str("600000").unwrap();
        assert_eq!(d, Duration::from_mins(10));
        assert_eq!(serde_yaml_ng::to_string(&d).unwrap().trim(), "10m");
        assert!(serde_yaml_ng::from_str::<Duration>("-5").is_err());
    }
}
