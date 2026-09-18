//! The operation table: one row per `(method, path)` in `openapi/openapi.yaml`, generated
//! alongside it so the two can never drift silently (see the generator note at the top of
//! `openapi/openapi.yaml`). [`crate::router`] builds the skeleton router by iterating this table;
//! `tests/contract.rs` asserts the table and the OpenAPI document still agree.

use serde::Deserialize;

use crate::model::Scope;

const OPERATIONS_JSON: &str = include_str!("../openapi/operations.json");

#[derive(Debug, Clone, Deserialize)]
struct RawOperation {
    method: String,
    path: String,
    operation_id: String,
    scope: Option<String>,
    #[serde(default)]
    public: bool,
    #[allow(dead_code)]
    tag: Option<String>,
    idempotent_post: bool,
}

#[derive(Debug, Deserialize)]
struct RawTable {
    operations: Vec<RawOperation>,
}

/// One operation: everything the router skeleton needs to register a route and enforce its
/// scope, without knowing anything about the handler's actual behavior yet.
#[derive(Debug, Clone)]
pub struct OperationDef {
    pub method: axum::http::Method,
    pub path: String,
    pub operation_id: String,
    /// `None` means "any authenticated principal" (only `GET /me` today). Ignored when
    /// `public` is true.
    pub scope: Option<Scope>,
    /// No authentication at all (`GET /openapi.yaml` and `.json`, RFC 0004 section 3.8). These
    /// two are registered with their real handlers in `crate::router::build_router`, not the
    /// generic `501` one, so the router skips them when iterating this table.
    pub public: bool,
    pub idempotent_post: bool,
}

/// Parses `openapi/operations.json` (generated alongside `openapi.yaml`) into the operation
/// table. Panics on malformed JSON, which would mean the generator or this file is broken, not
/// something a caller can recover from.
pub fn load() -> Vec<OperationDef> {
    let raw: RawTable =
        serde_json::from_str(OPERATIONS_JSON).expect("openapi/operations.json is malformed");
    raw.operations
        .into_iter()
        .map(|op| OperationDef {
            method: op.method.parse().unwrap_or(axum::http::Method::GET),
            path: op.path,
            operation_id: op.operation_id,
            scope: op.scope.as_deref().and_then(Scope::parse),
            public: op.public,
            idempotent_post: op.idempotent_post,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_a_non_trivial_number_of_operations() {
        let ops = load();
        assert!(
            ops.len() > 100,
            "expected >100 operations, got {}",
            ops.len()
        );
    }

    #[test]
    fn every_operation_has_a_leading_slash_path() {
        for op in load() {
            assert!(op.path.starts_with('/'), "{} has no leading slash", op.path);
        }
    }

    #[test]
    fn me_requires_no_specific_scope() {
        let ops = load();
        let me = ops
            .iter()
            .find(|o| o.operation_id == "me.get")
            .expect("me.get missing");
        assert_eq!(me.scope, None);
    }

    #[test]
    fn users_list_requires_admin_read() {
        let ops = load();
        let users_list = ops
            .iter()
            .find(|o| o.operation_id == "users.list")
            .expect("users.list missing");
        assert_eq!(users_list.scope, Some(Scope::AdminRead));
    }
}
