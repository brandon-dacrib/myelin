//! The in-memory backend must pass the shared conformance suite.

#[test]
fn memory_backend_passes_conformance_suite() {
    hs_kv::conformance::run_conformance_suite(hs_kv::memory::MemoryBackend::new);
}
