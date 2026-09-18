//! Contract test (RFC 0004 decision D15.16, track 15's definition of done): the router built
//! from `openapi/operations.json` must declare exactly the `(method, path)` pairs that
//! `openapi/openapi.yaml` declares. If either file is edited without the other, this fails.

use std::sync::Arc;

use hs_admin::audit::InMemoryAuditSink;
use hs_admin::auth::StaticVerifier;
use hs_admin::events::EventBus;
use hs_admin::router::{AdminState, build_router};

#[test]
fn router_and_openapi_document_agree_on_paths_and_methods() {
    let state = AdminState::new(
        Arc::new(StaticVerifier::new()),
        Arc::new(InMemoryAuditSink::new()),
        Arc::new(EventBus::new()),
    );
    let (_router, manifest) = build_router(state);

    let openapi_yaml = hs_admin::openapi::DOCUMENT;
    match hs_http::router::assert_matches_openapi(openapi_yaml, &manifest) {
        Ok(()) => {}
        Err(problems) => panic!("router and openapi.yaml disagree:\n{}", problems.join("\n")),
    }
}

#[test]
fn openapi_document_is_non_trivial() {
    let paths = hs_http::router::openapi_method_paths(hs_admin::openapi::DOCUMENT)
        .expect("openapi.yaml must parse");
    assert!(
        paths.len() > 100,
        "expected the admin API to declare >100 operations, got {}",
        paths.len()
    );
}
