//! Thumbnail limits through the real `hs` binary: the `thumbnail_generate` fuzz target's
//! out-of-memory input of 2026-10-04 (a GIF whose logical screen is 1326 x 0, which made the
//! thumbnailer ask for a 16 GiB buffer) and a PNG whose header declares 64 megapixels are each
//! uploaded and asked for a thumbnail. Each answers `400 M_UNKNOWN` (what Synapse answers for an
//! image over `max_image_pixels`), the log names the media and its declared size, `/metrics`
//! counts the refusal by reason, and the server stays up: a real image's thumbnail is still
//! made afterwards.
//!
//! Before the fix the first thumbnail request took the process down (or, under an allocator
//! that overcommits, made it touch 16 GiB).

use reqwest::StatusCode;
use serde_json::{Value, json};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The crashing input minus the harness's three leading bytes (target width, height, method).
const FUZZ_OOM_GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x37, 0x61, 0x2e, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x4d, 0xff,
    0x05, 0x00, 0xdb, 0xb8, 0x00, 0x00, 0xd2, 0x00, 0x00, 0x89, 0x2a, 0x00, 0x00, 0x00, 0x00, 0xff,
    0xff, 0xff, 0x21, 0x01, 0x00, 0x03, 0x00, 0x00, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x08, 0x08, 0x08, 0x00, 0x71, 0x6f, 0x69, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// One run of the real `hs` binary, its stdout log read line by line (the harness of
/// `guest_access.rs`), killed when dropped.
struct HsProcess {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl HsProcess {
    fn serve(config_path: &std::path::Path) -> Self {
        use std::io::BufRead;
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
            .args(["serve", "-c"])
            .arg(config_path)
            .env_remove("RUST_LOG")
            .env_remove("HS_DATA_DIR")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("the hs binary should start");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Reads the log until a line contains every one of `needles`.
    fn wait_for(&mut self, needles: &[&str]) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if needles.iter().all(|needle| line.contains(needle)) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "the log never said {needles:?}; it said:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }

    fn assert_running(&mut self) {
        if let Ok(Some(status)) = self.child.try_wait() {
            panic!("hs exited: {status}");
        }
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
    let session = first["session"].as_str().unwrap();
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

/// Uploads `bytes` and returns the `mxc://` URI's `server/media_id`.
async fn upload(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    content_type: &str,
    bytes: Vec<u8>,
) -> String {
    let response: Value = client
        .post(format!("{base}/_matrix/client/v1/media/upload"))
        .bearer_auth(token)
        .header("Content-Type", content_type)
        .body(bytes)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    response["content_uri"]
        .as_str()
        .and_then(|uri| uri.strip_prefix("mxc://"))
        .unwrap_or_else(|| panic!("upload failed: {response}"))
        .to_owned()
}

async fn thumbnail(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    mxc: &str,
    (width, height, method): (u32, u32, &str),
) -> reqwest::Response {
    client
        .get(format!(
            "{base}/_matrix/client/v1/media/thumbnail/{mxc}?width={width}&height={height}&method={method}"
        ))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
}

fn metric(text: &str, series: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix(series))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crafted_image_is_refused_a_thumbnail_and_the_server_stays_up() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             auth:\n  enable_registration: true\n",
            dir.path().join("data"),
            dir.path().join("media"),
        ),
    )
    .unwrap();
    let mut hs = HsProcess::serve(&config_path);
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if let Ok(response) = client.get(format!("{base}/health/live")).send().await
            && response.status().is_success()
        {
            break;
        }
        hs.assert_running();
        assert!(
            std::time::Instant::now() < deadline,
            "hs was not live within 120s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let token = register(&client, &base, "uploader").await;

    let gif = upload(&client, &base, &token, "image/gif", FUZZ_OOM_GIF.to_vec()).await;
    let bomb = upload(
        &client,
        &base,
        &token,
        "image/png",
        hs_media::test_fixtures::decompression_bomb_png(8_000, 8_000),
    )
    .await;

    // Default sizes of both methods.
    for size in [
        (32, 32, "crop"),
        (96, 96, "crop"),
        (320, 240, "scale"),
        (800, 600, "scale"),
    ] {
        for mxc in [&gif, &bomb] {
            let response = thumbnail(&client, &base, &token, mxc, size).await;
            let status = response.status();
            let body: Value = response.json().await.unwrap();
            assert_eq!(status, StatusCode::BAD_REQUEST, "{mxc} {size:?}: {body}");
            assert_eq!(body["errcode"], "M_UNKNOWN", "{body}");
            assert!(
                body["error"]
                    .as_str()
                    .unwrap()
                    .starts_with("Failed to generate thumbnail"),
                "{body}"
            );
        }
    }
    hs.assert_running();

    let gif_id = gif.rsplit_once('/').unwrap().1;
    let line = hs.wait_for(&["image refused for a thumbnail", gif_id]);
    assert!(line.contains("1326") && line.contains("empty"), "{line}");
    let bomb_id = bomb.rsplit_once('/').unwrap().1;
    let line = hs.wait_for(&["image refused for a thumbnail", bomb_id]);
    assert!(line.contains("8000") && line.contains("pixels"), "{line}");

    // Still up, and a real image still gets its thumbnail.
    let png = upload(
        &client,
        &base,
        &token,
        "image/png",
        hs_media::test_fixtures::valid_png(),
    )
    .await;
    let response = thumbnail(&client, &base, &token, &png, (32, 32, "crop")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/png");
    let bytes = response.bytes().await.unwrap();
    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "{bytes:?}");

    let metrics = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        metric(
            &metrics,
            "hs_media_thumbnail_refused_total{reason=\"empty\"}"
        ),
        4,
        "{metrics}"
    );
    assert_eq!(
        metric(
            &metrics,
            "hs_media_thumbnail_refused_total{reason=\"pixels\"}"
        ),
        4,
        "{metrics}"
    );
    hs.assert_running();
}
