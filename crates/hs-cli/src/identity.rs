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
//! One is generated and **written** to `signing_key_path`, then loaded back, so a first run is a
//! first run rather than a broken one: an operator who has never heard of
//! `hs generate-signing-key` gets a server whose events keep verifying across restarts.
//!
//! This used to generate an ephemeral key and keep it only in memory, logged at `warn`. That was
//! honest but the consequence was not obvious from the warning: every restart signed with a
//! *different* key, so events this server had already sent stopped verifying against the key it
//! now advertised, and any room it shared with another server quietly broke. If the directory
//! cannot be written — a read-only mount, a path owned by somebody else — the old behaviour is
//! still what happens, since refusing to start would be worse than running; the warning now says
//! which of the two situations the operator is in.

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

/// Writes a newly generated signing key into `dir` and returns it, or `None` if the directory
/// could not be written.
///
/// `create_new` rather than `write`: two processes starting at once on the same data directory
/// must not each write a key and leave the loser signing with one the winner's file does not
/// contain. The loser's write fails, it re-reads the directory, and both end up on the same key.
///
/// The file is `0600` where the platform has permissions to set. A private signing key readable
/// by every account on the host is the sort of thing that is discovered years later.
fn generate_and_persist(dir: &Path) -> Option<SigningKeyPair> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join("hs.signing.key");
    let line = crate::signing_key::generate_signing_key_line();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(line.as_bytes()).ok()?;
            file.sync_all().ok()?;
            tracing::info!(path = %path.display(), "generated this server's signing key");
            parse_signing_key_line(line.trim_end())
        }
        // Somebody else won the race, or a key appeared between the scan and now: whatever is
        // there is the key, and it is the one both processes must use.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => first_signing_key_in_dir(dir),
        Err(_) => None,
    }
}

/// Builds this server's [`HomeserverIdentity`]: `config.server.server_name` plus the signing key
/// found at `config.server.signing_key_path`, generating and persisting one there if the
/// directory holds none. See the module doc for what happens when it cannot be written.
///
/// # Errors
/// Returns the [`ruma::IdParseError`] from parsing `server.server_name` if it is not a valid
/// Matrix server name (mirrors `crate::config_bridge`'s same check for `hs-auth`'s config).
pub fn load_or_generate(
    config: &hs_config::Config,
) -> Result<HomeserverIdentity, ruma::IdParseError> {
    let server_name: ruma::OwnedServerName = ruma::ServerName::parse(&config.server.server_name)?;
    let dir = &config.server.signing_key_path;
    let signing_key = match first_signing_key_in_dir(dir) {
        Some(pair) => pair,
        None => match generate_and_persist(dir) {
            Some(pair) => pair,
            None => {
                tracing::warn!(
                    path = %dir.display(),
                    "no ed25519 signing key found and none could be written there; signing with \
                     an ephemeral key for this process only. Every restart will sign with a \
                     different key and events this server has already sent will stop verifying \
                     -- make that directory writable, or run `hs generate-signing-key -o \
                     <path>/hs.signing.key` somewhere this process can read."
                );
                SigningKeyPair::generate("a_ephemeral")
            }
        },
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

    /// The first-run behaviour: no key, no `hs generate-signing-key`, and yet the server comes up
    /// with a key that is still there — and still the same key — on the next boot.
    #[test]
    fn a_first_run_generates_a_key_and_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = hs_config::Config::default();
        config.server.server_name = "example.org".to_owned();
        config.server.signing_key_path = dir.path().join("keys");

        let first = load_or_generate(&config).unwrap();
        assert_eq!(first.server_name.as_str(), "example.org");
        assert!(dir.path().join("keys").join("hs.signing.key").is_file());

        let second = load_or_generate(&config).unwrap();
        assert_eq!(
            first.signing_key.version(),
            second.signing_key.version(),
            "a restart must sign with the key the first run wrote, not a new one"
        );
    }

    /// A directory that cannot be created falls back to an ephemeral key rather than refusing to
    /// start: running with a warning beats not running at all.
    #[test]
    fn an_unwritable_key_directory_still_starts() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut config = hs_config::Config::default();
        config.server.server_name = "example.org".to_owned();
        // A path *under a regular file* can never be created as a directory.
        config.server.signing_key_path = file.path().join("keys");
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
