//! `storage.postgres.ssl_mode` through the real `hs` binary, against a PostgreSQL with
//! `ssl = on` and one without. Runs when both are reachable and prints `SKIP` otherwise.
//!
//! - `HS_CLUSTER_TEST_POSTGRES_TLS_DSN`: a server with `ssl = on` whose certificate names
//!   `localhost` (and not the DSN's address, so `verify-full` can be seen to check the name).
//! - `HS_CLUSTER_TEST_POSTGRES_TLS_CERT`: the PEM that certificate chains to (the CA, or the
//!   certificate itself when self-signed with `basicConstraints=CA:FALSE`).
//! - `HS_CLUSTER_TEST_POSTGRES_DSN`: a server with no TLS (`cluster_admin.rs`'s; defaults as
//!   there).
//!
//! See `crates/hs-kv/tests/postgres_tls.rs` for a `docker run` that makes the TLS server, then:
//!
//! ```sh
//! HS_CLUSTER_TEST_POSTGRES_TLS_DSN=postgres://postgres:hspg@127.0.0.1:5463/postgres \
//! HS_CLUSTER_TEST_POSTGRES_TLS_CERT=$PWD/ca.crt \
//! HS_CLUSTER_TEST_POSTGRES_DSN=postgres://postgres:hspg@127.0.0.1:5462/postgres \
//!     cargo test -p hs-cli --test postgres_tls
//! ```
//!
//! Each boot makes a database of its own on the server it uses and drops it after. A boot is
//! waited on only until the storage backend's own log line, not until the server listens, so
//! the six boots stay short.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A port for a configuration file; never the same one twice in this process.
fn reserve_port() -> u16 {
    static HANDED_OUT: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut handed_out = HANDED_OUT.lock().unwrap();
        if !handed_out.contains(&port) {
            handed_out.push(port);
            return port;
        }
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// A fresh database on one test server, dropped when this is.
struct Database {
    admin_dsn: String,
    name: String,
    host: String,
    port: u16,
    user: String,
    password: String,
}

impl Database {
    /// Makes one on `admin_dsn`'s server, or says why not. On a thread of its own: the
    /// synchronous `postgres` client runs a runtime inside, which cannot start within a test's.
    fn create(admin_dsn: String, what: &str) -> Option<Self> {
        let what = what.to_owned();
        std::thread::spawn(move || {
            let config: postgres::Config = admin_dsn.parse().expect("a postgres:// DSN");
            // The admin connection is `NoTls` on purpose: the TLS server accepts plain
            // connections too, and this test is about what `hs` does, not the harness.
            let mut client = match config.connect(postgres::NoTls) {
                Ok(client) => client,
                Err(e) => {
                    eprintln!("SKIP: postgres_tls needs {what} at {admin_dsn:?}: {e}");
                    return None;
                }
            };
            let name = format!("hs_pg_tls_{}_{}", std::process::id(), nanos());
            client
                .batch_execute(&format!("CREATE DATABASE {name}"))
                .unwrap();
            let host = match config.get_hosts().first() {
                Some(postgres::config::Host::Tcp(host)) => host.clone(),
                _ => "127.0.0.1".to_owned(),
            };
            Some(Self {
                admin_dsn,
                name,
                host,
                port: config.get_ports().first().copied().unwrap_or(5432),
                user: config.get_user().unwrap_or("postgres").to_owned(),
                password: String::from_utf8_lossy(config.get_password().unwrap_or_default())
                    .into_owned(),
            })
        })
        .join()
        .unwrap()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let (dsn, name) = (self.admin_dsn.clone(), self.name.clone());
        let _ = std::thread::spawn(move || {
            if let Ok(mut client) = dsn
                .parse::<postgres::Config>()
                .and_then(|c| c.connect(postgres::NoTls))
            {
                let _ =
                    client.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"));
            }
        })
        .join();
    }
}

struct Servers {
    tls: String,
    cert: PathBuf,
    plain: String,
}

fn servers() -> Option<Servers> {
    let (Ok(tls), Ok(cert)) = (
        std::env::var("HS_CLUSTER_TEST_POSTGRES_TLS_DSN"),
        std::env::var("HS_CLUSTER_TEST_POSTGRES_TLS_CERT"),
    ) else {
        eprintln!(
            "SKIP: postgres_tls needs HS_CLUSTER_TEST_POSTGRES_TLS_DSN and \
             HS_CLUSTER_TEST_POSTGRES_TLS_CERT (see the test file's docs)"
        );
        return None;
    };
    let plain = std::env::var("HS_CLUSTER_TEST_POSTGRES_DSN")
        .unwrap_or_else(|_| "postgres://postgres:hspg@127.0.0.1:5439/postgres".to_owned());
    Some(Servers {
        tls,
        cert: PathBuf::from(cert),
        plain,
    })
}

/// A configuration for one boot: `ssl_mode` (and a root cert) against `db`, connecting to
/// `host` (the database's own address unless the case needs the certificate's name).
fn config(
    db: &Database,
    dir: &Path,
    host: Option<&str>,
    ssl_mode: &str,
    root_cert: Option<&Path>,
) -> String {
    let port = reserve_port();
    let root_cert = root_cert
        .map(|p| format!("  ssl_root_cert: {p:?}\n"))
        .unwrap_or_default();
    format!(
        "server:\n  server_name: tls.example.org\n  signing_key_path: {keys:?}\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health]\n\
         storage:\n  backend: postgres\n  host: {host}\n  port: {pg_port}\n  database: {name}\n  user: {user}\n  password: {password}\n  ssl_mode: {ssl_mode}\n  pool_size: 3\n  schema: hs_tls_test\n{root_cert}\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n",
        keys = dir.join("keys"),
        media = dir.join("media"),
        host = host.unwrap_or(&db.host),
        pg_port = db.port,
        name = db.name,
        user = db.user,
        password = db.password,
    )
}

/// The storage line and exit status of one `hs serve`: the log line the backend writes on
/// opening (`Ok`), or the process's whole output once it has exited (`Err`).
fn boot(config_path: &Path) -> Result<String, String> {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
        .args(["serve", "-c"])
        .arg(config_path)
        .env_remove("RUST_LOG")
        .env_remove("HS_DATA_DIR")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the hs binary should start");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    for reader in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(reader)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut seen = Vec::new();
    let outcome = loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) => {
                seen.push(line.clone());
                if line.contains("opened the PostgreSQL storage backend") {
                    break Ok(line);
                }
                if let Ok(Some(status)) = child.try_wait()
                    && !status.success()
                {
                    // Give the readers a moment to deliver the rest of what it printed.
                    while let Ok(line) = rx.recv_timeout(Duration::from_millis(500)) {
                        seen.push(line);
                    }
                    break Err(seen.join("\n"));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Err(seen.join("\n")),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!(
                    "hs never opened storage or exited; it said:\n{}",
                    seen.join("\n")
                )
            }
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

/// The `key=value` field of a tracing log line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(&format!("{key}=")))
        .map(|v| v.trim_matches('"'))
}

#[test]
fn every_ssl_mode_through_the_real_binary() {
    let Some(servers) = servers() else {
        return;
    };
    let Some(tls_db) = Database::create(servers.tls.clone(), "a PostgreSQL with ssl = on") else {
        return;
    };
    let Some(plain_db) = Database::create(servers.plain.clone(), "a PostgreSQL without TLS") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cert = servers.cert.as_path();

    // Every boot that should succeed.
    struct Boot<'a> {
        db: &'a Database,
        /// The host to connect to when the case needs the certificate's name.
        host: Option<&'a str>,
        mode: &'a str,
        root_cert: Option<&'a Path>,
        encrypted: bool,
    }
    let ok_cases = [
        Boot {
            db: &tls_db,
            host: None,
            mode: "require",
            root_cert: None,
            encrypted: true,
        },
        Boot {
            db: &tls_db,
            host: None,
            mode: "verify-ca",
            root_cert: Some(cert),
            encrypted: true,
        },
        Boot {
            db: &tls_db,
            host: Some("localhost"),
            mode: "verify-full",
            root_cert: Some(cert),
            encrypted: true,
        },
        Boot {
            db: &tls_db,
            host: None,
            mode: "prefer",
            root_cert: None,
            encrypted: true,
        },
        Boot {
            db: &plain_db,
            host: None,
            mode: "prefer",
            root_cert: None,
            encrypted: false,
        },
        Boot {
            db: &tls_db,
            host: None,
            mode: "disable",
            root_cert: None,
            encrypted: false,
        },
    ];
    for (i, case) in ok_cases.iter().enumerate() {
        let (db, mode) = (case.db, case.mode);
        let path = dir.path().join(format!("ok-{i}-{mode}.yaml"));
        std::fs::write(
            &path,
            config(db, dir.path(), case.host, mode, case.root_cert),
        )
        .unwrap();
        let line = boot(&path).unwrap_or_else(|out| panic!("{mode} should boot; it said:\n{out}"));
        assert_eq!(field(&line, "ssl_mode"), Some(mode), "{line}");
        assert_eq!(
            field(&line, "encrypted"),
            Some(if case.encrypted { "true" } else { "false" }),
            "{line}"
        );
        assert_eq!(field(&line, "schema"), Some("hs_tls_test"), "{line}");
        assert_eq!(field(&line, "pool_size"), Some("3"), "{line}");
        assert_eq!(field(&line, "database"), Some(db.name.as_str()), "{line}");
    }

    // require against the plain server: refused at startup, naming the setting.
    let path = dir.path().join("fail-require-plain.yaml");
    std::fs::write(&path, config(&plain_db, dir.path(), None, "require", None)).unwrap();
    let out = boot(&path).expect_err("require against a plain server must not boot");
    assert!(
        out.contains("storage.postgres.ssl_mode = require could not be satisfied"),
        "{out}"
    );
    assert!(out.contains("server does not support TLS"), "{out}");

    // verify-full by IP address: the certificate names localhost, so the name check fails.
    let path = dir.path().join("fail-verify-full-ip.yaml");
    std::fs::write(
        &path,
        config(
            &tls_db,
            dir.path(),
            Some("127.0.0.1"),
            "verify-full",
            Some(cert),
        ),
    )
    .unwrap();
    let out = boot(&path).expect_err("verify-full by an address the certificate does not name");
    assert!(
        out.contains("storage.postgres.ssl_mode = verify-full could not be satisfied"),
        "{out}"
    );
}
