//! The real binary serves listening sockets it inherited (`HS_INHERITED_LISTENER_FDS`), which is
//! how every real-binary test here starts `hs` on a port it reserved without releasing it
//! (`hs_testkit::ports`).
//!
//! The listener and the mesh port are held by this process from the moment the system picked
//! them; while `hs` runs, nothing else can bind them, and `hs` serves them. A socket that
//! matches no configured address is not served.

use std::time::{Duration, Instant};

#[test]
fn the_variable_has_one_name() {
    assert_eq!(
        hs_testkit::ports::INHERITED_LISTENERS_ENV,
        hs_cli::serve::INHERITED_LISTENERS_ENV
    );
}

/// `hs serve` as a child, with `listeners` passed down, its output collected.
struct Hs(std::process::Child);

impl Drop for Hs {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(config: &std::path::Path, listeners: Vec<hs_testkit::ports::ReservedPort>) -> Hs {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_hs"));
    command
        .args(["serve", "-c"])
        .arg(config)
        .env_remove("RUST_LOG")
        .env_remove("HS_DATA_DIR")
        .stdout(std::process::Stdio::null())
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hs_serves_the_listener_it_inherited_and_ignores_one_that_matches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let listener = hs_testkit::ports::reserve();
    let stray = hs_testkit::ports::reserve();
    let (port, stray_addr) = (listener.port(), stray.addr());
    let config = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health]\n\
             storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n",
            data_dir.join("media")
        ),
    )
    .unwrap();

    let mut hs = serve(&config, vec![listener, stray]);
    let base = format!("http://127.0.0.1:{port}");
    wait_live(&base, &mut hs).await;
    // This process no longer holds the listener (the command that held it is gone), so what
    // answered is the child serving the socket it inherited; binding the port fails because
    // the child has it.
    let err = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    // The stray socket is open in the child but nothing accepts on it: a request to it is
    // never answered.
    let stray = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .get(format!("http://{stray_addr}/health/live"))
        .send()
        .await;
    assert!(
        stray.is_err(),
        "nothing serves an unmatched socket: {stray:?}"
    );

    drop(hs);
    // Once the child is gone, so is the last handle on the socket: the port is free again.
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "port {port} is still held");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn hs_refuses_to_start_on_a_descriptor_that_is_not_a_listener() {
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
        .args(["serve", "-c"])
        .arg(dir.path().join("missing.yaml"))
        .env_remove("RUST_LOG")
        // Standard error is not an inherited listener.
        .env(hs_testkit::ports::INHERITED_LISTENERS_ENV, "2")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HS_INHERITED_LISTENER_FDS") && stderr.contains("\"2\""),
        "{stderr}"
    );
}
