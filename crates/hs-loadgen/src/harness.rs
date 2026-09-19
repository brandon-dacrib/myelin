//! Boots a real `hs serve` process (the compiled `hs` binary, not an in-process router) against a
//! temporary data directory and a free TCP port, and tears it down again.
//!
//! This is deliberately a separate OS process rather than `hs_cli::serve::spawn_serve` in-process
//! (as `crates/hs-cli/tests/e2e.rs` does): the point of this crate is to prove a real client
//! talking to a real, independently-running server over a real socket, the way Element Web or any
//! other client would, with no shortcuts through this workspace's own test scaffolding.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// A running `hs serve` process, bound to `base_url`, backed by a temporary data directory that
/// is removed when this handle is dropped.
pub struct ServerHandle {
    child: Child,
    base_url: String,
    // Held only for its `Drop` impl (removes the temp dir on scenario teardown); never read.
    _data_dir: tempfile::TempDir,
}

impl ServerHandle {
    /// This server's base URL, e.g. `http://127.0.0.1:41231`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sends SIGTERM-equivalent shutdown (a plain process kill; `hs serve`'s graceful-drain path
    /// is `hs-cli`'s own concern and is exercised by its own e2e test) and waits for exit.
    pub fn shutdown(mut self) -> Result<()> {
        self.child.kill().context("killing hs serve process")?;
        self.child.wait().context("waiting for hs serve to exit")?;
        Ok(())
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        // Best-effort: if `shutdown()` was already called this is a no-op (kill on an exited
        // child just returns an error we ignore), and if the test panicked before calling it,
        // this still keeps a runaway server from outliving the test process.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Locates the compiled `hs` binary in the workspace's shared `target/` directory.
///
/// Checked in order: the `HS_LOADGEN_BIN` environment variable (an explicit override), then
/// `target/debug/hs`, then `target/release/hs`, relative to this crate's manifest directory (two
/// levels up is the workspace root, per `crates/<name>/Cargo.toml`'s standard layout). Never sets
/// or reads `CARGO_TARGET_DIR` beyond that default relative path, per this workspace's convention
/// of a single shared `target/` directory.
fn find_hs_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("HS_LOADGEN_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        bail!("HS_LOADGEN_BIN={path:?} does not exist");
    }

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .context("crates/hs-loadgen should be two levels below the workspace root")?;

    for profile in ["debug", "release"] {
        let candidate = workspace_root.join("target").join(profile).join("hs");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    bail!(
        "no `hs` binary found under {:?}/target/{{debug,release}}; run \
         `cargo build -p hs-cli --bin hs` first",
        workspace_root
    );
}

/// Picks a free TCP port by binding to port 0 and immediately releasing it. There is an
/// unavoidable race between this and the child process binding the same port (something else
/// could grab it in between); `crates/hs-cli/tests/e2e.rs` accepts the same race for the same
/// reason: no better option exists without threading a socket fd into the child.
fn reserve_ephemeral_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("binding an ephemeral port")?;
    Ok(listener.local_addr()?.port())
}

fn write_config(data_dir: &Path, port: u16, server_name: &str) -> Result<PathBuf> {
    let yaml = format!(
        "server:\n  server_name: {server_name}\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         auth:\n  enable_registration: true\n"
    );
    let config_path = data_dir.join("hs-loadgen.yaml");
    std::fs::write(&config_path, yaml).context("writing generated config")?;
    Ok(config_path)
}

/// Boots `hs serve` against a fresh temp data directory and a free port, and waits for
/// `/_matrix/client/versions` to answer before returning — the same readiness signal a real
/// client would use (there is no other liveness probe a client-only harness can observe from
/// outside the process).
///
/// # Errors
///
/// Returns an error naming the missing binary, the config write failure, or (if the server never
/// becomes reachable within the timeout) the last connection error observed, so a caller sees
/// *why* boot failed rather than a bare panic.
pub async fn spawn(server_name: &str) -> Result<ServerHandle> {
    let binary = find_hs_binary()?;
    let data_dir = tempfile::tempdir().context("creating temp data dir")?;
    let port = reserve_ephemeral_port()?;
    let config_path = write_config(data_dir.path(), port, server_name)?;
    let base_url = format!("http://127.0.0.1:{port}");

    let child = Command::new(&binary)
        .arg("serve")
        .arg("-c")
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {binary:?} serve -c {config_path:?}"))?;

    let mut handle = ServerHandle {
        child,
        base_url,
        _data_dir: data_dir,
    };

    wait_for_ready(&mut handle).await?;
    Ok(handle)
}

async fn wait_for_ready(handle: &mut ServerHandle) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut last_err: Option<String> = None;

    while tokio::time::Instant::now() < deadline {
        // The process may have exited already (bad config, port race, panic); surface that
        // immediately with its captured stderr instead of spinning until the timeout.
        if let Some(status) = handle
            .child
            .try_wait()
            .context("polling hs serve process status")?
        {
            let mut stderr = String::new();
            if let Some(mut out) = handle.child.stderr.take() {
                use std::io::Read;
                let _ = out.read_to_string(&mut stderr);
            }
            bail!("hs serve exited early with {status}; stderr: {stderr}");
        }

        match client
            .get(format!("{}/_matrix/client/versions", handle.base_url))
            .timeout(Duration::from_millis(500))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => last_err = Some(format!("HTTP {}", resp.status())),
            Err(e) => last_err = Some(e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    bail!(
        "hs serve at {} never became ready within 15s; last error: {}",
        handle.base_url,
        last_err.unwrap_or_else(|| "none observed".to_owned())
    );
}
