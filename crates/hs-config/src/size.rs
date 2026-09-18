//! Human-readable byte sizes (`"50M"`, `"10K"`, `"1G"`, `"512"`, `"20MiB"`, `"20MB"`).
//!
//! Synapse's `parse_size` understands `K` and `M` with 1024 multipliers; this
//! parser accepts those plus `G` and `T`, the IEC forms (`KiB`, `MiB`, ...)
//! with 1024 multipliers, and the SI forms (`KB`, `MB`, ...) with 1000
//! multipliers. A bare integer is a byte count. Serialisation emits the
//! largest exact binary unit in Synapse's single-letter style (`"50M"`).

use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// A byte count parsed from a human-readable string or an integer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ByteSize(u64);

/// Errors from parsing a [`ByteSize`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ByteSizeParseError {
    /// The string was empty.
    #[error("empty size")]
    Empty,
    /// Not `<number>[unit]`.
    #[error("invalid size syntax in {0:?}: expected a number with an optional unit (K, M, G, T, KiB, MB, ...)")]
    Syntax(String),
    /// Unknown unit suffix.
    #[error("unknown size unit {0:?}")]
    Unit(String),
    /// The value overflows `u64`.
    #[error("size out of range")]
    Overflow,
}

impl ByteSize {
    /// Builds a size from a byte count.
    pub const fn bytes(n: u64) -> Self {
        Self(n)
    }

    /// Builds a size from kibibytes.
    pub const fn kib(n: u64) -> Self {
        Self(n * 1024)
    }

    /// Builds a size from mebibytes.
    pub const fn mib(n: u64) -> Self {
        Self(n * 1024 * 1024)
    }

    /// Builds a size from gibibytes.
    pub const fn gib(n: u64) -> Self {
        Self(n * 1024 * 1024 * 1024)
    }

    /// The byte count.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

fn unit_multiplier(unit: &str) -> Option<u64> {
    let u = unit.to_ascii_lowercase();
    Some(match u.as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        _ => return None,
    })
}

impl FromStr for ByteSize {
    type Err = ByteSizeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ByteSizeParseError::Empty);
        }
        let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        if digits_end == 0 {
            return Err(ByteSizeParseError::Syntax(s.to_owned()));
        }
        let (num, unit) = s.split_at(digits_end);
        let unit = unit.trim();
        let n: u64 = num.parse().map_err(|_| ByteSizeParseError::Overflow)?;
        let mult = unit_multiplier(unit).ok_or_else(|| ByteSizeParseError::Unit(unit.to_owned()))?;
        n.checked_mul(mult).map(Self).ok_or(ByteSizeParseError::Overflow)
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0;
        if n == 0 {
            return write!(f, "0");
        }
        for (unit, shift) in [("T", 40u32), ("G", 30), ("M", 20), ("K", 10)] {
            let mult = 1u64 << shift;
            if n % mult == 0 {
                return write!(f, "{}{unit}", n / mult);
            }
        }
        write!(f, "{n}")
    }
}

impl fmt::Debug for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = ByteSize;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a size string like \"50M\" or a byte count")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<ByteSize, E> {
                Ok(ByteSize(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<ByteSize, E> {
                u64::try_from(v).map(ByteSize).map_err(|_| E::custom("negative size"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<ByteSize, E> {
                v.parse().map_err(E::custom)
            }
        }
        deserializer.deserialize_any(V)
    }
}

impl JsonSchema for ByteSize {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ByteSize".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": ["string", "integer"],
            "description": "A byte size: a number with an optional unit (K, M, G, T with 1024 multipliers; KiB/MiB/GiB; KB/MB/GB with 1000 multipliers), or an integer byte count.",
            "x-bytesize": true
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_synapse_and_iec_forms() {
        assert_eq!("50M".parse::<ByteSize>().unwrap(), ByteSize::mib(50));
        assert_eq!("10K".parse::<ByteSize>().unwrap(), ByteSize::kib(10));
        assert_eq!("1G".parse::<ByteSize>().unwrap(), ByteSize::gib(1));
        assert_eq!("512".parse::<ByteSize>().unwrap(), ByteSize::bytes(512));
        assert_eq!("20MiB".parse::<ByteSize>().unwrap(), ByteSize::mib(20));
        assert_eq!("20 MB".parse::<ByteSize>().unwrap(), ByteSize::bytes(20_000_000));
        assert_eq!("32m".parse::<ByteSize>().unwrap(), ByteSize::mib(32));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!("".parse::<ByteSize>(), Err(ByteSizeParseError::Empty));
        assert!(matches!("M".parse::<ByteSize>(), Err(ByteSizeParseError::Syntax(_))));
        assert!(matches!("5X".parse::<ByteSize>(), Err(ByteSizeParseError::Unit(_))));
        assert!(matches!("99999999999999999999T".parse::<ByteSize>(), Err(ByteSizeParseError::Overflow)));
    }

    #[test]
    fn display_and_serde() {
        assert_eq!(ByteSize::mib(50).to_string(), "50M");
        assert_eq!(ByteSize::bytes(1536).to_string(), "1536");
        assert_eq!(ByteSize::kib(1536).to_string(), "1536K");
        let s: ByteSize = serde_yaml_ng::from_str("50M").unwrap();
        assert_eq!(s, ByteSize::mib(50));
        let s: ByteSize = serde_yaml_ng::from_str("1024").unwrap();
        assert_eq!(s, ByteSize::kib(1));
        assert_eq!(serde_yaml_ng::to_string(&s).unwrap().trim(), "1K");
    }
}
