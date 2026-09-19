//! Runs the full `hs-loadgen` scenario against a real, freshly built `hs serve` process.
//!
//! Run with: `cargo test -p hs-loadgen --test real_client -- --nocapture` (the scenario's step
//! log and, on failure, the exact HTTP call and body that broke, are only visible with
//! `--nocapture`; without it a failure still prints the same detail as the test's panic message).
//!
//! Requires `cargo build -p hs-cli --bin hs` to have been run first (or set `HS_LOADGEN_BIN` to
//! an already-built binary); see `hs_loadgen::harness::spawn`'s doc comment.

#[tokio::test(flavor = "multi_thread")]
async fn matrix_rust_sdk_talks_to_a_real_hs_serve() {
    let _ = tracing_subscriber::fmt::try_init();

    let server = hs_loadgen::harness::spawn("hs-loadgen.test")
        .await
        .expect("hs serve should boot and become ready");

    let result = hs_loadgen::scenario::run(server.base_url()).await;

    match result {
        Ok(log) => {
            println!("hs-loadgen scenario completed {} steps:", log.len());
            for line in &log {
                println!("  - {line}");
            }
        }
        Err(err) => {
            panic!("hs-loadgen scenario failed: {err:?}");
        }
    }

    server
        .shutdown()
        .expect("hs serve should shut down cleanly");
}
