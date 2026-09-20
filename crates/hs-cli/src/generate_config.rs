//! `hs generate-config`: writes the *bootstrap* file — the few settings that have to be known
//! before this server can open the database the rest of its configuration lives in.
//!
//! It used to serialize a whole [`hs_config::Config`], which came to 158 lines of YAML, three of
//! which (the database directory, the media path and the signing-key directory) had to be
//! repointed by hand before a container could write anything, and a fourth (`enable_registration`)
//! before anybody could sign up. That is four hand-edits standing between a `docker pull` and a
//! working server, and a file an operator is then responsible for forever.
//!
//! What it writes now is the answer to two questions — what is this server called, and where does
//! it keep its data — with every path already pointing inside one directory, so the file works as
//! written. Everything else is a default until somebody changes it, and a change belongs in the
//! database, where the web interface can make it and a restart cannot undo it
//! (`crate::bootstrap`, `hs_config::layered`).
//!
//! An operator who wants no file at all does not need one: `hs serve --data-dir <dir>
//! --server-name <name>` is the same configuration without the intermediate step.

use std::path::Path;

use crate::bootstrap::DataLayout;

/// Renders the bootstrap file for `server_name`, with every filesystem path underneath
/// `data_dir`.
#[must_use]
pub fn render_yaml(server_name: &str, data_dir: &Path) -> String {
    let layout = DataLayout::new(data_dir);
    format!(
        "# Myelin bootstrap configuration for {server_name:?}, written by `hs generate-config`.\n\
         #\n\
         # This file holds only what has to be known before the database opens. Every other\n\
         # setting lives in the database, where the admin web interface can change it without a\n\
         # restart and without editing anything here: see `hs config show` for what this server\n\
         # is actually running on, and where each value came from.\n\
         #\n\
         # Settings in this file seed the database on the first run and are outranked by it\n\
         # afterwards, so changing a value here later will not undo a change made in the UI.\n\
         server:\n\
         \x20 server_name: {server_name:?}\n\
         \x20 signing_key_path: {keys:?}\n\
         storage:\n\
         \x20 backend: embedded\n\
         \x20 data_dir: {database:?}\n\
         media:\n\
         \x20 storage:\n\
         \x20   backend: local\n\
         \x20   path: {media:?}\n",
        keys = layout.signing_keys.display().to_string(),
        database = layout.database.display().to_string(),
        media = layout.media.display().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_config::Config;

    #[test]
    fn what_it_writes_is_a_valid_configuration() {
        let yaml = render_yaml("example.org", Path::new("/var/lib/myelin"));
        let config = Config::from_yaml(&yaml).unwrap();
        assert_eq!(config.server.server_name, "example.org");
        assert_eq!(
            config.server.signing_key_path,
            Path::new("/var/lib/myelin/keys")
        );
    }

    /// The bug this file exists to fix: every path the server writes to has to already be inside
    /// the directory the operator mounted, or the first run is three hand-edits long.
    #[test]
    fn every_path_it_writes_is_under_the_data_directory() {
        let yaml = render_yaml("example.org", Path::new("/var/lib/myelin"));
        let config = Config::from_yaml(&yaml).unwrap();
        assert!(
            config
                .server
                .signing_key_path
                .starts_with("/var/lib/myelin")
        );
        match &config.storage {
            hs_config::StorageConfig::Embedded(e) => {
                assert!(e.data_dir.starts_with("/var/lib/myelin"));
            }
            other => panic!("expected the embedded backend, got {other:?}"),
        }
        match &config.media.storage {
            hs_config::media::MediaStorageBackend::Local { path } => {
                assert!(path.starts_with("/var/lib/myelin"));
            }
            other => panic!("expected local media storage, got {other:?}"),
        }
    }

    /// The measurement this change is judged on. The old file was 158 lines; anything that grows
    /// this back into a full configuration dump should fail here and be argued for on purpose.
    #[test]
    fn it_stays_small() {
        let yaml = render_yaml("example.org", Path::new("./data"));
        let settings = yaml
            .lines()
            .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
            .count();
        assert!(
            settings <= 12,
            "the bootstrap file grew to {settings} lines of settings:\n{yaml}"
        );
    }

    /// A generated file and `hs serve --data-dir` must describe the same server, or the two ways
    /// of starting one would drift apart.
    #[test]
    fn it_agrees_with_what_the_data_dir_flag_derives() {
        let yaml = render_yaml("example.org", Path::new("/srv/hs"));
        let config = Config::from_yaml(&yaml).unwrap();
        let layout = DataLayout::new(Path::new("/srv/hs"));
        assert_eq!(config.server.signing_key_path, layout.signing_keys);
        match &config.storage {
            hs_config::StorageConfig::Embedded(e) => assert_eq!(e.data_dir, layout.database),
            other => panic!("expected the embedded backend, got {other:?}"),
        }
    }
}
