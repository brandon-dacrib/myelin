//! A listener serves only the route families its `resources` names (`crate::listener_resources`):
//! the real binary with two listeners, one `[client, health]` and one `[metrics, admin]`, each
//! answering what it lists and refusing the rest with the unknown-endpoint `404`.

use std::time::{Duration, Instant};

/// `hs serve` as a child, with `listeners` passed down, its log (standard output) collected in
/// a file.
struct Hs(std::process::Child);

impl Drop for Hs {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(
    config: &std::path::Path,
    listeners: Vec<hs_testkit::ports::ReservedPort>,
    log: std::fs::File,
) -> Hs {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_hs"));
    command
        .args(["serve", "-c"])
        .arg(config)
        .env_remove("RUST_LOG")
        .env_remove("HS_DATA_DIR")
        .stdout(log)
        .stderr(std::process::Stdio::inherit());
    hs_testkit::ports::pass_to(&mut command, listeners);
    let child = command.spawn().expect("the hs binary starts");
    drop(command);
    Hs(child)
}

async fn wait_live(base: &str, hs: &mut Hs) {
    // A debug binary on a loaded machine can take a minute to boot.
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if let Some(status) = hs.0.try_wait().unwrap() {
            panic!("hs exited before serving: {status}");
        }
        if let Ok(response) = reqwest::get(format!("{base}/health/live")).await
            && response.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{base} never became live");
}

async fn get(url: String) -> (reqwest::StatusCode, String) {
    let response = reqwest::get(&url).await.unwrap();
    let status = response.status();
    (status, response.text().await.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_listener_serves_only_the_resources_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let public = hs_testkit::ports::reserve();
    let private = hs_testkit::ports::reserve();
    let (public_port, private_port) = (public.port(), private.port());
    let config = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n\
             \x20   - port: {public_port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health]\n\
             \x20   - port: {private_port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [metrics, admin]\n\
             storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n",
            data_dir.join("media")
        ),
    )
    .unwrap();
    let log_path = dir.path().join("hs.log");
    let log = std::fs::File::create(&log_path).unwrap();

    let mut hs = serve(&config, vec![public, private], log);
    let public = format!("http://127.0.0.1:{public_port}");
    let private = format!("http://127.0.0.1:{private_port}");
    wait_live(&public, &mut hs).await;

    // The public listener: the client API and the probes, nothing private.
    let (status, body) = get(format!("{public}/_matrix/client/versions")).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"versions\""), "{body}");
    let (status, _) = get(format!("{public}/health/ready")).await;
    assert_eq!(status, 200);
    for path in [
        "/metrics",
        "/admin/",
        "/admin/setup",
        "/_synapse/admin/v1/register",
    ] {
        let (status, body) = get(format!("{public}{path}")).await;
        assert_eq!(status, 404, "{path}: {body}");
        assert!(body.contains("M_UNRECOGNIZED"), "{path}: {body}");
    }
    // The admin API's refusal is in its own error shape, as its unknown-endpoint answer is.
    let (status, body) = get(format!("{public}/api/v1/server")).await;
    assert_eq!(status, 404, "{body}");
    assert!(body.contains("no such endpoint"), "{body}");

    // The private listener: metrics and the admin surfaces, not the client API or the probes.
    let (status, body) = get(format!("{private}/metrics")).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("hs_http_requests_total"), "{body}");
    let (status, _) = get(format!("{private}/admin/")).await;
    assert_eq!(status, 200);
    // Served, so refused for want of a token rather than for want of a route.
    let (status, body) = get(format!("{private}/api/v1/server")).await;
    assert_eq!(status, 401, "{body}");
    for path in [
        "/_matrix/client/versions",
        "/health/live",
        "/.well-known/matrix/client",
    ] {
        let (status, body) = get(format!("{private}{path}")).await;
        assert_eq!(status, 404, "{path}: {body}");
        assert!(body.contains("M_UNRECOGNIZED"), "{path}: {body}");
    }

    // The startup log says, per listener, what it serves.
    drop(hs);
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains(&format!("127.0.0.1:{public_port}")) && log.contains("client,health"),
        "{log}"
    );
    assert!(
        log.contains(&format!("127.0.0.1:{private_port}")) && log.contains("metrics,admin"),
        "{log}"
    );
}
