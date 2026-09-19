//! Runs the encrypted `hs-loadgen` scenario against a real, freshly built `hs serve` process.
//!
//! Run with: `cargo test -p hs-loadgen --test real_client_encrypted -- --nocapture` (the
//! scenario's step log -- including any `KNOWN BUG` lines for gaps this run found in a crate
//! outside track 08's ownership -- is only visible with `--nocapture`).
//!
//! Requires `cargo build -p hs-cli --bin hs` to have been run first (or set `HS_LOADGEN_BIN` to
//! an already-built binary); see `hs_loadgen::harness::spawn`'s doc comment. The first build of
//! this crate after enabling `matrix-sdk`'s `e2e-encryption` feature pulls in `vodozemac` and
//! takes noticeably longer than `real_client.rs` alone; that is expected.

#[tokio::test(flavor = "multi_thread")]
async fn matrix_rust_sdk_encrypts_and_decrypts_against_a_real_hs_serve() {
    let _ = tracing_subscriber::fmt::try_init();

    let server = hs_loadgen::harness::spawn("hs-loadgen-encrypted.test")
        .await
        .expect("hs serve should boot and become ready");

    let result = hs_loadgen::scenario_encrypted::run(server.base_url()).await;

    match result {
        Ok(log) => {
            println!(
                "hs-loadgen encrypted scenario completed {} steps:",
                log.len()
            );
            for line in &log {
                println!("  - {line}");
            }
        }
        Err(err) => {
            panic!("hs-loadgen encrypted scenario failed: {err:?}");
        }
    }

    server
        .shutdown()
        .expect("hs serve should shut down cleanly");
}
