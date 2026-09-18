//! Wires `hs_telemetry::metrics::Metrics` into the router: every request is timed and recorded
//! as `hs_http_requests_total`/`hs_http_request_duration_seconds`, labeled with the *matched*
//! route template — never the raw path, per `docs/decisions/0004-telemetry-conventions.md`'s
//! cardinality rule — so `/metrics` actually has data for
//! `deploy/observability/grafana/hs-overview.json` and `deploy/observability/alerts/hs-rules.yaml`
//! to read. Before this module existed, `Metrics` was constructed and served at `/metrics` but
//! nothing ever called `record_http_request`, so every scrape returned an empty body.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;

use hs_telemetry::Metrics;

/// Applied via `axum::middleware::from_fn_with_state(metrics, track_metrics)`, so it carries its
/// own bound state independent of whatever state type the rest of the router uses — see
/// `crate::serve::build_router`.
///
/// `matched_path` is `None` for a request that hit no route at all (a 404 with no matching
/// pattern); those are recorded under the literal route label `"{unmatched}"` rather than
/// dropped, so a spike in unmatched requests (a bridge probing an endpoint we do not serve, a
/// misconfigured client) is visible as its own time series instead of silently missing.
pub async fn track_metrics(
    State(metrics): State<Arc<Metrics>>,
    matched_path: Option<MatchedPath>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().as_str().to_owned();
    let route = matched_path
        .as_ref()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "{unmatched}".to_owned());
    let start = Instant::now();
    let response = next.run(req).await;
    metrics.record_http_request(
        &method,
        &route,
        response.status().as_u16(),
        start.elapsed().as_secs_f64(),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::middleware;
    use axum::routing::get;
    use tower::ServiceExt as _;

    async fn ok_handler() -> &'static str {
        "ok"
    }

    #[tokio::test]
    async fn records_a_request_under_its_matched_route_template() {
        let metrics = Arc::new(Metrics::new());
        let app: Router<()> = Router::new().route("/health/live", get(ok_handler)).layer(
            middleware::from_fn_with_state(metrics.clone(), track_metrics),
        );

        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let text = metrics.encode_to_string().unwrap();
        assert!(text.contains("route=\"/health/live\""), "{text}");
        assert!(text.contains("hs_http_requests_total"), "{text}");
        assert!(text.contains("hs_http_request_duration_seconds"), "{text}");
    }

    #[tokio::test]
    async fn records_an_unmatched_request_under_the_unmatched_label() {
        let metrics = Arc::new(Metrics::new());
        let app: Router<()> = Router::new().route("/health/live", get(ok_handler)).layer(
            middleware::from_fn_with_state(metrics.clone(), track_metrics),
        );

        let _ = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/no/such/route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let text = metrics.encode_to_string().unwrap();
        assert!(text.contains("route=\"{unmatched}\""), "{text}");
    }
}
