//! The version this build reports: `hs version`, `/_matrix/federation/v1/version`, the admin
//! API's `GET /server` (and the `/_synapse/admin/v1/server_version` shim over it), the cluster
//! replica registry and the `service.version` on every exported span and Sentry event all say
//! the same string, and it comes from here.
//!
//! The workspace's `Cargo.toml` names the release *line* (`0.3.0`) and CD derives each release's
//! patch (`deploy/helm/hs/ci/chart-version.sh`): the image tagged `0.3.32` was built from a tree
//! whose `CARGO_PKG_VERSION` is `0.3.0`, and so was `0.3.31`. Until 2026-10-16 the binary knew
//! only `CARGO_PKG_VERSION`, so every release of a line introduced itself as its `.0` and an
//! operator could not tell from the server which release they were running. CD now passes the
//! release version into the build as the `HS_RELEASE_VERSION` environment variable (the
//! binaries' `cargo build` env, and a build arg `deploy/Dockerfile` forwards), and this module
//! reads it at compile time. A build without it (a developer's `cargo build`, a manual CD run on
//! a branch, an image built by hand) reports the line version, as before.
//!
//! The variable is read in this crate, not in each caller: `option_env!` is evaluated where it is
//! written, and one site means one answer. Cargo records the variable in the crate's dep-info, so
//! a changed `HS_RELEASE_VERSION` rebuilds this crate and everything above it rather than
//! leaving a stale string in a cached object.

/// The release version compiled into this build: `HS_RELEASE_VERSION` from the build
/// environment when it was set and non-empty, else the workspace's `CARGO_PKG_VERSION`.
///
/// CD sets the variable to the version it releases (`0.3.32`); a plain `cargo build` leaves it
/// unset and gets the line (`0.3.0`). Empty counts as unset because `deploy/Dockerfile` declares
/// the build arg with an empty default, and a `docker build` without `--build-arg` must not
/// produce a server whose version is `""`.
#[must_use]
pub const fn release_version() -> &'static str {
    resolve(option_env!("HS_RELEASE_VERSION"), env!("CARGO_PKG_VERSION"))
}

/// The line this build belongs to: the workspace's `CARGO_PKG_VERSION` (`0.3.0`), whatever the
/// release version says. For a message that wants to name both.
#[must_use]
pub const fn line_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The rule [`release_version`] applies, over the two inputs, so it can be tested without a
/// recompile: `release` when it is `Some` and non-empty, else `line`.
const fn resolve(release: Option<&'static str>, line: &'static str) -> &'static str {
    match release {
        Some(v) if !v.is_empty() => v,
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_release_version_wins() {
        assert_eq!(resolve(Some("0.3.32"), "0.3.0"), "0.3.32");
        assert_eq!(resolve(Some("1.0.0-rc.1"), "1.0.0"), "1.0.0-rc.1");
    }

    #[test]
    fn unset_and_empty_fall_back_to_the_line() {
        assert_eq!(resolve(None, "0.3.0"), "0.3.0");
        assert_eq!(resolve(Some(""), "0.3.0"), "0.3.0");
    }

    #[test]
    fn the_build_reports_the_variable_or_the_line() {
        // What this very build was given, so the function agrees with the environment it was
        // compiled under: the variable when set (`HS_RELEASE_VERSION=9.9.9 cargo test`), the
        // manifest's version otherwise.
        let expected = match option_env!("HS_RELEASE_VERSION") {
            Some(v) if !v.is_empty() => v,
            _ => env!("CARGO_PKG_VERSION"),
        };
        assert_eq!(release_version(), expected);
        assert_eq!(line_version(), env!("CARGO_PKG_VERSION"));
        assert!(!release_version().is_empty());
    }

    #[test]
    fn the_telemetry_default_reports_the_same_version() {
        assert_eq!(crate::Options::default().service_version, release_version());
    }
}
