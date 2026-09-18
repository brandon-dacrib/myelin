//! Loads this server's [`hs_room::identity::HomeserverIdentity`] (the signing key `hs-room`
//! stamps onto every locally-originated event) from `server.signing_key_path`
//! (`hs_config::ServerConfig::signing_key_path`), the same directory `hs generate-signing-key`
//! writes into (`crate::signing_key`, Synapse's `signing.key` text format: one
//! `<algorithm> <key_id> <base64-seed>` line per file).
//!
//! # Why this lives in `hs-cli`, not `hs-model` or `hs-room`
//!
//! `hs-model::signing::SigningKeyPair` deliberately exposes no way to read a private key back out
//! (`crate::signing_key`'s own doc comment: "nothing else in the workspace needs to serialize a
//! private signing key to a file"), so `hs generate-signing-key` re-derives the encoding
//! independently rather than adding an accessor to a crate this track does not own. This module
//! is the read-side mirror of that same choice: it decodes a Synapse-shaped signing-key file
//! directly with `ed25519-dalek`, then hands the raw key to
//! [`hs_model::signing::SigningKeyPair::new`] (which *does* accept an already-built key).
//!
//! # What happens with no key on disk
//!
//! A fresh [`hs_model::signing::SigningKeyPair::generate`] is used instead, logged at `warn`. This
//! is honest, not silent: the generated key is **not** persisted anywhere, so every `hs serve`
//! restart with no signing key file signs with a *different* key, and past events' signatures
//! stop matching the server's currently-advertised key. Run `hs generate-signing-key -o
//! <signing_key_path>/hs.signing.key` (or point `--synapse-config` at a Synapse deployment that
//! already has one) before relying on room state surviving a restart. Tracked as a known gap in
//! `docs/status/12-platform-and-kubernetes.md`.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ed25519_dalek::SigningKey;
use hs_model::signing::SigningKeyPair;
use hs_room::identity::HomeserverIdentity;

/// Parses one Synapse-shaped signing-key line (`<algorithm> <key_id> <base64-seed>`) into a
/// [`SigningKeyPair`]. Only the `ed25519` algorithm is understood (the only one this workspace
/// ever writes or the spec requires); any other algorithm on the line, or a malformed line, is
/// skipped by the caller rather than treated as fatal, matching Synapse's own tolerant multi-line
/// signing-key file format (a directory or file may carry keys for algorithms/purposes this
/// server does not use).
fn parse_signing_key_line(line: &str) -> Option<SigningKeyPair> {
    let mut fields = line.split_whitespace();
    let algorithm = fields.next()?;
    let key_id = fields.next()?;
    let encoded = fields.next()?;
    if algorithm != "ed25519" {
        return None;
    }
    let seed = STANDARD_NO_PAD.decode(encoded).ok()?;
    let seed: [u8; 32] = seed.try_into().ok()?;
    let key = SigningKey::from_bytes(&seed);
    Some(SigningKeyPair::new(key_id, key))
}

/// Reads every `ed25519` signing-key line out of every regular file directly inside `dir`
/// (non-recursive — matching Synapse's own `signing_key_path` layout, one key file per directory
/// entry), in directory-listing order, and returns the first one found. `hs generate-signing-key`
/// writes one key per file, so in the common case this is simply "the key in the one file that
/// exists".
fn first_signing_key_in_dir(dir: &Path) -> Option<SigningKeyPair> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    // Deterministic order: `read_dir` gives no ordering guarantee, and picking "the first key
    // found" should not depend on filesystem-specific directory-entry ordering.
    paths.sort();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(pair) = parse_signing_key_line(line) {
                return Some(pair);
            }
        }
    }
    None
}

/// Builds this server's [`HomeserverIdentity`]: `config.server.server_name` plus the signing key
/// found at `config.server.signing_key_path`, or a freshly generated (unpersisted) one if none is
/// found there. See the module doc for what "none found" means for restart durability.
///
/// # Errors
/// Returns the [`ruma::IdParseError`] from parsing `server.server_name` if it is not a valid
/// Matrix server name (mirrors `crate::config_bridge`'s same check for `hs-auth`'s config).
pub fn load_or_generate(
    config: &hs_config::Config,
) -> Result<HomeserverIdentity, ruma::IdParseError> {
    let server_name: ruma::OwnedServerName = ruma::ServerName::parse(&config.server.server_name)?;
    let signing_key = match first_signing_key_in_dir(&config.server.signing_key_path) {
        Some(pair) => pair,
        None => {
            tracing::warn!(
                path = %config.server.signing_key_path.display(),
                "no ed25519 signing key found; generating an ephemeral one for this process \
                 only -- run `hs generate-signing-key` to persist one across restarts"
            );
            SigningKeyPair::generate("a_ephemeral")
        }
    };
    Ok(HomeserverIdentity {
        server_name,
        signing_key: std::sync::Arc::new(signing_key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_synapse_shaped_line() {
        let line = crate::signing_key::generate_signing_key_line();
        let pair = parse_signing_key_line(line.trim_end()).unwrap();
        assert!(pair.version().starts_with("a_"));
    }

    #[test]
    fn rejects_non_ed25519_algorithm() {
        assert!(parse_signing_key_line("rsa a_1 deadbeef").is_none());
    }

    #[test]
    fn rejects_malformed_line() {
        assert!(parse_signing_key_line("ed25519 a_1").is_none());
        assert!(parse_signing_key_line("not enough").is_none());
    }

    #[test]
    fn finds_a_key_written_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let line = crate::signing_key::generate_signing_key_line();
        std::fs::write(dir.path().join("hs.signing.key"), &line).unwrap();
        let found = first_signing_key_in_dir(dir.path()).unwrap();
        let expected = parse_signing_key_line(line.trim_end()).unwrap();
        assert_eq!(found.version(), expected.version());
    }

    #[test]
    fn generates_an_ephemeral_key_when_the_directory_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = hs_config::Config::default();
        config.server.server_name = "example.org".to_owned();
        config.server.signing_key_path = dir.path().to_owned();
        let identity = load_or_generate(&config).unwrap();
        assert_eq!(identity.server_name.as_str(), "example.org");
    }

    #[test]
    fn rejects_an_invalid_server_name() {
        let mut config = hs_config::Config::default();
        config.server.server_name = "not a valid server name".to_owned();
        assert!(load_or_generate(&config).is_err());
    }
}
