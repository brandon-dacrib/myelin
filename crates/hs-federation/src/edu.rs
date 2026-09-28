//! EDUs (ephemeral data units) inside a `/send` transaction body: structural validation
//! ([`parse_edu`], fuzzed) and the seam every valid one is handed to ([`InboundEduSink`]).
//!
//! This crate interprets no EDU's content. `crate::inbound::process_transaction` validates each
//! EDU's shape and hands it, with the transaction's authenticated `origin`, to the installed
//! sink -- in `hs serve`, `hs-cli`'s dispatcher, which knows the session hub (typing, receipts,
//! presence) and the device-key store (device-list and signing-key updates). The sink is where
//! "an EDU speaks only for the origin's own users" is enforced, per EDU type, since only it knows
//! where each type names its user.

use hs_model::canonical::to_canonical_object;

/// Where `crate::inbound::process_transaction` hands every structurally valid EDU. See the
/// module docs.
#[async_trait::async_trait]
pub trait InboundEduSink: Send + Sync {
    /// Applies one EDU that arrived from `origin` (the `X-Matrix`-authenticated sender of the
    /// transaction). Never fails the transaction: an EDU that cannot be applied is logged and
    /// dropped, as the spec expects of ephemeral data.
    async fn receive_edu(&self, origin: &str, edu: Edu);
}

/// Max bytes for one EDU (threat model section 3, matching [`hs_model::event::MAX_PDU_BYTES`]'s
/// figure — the spec gives PDUs and EDUs the same size ceiling).
pub const MAX_EDU_BYTES: usize = 65_535;

/// A structurally valid EDU: `{"edu_type": "<string>", "content": {...}}`. This only validates
/// shape (present, correctly-typed fields, size), not any particular `edu_type`'s content schema
/// — those are defined per-EDU-type elsewhere once this crate implements handling for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edu {
    pub edu_type: String,
    pub content: serde_json::Value,
}

/// Errors from [`parse_edu`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EduError {
    #[error("EDU is not a JSON object")]
    NotObject,
    #[error("EDU exceeds {MAX_EDU_BYTES} bytes")]
    TooLarge,
    #[error("EDU is missing or has a non-string `edu_type`")]
    MissingEduType,
    #[error("EDU `content` is present but is not an object")]
    ContentNotObject,
    #[error("EDU could not be canonicalized: {0}")]
    Canonical(String),
}

/// Parses and structurally validates one EDU. Does not interpret `content` beyond requiring it be
/// an object when present (defaults to `{}` when absent, matching the spec's EDU shape).
///
/// # Errors
/// See [`EduError`].
pub fn parse_edu(value: &serde_json::Value) -> Result<Edu, EduError> {
    let canonical_len = serde_json::to_vec(value)
        .map(|v| v.len())
        .unwrap_or(usize::MAX);
    if canonical_len > MAX_EDU_BYTES {
        return Err(EduError::TooLarge);
    }
    let object = value.as_object().ok_or(EduError::NotObject)?;
    // Reuse `hs-model`'s canonical-JSON validation so a malformed number (out-of-range integer,
    // non-finite float) is rejected the same way it would be for a PDU, rather than silently
    // accepted by `serde_json::Value`'s more permissive number handling.
    to_canonical_object(value, true).map_err(|e| EduError::Canonical(e.to_string()))?;

    let edu_type = object
        .get("edu_type")
        .and_then(serde_json::Value::as_str)
        .ok_or(EduError::MissingEduType)?
        .to_string();

    let content = match object.get("content") {
        None => serde_json::Value::Object(serde_json::Map::new()),
        Some(v) if v.is_object() => v.clone(),
        Some(_) => return Err(EduError::ContentNotObject),
    };

    Ok(Edu { edu_type, content })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_well_formed_edu() {
        let edu = parse_edu(&json!({"edu_type": "m.typing", "content": {"user_ids": []}})).unwrap();
        assert_eq!(edu.edu_type, "m.typing");
    }

    #[test]
    fn missing_content_defaults_to_empty_object() {
        let edu = parse_edu(&json!({"edu_type": "m.typing"})).unwrap();
        assert_eq!(edu.content, json!({}));
    }

    #[test]
    fn rejects_non_object_top_level() {
        assert_eq!(
            parse_edu(&json!([1, 2, 3])).unwrap_err(),
            EduError::NotObject
        );
    }

    #[test]
    fn rejects_missing_edu_type() {
        assert_eq!(
            parse_edu(&json!({"content": {}})).unwrap_err(),
            EduError::MissingEduType
        );
    }

    #[test]
    fn rejects_non_object_content() {
        assert_eq!(
            parse_edu(&json!({"edu_type": "m.typing", "content": "nope"})).unwrap_err(),
            EduError::ContentNotObject
        );
    }

    #[test]
    fn rejects_oversized_edu() {
        let big = "x".repeat(MAX_EDU_BYTES);
        let value = json!({"edu_type": "m.typing", "content": {"padding": big}});
        assert_eq!(parse_edu(&value).unwrap_err(), EduError::TooLarge);
    }
}
