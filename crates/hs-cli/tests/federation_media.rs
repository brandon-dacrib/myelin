//! Media across servers, with two real `hs serve` instances in one process (the harness of
//! `federation_two_servers.rs`: plain-HTTP federation, IP-literal server names):
//!
//! - a user on B downloads and thumbnails media uploaded on A through B's authenticated
//!   `/_matrix/client/v1/media/...` endpoints, B fetching it from A over A's signed
//!   `/_matrix/federation/v1/media/download` (MSC3916), and gets it again from B's copy after A
//!   has been shut down;
//! - A serves its own media to a signed request, and to nothing else;
//! - B fetches from an origin that predates the federation media API (the legacy
//!   `/_matrix/media/v3/download` fallback), and from one that answers with the redirect form.
//!
//! The third origin is a small axum server standing in for another implementation; it is the
//! only faked part, and only because no second implementation runs in-process.

use serde_json::{Value, json};

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(port: u16, data_dir: &std::path::Path) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    // `url_preview_ip_range_blocklist: []` lets a redirect to the loopback stand-in origin be
    // followed; the default blocks loopback, as a real deployment must.
    let yaml = format!(
        "server:\n  server_name: \"127.0.0.1:{port}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n  url_preview_ip_range_blocklist: []\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

struct Server {
    handle: hs_cli::serve::ServeHandle,
    base: String,
    name: String,
    _dir: tempfile::TempDir,
}

async fn start(port: u16) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let handle = hs_cli::serve::spawn_serve(
        config(port, dir.path()),
        hs_cli::serve::ServeOptions {
            federation_scheme: Some("http"),
            ..Default::default()
        },
    )
    .await
    .expect("the server boots");
    Server {
        base: handle.base_url(),
        name: format!("127.0.0.1:{port}"),
        handle,
        _dir: dir,
    }
}

/// Registers `username` through the real UIA dance and returns its access token.
async fn register(client: &reqwest::Client, base: &str, username: &str) -> String {
    let first: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": username, "password": "correct horse"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = first["session"].as_str().unwrap();
    let done: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": username,
            "password": "correct horse",
            "auth": {"type": "m.login.dummy", "session": session},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    done["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("registration failed: {done}"))
        .to_owned()
}

/// Uploads `bytes` to `base` and returns the `mxc://` URI's `(server, media_id)`.
async fn upload(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    content_type: &str,
    filename: &str,
    bytes: Vec<u8>,
) -> (String, String) {
    let response: Value = client
        .post(format!(
            "{base}/_matrix/client/v1/media/upload?filename={filename}"
        ))
        .bearer_auth(token)
        .header("Content-Type", content_type)
        .body(bytes)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let uri = response["content_uri"]
        .as_str()
        .unwrap_or_else(|| panic!("upload failed: {response}"));
    let rest = uri.strip_prefix("mxc://").unwrap();
    let (server, id) = rest.rsplit_once('/').unwrap();
    (server.to_owned(), id.to_owned())
}

async fn get(client: &reqwest::Client, url: &str, token: &str) -> reqwest::Response {
    client.get(url).bearer_auth(token).send().await.unwrap()
}

/// A small real PNG, so B has something to decode for a thumbnail.
fn png() -> Vec<u8> {
    hs_media::test_fixtures::valid_png()
}

fn metric(text: &str, series: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix(series))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

#[tokio::test]
async fn a_user_on_b_gets_an_avatar_and_an_attachment_from_a_and_still_does_with_a_down() {
    let a = start(reserve_port()).await;
    let b = start(reserve_port()).await;
    let client = reqwest::Client::new();
    let alice = register(&client, &a.base, "alice").await;
    let bob = register(&client, &b.base, "bob").await;

    let avatar = png();
    let attachment = b"%PDF-1.4 minutes of the meeting".to_vec();
    let (avatar_server, avatar_id) = upload(
        &client,
        &a.base,
        &alice,
        "image/png",
        "avatar.png",
        avatar.clone(),
    )
    .await;
    assert_eq!(avatar_server, a.name);
    let (_, attachment_id) = upload(
        &client,
        &a.base,
        &alice,
        "application/pdf",
        "minutes.pdf",
        attachment.clone(),
    )
    .await;

    let media = format!("{}/_matrix/client/v1/media", b.base);
    let avatar_url = format!("{media}/download/{}/{avatar_id}", a.name);
    let attachment_url = format!("{media}/download/{}/{attachment_id}", a.name);
    let thumbnail_url = |size: &str| {
        format!(
            "{media}/thumbnail/{}/{avatar_id}?{size}&method=crop",
            a.name
        )
    };

    // Through B, fetched from A.
    let response = get(&client, &avatar_url, &bob).await;
    assert_eq!(response.status(), 200, "B fetches A's avatar");
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.bytes().await.unwrap().to_vec(), avatar);

    let response = get(&client, &attachment_url, &bob).await;
    assert_eq!(response.status(), 200, "B fetches A's attachment");
    assert_eq!(response.headers()["content-type"], "application/pdf");
    let disposition = response.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(disposition.contains("minutes.pdf"), "{disposition}");
    assert_eq!(response.bytes().await.unwrap().to_vec(), attachment);

    let response = get(&client, &thumbnail_url("width=32&height=32"), &bob).await;
    assert_eq!(response.status(), 200, "B thumbnails A's avatar");
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("image/")
    );

    // A goes away; B still has everything, including a thumbnail size it never made before.
    a.handle.shutdown().await;
    let response = get(&client, &avatar_url, &bob).await;
    assert_eq!(response.status(), 200, "the avatar is served from B's copy");
    assert_eq!(response.bytes().await.unwrap().to_vec(), avatar);
    let response = get(&client, &attachment_url, &bob).await;
    assert_eq!(
        response.status(),
        200,
        "the attachment is served from B's copy"
    );
    assert_eq!(response.bytes().await.unwrap().to_vec(), attachment);
    let response = get(&client, &thumbnail_url("width=96&height=96"), &bob).await;
    assert_eq!(
        response.status(),
        200,
        "a new thumbnail comes from B's copy"
    );

    // And something B never fetched is not there now: A is down.
    let response = get(
        &client,
        &format!("{media}/download/{}/neverfetched", a.name),
        &bob,
    )
    .await;
    assert_eq!(response.status(), 502);

    let metrics = client
        .get(format!("{}/metrics", b.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        metric(&metrics, "hs_media_remote_requests_total{result=\"miss\"}"),
        3,
        "two fetched, one failed:\n{metrics}"
    );
    assert!(metric(&metrics, "hs_media_remote_requests_total{result=\"hit\"}") >= 3);
    assert_eq!(
        metric(
            &metrics,
            "hs_media_remote_fetches_total{outcome=\"success\",via=\"federation\"}"
        ),
        2
    );
    assert_eq!(
        metric(
            &metrics,
            "hs_media_remote_fetches_total{outcome=\"failure\",via=\"none\"}"
        ),
        1
    );
    assert_eq!(
        metric(
            &metrics,
            "hs_media_remote_fetch_bytes_total{via=\"federation\"}"
        ),
        (avatar.len() + attachment.len()) as u64
    );

    b.handle.shutdown().await;
}

/// A stand-in for a third server: publishes a signing key, and answers the media paths either as
/// a server older than the federation media API would or with the redirect form.
struct StandIn {
    name: String,
    key: hs_model::signing::SigningKeyPair,
}

async fn stand_in(content: &'static [u8]) -> StandIn {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::get;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let name = format!("127.0.0.1:{port}");
    let key = hs_model::signing::SigningKeyPair::generate("stand_in");
    let keys = hs_federation::keys::OwnSigningKeys::from_keys(vec![key.clone()]);
    let key_response =
        hs_federation::keys::build_server_key_response(&name, &keys, &[], 3600).unwrap();
    let cdn = format!("http://{name}/cdn/blob");

    let app = axum::Router::new()
        .route(
            "/_matrix/key/v2/server",
            get(move || {
                let body = key_response.clone();
                async move { axum::Json(body) }
            }),
        )
        // A server from before spec v1.11 does not know this path at all.
        .route(
            "/_matrix/federation/v1/media/download/old",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(
                        json!({"errcode": "M_UNRECOGNIZED", "error": "Unrecognized request"}),
                    ),
                )
            }),
        )
        .route(
            "/_matrix/media/v3/download/{server}/old",
            get(move || async move {
                (
                    [
                        (header::CONTENT_TYPE, "text/plain"),
                        (
                            header::CONTENT_DISPOSITION,
                            "inline; filename=\"notes.txt\"",
                        ),
                    ],
                    content,
                )
            }),
        )
        // One that offloads downloads: the content part is only a Location.
        .route(
            "/_matrix/federation/v1/media/download/cdn",
            get(move || {
                let cdn = cdn.clone();
                async move {
                    let (content_type, body) = hs_media::multipart::build_media_response(
                        &[("Location", cdn.as_str())],
                        b"",
                    );
                    ([(header::CONTENT_TYPE, content_type)], body).into_response()
                }
            }),
        )
        .route(
            "/cdn/blob",
            get(move || async move { ([(header::CONTENT_TYPE, "image/gif")], content) }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    StandIn { name, key }
}

#[tokio::test]
async fn b_fetches_from_an_older_server_and_from_one_that_redirects() {
    const CONTENT: &[u8] = b"GIF89a but really just some bytes";
    let other = stand_in(CONTENT).await;
    let b = start(reserve_port()).await;
    let client = reqwest::Client::new();
    let bob = register(&client, &b.base, "bob").await;
    let media = format!("{}/_matrix/client/v1/media", b.base);

    let response = get(
        &client,
        &format!("{media}/download/{}/old", other.name),
        &bob,
    )
    .await;
    assert_eq!(response.status(), 200, "the legacy fallback");
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert!(
        response.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("notes.txt")
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), CONTENT);

    let response = get(
        &client,
        &format!("{media}/download/{}/cdn", other.name),
        &bob,
    )
    .await;
    assert_eq!(response.status(), 200, "the redirect form");
    assert_eq!(response.headers()["content-type"], "image/gif");
    assert_eq!(response.bytes().await.unwrap().as_ref(), CONTENT);

    let response = get(
        &client,
        &format!("{media}/download/{}/unknown", other.name),
        &bob,
    )
    .await;
    // Unknown on both paths: the stand-in's router answers the legacy path with a bare 404.
    assert_eq!(response.status(), 404);

    b.handle.shutdown().await;
}

#[tokio::test]
async fn a_serves_its_own_media_to_a_signed_request_and_to_nothing_else() {
    let other = stand_in(b"unused").await;
    let a = start(reserve_port()).await;
    let client = reqwest::Client::new();
    let alice = register(&client, &a.base, "alice").await;
    let avatar = png();
    let (_, id) = upload(
        &client,
        &a.base,
        &alice,
        "image/png",
        "avatar.png",
        avatar.clone(),
    )
    .await;

    let signed_get = |path: String| {
        let auth = hs_federation::xmatrix::sign_request(
            "GET",
            &path,
            &other.name,
            &a.name,
            None,
            &other.key,
        )
        .unwrap();
        client
            .get(format!("{}{path}", a.base))
            .header("Authorization", auth)
            .send()
    };

    let unsigned = client
        .get(format!(
            "{}/_matrix/federation/v1/media/download/{id}",
            a.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unsigned.status(),
        401,
        "an unsigned request never reaches the handler"
    );

    let response = signed_get(format!("/_matrix/federation/v1/media/download/{id}"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let content_type = response.headers()["content-type"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.bytes().await.unwrap();
    let boundary = hs_media::multipart::boundary_from_content_type(&content_type).unwrap();
    let parts = hs_media::multipart::parse(&boundary, &body).unwrap();
    assert_eq!(parts.parts[0].body, b"{}");
    assert_eq!(parts.parts[1].header("Content-Type"), Some("image/png"));
    assert_eq!(parts.parts[1].body, &avatar[..]);

    let response = signed_get(format!(
        "/_matrix/federation/v1/media/thumbnail/{id}?width=32&height=32&method=crop"
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    let content_type = response.headers()["content-type"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.bytes().await.unwrap();
    let boundary = hs_media::multipart::boundary_from_content_type(&content_type).unwrap();
    let parts = hs_media::multipart::parse(&boundary, &body).unwrap();
    assert!(
        parts.parts[1]
            .header("Content-Type")
            .unwrap()
            .starts_with("image/")
    );

    let response = signed_get("/_matrix/federation/v1/media/download/nosuchmedia".to_owned())
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    a.handle.shutdown().await;
}
