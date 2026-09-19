//! One-shot live smoke test: runs the real [`hs_operator::reconcile::reconcile_homeserver`]
//! function inside a real `kube::runtime::Controller`, watching whatever cluster the current
//! `kubectl` context / `KUBECONFIG` points at, and exits as soon as it observes one reconcile (or
//! after a timeout).
//!
//! This exists because every other test in this crate (`crds::tests`, `reconcile::tests`)
//! exercises the CRD schemas and the stub reconcile functions as plain Rust values, with no
//! `kube::Client` and no API server involved (`crate` module docs, before this bin existed,
//! documented that as a known gap: "None of this proves ... a real `Controller` watch loop
//! behaves correctly under real events"). This bin is the proof. It is deliberately *not* a
//! long-running operator binary — no `Deployment` in `deploy/` points at it, and it is not built
//! into `deploy/Dockerfile`'s image. It is a manual verification tool, run once against a real
//! cluster and recorded in `docs/status/12-platform-and-kubernetes.md`, and left in the crate so
//! the next person (or a future `kind`-based CI job) can re-run the same proof rather than take
//! this session's word for it.
//!
//! Requires the `Homeserver` CRD (`deploy/crds/homeserver.yaml`) already applied to the cluster,
//! and at least one `Homeserver` object present in the target namespace (`watcher` emits an
//! `Apply` event for every object already on the server when the watch starts, so a
//! pre-existing object is enough — nothing needs to be created *during* the run).
//!
//! Usage:
//! ```sh
//! kubectl apply -f deploy/crds/homeserver.yaml
//! kubectl create namespace hs-operator-smoke
//! kubectl apply -n hs-operator-smoke -f <a sample Homeserver manifest>
//! HS_OPERATOR_SMOKE_NAMESPACE=hs-operator-smoke cargo run -p hs-operator --bin live-smoke
//! ```

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use hs_operator::crds::Homeserver;
use hs_operator::reconcile::{error_policy, reconcile_homeserver};
use kube::runtime::Controller;
use kube::runtime::watcher;
use kube::{Api, Client};

/// How long to wait for at least one reconcile before giving up and exiting non-zero.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() {
    // `kube`'s `rustls-tls` feature needs a process-level `CryptoProvider` installed before the
    // first TLS handshake (the API server connection); nothing else in this crate does that for
    // it. `install_default` only fails if a provider was already installed, which cannot happen
    // this early in `main`, so a bare `let _ =` is the deliberate choice here, not oversight.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let namespace =
        std::env::var("HS_OPERATOR_SMOKE_NAMESPACE").unwrap_or_else(|_| "default".to_owned());

    let client = match Client::try_default().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "SMOKE-ERR: could not build a kube::Client from the ambient kubeconfig/context: {e}"
            );
            std::process::exit(1);
        }
    };
    println!("SMOKE: connected, watching Homeserver objects in namespace {namespace:?}");

    let api: Api<Homeserver> = Api::namespaced(client, &namespace);
    let controller = Controller::new(api, watcher::Config::default()).run(
        reconcile_homeserver,
        error_policy,
        Arc::new(()),
    );
    tokio::pin!(controller);

    match tokio::time::timeout(SMOKE_TIMEOUT, controller.next()).await {
        Ok(Some(Ok((obj_ref, action)))) => {
            println!("SMOKE-OK: reconciled {obj_ref:?} -> {action:?}");
        }
        Ok(Some(Err(e))) => {
            eprintln!("SMOKE-ERR: controller reported an error: {e:?}");
            std::process::exit(1);
        }
        Ok(None) => {
            eprintln!("SMOKE-ERR: controller stream ended with no events");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!(
                "SMOKE-TIMEOUT: no reconcile observed within {SMOKE_TIMEOUT:?} \
                 (is the CRD applied? is there a Homeserver object in {namespace:?}?)"
            );
            std::process::exit(1);
        }
    }
}
