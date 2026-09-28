//! The server's bare root through the real `hs` binary: someone who types the server's address
//! into a browser lands on the management interface (a redirect to `/admin/`) when the binary
//! carries it, and on a small page naming the server when it does not -- never on a `404`. And
//! only the exact root is taken: every other path still answers what it answered before.

use std::time::{Duration, Instant};

use hs_admin::assets::{EMBEDDED_UI, EmbeddedUi};
use reqwest::StatusCode;
use serde_json::Value;

/// A port nothing is listening on right now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The real `hs` binary, killed when dropped.
struct HsProcess(std::process::Child);

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_root_sends_a_browser_somewhere_useful_and_takes_no_other_path() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n",
            dir.path().join("data"),
            dir.path().join("media"),
        ),
    )
    .unwrap();
    let mut hs = HsProcess(
        std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
            .args(["serve", "-c"])
            .arg(&config_path)
            .env_remove("RUST_LOG")
            .env_remove("HS_DATA_DIR")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("the hs binary should start"),
    );
    let base = format!("http://127.0.0.1:{port}");
    // A browser follows redirects; this client must not, or it could not see the redirect.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // Generous: a debug build's first boot creates every keyspace, each an fsync, and on a disk
    // shared with other builds that has taken well over a minute.
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if let Ok(response) = client.get(format!("{base}/health/live")).send().await
            && response.status().is_success()
        {
            break;
        }
        if let Ok(Some(status)) = hs.0.try_wait() {
            panic!("hs exited before it was live: {status}");
        }
        assert!(Instant::now() < deadline, "hs was not live within 300s");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let root = client.get(format!("{base}/")).send().await.unwrap();
    let root_status = root.status();
    let root_location = root.headers().get("location").cloned();
    match EMBEDDED_UI {
        EmbeddedUi::Built => {
            assert_eq!(root_status, StatusCode::TEMPORARY_REDIRECT);
            assert_eq!(root_location.as_ref().unwrap(), "/admin/");
            // And where it points is the interface itself.
            let admin = client.get(format!("{base}/admin/")).send().await.unwrap();
            assert_eq!(admin.status(), StatusCode::OK);
        }
        EmbeddedUi::Placeholder => {
            assert_eq!(root_status, StatusCode::OK);
            assert!(root_location.is_none());
            assert!(
                root.headers()["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with("text/html")
            );
            let page = root.text().await.unwrap();
            assert!(page.contains("Myelin"), "{page}");
            assert!(page.contains("Matrix"), "{page}");
        }
    }

    // `HEAD /` answers the same way, without a body.
    let head = client.head(format!("{base}/")).send().await.unwrap();
    assert_eq!(head.status(), root_status);
    assert_eq!(head.headers().get("location").cloned(), root_location);
    assert!(head.bytes().await.unwrap().is_empty());

    // The root is not a Matrix endpoint for any other method.
    let post = client.post(format!("{base}/")).send().await.unwrap();
    assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);

    // Nothing else is shadowed: an unknown path is still the Matrix `404 M_UNRECOGNIZED`, and the
    // routes a client and the admin API need still answer.
    for path in ["/index.html", "/foo", "/_matrix/client/v3/no-such-endpoint"] {
        let response = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["errcode"], "M_UNRECOGNIZED", "{path}: {body}");
    }
    let versions = client
        .get(format!("{base}/_matrix/client/versions"))
        .send()
        .await
        .unwrap();
    assert_eq!(versions.status(), StatusCode::OK);
    let setup = client
        .get(format!("{base}/api/v1/setup"))
        .send()
        .await
        .unwrap();
    assert_eq!(setup.status(), StatusCode::OK);
}
