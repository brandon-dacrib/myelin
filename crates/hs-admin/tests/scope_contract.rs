//! Scope contract (RFC 0004 section 8.2): the scope the router enforces on every operation is the
//! scope `openapi/openapi.yaml` documents for it, and `openapi/operations.json` (the table the
//! router is built from) says the same as the document.
//!
//! Handlers check their scope inline (`require_scope(.., Some(Scope::..))`), so the operation
//! table alone cannot prove what is enforced: on 2026-10-01 the `appservices.*`,
//! `bridge_types.*` and `bridge_offerings.*` read handlers enforced `admin:read` while the
//! document said `bridges:read`, and a token holding only `bridges:read` was refused by them.
//! This test therefore asks the real router, operation by operation, with two tokens:
//!
//! - one holding exactly the documented scope, which must not be refused with
//!   `403 insufficient-scope` (catches a handler stricter than the document), and
//! - one holding every scope that does *not* satisfy the documented one, which must be refused
//!   with `403 insufficient-scope` naming the documented scope (catches a handler looser than,
//!   or different from, the document).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use hs_admin::audit::InMemoryAuditSink;
use hs_admin::auth::StaticVerifier;
use hs_admin::events::EventBus;
use hs_admin::model::{Principal, PrincipalKind, Scope};
use hs_admin::operations::{OperationDef, load};
use hs_admin::router::{AdminState, build_router};

const ALL_SCOPES: [Scope; 6] = [
    Scope::AdminRead,
    Scope::AdminWrite,
    Scope::BridgesRead,
    Scope::BridgesWrite,
    Scope::ModerationRead,
    Scope::ModerationWrite,
];

fn principal(scopes: Vec<Scope>) -> Principal {
    Principal {
        kind: PrincipalKind::User,
        id: "@ops:example.org".into(),
        display_name: None,
        scopes,
        token_id: None,
        expires_at: None,
        issued_by: None,
    }
}

/// A token name for "exactly this one scope".
fn exact_token(scope: Scope) -> String {
    format!("exact-{}", scope.as_str())
}

/// A token name for "every scope that does not satisfy this one".
fn without_token(scope: Scope) -> String {
    format!("without-{}", scope.as_str())
}

/// The scopes a principal can hold without satisfying `required`.
fn non_satisfying(required: Scope) -> Vec<Scope> {
    ALL_SCOPES
        .into_iter()
        .filter(|s| !s.satisfies(required))
        .collect()
}

fn router() -> axum::Router {
    let mut verifier = StaticVerifier::new().with_token("no-scopes", principal(Vec::new()));
    for scope in ALL_SCOPES {
        verifier = verifier
            .with_token(exact_token(scope), principal(vec![scope]))
            .with_token(without_token(scope), principal(non_satisfying(scope)));
    }
    let state = AdminState::new(
        Arc::new(verifier),
        Arc::new(InMemoryAuditSink::new()),
        Arc::new(EventBus::new()),
    );
    build_router(state).0
}

/// A concrete request path for `op`: each `{param}` replaced by a well-formed value of its kind,
/// so path extraction never refuses the request before the handler's scope check runs.
fn concrete_path(op: &OperationDef) -> String {
    let mut path = format!("/api/v1{}", op.path);
    for (param, value) in [
        ("{user_id}", "@alice:example.org"),
        ("{room_id}", "!room:example.org"),
        ("{event_id}", "$event"),
        ("{server_name}", "remote.example.org"),
        ("{media_id}", "abcdef"),
        ("{device_id}", "DEVICE"),
        // `#` would start a fragment, so the alias is percent-encoded as a client sends it.
        ("{alias}", "%23alias:example.org"),
        ("{medium}", "email"),
        ("{address}", "alice@example.org"),
        ("{provider}", "oidc"),
        ("{external_id}", "ext"),
        ("{revision}", "1"),
        ("{section}", "server"),
        ("{token}", "tok"),
        ("{type}", "mautrix-whatsapp"),
        ("{id}", "id1"),
    ] {
        path = path.replace(param, value);
    }
    path
}

/// What the router answered `op` for `token`: `Some(required_scope)` when it was refused with
/// `403 insufficient-scope`, `None` otherwise.
async fn refusal(router: &axum::Router, op: &OperationDef, token: &str) -> Option<String> {
    let mut request = Request::builder()
        .method(op.method.clone())
        .uri(concrete_path(op))
        .header("authorization", format!("Bearer {token}"));
    let body = if op.method == Method::GET || op.method == Method::DELETE {
        Body::empty()
    } else {
        request = request.header("content-type", "application/json");
        Body::from("{}")
    };
    let response = router
        .clone()
        .oneshot(request.body(body).expect("request builds"))
        .await
        .expect("the router is infallible");
    if response.status() != StatusCode::FORBIDDEN {
        return None;
    }
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    let problem: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    problem
        .get("required_scope")
        .and_then(|s| s.as_str())
        .map(str::to_owned)
}

#[tokio::test]
async fn router_enforces_exactly_the_documented_scope_on_every_operation() {
    let router = router();
    let mut checked = 0usize;
    let mut problems = Vec::new();
    let mut mismatched = std::collections::BTreeSet::new();
    for op in load().into_iter().filter(|op| !op.public) {
        checked += 1;
        let id = &op.operation_id;
        let documented = op
            .scope
            .map_or("(any authenticated principal)", Scope::as_str);
        match op.scope {
            None => {
                if let Some(enforced) = refusal(&router, &op, "no-scopes").await {
                    problems.push(format!(
                        "{id}: documented {documented}, but a token with no scopes is refused for \
                         lacking {enforced}"
                    ));
                }
            }
            Some(scope) => {
                if let Some(enforced) = refusal(&router, &op, &exact_token(scope)).await {
                    problems.push(format!(
                        "{id}: documented {documented}, but a token holding exactly \
                         {documented} is refused for lacking {enforced}"
                    ));
                }
                match refusal(&router, &op, &without_token(scope)).await {
                    Some(enforced) if enforced == scope.as_str() => {}
                    Some(enforced) => problems.push(format!(
                        "{id}: documented {documented}, but refusals name {enforced}"
                    )),
                    None => problems.push(format!(
                        "{id}: documented {documented}, but a token holding only scopes that do \
                         not satisfy it ({:?}) is not refused",
                        non_satisfying(scope)
                            .into_iter()
                            .map(Scope::as_str)
                            .collect::<Vec<_>>()
                    )),
                }
            }
        }
    }
    for problem in &problems {
        if let Some((id, _)) = problem.split_once(':') {
            mismatched.insert(id.to_owned());
        }
    }
    assert!(checked > 150, "expected >150 operations, checked {checked}");
    assert!(
        problems.is_empty(),
        "{} of {checked} operations enforce a scope other than the documented one:\n{}",
        mismatched.len(),
        problems.join("\n")
    );
}

/// `operations.json` is the router's table and `openapi.yaml` is the contract; their scopes must
/// be the same, operation by operation.
#[test]
fn operations_table_scopes_match_the_openapi_document() {
    let doc: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(hs_admin::openapi::DOCUMENT).expect("openapi.yaml parses");
    let paths = doc
        .get("paths")
        .and_then(|p| p.as_mapping())
        .expect("openapi.yaml has paths");
    // (METHOD, path) -> the scopes its `security` requirement lists (empty: none required).
    let mut documented: BTreeMap<(String, String), (String, Vec<String>)> = BTreeMap::new();
    for (path, ops) in paths {
        let (Some(path), Some(ops)) = (path.as_str(), ops.as_mapping()) else {
            continue;
        };
        for (method, op) in ops {
            let Some(method) = method.as_str() else {
                continue;
            };
            let Some(operation_id) = op.get("operationId").and_then(|o| o.as_str()) else {
                continue;
            };
            let scopes = op
                .get("security")
                .and_then(|s| s.as_sequence())
                .map(|requirements| {
                    requirements
                        .iter()
                        .filter_map(|r| r.as_mapping())
                        .flat_map(|r| r.values())
                        .filter_map(|v| v.as_sequence())
                        .flatten()
                        .filter_map(|s| s.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            documented.insert(
                (method.to_ascii_uppercase(), path.to_string()),
                (operation_id.to_string(), scopes),
            );
        }
    }

    let mut problems = Vec::new();
    let table = load();
    for op in &table {
        let key = (op.method.as_str().to_string(), op.path.clone());
        let Some((doc_id, doc_scopes)) = documented.get(&key) else {
            problems.push(format!("{} {} is not in openapi.yaml", key.0, key.1));
            continue;
        };
        if doc_id != &op.operation_id {
            problems.push(format!(
                "{} {}: operations.json says {}, openapi.yaml says {doc_id}",
                key.0, key.1, op.operation_id
            ));
        }
        let table_scopes: Vec<String> = op
            .scope
            .map(|s| s.as_str().to_string())
            .into_iter()
            .collect();
        if op.public {
            if !doc_scopes.is_empty() {
                problems.push(format!(
                    "{}: public in operations.json, but openapi.yaml requires {doc_scopes:?}",
                    op.operation_id
                ));
            }
        } else if &table_scopes != doc_scopes {
            problems.push(format!(
                "{}: operations.json requires {table_scopes:?}, openapi.yaml requires \
                 {doc_scopes:?}",
                op.operation_id
            ));
        }
    }
    assert_eq!(
        table.len(),
        documented.len(),
        "operations.json and openapi.yaml declare different numbers of operations"
    );
    assert!(
        problems.is_empty(),
        "operations.json and openapi.yaml disagree on scopes:\n{}",
        problems.join("\n")
    );
}
