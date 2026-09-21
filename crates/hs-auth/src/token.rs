//! Token formats: generation, shape validation and hashing at rest.
//!
//! Shapes match Synapse 1.161 (`synapse/handlers/auth.py`) byte-for-byte so that a token minted
//! by Synapse and imported into this server's user/device/token tables keeps working, and so that
//! a token minted by this server is indistinguishable from one Synapse would have issued:
//!
//! - Access token: `syt_<unpadded base64 localpart>_<20 random ascii letters>_<6+ char base62 crc32>`
//! - Refresh token: `syr_<unpadded base64 localpart>_<20 random ascii letters>_<6+ char base62 crc32>`
//! - Short-term login token (`m.login.token`): `syl_<20 random ascii letters>_<6+ char base62 crc32>`
//!
//! The checksum is `base62(crc32(ascii_bytes(prefix_and_random)))`, zero-padded to at least 6
//! base62 digits (Synapse's `base62_encode(crc32(base), minwidth=6)`). It is not a security
//! boundary — tokens are looked up by their hash at rest regardless of shape — it exists so the
//! generated strings are structurally identical to Synapse's for tooling that pattern-matches on
//! them (log scrubbers, some client SDKs) and so imported tokens round-trip through
//! [`TokenKind::parse_shape`] the same way freshly minted ones do.
//!
//! See `docs/rfcs/0002-auth-tokens-and-requester.md` section 3 for the full design, including why
//! hashing (not encrypting or storing in cleartext) is used at rest.

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use rand::Rng;
use rand::distr::Alphanumeric;
use sha2::{Digest, Sha256};

const RANDOM_LEN: usize = 20;
const CRC_MINWIDTH: usize = 6;
const BASE62_ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Which of the three Synapse-shaped opaque tokens a string looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// `syt_...`: a long-lived (or refresh-bounded) access token.
    Access,
    /// `syr_...`: a refresh token, exchanged at `/refresh` for a new access/refresh pair.
    Refresh,
    /// `syl_...`: a short-term login token, exchanged at `/login` with `m.login.token`.
    Login,
}

impl TokenKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Access => "syt_",
            Self::Refresh => "syr_",
            Self::Login => "syl_",
        }
    }
}

fn base62_encode(mut num: u32, minwidth: usize) -> String {
    let mut out = Vec::new();
    if num == 0 {
        out.push(BASE62_ALPHABET[0]);
    }
    while num > 0 {
        let rem = (num % 62) as usize;
        out.push(BASE62_ALPHABET[rem]);
        num /= 62;
    }
    out.reverse();
    let mut s = String::from_utf8(out).expect("base62 alphabet is ascii");
    while s.len() < minwidth {
        s.insert(0, '0');
    }
    s
}

fn random_ascii_letters(len: usize) -> String {
    let mut rng = rand::rng();
    // Matches Synapse's `random_string`: uniform over `[a-zA-Z]`. `Alphanumeric` draws from
    // `[a-zA-Z0-9]`, so digits are filtered and re-drawn to keep the distribution uniform over
    // just letters rather than biasing toward the digits that happen to survive a mod-52 fold.
    std::iter::repeat_with(|| rng.sample(Alphanumeric) as char)
        .filter(char::is_ascii_alphabetic)
        .take(len)
        .collect()
}

fn crc32_base62(base: &str) -> String {
    let crc = crc32fast::hash(base.as_bytes());
    base62_encode(crc, CRC_MINWIDTH)
}

/// Generates a fresh access token for `localpart`, shaped like Synapse's `syt_` tokens.
#[must_use]
pub fn generate_access_token(localpart: &str) -> String {
    generate_user_scoped(TokenKind::Access, localpart)
}

/// Generates a fresh refresh token for `localpart`, shaped like Synapse's `syr_` tokens.
#[must_use]
pub fn generate_refresh_token(localpart: &str) -> String {
    generate_user_scoped(TokenKind::Refresh, localpart)
}

/// Generates a fresh short-term login token, shaped like Synapse's `syl_` tokens. Unlike access
/// and refresh tokens, the login token does not embed the localpart (Synapse's
/// `generate_login_token` takes no user argument either); the association with a user lives in
/// the token store record, not in the token's bytes.
#[must_use]
pub fn generate_login_token() -> String {
    let random = random_ascii_letters(RANDOM_LEN);
    let base = format!("syl_{random}");
    let crc = crc32_base62(&base);
    format!("{base}_{crc}")
}

/// How many letters a setup token has. Each is one of 52, so 40 of them is about 228 bits:
/// guessing it is not a way in.
const SETUP_TOKEN_LEN: usize = 40;

/// Generates a first-run setup token (see [`crate::setup`]). Letters only, so it survives being
/// put in a URL fragment, pasted from a terminal, or read out of a JSON log line without any
/// escaping to get wrong.
#[must_use]
pub fn generate_setup_token() -> String {
    random_ascii_letters(SETUP_TOKEN_LEN)
}

fn generate_user_scoped(kind: TokenKind, localpart: &str) -> String {
    let b64local = STANDARD_NO_PAD.encode(localpart.as_bytes());
    let random = random_ascii_letters(RANDOM_LEN);
    let base = format!("{}{b64local}_{random}", kind.prefix());
    let crc = crc32_base62(&base);
    format!("{base}_{crc}")
}

/// Checks that `token` has the `syr_<localpart>_<random>_<crc>` shape and that its checksum is
/// correct, the way Synapse's `_verify_refresh_token` does before bothering the store with a
/// lookup. This is a cheap sanity filter, not a security check: a token failing this check is
/// certainly invalid, but a token passing it still has to be looked up (by hash) in the token
/// store to be trusted. Tokens imported from Synapse always pass this, since the shape is
/// identical; hand-rolled or truncated tokens usually fail it immediately.
#[must_use]
pub fn parse_shape(token: &str, kind: TokenKind) -> bool {
    let want_prefix = &kind.prefix()[..kind.prefix().len() - 1]; // drop trailing '_'

    match kind {
        TokenKind::Login => {
            // syl_<random>_<crc>: exactly three underscore-delimited parts.
            let parts: Vec<&str> = token.splitn(3, '_').collect();
            let [prefix, random, crc] = parts.as_slice() else {
                return false;
            };
            *prefix == want_prefix && crc32_base62(&format!("{prefix}_{random}")) == *crc
        }
        TokenKind::Access | TokenKind::Refresh => {
            // syt_/syr_<localpart>_<random>_<crc>: exactly four underscore-delimited parts.
            let parts: Vec<&str> = token.splitn(4, '_').collect();
            let [prefix, localpart, random, crc] = parts.as_slice() else {
                return false;
            };
            *prefix == want_prefix
                && crc32_base62(&format!("{prefix}_{localpart}_{random}")) == *crc
        }
    }
}

/// The hash of a token as stored at rest. Tokens are opaque bearer secrets; the store never
/// holds the token string itself, only this hash, so a compromise of the store (a backup, a
/// misconfigured export) does not hand out working credentials.
///
/// SHA-256 (not a slow password hash) is correct here because the input is already
/// high-entropy — 20 cryptographically random ASCII letters is about 114 bits of entropy — so
/// unlike a password there is no offline dictionary to defend against; a fast, deterministic hash
/// is exactly what a keyed lookup by opaque bearer token needs.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct TokenHash(#[serde(with = "hex_bytes")] pub [u8; 32]);

impl TokenHash {
    /// Hashes a token string for storage or lookup.
    #[must_use]
    pub fn of(token: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Self(out)
    }

    /// Lowercase hex encoding, for logging (never log the token itself) and for use as a plain
    /// string key in tables that want one.
    #[must_use]
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl std::fmt::Display for TokenHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        bytes
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_has_expected_shape() {
        let token = generate_access_token("alice");
        assert!(token.starts_with("syt_"));
        assert!(parse_shape(&token, TokenKind::Access));
        assert!(!parse_shape(&token, TokenKind::Refresh));
        // localpart round-trips through the unpadded base64 segment.
        let b64local = STANDARD_NO_PAD.encode("alice");
        assert!(token.contains(&format!("syt_{b64local}_")));
    }

    #[test]
    fn refresh_token_has_expected_shape() {
        let token = generate_refresh_token("bob");
        assert!(token.starts_with("syr_"));
        assert!(parse_shape(&token, TokenKind::Refresh));
        assert!(!parse_shape(&token, TokenKind::Access));
    }

    #[test]
    fn login_token_has_expected_shape_and_no_localpart() {
        let token = generate_login_token();
        assert!(token.starts_with("syl_"));
        assert!(parse_shape(&token, TokenKind::Login));
        // syl_<20 letters>_<crc>: exactly 4 underscore-delimited-ish parts when split on '_'
        // starting after the fixed prefix; no base64 localpart segment is present.
        let random_part = token
            .strip_prefix("syl_")
            .unwrap()
            .rsplit_once('_')
            .unwrap()
            .0;
        assert_eq!(random_part.len(), RANDOM_LEN);
        assert!(random_part.chars().all(|c| c.is_ascii_alphabetic()));
    }

    #[test]
    fn tampering_breaks_the_checksum() {
        let mut token = generate_access_token("alice");
        // Flip the last character of the random segment.
        let last = token.pop().unwrap();
        let replacement = if last == 'a' { 'b' } else { 'a' };
        token.push(replacement);
        assert!(!parse_shape(&token, TokenKind::Access));
    }

    #[test]
    fn garbage_strings_do_not_parse() {
        assert!(!parse_shape("", TokenKind::Access));
        assert!(!parse_shape("not_a_token", TokenKind::Access));
        assert!(!parse_shape("syt_", TokenKind::Access));
        assert!(!parse_shape("syr_abc_def_ghijkl", TokenKind::Access));
    }

    #[test]
    fn tokens_are_unique() {
        let a = generate_access_token("alice");
        let b = generate_access_token("alice");
        assert_ne!(a, b);
    }

    #[test]
    fn hash_is_deterministic_and_sensitive_to_input() {
        let h1 = TokenHash::of("syt_abc");
        let h2 = TokenHash::of("syt_abc");
        let h3 = TokenHash::of("syt_abd");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1.to_hex().len(), 64);
    }

    #[test]
    fn hash_serde_round_trips() {
        let h = TokenHash::of("some-token-value");
        let json = serde_json::to_string(&h).unwrap();
        let back: TokenHash = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    /// Cross-check against Synapse's actual algorithm, transcribed from
    /// `synapse/handlers/auth.py` and `synapse/util/stringutils.py` (behavioral reference only,
    /// no code copied): `base62_encode(crc32(base.encode("ascii")), minwidth=6)`. The expected
    /// `crc`/`base62` values below were computed independently with Python's `zlib.crc32` and a
    /// transcription of Synapse's `base62_encode`, not derived from this module, so this is a
    /// genuine cross-check of both the CRC-32 variant (`crc32fast` must agree with
    /// `zlib.crc32`/`binascii.crc32`, i.e. CRC-32/ISO-HDLC) and our base62 alphabet and padding.
    #[test]
    fn crc32_base62_matches_an_independently_computed_oracle() {
        let cases: &[(&str, u32, &str)] = &[
            ("syt_YWxpY2U_abcdefghijklmnopqrst", 1_349_959_498, "1TMI8I"),
            ("syr_Ym9i_ZYXWVUTSRQPONMLKJIHGFE", 335_419_768, "0MhO0G"),
            ("syl_QRSTUVWXYZabcdefghij", 2_801_688_739, "33bbE3"),
        ];
        for (base, expected_crc, expected_base62) in cases {
            let crc = crc32fast::hash(base.as_bytes());
            assert_eq!(crc, *expected_crc, "crc32 mismatch for {base}");
            assert_eq!(
                crc32_base62(base),
                *expected_base62,
                "base62 mismatch for {base}"
            );
        }
    }
}
