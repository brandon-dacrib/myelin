//! The first-run setup link, through the real `hs` binary.
//!
//! - Without `server.public_baseurl`, the link is rooted at the first listener's own address and
//!   port (here `127.0.0.1` on a port that is not 8008), and the next log line says the host is a
//!   guess, that it may be replaced, and how to configure the right one.
//! - The token works whatever host the browser used: the setup page and `POST /api/v1/setup`
//!   answer on a `Host` the link never named.
//! - With `server.public_baseurl` set, the link is rooted there and no hint follows it.

use reqwest::StatusCode;
use serde_json::json;

const HINT: &str = "replacing the host if needed";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config_yaml(port: u16, data_dir: &std::path::Path, public_baseurl: Option<&str>) -> String {
    let server_extra = public_baseurl
        .map(|url| format!("  public_baseurl: {url:?}\n"))
        .unwrap_or_default();
    format!(
        "server:\n  server_name: example.org\n{server_extra}\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {:?}\n\
         media:\n  storage:\n    backend: local\n    path: {:?}\n",
        data_dir,
        data_dir.join("media"),
    )
}

/// One run of the real `hs` binary, its stdout read line by line.
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
            .env_remove("HS__SERVER__PUBLIC_BASEURL")
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

    /// Reads the log until a line contains `needle`.
    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(needle) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "the log never said {needle:?}; it said:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }

    /// The next line logged after the ones already read.
    fn next_line(&mut self) -> String {
        let line = self
            .lines
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("nothing was logged after:\n{}", self.seen.join("\n")));
        self.seen.push(line.clone());
        line
    }

    /// Everything logged in the next moment.
    fn drain(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(line) = self
            .lines
            .recv_timeout(std::time::Duration::from_millis(500))
        {
            self.seen.push(line.clone());
            out.push(line);
        }
        out
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The link itself, out of the `setup_link=` log line.
fn link_of(line: &str) -> String {
    let start = line.find("http").expect("the setup line carries a URL");
    line[start..]
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '"')
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_public_baseurl_the_link_names_the_listener_and_works_on_any_host() {
    let dir = tempfile::tempdir().unwrap();
    // Never 8008, so a link that still says `localhost:8008` or ignores the port fails.
    let mut port = free_port();
    while port == 8008 {
        port = free_port();
    }
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config_path,
        config_yaml(port, &dir.path().join("data"), None),
    )
    .unwrap();
    let mut server = HsProcess::serve(&config_path);

    let line = server.wait_for("setup_link=");
    let link = link_of(&line);
    let prefix = format!("http://127.0.0.1:{port}/admin/setup#token=");
    assert!(
        link.starts_with(&prefix),
        "the link should be rooted at the listener's own address {prefix}...; it was {link}"
    );
    let hint = server.next_line();
    assert!(
        hint.contains(HINT) && hint.contains("HS__SERVER__PUBLIC_BASEURL"),
        "the line after the link should say the host can be replaced and how to configure it; it was {hint}"
    );
    let token = link.split_once("#token=").unwrap().1.to_owned();
    assert!(!token.is_empty());

    // The browser reached this server some other way: a proxy's name, a remapped port. Only the
    // TCP connection goes to the listener; the `Host` is what the browser typed.
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let elsewhere = "matrix.elsewhere.example:4443";
    let page = client
        .get(format!("{base}/admin/setup"))
        .header(reqwest::header::HOST, elsewhere)
        .send()
        .await
        .unwrap();
    assert_eq!(
        page.status(),
        StatusCode::OK,
        "the setup page on another host"
    );
    let status = client
        .get(format!("{base}/api/v1/setup"))
        .header(reqwest::header::HOST, elsewhere)
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status: serde_json::Value = status.json().await.unwrap();
    assert_eq!(status["needs_setup"], true);
    let created = client
        .post(format!("{base}/api/v1/setup"))
        .header(reqwest::header::HOST, elsewhere)
        .json(&json!({"setup_token": token, "username": "ops", "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        created.status(),
        StatusCode::CREATED,
        "the token is what matters, not the host: {}",
        created.text().await.unwrap_or_default()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_public_baseurl_the_link_names_it_and_no_hint_follows() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config_path,
        config_yaml(
            port,
            &dir.path().join("data"),
            Some("https://matrix.example.org/"),
        ),
    )
    .unwrap();
    let mut server = HsProcess::serve(&config_path);

    let line = server.wait_for("setup_link=");
    let link = link_of(&line);
    assert!(
        link.starts_with("https://matrix.example.org/admin/setup#token="),
        "the link should be rooted at public_baseurl; it was {link}"
    );
    let after = server.drain();
    assert!(
        after.iter().all(|l| !l.contains(HINT)),
        "no hint when public_baseurl is set; the log said:\n{}",
        after.join("\n")
    );
}
