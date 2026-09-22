//! The one way this server builds an outbound HTTP client.
//!
//! `reqwest::Client::builder().build()` loads the operating system's root certificates every
//! time it is called, because this workspace asks reqwest for `rustls-tls-native-roots` (this
//! crate's `Cargo.toml` does, deliberately, so that the builder below always exists; before it,
//! `hs-federation`'s did, and Cargo unified the feature into every binary by accident). Loading them is a
//! keychain read on macOS -- 165 ms per client, and close to three seconds the first time in a
//! process -- done on whichever thread asks, which at boot is the async runtime's. `hs serve`
//! builds five or six clients (the federation sender, the pusher, the media previewer, the
//! appservice pinger and sender, the modules client), so a first boot on a developer's machine
//! spent seconds in the keychain, and every test that boots a server paid the same again.
//!
//! [`builder`] loads the roots once per process and hands every client the same set. On Linux
//! the roots are a file and the saving is small; the point there is that the work happens once.

use std::sync::OnceLock;

static NATIVE_ROOTS: OnceLock<Vec<reqwest::Certificate>> = OnceLock::new();

/// The operating system's root certificates, loaded on the first call and kept for the life of
/// the process. Blocking on that first call -- callers on an async runtime at boot should
/// [`warm_native_roots`] first, off the runtime thread.
pub fn native_roots() -> &'static [reqwest::Certificate] {
    NATIVE_ROOTS.get_or_init(|| {
        let loaded = rustls_native_certs::load_native_certs();
        for error in &loaded.errors {
            tracing::warn!(%error, "could not load one of the operating system's root certificates");
        }
        let roots: Vec<reqwest::Certificate> = loaded
            .certs
            .iter()
            .filter_map(|der| reqwest::Certificate::from_der(der).ok())
            .collect();
        if roots.is_empty() {
            tracing::warn!(
                "no operating system root certificates could be loaded; outbound HTTPS will fail \
                 unless a certificate is added explicitly"
            );
        }
        roots
    })
}

/// Loads [`native_roots`] on a blocking thread, so that the first (slow) load does not stall
/// the runtime. Cheap to call again.
pub async fn warm_native_roots() {
    if NATIVE_ROOTS.get().is_some() {
        return;
    }
    let _ = tokio::task::spawn_blocking(|| {
        native_roots();
    })
    .await;
}

/// A [`reqwest::ClientBuilder`] trusting the public (webpki) roots compiled in and the
/// operating system's, loaded once -- the same trust `reqwest::Client::builder()` gives in this
/// workspace, without the reload. Set a timeout and build, as with that.
pub fn builder() -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(false);
    for root in native_roots() {
        builder = builder.add_root_certificate(root.clone());
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The second client costs nothing worth measuring: the roots were loaded by the first.
    #[test]
    fn roots_are_loaded_once() {
        let _ = builder().build().unwrap();
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let _ = builder().build().unwrap();
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "twenty clients took {:?}",
            started.elapsed()
        );
    }
}
