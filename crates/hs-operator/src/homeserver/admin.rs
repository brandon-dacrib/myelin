//! The operator as a client of the server's admin API: the four calls a drain needs
//! (`cluster.replicas.list`, `cluster.replicas.drain`, `cluster.replicas.undrain`, `tasks.get`;
//! `crates/hs-admin/openapi/openapi.yaml`, decision 0012). [`AdminApi`] is the seam the
//! reconciler is written against; [`HttpAdminApi`] is the real one, over `reqwest`.

use std::future::Future;
use std::time::Duration;

use serde::Deserialize;

/// Where the admin API is and how to authenticate to it.
#[derive(Clone, PartialEq, Eq)]
pub struct AdminEndpoint {
    /// The server's base URL (`http://hs.matrix.svc:8008`), without `/api/v1`.
    pub base_url: String,
    /// The bearer token.
    pub token: String,
}

impl std::fmt::Debug for AdminEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminEndpoint")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// One replica as the admin API reports it (the parts of the OpenAPI `Replica` a drain reads).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReplicaView {
    /// Its id: in cluster mode, its mesh address `<pod>.<headless domain>:<port>`.
    pub id: String,
    /// `joining`, `active`, `draining`, `drained` or `unreachable`.
    pub status: String,
    /// Shards it owns.
    pub shard_count: u64,
    /// The task following its drain, while a drain request is in force.
    #[serde(default)]
    pub drain_task_id: Option<String>,
    /// When a drain was requested, while one is in force.
    #[serde(default)]
    pub drain_requested_at: Option<String>,
}

impl ReplicaView {
    /// Whether this replica is `pod`'s: its id is the pod's DNS name under the headless
    /// Service, so it starts with `<pod>.` (or, outside Kubernetes, is the pod name).
    #[must_use]
    pub fn belongs_to(&self, pod: &str) -> bool {
        self.id == pod
            || self
                .id
                .strip_prefix(pod)
                .is_some_and(|rest| rest.starts_with('.'))
    }

    /// Whether it has handed off everything: it owns no shards.
    #[must_use]
    pub fn owns_nothing(&self) -> bool {
        self.shard_count == 0
    }
}

/// A task, as far as the operator reads it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TaskView {
    /// Its id.
    pub id: String,
    /// `scheduled`, `running`, `succeeded`, `failed` or `cancelled`.
    pub status: String,
}

/// What an admin API call can fail with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdminError {
    /// `409`: the server refused the drain because no other replica is active to take the
    /// shards.
    #[error("refused: {0}")]
    Conflict(String),
    /// `404`.
    #[error("not found")]
    NotFound,
    /// `401` or `403`: the token is wrong or lacks `admin:write`.
    #[error("unauthorized ({0}): check the Homeserver's adminApi.tokenSecretRef")]
    Unauthorized(u16),
    /// Any other status.
    #[error("HTTP {status}: {body}")]
    Status {
        /// The status code.
        status: u16,
        /// The start of the body.
        body: String,
    },
    /// The server could not be reached, or answered something unreadable.
    #[error("transport: {0}")]
    Transport(String),
}

/// The admin API calls the reconciler makes. Every call takes the endpoint, since each
/// `Homeserver` has its own.
pub trait AdminApi: Send + Sync {
    /// Every replica.
    fn list_replicas(
        &self,
        endpoint: &AdminEndpoint,
    ) -> impl Future<Output = Result<Vec<ReplicaView>, AdminError>> + Send;
    /// Drains one. Idempotent: draining a draining or drained replica changes nothing.
    fn drain(
        &self,
        endpoint: &AdminEndpoint,
        replica_id: &str,
    ) -> impl Future<Output = Result<ReplicaView, AdminError>> + Send;
    /// Withdraws a drain. Idempotent.
    fn undrain(
        &self,
        endpoint: &AdminEndpoint,
        replica_id: &str,
    ) -> impl Future<Output = Result<ReplicaView, AdminError>> + Send;
    /// Reads a task.
    fn task(
        &self,
        endpoint: &AdminEndpoint,
        task_id: &str,
    ) -> impl Future<Output = Result<TaskView, AdminError>> + Send;
}

/// Finds `pod`'s replica.
///
/// # Errors
/// As [`AdminApi::list_replicas`].
pub async fn find_replica<A: AdminApi>(
    api: &A,
    endpoint: &AdminEndpoint,
    pod: &str,
) -> Result<Option<ReplicaView>, AdminError> {
    Ok(api
        .list_replicas(endpoint)
        .await?
        .into_iter()
        .find(|r| r.belongs_to(pod)))
}

/// [`AdminApi`] over HTTP.
#[derive(Debug, Clone)]
pub struct HttpAdminApi {
    client: reqwest::Client,
}

impl HttpAdminApi {
    /// A client with a ten-second timeout per call.
    ///
    /// # Errors
    /// When the TLS backend cannot be initialised.
    pub fn new() -> Result<Self, AdminError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("hs-operator/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AdminError::Transport(e.to_string()))?;
        Ok(Self { client })
    }

    fn url(endpoint: &AdminEndpoint, path: &str) -> String {
        format!("{}/api/v1{path}", endpoint.base_url.trim_end_matches('/'))
    }

    async fn send<T: for<'de> Deserialize<'de>>(
        &self,
        request: reqwest::RequestBuilder,
        endpoint: &AdminEndpoint,
    ) -> Result<T, AdminError> {
        let response = request
            .bearer_auth(&endpoint.token)
            .send()
            .await
            .map_err(|e| AdminError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return response
                .json::<T>()
                .await
                .map_err(|e| AdminError::Transport(format!("unreadable answer: {e}")));
        }
        let body: String = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        Err(match status {
            404 => AdminError::NotFound,
            401 | 403 => AdminError::Unauthorized(status),
            409 => AdminError::Conflict(problem_detail(&body)),
            _ => AdminError::Status { status, body },
        })
    }
}

/// The `detail` of an RFC 9457 problem body, else the body.
fn problem_detail(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("detail").and_then(|d| d.as_str()).map(str::to_owned))
        .unwrap_or_else(|| body.to_owned())
}

/// Percent-encodes a path segment (a replica id holds `:` and `.`).
fn segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Deserialize)]
struct Page<T> {
    items: Vec<T>,
    #[serde(default)]
    next_cursor: Option<String>,
}

impl AdminApi for HttpAdminApi {
    async fn list_replicas(
        &self,
        endpoint: &AdminEndpoint,
    ) -> Result<Vec<ReplicaView>, AdminError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        // A bounded walk: a cluster has a handful of replicas, and a server that kept handing
        // back cursors must not keep the reconciler here forever.
        for _ in 0..20 {
            let mut request = self
                .client
                .get(Self::url(endpoint, "/cluster/replicas"))
                .query(&[("limit", "100")]);
            if let Some(c) = &cursor {
                request = request.query(&[("cursor", c.as_str())]);
            }
            let page: Page<ReplicaView> = self.send(request, endpoint).await?;
            out.extend(page.items);
            match page.next_cursor {
                Some(next) if !next.is_empty() => cursor = Some(next),
                _ => break,
            }
        }
        Ok(out)
    }

    async fn drain(
        &self,
        endpoint: &AdminEndpoint,
        replica_id: &str,
    ) -> Result<ReplicaView, AdminError> {
        let path = format!("/cluster/replicas/{}/drain", segment(replica_id));
        let request = self.client.post(Self::url(endpoint, &path));
        self.send(request, endpoint).await
    }

    async fn undrain(
        &self,
        endpoint: &AdminEndpoint,
        replica_id: &str,
    ) -> Result<ReplicaView, AdminError> {
        let path = format!("/cluster/replicas/{}/undrain", segment(replica_id));
        let request = self.client.post(Self::url(endpoint, &path));
        self.send(request, endpoint).await
    }

    async fn task(&self, endpoint: &AdminEndpoint, task_id: &str) -> Result<TaskView, AdminError> {
        let path = format!("/tasks/{}", segment(task_id));
        let request = self.client.get(Self::url(endpoint, &path));
        self.send(request, endpoint).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use serde_json::json;

    use super::*;

    #[test]
    fn a_replica_belongs_to_its_pod_by_dns_name() {
        let r = ReplicaView {
            id: "hs-1.hs-headless.ns.svc.cluster.local:8449".to_owned(),
            status: "active".to_owned(),
            shard_count: 3,
            drain_task_id: None,
            drain_requested_at: None,
        };
        assert!(r.belongs_to("hs-1"));
        assert!(!r.belongs_to("hs-10"));
        assert!(!r.belongs_to("hs"));
        assert!(!r.owns_nothing());
    }

    #[test]
    fn segments_are_percent_encoded() {
        assert_eq!(segment("hs-0.hs-headless:8449"), "hs-0.hs-headless%3A8449");
    }

    #[test]
    fn the_endpoint_never_prints_its_token() {
        let e = AdminEndpoint {
            base_url: "http://hs:8008".to_owned(),
            token: "secret-token".to_owned(),
        };
        assert!(!format!("{e:?}").contains("secret-token"));
    }

    /// What the fake server saw.
    #[derive(Default)]
    struct Seen {
        calls: Vec<String>,
    }

    fn authorized(headers: &HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "Bearer good")
    }

    async fn serve() -> (String, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let replica = |id: &str, status: &str, shards: u64| {
            json!({"id": id, "role": "replica", "status": status, "shard_count": shards,
                   "epoch": 1, "this_replica": false, "mesh_addr": id, "version": null,
                   "zone": null, "last_heartbeat_at": null, "drain_requested_at": null,
                   "drain_requested_by": null, "drain_task_id": null})
        };
        let app = Router::new()
            .route(
                "/api/v1/cluster/replicas",
                get(
                    move |State(seen): State<Arc<Mutex<Seen>>>, headers: HeaderMap| async move {
                        if !authorized(&headers) {
                            return StatusCode::UNAUTHORIZED.into_response();
                        }
                        seen.lock().unwrap().calls.push("list".to_owned());
                        axum::Json(json!({"items": [
                        replica("hs-0.hs-headless.ns.svc.cluster.local:8449", "active", 5),
                        replica("hs-1.hs-headless.ns.svc.cluster.local:8449", "active", 4),
                    ], "next_cursor": null, "prev_cursor": null}))
                        .into_response()
                    },
                ),
            )
            .route(
                "/api/v1/cluster/replicas/{id}/drain",
                post(
                    move |State(seen): State<Arc<Mutex<Seen>>>, Path(id): Path<String>| async move {
                        seen.lock().unwrap().calls.push(format!("drain {id}"));
                        if id.starts_with("hs-0") {
                            return (
                                StatusCode::CONFLICT,
                                axum::Json(json!({"type": "urn:hs:problem:conflict",
                                    "title": "Conflict", "status": 409,
                                    "detail": "no other replica is active"})),
                            )
                                .into_response();
                        }
                        let mut r = replica(&id, "draining", 4);
                        r["drain_task_id"] = json!("task-1");
                        axum::Json(r).into_response()
                    },
                ),
            )
            .route(
                "/api/v1/cluster/replicas/{id}/undrain",
                post(
                    move |State(seen): State<Arc<Mutex<Seen>>>, Path(id): Path<String>| async move {
                        seen.lock().unwrap().calls.push(format!("undrain {id}"));
                        axum::Json(replica(&id, "active", 0)).into_response()
                    },
                ),
            )
            .route(
                "/api/v1/tasks/{id}",
                get(|Path(id): Path<String>| async move {
                    if id == "task-1" {
                        axum::Json(json!({"id": id, "action": "cluster.replicas.drain",
                            "status": "running", "created_at": "2026-09-28T00:00:00Z"}))
                        .into_response()
                    } else {
                        StatusCode::NOT_FOUND.into_response()
                    }
                }),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn the_http_client_speaks_the_admin_api() {
        let (base_url, seen) = serve().await;
        let api = HttpAdminApi::new().unwrap();
        let endpoint = AdminEndpoint {
            base_url: format!("{base_url}/"),
            token: "good".to_owned(),
        };

        let found = find_replica(&api, &endpoint, "hs-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.shard_count, 4);

        let drained = api.drain(&endpoint, &found.id).await.unwrap();
        assert_eq!(drained.status, "draining");
        assert_eq!(drained.drain_task_id.as_deref(), Some("task-1"));

        let refused = api
            .drain(&endpoint, "hs-0.hs-headless.ns.svc.cluster.local:8449")
            .await
            .unwrap_err();
        assert_eq!(
            refused,
            AdminError::Conflict("no other replica is active".to_owned())
        );

        let task = api.task(&endpoint, "task-1").await.unwrap();
        assert_eq!(task.status, "running");
        assert_eq!(
            api.task(&endpoint, "nope").await.unwrap_err(),
            AdminError::NotFound
        );

        let undrained = api.undrain(&endpoint, &found.id).await.unwrap();
        assert_eq!(undrained.status, "active");

        let bad = AdminEndpoint {
            base_url,
            token: "bad".to_owned(),
        };
        assert_eq!(
            api.list_replicas(&bad).await.unwrap_err(),
            AdminError::Unauthorized(401)
        );

        let calls = seen.lock().unwrap().calls.clone();
        assert_eq!(
            calls,
            [
                "list",
                "drain hs-1.hs-headless.ns.svc.cluster.local:8449",
                "drain hs-0.hs-headless.ns.svc.cluster.local:8449",
                "undrain hs-1.hs-headless.ns.svc.cluster.local:8449",
            ]
        );
    }
}
