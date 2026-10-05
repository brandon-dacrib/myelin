//! The media behaviours Complement and Sytest grade, through a real `hs serve` (in process,
//! federating over plain HTTP; see `federation_two_servers.rs` for the server-name choice):
//!
//! - a thumbnail size nobody configured (`32x32 scale`, Complement's `TestLocalPngThumbnail`)
//!   is served from the nearest configured size, the same bytes on the authenticated and the
//!   legacy path, rather than `400 Unsupported thumbnail size or method`;
//! - a remote item asked for on the legacy `/_matrix/media/v3/download` path is fetched from its
//!   origin's legacy path first (Complement's `TestMediaWithoutFileName` over federation, whose
//!   origin refuses the federation media API with a bare 400);
//! - a URL preview carries every `og:` tag and the image's width and height (Sytest's
//!   `51media/20urlpreview.pl`, Complement's `TestUrlPreview`), and refuses an image over the
//!   decode limits.

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
    let yaml = format!(
        "server:\n  server_name: \"127.0.0.1:{port}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, media, health]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n  url_preview_enabled: true\n  url_preview_ip_range_blocklist: []\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n\
         rate_limits:\n  enabled: false\n"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

async fn start() -> (hs_cli::serve::ServeHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let handle = hs_cli::serve::spawn_serve(
        config(reserve_port(), dir.path()),
        hs_cli::serve::ServeOptions {
            federation_scheme: Some("http"),
            ..Default::default()
        },
    )
    .await
    .expect("the server boots");
    (handle, dir)
}

async fn register(client: &reqwest::Client, base: &str, username: &str) -> String {
    let url = format!("{base}/_matrix/client/v3/register");
    let first: Value = client
        .post(&url)
        .json(&json!({"username": username, "password": "correct horse"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = first["session"].as_str().unwrap().to_owned();
    let done: Value = client
        .post(&url)
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

/// Sytest's `tests/51media/test.png` (Apache-2.0, matrix-org/sytest): 279 x 129, 2239 bytes.
const SYTEST_PNG: &[u8] = include_bytes!("../../hs-media/tests/fixtures/images/sytest_preview.png");

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("127.0.0.1:{}", addr.port())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thumbnails_previews_and_legacy_remote_downloads_answer_as_synapse_does() {
    use axum::http::{StatusCode, header};
    use axum::routing::get;

    let (server, _dir) = start().await;
    let base = server.base_url();
    let client = reqwest::Client::new();
    let token = register(&client, &base, "alice").await;

    // --- A thumbnail size nobody configured -----------------------------------------------
    let uploaded: Value = client
        .post(format!("{base}/_matrix/media/v3/upload?filename=test.png"))
        .bearer_auth(&token)
        .header("Content-Type", "image/png")
        .body(SYTEST_PNG.to_vec())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let uri = uploaded["content_uri"].as_str().unwrap();
    let (origin, media_id) = uri
        .strip_prefix("mxc://")
        .unwrap()
        .rsplit_once('/')
        .unwrap();
    let mut bodies = Vec::new();
    for path in [
        format!("{base}/_matrix/client/v1/media/thumbnail/{origin}/{media_id}"),
        format!("{base}/_matrix/media/v3/thumbnail/{origin}/{media_id}"),
    ] {
        let response = client
            .get(format!("{path}?width=32&height=32&method=scale"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        bodies.push(response.bytes().await.unwrap());
    }
    assert_eq!(bodies[0], bodies[1], "one variant for both paths");
    assert!(
        image_dimensions(&bodies[0]).0 > 32,
        "the nearest configured size (320x240 scale), not a 32x32 one"
    );

    // --- A remote item on the legacy path, from an origin that refuses the federation API --
    let origin = serve(
        axum::Router::new()
            .route(
                "/_matrix/federation/v1/media/download/{media_id}",
                get(|| async { (StatusCode::BAD_REQUEST, "complement: Invalid Origin") }),
            )
            .route(
                "/_matrix/media/v3/download/{server}/{media_id}",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/plain")],
                        "Hello from the other side",
                    )
                }),
            ),
    )
    .await;
    let response = client
        .get(format!(
            "{base}/_matrix/media/v3/download/{origin}/PlainTextFile"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain");
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        b"Hello from the other side"
    );

    // --- URL previews -----------------------------------------------------------------------
    let bomb = hs_media::test_fixtures::decompression_bomb_png(60_000, 60_000);
    let web = serve(
        axum::Router::new()
            .route(
                "/test.html",
                get(|| async {
                    axum::response::Html(
                        r#"<html prefix="og: http://ogp.me/ns#"><head>
<title>The Rock (1996)</title>
<meta property="og:title" content="The Rock" />
<meta property="og:type" content="video.movie" />
<meta property="og:url" content="http://www.imdb.com/title/tt0117500/" />
<meta property="og:image" content="test.png" />
</head><body></body></html>"#,
                    )
                }),
            )
            .route(
                "/test.png",
                get(|| async { ([(header::CONTENT_TYPE, "image/png")], SYTEST_PNG) }),
            )
            .route(
                "/huge.html",
                get(|| async {
                    axum::response::Html(
                        r#"<meta property="og:title" content="Huge"><meta property="og:image" content="/huge.png">"#,
                    )
                }),
            )
            .route(
                "/huge.png",
                get(move || {
                    let bomb = bomb.clone();
                    async move { ([(header::CONTENT_TYPE, "image/png")], bomb) }
                }),
            ),
    )
    .await;
    let preview = |page: &str| {
        let url = format!("{base}/_matrix/media/v3/preview_url");
        let page = format!("http://{web}/{page}");
        let request = client
            .get(url)
            .query(&[("url", page.as_str())])
            .bearer_auth(&token);
        async move {
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), 200);
            response.json::<Value>().await.unwrap()
        }
    };
    let rock = preview("test.html").await;
    assert_eq!(rock["og:title"], "The Rock", "{rock}");
    assert_eq!(rock["og:type"], "video.movie");
    assert_eq!(rock["og:url"], "http://www.imdb.com/title/tt0117500/");
    assert_eq!(rock["matrix:image:size"], 2239);
    assert_eq!(rock["og:image:width"], 279);
    assert_eq!(rock["og:image:height"], 129);
    let image = rock["og:image"].as_str().unwrap();
    assert!(image.starts_with("mxc://"), "{rock}");
    // The cached image is served like any other media.
    let (image_origin, image_id) = image
        .strip_prefix("mxc://")
        .unwrap()
        .rsplit_once('/')
        .unwrap();
    let cached = client
        .get(format!(
            "{base}/_matrix/client/v1/media/download/{image_origin}/{image_id}"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(cached.status(), 200);
    assert_eq!(cached.bytes().await.unwrap().as_ref(), SYTEST_PNG);

    let huge = preview("huge.html").await;
    assert_eq!(huge["og:title"], "Huge");
    assert!(
        huge.get("og:image").is_none(),
        "an image over max_image_pixels is refused: {huge}"
    );

    server.shutdown().await;
}

/// Width and height from a PNG's IHDR.
fn image_dimensions(png: &[u8]) -> (u32, u32) {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "a PNG");
    let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
    (width, height)
}
