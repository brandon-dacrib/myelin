//! `hs_telemetry::testing` records a span whose callsite was first reached on another thread
//! while the recorder was being installed.
//!
//! `tracing-core` caches each callsite's interest the first time it is reached. A callsite
//! reached while the global default is being set can cache "never", asked of the no-op
//! dispatcher that is still the default, after the new subscriber's own registration rebuilt
//! it; without a rebuild once the default is set, that span is never made again in the
//! process. In a crate's test binary the other tests keep running while one installs the
//! recorder: `hs-federation`'s sender test once found no `federation.transaction` span at all.
//! Without the rebuild this test failed about one run in ten (8 threads reaching 2048 fresh
//! callsites while `install` ran); with it, none in 500. It has a binary of its own because the
//! install happens once per process.
#![cfg(feature = "testing")]

use std::sync::{Arc, Barrier};

/// Expands its body twice: each expansion of a `tracing` macro is a callsite of its own.
macro_rules! twice {
    ($($body:tt)*) => {
        $($body)*
        $($body)*
    };
}

/// 2048 functions, each opening a span from a callsite of its own.
fn fresh_callsites() -> Vec<fn()> {
    let mut callsites: Vec<fn()> = Vec::new();
    twice! { twice! { twice! { twice! { twice! { twice! { twice! { twice! { twice! { twice! {
    twice! {
        callsites.push(|| drop(tracing::info_span!(target: "hs_probe", "probe").entered()));
    }
    } } } } } } } } } }
    callsites
}

#[test]
fn a_callsite_first_reached_during_the_install_is_recorded() {
    const THREADS: usize = 8;
    let callsites = Arc::new(fresh_callsites());
    let start = Arc::new(Barrier::new(THREADS + 1));
    let reachers: Vec<_> = (0..THREADS)
        .map(|thread| {
            let callsites = Arc::clone(&callsites);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                for callsite in callsites.iter().skip(thread).step_by(THREADS) {
                    callsite();
                    // Spread the first uses over the time the install takes.
                    for _ in 0..100 {
                        std::hint::spin_loop();
                    }
                }
            })
        })
        .collect();
    start.wait();
    hs_telemetry::testing::install();
    for reacher in reachers {
        reacher.join().unwrap();
    }

    // Each callsite once more, in a root of its own, read back without installing again.
    for callsite in callsites.iter() {
        tracing::info_span!(target: "hs_probe", parent: None, "root").in_scope(callsite);
    }
    let recorded = hs_telemetry::testing::traces_rooted_at("root")
        .iter()
        .filter(|trace| trace.find("probe").is_some())
        .count();
    assert_eq!(
        recorded,
        callsites.len(),
        "callsites whose spans are no longer made"
    );
}
