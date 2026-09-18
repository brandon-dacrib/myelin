//! Canonical JSON: the spec's integer and ordering rules.
//!
//! Matrix's [canonical JSON] is used to compute event hashes and signatures. The rules:
//!
//! 1. Object keys are sorted lexicographically by Unicode code point (byte-wise on the UTF-8
//!    encoding, which agrees with code point order).
//! 2. No whitespace outside quoted strings.
//! 3. No superfluous escaping: strings escape only `"`, `\` and control characters; non-ASCII
//!    characters are emitted as raw UTF-8, not `\uXXXX`.
//! 4. Numbers are integers; floats are not permitted. Integers must lie in
//!    `[-(2^53)+1, 2^53-1]` (JavaScript's safe integer range), have no leading zero (other than
//!    the literal `0`), and `-0` must not appear.
//!
//! Rule 4 was only *enforced by the authorization rules* starting room version 6
//! ([`AuthorizationRules::strict_canonical_json`](crate::room_version::AuthorizationRules::strict_canonical_json));
//! older rooms sometimes carry events with floats or oversized integers that Synapse accepted.
//! This module therefore separates two things: [`CanonicalJsonValue`], a strict in-memory
//! representation that can only hold spec-legal integers, and [`to_canonical_value`], the
//! conversion from [`serde_json::Value`] that is parameterized by a `strict` flag. In strict mode
//! a non-conforming number is an error ([`CanonicalJsonError`]); in lenient mode (room versions 1
//! to 5) it is coerced into the nearest representable value so that older rooms still canonicalize
//! deterministically, matching the "best effort" support `ruma-state-res` itself documents for
//! those versions.
//!
//! Re-expressed from the algorithm described in the Matrix specification appendices
//! (`refs/matrix-spec/content/appendices.md`, Apache-2.0) and cross-checked in tests against
//! `ruma_common::canonical_json` (MIT).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::error::CanonicalJsonError;

/// The largest integer value permitted in canonical JSON: `2^53 - 1`.
pub const MAX_SAFE_INT: i64 = (1i64 << 53) - 1;
/// The smallest integer value permitted in canonical JSON: `-(2^53) + 1`.
pub const MIN_SAFE_INT: i64 = -((1i64 << 53) - 1);

/// A JSON object in canonical form: keys are a `BTreeMap`, which iterates in sorted order.
pub type CanonicalJsonObject = BTreeMap<String, CanonicalJsonValue>;

/// A JSON value restricted to what canonical JSON can represent.
///
/// Unlike [`serde_json::Value`], there is no separate "float" case in the strict encoding: values
/// that would need one are rejected by [`to_canonical_value`] in strict mode. [`Self::Float`]
/// exists only to let lenient mode (pre-v6 rooms) round-trip a non-conforming number
/// deterministically; its wire encoding is best-effort and not guaranteed to match other
/// implementations, exactly as the spec allows for those room versions.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonicalJsonValue {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer in `[MIN_SAFE_INT, MAX_SAFE_INT]`.
    Integer(i64),
    /// A non-conforming number, kept only in lenient mode. See the type-level docs.
    Float(f64),
    /// A UTF-8 string.
    String(String),
    /// An array; element order is preserved.
    Array(Vec<CanonicalJsonValue>),
    /// An object; keys are sorted by the underlying `BTreeMap`.
    Object(CanonicalJsonObject),
}

impl CanonicalJsonValue {
    /// Borrows this value as an object, if it is one.
    #[must_use]
    pub fn as_object(&self) -> Option<&CanonicalJsonObject> {
        match self {
            Self::Object(map) => Some(map),
            _ => None,
        }
    }

    /// Mutably borrows this value as an object, if it is one.
    pub fn as_object_mut(&mut self) -> Option<&mut CanonicalJsonObject> {
        match self {
            Self::Object(map) => Some(map),
            _ => None,
        }
    }

    /// Borrows this value as a string, if it is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// Borrows this value as an array, if it is one.
    #[must_use]
    pub fn as_array(&self) -> Option<&[CanonicalJsonValue]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Builds a checked float value, rejecting non-finite numbers.
    ///
    /// # Errors
    /// Returns [`CanonicalJsonError::NonFinite`] for `NaN` or infinite values.
    pub fn checked_float(value: f64) -> Result<Self, CanonicalJsonError> {
        if value.is_finite() {
            Ok(Self::Float(value))
        } else {
            Err(CanonicalJsonError::NonFinite)
        }
    }

    /// Serializes this value as canonical JSON bytes: sorted keys, no whitespace, minimal string
    /// escaping.
    #[must_use]
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        write_canonical(self, &mut out);
        out.into_bytes()
    }
}

impl From<bool> for CanonicalJsonValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<String> for CanonicalJsonValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for CanonicalJsonValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<CanonicalJsonObject> for CanonicalJsonValue {
    fn from(value: CanonicalJsonObject) -> Self {
        Self::Object(value)
    }
}

/// Converts a [`serde_json::Value`] into strict canonical form.
///
/// In `strict` mode (room versions 6 and later), a float or an integer outside
/// `[MIN_SAFE_INT, MAX_SAFE_INT]` is an error. In lenient mode (room versions 1 to 5), such values
/// are kept as [`CanonicalJsonValue::Float`] so the object can still be encoded, matching the
/// leeway those room versions' authorization rules give (see the module docs).
///
/// # Errors
/// Returns [`CanonicalJsonError`] if `strict` is `true` and a number does not conform.
pub fn to_canonical_value(
    value: &serde_json::Value,
    strict: bool,
) -> Result<CanonicalJsonValue, CanonicalJsonError> {
    use serde_json::Value;
    Ok(match value {
        Value::Null => CanonicalJsonValue::Null,
        Value::Bool(b) => CanonicalJsonValue::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if (MIN_SAFE_INT..=MAX_SAFE_INT).contains(&i) {
                    CanonicalJsonValue::Integer(i)
                } else if strict {
                    return Err(CanonicalJsonError::IntegerOutOfRange(n.to_string()));
                } else {
                    CanonicalJsonValue::Float(i as f64)
                }
            } else if strict {
                return Err(CanonicalJsonError::Float(n.to_string()));
            } else {
                let f = n.as_f64().ok_or(CanonicalJsonError::NonFinite)?;
                if !f.is_finite() {
                    return Err(CanonicalJsonError::NonFinite);
                }
                CanonicalJsonValue::Float(f)
            }
        }
        Value::String(s) => CanonicalJsonValue::String(s.clone()),
        Value::Array(items) => CanonicalJsonValue::Array(
            items
                .iter()
                .map(|v| to_canonical_value(v, strict))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => {
            let mut out = CanonicalJsonObject::new();
            for (k, v) in map {
                out.insert(k.clone(), to_canonical_value(v, strict)?);
            }
            CanonicalJsonValue::Object(out)
        }
    })
}

/// Convenience: parses `bytes` as JSON, then canonicalizes it. See [`to_canonical_value`].
///
/// # Errors
/// Returns [`CanonicalJsonError`] via the same rules as [`to_canonical_value`]. JSON syntax errors
/// are not representable in [`CanonicalJsonError`]; callers that need to distinguish them should
/// parse with `serde_json` first and call [`to_canonical_value`] directly.
pub fn to_canonical_object(
    value: &serde_json::Value,
    strict: bool,
) -> Result<CanonicalJsonObject, CanonicalJsonError> {
    match to_canonical_value(value, strict)? {
        CanonicalJsonValue::Object(map) => Ok(map),
        _ => Ok(CanonicalJsonObject::new()),
    }
}

/// Writes the canonical encoding of `value` into `out`.
fn write_canonical(value: &CanonicalJsonValue, out: &mut String) {
    match value {
        CanonicalJsonValue::Null => out.push_str("null"),
        CanonicalJsonValue::Bool(true) => out.push_str("true"),
        CanonicalJsonValue::Bool(false) => out.push_str("false"),
        CanonicalJsonValue::Integer(i) => {
            let _ = write!(out, "{i}");
        }
        CanonicalJsonValue::Float(f) => {
            // Best-effort only; see the module docs. `serde_json` produces a deterministic,
            // round-trippable rendering for finite f64s.
            let rendered = serde_json::to_string(f).unwrap_or_else(|_| "0".to_owned());
            out.push_str(&rendered);
        }
        CanonicalJsonValue::String(s) => write_canonical_string(s, out),
        CanonicalJsonValue::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        CanonicalJsonValue::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_string(k, out);
                out.push(':');
                write_canonical(v, out);
            }
            out.push('}');
        }
    }
}

/// Writes a JSON string literal using canonical JSON's minimal escaping: only `"`, `\` and C0
/// control characters are escaped; everything else, including all non-ASCII text, is emitted
/// verbatim as UTF-8.
fn write_canonical_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Spec appendix example: canonicalizing `{"one":1,"two":"Two"}`.
    #[test]
    fn spec_example_one_two() {
        let v = json!({"one": 1, "two": "Two"});
        let canon = to_canonical_value(&v, true).unwrap();
        assert_eq!(
            String::from_utf8(canon.to_canonical_bytes()).unwrap(),
            r#"{"one":1,"two":"Two"}"#
        );
    }

    /// Spec appendix example: key ordering and non-ASCII / control character escaping.
    #[test]
    fn spec_example_unicode_and_control_chars() {
        let v = json!({"b": "2", "a": "1"});
        let canon = to_canonical_value(&v, true).unwrap();
        assert_eq!(
            String::from_utf8(canon.to_canonical_bytes()).unwrap(),
            r#"{"a":"1","b":"2"}"#
        );

        let v = json!({"a": "\u{65e5}\u{672c}"});
        let canon = to_canonical_value(&v, true).unwrap();
        let bytes = canon.to_canonical_bytes();
        // Non-ASCII must be emitted as raw UTF-8, not \u-escaped.
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "{\"a\":\"\u{65e5}\u{672c}\"}"
        );

        let v = json!({"a": "\u{0}"});
        let canon = to_canonical_value(&v, true).unwrap();
        // Built via `format!` rather than a literal escape sequence in the source: the
        // pipeline that carries this file's content has been observed to collapse a bare
        // four-hex-digit JSON unicode escape written directly into source text.
        let expected = format!("{{\"a\":\"{}{}0000\"}}", '\\', 'u');
        assert_eq!(
            String::from_utf8(canon.to_canonical_bytes()).unwrap(),
            expected
        );
    }

    #[test]
    fn object_keys_sort_by_code_point() {
        let v = json!({"b": 1, "a": 2, "aa": 3, "A": 4});
        let canon = to_canonical_value(&v, true).unwrap();
        assert_eq!(
            String::from_utf8(canon.to_canonical_bytes()).unwrap(),
            r#"{"A":4,"a":2,"aa":3,"b":1}"#
        );
    }

    #[test]
    fn strict_mode_rejects_floats() {
        let v = json!({"a": 1.5});
        let err = to_canonical_value(&v, true).unwrap_err();
        assert!(matches!(err, CanonicalJsonError::Float(_)));
    }

    #[test]
    fn strict_mode_rejects_out_of_range_integers() {
        let v = json!({"a": MAX_SAFE_INT + 1});
        let err = to_canonical_value(&v, true).unwrap_err();
        assert!(matches!(err, CanonicalJsonError::IntegerOutOfRange(_)));

        let ok = json!({"a": MAX_SAFE_INT});
        assert!(to_canonical_value(&ok, true).is_ok());
    }

    #[test]
    fn lenient_mode_accepts_and_encodes_floats() {
        let v = json!({"a": 1.5});
        let canon = to_canonical_value(&v, false).unwrap();
        assert_eq!(
            String::from_utf8(canon.to_canonical_bytes()).unwrap(),
            r#"{"a":1.5}"#
        );
    }

    #[test]
    fn checked_float_rejects_non_finite() {
        assert!(matches!(
            CanonicalJsonValue::checked_float(f64::NAN),
            Err(CanonicalJsonError::NonFinite)
        ));
        assert!(matches!(
            CanonicalJsonValue::checked_float(f64::INFINITY),
            Err(CanonicalJsonError::NonFinite)
        ));
        assert!(CanonicalJsonValue::checked_float(1.0).is_ok());
    }

    #[test]
    fn cross_check_against_ruma_canonical_json() {
        // ruma_common::CanonicalJsonValue implements the same appendix algorithm; the two must
        // agree byte-for-byte on well-formed input.
        let samples = [
            json!({"a": 1, "b": "two", "c": [1, 2, 3], "d": {"e": true, "f": null}}),
            json!({"z": "last", "a": "first", "m": 42}),
            json!({"nested": {"b": 2, "a": 1}}),
        ];
        for sample in samples {
            let ours = to_canonical_value(&sample, true)
                .unwrap()
                .to_canonical_bytes();
            let ruma_value: ruma::CanonicalJsonValue = sample.clone().try_into().unwrap();
            let theirs = serde_json::to_string(&ruma_value).unwrap();
            assert_eq!(
                String::from_utf8(ours).unwrap(),
                theirs,
                "mismatch for {sample}"
            );
        }
    }
}
