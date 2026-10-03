//! The cross-signing identity of an instance's bot, held by the manager.
//!
//! A client that excludes insecure devices (Element's "Exclude insecure devices when
//! sending/receiving messages", Element X's invisible crypto, `matrix-sdk`'s
//! `CollectStrategy::IdentityBasedStrategy`, MSC4153) shares a room's Megolm keys only with
//! devices signed by their owner's self-signing key, and with no device at all of a user who has
//! published no identity. A mautrix bridge's bot has no identity unless `encryption.self_sign`
//! is on, so such a client withholds every key from it (`m.room_key.withheld`, `m.unverified`)
//! and the bridge answers "⚠️ Your message was not bridged: your client refused to share
//! decryption keys with the bridge". The bridge's own `self_sign` was not used (status 11,
//! 2026-10-03): it keeps the recovery key in the bridge's database and exits when the server
//! has keys that database does not (every instance recreated for the same owner, every reset
//! bridge database). Instead the manager mints the bot's master and self-signing keys, keeps
//! their seeds on the instance row beside the pickle key, uploads the public keys as the
//! appservice (which this server lets replace existing keys without user-interactive auth,
//! MSC4190), and signs the bot's device with the self-signing key once the device exists;
//! a device the bridge makes later (a reset database) is signed on a later step.
//!
//! The shapes are the spec's (`/keys/device_signing/upload`, `/keys/signatures/upload`): a key
//! object `{user_id, usage, keys: {"ed25519:<public key>": "<public key>"}, signatures}`, every
//! signature over the object's canonical JSON with `signatures` and `unsigned` stripped, stored
//! under `signatures.<user id>.<key id>`, the way `crates/hs-e2e/tests/cross_signing.rs` builds
//! one.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ed25519_dalek::SigningKey;
use hs_model::canonical::CanonicalJsonValue;
use hs_model::signing::{SigningKeyPair, sign_bytes, to_signable_object};
use serde_json::{Value, json};

/// The two seeds, hex-encoded, as the instance row keeps them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seeds {
    /// The master key's 32-byte seed, hex.
    pub master: String,
    /// The self-signing key's 32-byte seed, hex.
    pub self_signing: String,
}

impl Seeds {
    /// Fresh random seeds.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            master: crate::random_hex(32),
            self_signing: crate::random_hex(32),
        }
    }
}

/// A bot's identity: its master key and the self-signing key the master key vouches for.
pub struct BotIdentity {
    user_id: String,
    master: SigningKeyPair,
    self_signing: SigningKeyPair,
}

fn pair_from_seed(seed_hex: &str) -> Result<SigningKeyPair, String> {
    let bytes = hex::decode(seed_hex).map_err(|e| format!("a seed is not hex: {e}"))?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "a seed is not 32 bytes".to_owned())?;
    let key = SigningKey::from_bytes(&seed);
    let public = STANDARD_NO_PAD.encode(key.verifying_key().to_bytes());
    Ok(SigningKeyPair::new(public, key))
}

/// `value`'s signature by `key`: over its canonical JSON with `signatures` and `unsigned`
/// stripped, base64 without padding, as signed JSON is.
fn signature_of(value: &Value, key: &SigningKeyPair) -> Result<String, String> {
    let mut canonical = to_signable_object(value).map_err(|e| e.to_string())?;
    canonical.remove("signatures");
    canonical.remove("unsigned");
    let bytes = CanonicalJsonValue::Object(canonical).to_canonical_bytes();
    Ok(STANDARD_NO_PAD.encode(sign_bytes(&bytes, key).to_bytes()))
}

impl BotIdentity {
    /// `user_id`'s identity from its seeds.
    ///
    /// # Errors
    /// A seed that is not 32 hex-encoded bytes.
    pub fn from_seeds(user_id: &str, seeds: &Seeds) -> Result<Self, String> {
        Ok(Self {
            user_id: user_id.to_owned(),
            master: pair_from_seed(&seeds.master)?,
            self_signing: pair_from_seed(&seeds.self_signing)?,
        })
    }

    /// The master key's ID, `ed25519:<public key>`: what `/keys/query` lists under
    /// `master_keys.<user>.keys`.
    #[must_use]
    pub fn master_key_id(&self) -> String {
        self.master.key_id()
    }

    /// The master public key, base64.
    #[must_use]
    pub fn master_public_key(&self) -> String {
        self.master.verifying_key_base64()
    }

    /// The self-signing key's ID, `ed25519:<public key>`: the signer a cross-signed device's
    /// `signatures.<user>` carries.
    #[must_use]
    pub fn self_signing_key_id(&self) -> String {
        self.self_signing.key_id()
    }

    fn key_object(&self, usage: &str, pair: &SigningKeyPair) -> Value {
        json!({
            "user_id": self.user_id,
            "usage": [usage],
            "keys": { pair.key_id(): pair.verifying_key_base64() },
        })
    }

    /// The body of `POST /keys/device_signing/upload`: the master key, and the self-signing
    /// key signed by it. No user-signing key: the bot vouches for nobody.
    ///
    /// # Errors
    /// The objects built here do not canonicalise, which they always do.
    pub fn upload_body(&self) -> Result<Value, String> {
        let master = self.key_object("master", &self.master);
        let mut self_signing = self.key_object("self_signing", &self.self_signing);
        let signature = signature_of(&self_signing, &self.master)?;
        self_signing["signatures"] =
            json!({ self.user_id.clone(): { self.master.key_id(): signature } });
        Ok(json!({ "master_key": master, "self_signing_key": self_signing }))
    }

    /// Whether `/keys/query`'s `master_keys.<bot>` entry is this identity's master key.
    #[must_use]
    pub fn is_published_master(&self, master_keys_entry: Option<&Value>) -> bool {
        master_keys_entry
            .and_then(|m| m.get("keys"))
            .and_then(|k| k.get(self.master_key_id()))
            .and_then(Value::as_str)
            == Some(self.master_public_key().as_str())
    }

    /// Whether a device-keys object (`/keys/query`'s `device_keys.<bot>.<device>`) carries this
    /// identity's self-signing signature.
    #[must_use]
    pub fn has_signed_device(&self, device_keys: &Value) -> bool {
        device_keys
            .get("signatures")
            .and_then(|s| s.get(&self.user_id))
            .and_then(|s| s.get(self.self_signing_key_id()))
            .is_some()
    }

    /// The body of `POST /keys/signatures/upload` that signs `device_keys` (a device-keys
    /// object from `/keys/query`, for `device_id`) with the self-signing key:
    /// `{<bot>: {<device>: <device keys with the signature added>}}`.
    ///
    /// # Errors
    /// `device_keys` is not an object, or does not canonicalise.
    pub fn sign_device(&self, device_id: &str, device_keys: &Value) -> Result<Value, String> {
        if !device_keys.is_object() {
            return Err("device keys are not an object".to_owned());
        }
        let signature = signature_of(device_keys, &self.self_signing)?;
        let mut signed = device_keys.clone();
        let own = signed
            .as_object_mut()
            .and_then(|o| {
                o.entry("signatures")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
            })
            .and_then(|s| {
                s.entry(self.user_id.clone())
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
            })
            .ok_or("device keys' signatures are not an object")?;
        own.insert(self.self_signing_key_id(), Value::String(signature));
        Ok(json!({ self.user_id.clone(): { device_id: signed } }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_model::signing::{verify_object, verifying_key_from_base64};

    const BOT: &str = "@whatsappbot_alice:example.org";

    fn verifies(value: &Value, signer_key_id: &str, public_key: &str) -> bool {
        let canonical = to_signable_object(value).unwrap();
        let key = verifying_key_from_base64(public_key).unwrap();
        verify_object(&canonical, BOT, signer_key_id, &key).is_ok()
    }

    #[test]
    fn the_same_seeds_give_the_same_identity() {
        let seeds = Seeds::generate();
        let a = BotIdentity::from_seeds(BOT, &seeds).unwrap();
        let b = BotIdentity::from_seeds(BOT, &seeds).unwrap();
        assert_eq!(a.master_key_id(), b.master_key_id());
        assert_eq!(a.self_signing_key_id(), b.self_signing_key_id());
        assert_ne!(a.master_key_id(), a.self_signing_key_id());
        assert!(a.master_key_id().starts_with("ed25519:"));
        let bad = Seeds {
            master: "zz".into(),
            self_signing: seeds.self_signing,
        };
        assert!(BotIdentity::from_seeds(BOT, &bad).is_err());
    }

    #[test]
    fn the_upload_body_is_a_master_key_and_a_self_signing_key_it_signed() {
        let identity = BotIdentity::from_seeds(BOT, &Seeds::generate()).unwrap();
        let body = identity.upload_body().unwrap();
        let master = &body["master_key"];
        assert_eq!(master["user_id"], BOT);
        assert_eq!(master["usage"], json!(["master"]));
        assert_eq!(
            master["keys"][identity.master_key_id()],
            identity.master_public_key()
        );
        assert!(master.get("signatures").is_none(), "a bare master key");
        let ssk = &body["self_signing_key"];
        assert_eq!(ssk["usage"], json!(["self_signing"]));
        assert!(ssk["signatures"][BOT][identity.master_key_id()].is_string());
        assert!(verifies(
            ssk,
            &identity.master_key_id(),
            &identity.master_public_key()
        ));
        assert!(body.get("user_signing_key").is_none());
        assert!(identity.is_published_master(Some(master)));
        assert!(!identity.is_published_master(None));
        assert!(!identity.is_published_master(Some(&json!({"keys": {"ed25519:other": "x"}}))));
    }

    #[test]
    fn a_device_is_signed_by_the_self_signing_key_and_keeps_its_own_signature() {
        let identity = BotIdentity::from_seeds(BOT, &Seeds::generate()).unwrap();
        let device = json!({
            "user_id": BOT,
            "device_id": "IEXNEKZESJ",
            "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
            "keys": {"curve25519:IEXNEKZESJ": "c", "ed25519:IEXNEKZESJ": "e"},
            "signatures": {BOT: {"ed25519:IEXNEKZESJ": "the device's own"}},
            "unsigned": {"device_display_name": "WhatsApp bridge"},
        });
        assert!(!identity.has_signed_device(&device));
        let body = identity.sign_device("IEXNEKZESJ", &device).unwrap();
        let signed = &body[BOT]["IEXNEKZESJ"];
        assert_eq!(
            signed["signatures"][BOT]["ed25519:IEXNEKZESJ"],
            "the device's own"
        );
        assert!(identity.has_signed_device(signed));
        let ssk_public = identity
            .self_signing_key_id()
            .trim_start_matches("ed25519:")
            .to_owned();
        assert!(verifies(
            signed,
            &identity.self_signing_key_id(),
            &ssk_public
        ));
        // `unsigned` is outside the signature: the same device with a new display name still
        // verifies, as the spec's signed JSON requires.
        let mut renamed = signed.clone();
        renamed["unsigned"]["device_display_name"] = json!("renamed");
        assert!(verifies(
            &renamed,
            &identity.self_signing_key_id(),
            &ssk_public
        ));
        assert!(identity.sign_device("X", &json!("not an object")).is_err());
    }
}
