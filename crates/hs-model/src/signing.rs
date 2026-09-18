//! Ed25519 signing and verification helpers over Ruma.
//!
//! Matrix signs canonical JSON objects: strip `signatures` and `unsigned`, canonicalize what is
//! left, sign or verify the resulting bytes, then store the signature base64-encoded (standard
//! alphabet, unpadded) under `signatures.<server name>.<algorithm>:<key id>`. This module
//! implements that directly against [`ed25519_dalek`] and this crate's own [`crate::canonical`]
//! (so the same cached canonical bytes an [`crate::event::Event`] already computed can be reused),
//! using Ruma's [`ServerName`] and key ID types at the API boundary. Its tests cross-check against
//! `ruma_signatures::{sign_json, verify_json}` (MIT license, Ruma project).
//!
//! Written from the "Signing JSON" and "Retrieving server keys" sections of the Matrix
//! specification appendices and server-server API
//! (`refs/matrix-spec/content/appendices.md`, `refs/matrix-spec/content/server-server-api.md`,
//! Apache-2.0).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use ruma::ServerName;

use crate::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
use crate::error::SigningError;

/// The signing algorithm name Matrix uses for ed25519 keys, as it appears in a key ID
/// (`ed25519:<version>`) and in `signatures.<server>.<algorithm>:<version>`.
pub const ALGORITHM: &str = "ed25519";

/// An ed25519 keypair identified by its Matrix key ID version (the part after `ed25519:`).
#[derive(Clone)]
pub struct SigningKeyPair {
    version: String,
    key: SigningKey,
}

impl SigningKeyPair {
    /// Wraps an existing ed25519 signing key under the given key ID version (for example, `"a_1"`
    /// in `ed25519:a_1`).
    #[must_use]
    pub fn new(version: impl Into<String>, key: SigningKey) -> Self {
        Self {
            version: version.into(),
            key,
        }
    }

    /// Generates a new random signing key under the given key ID version.
    #[must_use]
    pub fn generate(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            key: SigningKey::generate(&mut rand_core::OsRng),
        }
    }

    /// The key ID version (without the `ed25519:` prefix).
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The full Matrix key ID, `ed25519:<version>`.
    #[must_use]
    pub fn key_id(&self) -> String {
        format!("{ALGORITHM}:{}", self.version)
    }

    /// The public verifying key.
    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// The verifying key, base64-encoded (standard alphabet, unpadded), as published in a
    /// `/_matrix/key/v2/server` response.
    #[must_use]
    pub fn verifying_key_base64(&self) -> String {
        STANDARD_NO_PAD.encode(self.verifying_key().to_bytes())
    }
}

/// Signs `object`'s canonical form (with `signatures` and `unsigned` stripped) and inserts the
/// result into `object.signatures.<server_name>.<key_id>`.
///
/// # Errors
/// Returns [`SigningError::Canonical`] if `object` cannot be canonicalized in strict mode.
pub fn sign_object(
    object: &mut CanonicalJsonObject,
    server_name: &ServerName,
    key: &SigningKeyPair,
) -> Result<(), SigningError> {
    let signature = sign_bytes(&canonical_bytes_for_signing(object)?, key);
    let encoded = STANDARD_NO_PAD.encode(signature.to_bytes());
    insert_signature(object, server_name.as_str(), &key.key_id(), &encoded);
    Ok(())
}

/// Signs raw bytes directly, without any canonicalization. Exposed for callers (such as request
/// signing) that already have the exact bytes to sign.
#[must_use]
pub fn sign_bytes(bytes: &[u8], key: &SigningKeyPair) -> Signature {
    key.key.sign(bytes)
}

/// Verifies that `object` carries a valid signature from `server_name` under the given key ID and
/// verifying key.
///
/// # Errors
/// Returns [`SigningError`] if `object` cannot be canonicalized, has no `signatures` object, has
/// no signature from `server_name`/`key_id`, the signature is not valid base64 of the right
/// length, or verification fails.
pub fn verify_object(
    object: &CanonicalJsonObject,
    server_name: &str,
    key_id: &str,
    verifying_key: &VerifyingKey,
) -> Result<(), SigningError> {
    let encoded = signature_for(object, server_name, key_id)?;
    let raw = STANDARD_NO_PAD
        .decode(encoded)
        .map_err(|e| SigningError::InvalidEncoding(e.to_string()))?;
    let sig_bytes: [u8; 64] = raw
        .try_into()
        .map_err(|_| SigningError::InvalidSignatureLength)?;
    let signature = Signature::from_bytes(&sig_bytes);

    let bytes = canonical_bytes_for_signing(object)?;
    verifying_key
        .verify(&bytes, &signature)
        .map_err(|_| SigningError::VerificationFailed)
}

/// Builds a [`VerifyingKey`] from a base64-encoded (standard alphabet) public key, as published in
/// `/_matrix/key/v2/server`.
///
/// # Errors
/// Returns [`SigningError::InvalidEncoding`] or [`SigningError::InvalidKeyLength`] on malformed
/// input.
pub fn verifying_key_from_base64(encoded: &str) -> Result<VerifyingKey, SigningError> {
    let raw = STANDARD_NO_PAD
        .decode(encoded)
        .map_err(|e| SigningError::InvalidEncoding(e.to_string()))?;
    let bytes: [u8; 32] = raw.try_into().map_err(|_| SigningError::InvalidKeyLength)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| SigningError::InvalidKeyLength)
}

/// The canonical bytes to sign or verify: `object` with `signatures` and `unsigned` stripped,
/// then canonicalized in strict mode.
fn canonical_bytes_for_signing(object: &CanonicalJsonObject) -> Result<Vec<u8>, SigningError> {
    let mut stripped = object.clone();
    stripped.remove("signatures");
    stripped.remove("unsigned");
    Ok(CanonicalJsonValue::Object(stripped).to_canonical_bytes())
}

/// Reads an existing signature out of `object.signatures.<server_name>.<key_id>`.
fn signature_for<'o>(
    object: &'o CanonicalJsonObject,
    server_name: &str,
    key_id: &str,
) -> Result<&'o str, SigningError> {
    let signatures = object
        .get("signatures")
        .and_then(CanonicalJsonValue::as_object)
        .ok_or(SigningError::MissingSignatures)?;
    let by_server = signatures
        .get(server_name)
        .and_then(CanonicalJsonValue::as_object)
        .ok_or_else(|| SigningError::MissingServerSignature(server_name.to_owned()))?;
    by_server
        .get(key_id)
        .and_then(CanonicalJsonValue::as_str)
        .ok_or_else(|| SigningError::MissingKeySignature {
            server: server_name.to_owned(),
            key_id: key_id.to_owned(),
        })
}

/// Inserts a signature into `object.signatures.<server_name>.<key_id>`, creating the nested
/// objects as needed.
fn insert_signature(
    object: &mut CanonicalJsonObject,
    server_name: &str,
    key_id: &str,
    value: &str,
) {
    let signatures = object
        .entry("signatures".to_owned())
        .or_insert_with(|| CanonicalJsonValue::Object(CanonicalJsonObject::new()));
    if !matches!(signatures, CanonicalJsonValue::Object(_)) {
        *signatures = CanonicalJsonValue::Object(CanonicalJsonObject::new());
    }
    let CanonicalJsonValue::Object(signatures) = signatures else {
        unreachable!()
    };

    let by_server = signatures
        .entry(server_name.to_owned())
        .or_insert_with(|| CanonicalJsonValue::Object(CanonicalJsonObject::new()));
    if !matches!(by_server, CanonicalJsonValue::Object(_)) {
        *by_server = CanonicalJsonValue::Object(CanonicalJsonObject::new());
    }
    let CanonicalJsonValue::Object(by_server) = by_server else {
        unreachable!()
    };

    by_server.insert(
        key_id.to_owned(),
        CanonicalJsonValue::String(value.to_owned()),
    );
}

/// Converts a `serde_json::Value` object into strict canonical form for signing/verification
/// helpers that start from parsed JSON rather than an already-canonical object.
///
/// # Errors
/// Returns [`SigningError::Canonical`] if the value contains a non-conforming number.
pub fn to_signable_object(value: &serde_json::Value) -> Result<CanonicalJsonObject, SigningError> {
    Ok(to_canonical_object(value, true)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_sign_and_verify() {
        let key = SigningKeyPair::generate("1");
        let mut object = to_signable_object(&json!({"hello": "world"})).unwrap();

        sign_object(
            &mut object,
            ServerName::parse("example.org").unwrap().as_ref(),
            &key,
        )
        .unwrap();

        verify_object(&object, "example.org", &key.key_id(), &key.verifying_key()).unwrap();
    }

    #[test]
    fn verification_fails_on_tampered_content() {
        let key = SigningKeyPair::generate("1");
        let mut object = to_signable_object(&json!({"hello": "world"})).unwrap();
        sign_object(
            &mut object,
            ServerName::parse("example.org").unwrap().as_ref(),
            &key,
        )
        .unwrap();

        object.insert(
            "hello".to_owned(),
            CanonicalJsonValue::String("tampered".to_owned()),
        );

        let err =
            verify_object(&object, "example.org", &key.key_id(), &key.verifying_key()).unwrap_err();
        assert_eq!(err, SigningError::VerificationFailed);
    }

    #[test]
    fn missing_signature_is_reported() {
        let key = SigningKeyPair::generate("1");
        let object = to_signable_object(&json!({"hello": "world"})).unwrap();
        let err =
            verify_object(&object, "example.org", &key.key_id(), &key.verifying_key()).unwrap_err();
        assert_eq!(err, SigningError::MissingSignatures);
    }

    #[test]
    fn verifying_key_base64_round_trips() {
        let key = SigningKeyPair::generate("1");
        let encoded = key.verifying_key_base64();
        let decoded = verifying_key_from_base64(&encoded).unwrap();
        assert_eq!(decoded, key.verifying_key());
    }

    /// Cross-check against `ruma_signatures`: a signature we produce must verify under Ruma's own
    /// `verify_json`, given the same raw ed25519 public key bytes.
    #[test]
    fn cross_check_against_ruma_signatures() {
        let key = SigningKeyPair::generate("1");
        let mut object = to_signable_object(&json!({"a": 1, "b": [1, 2, 3]})).unwrap();
        sign_object(
            &mut object,
            ServerName::parse("example.org").unwrap().as_ref(),
            &key,
        )
        .unwrap();

        let ruma_object: ruma::CanonicalJsonObject = serde_json::from_slice(
            &CanonicalJsonValue::Object(object.clone()).to_canonical_bytes(),
        )
        .unwrap();

        let public_key_set: ruma::signatures::PublicKeySet = std::collections::BTreeMap::from([(
            key.key_id(),
            ruma::serde::Base64::new(key.verifying_key().to_bytes().to_vec()),
        )]);
        let public_key_map: ruma::signatures::PublicKeyMap =
            std::collections::BTreeMap::from([("example.org".to_owned(), public_key_set)]);

        ruma::signatures::verify_json(&public_key_map, &ruma_object).unwrap();
    }
}
