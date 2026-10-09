//! A listener with `tls:` is served as HTTPS by the real `hs` binary itself: a private CA and a
//! leaf for 127.0.0.1 from `rcgen`, the chain and key on disk, one TLS listener and one
//! plaintext listener in the configuration. A client that trusts the CA gets `/versions` and
//! `/health/ready` over HTTPS (over HTTP/2, which the listener offers through ALPN), a
//! plaintext request to the TLS port is refused without disturbing the next one, the
//! plaintext listener still answers, and a key that does not match its certificate stops the
//! start with the listener named.

use std::sync::Arc;
use std::time::{Duration, Instant};

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

struct Pki {
    ca_pem: String,
    cert_path: std::path::PathBuf,
    key_path: std::path::PathBuf,
    other_key_path: std::path::PathBuf,
}

fn mint(dir: &std::path::Path) -> Pki {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "tls_listener test CA");
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .signed_by(&leaf_key, &ca)
        .unwrap();
    let cert_path = dir.join("tls.crt");
    let key_path = dir.join("tls.key");
    let other_key_path = dir.join("other.key");
    // The chain: leaf first, then the CA, as a certificate file from an issuer comes.
    std::fs::write(&cert_path, format!("{}{}", leaf.pem(), ca.pem())).unwrap();
    std::fs::write(&key_path, leaf_key.serialize_pem()).unwrap();
    std::fs::write(
        &other_key_path,
        rcgen::KeyPair::generate().unwrap().serialize_pem(),
    )
    .unwrap();
    Pki {
        ca_pem: ca.pem(),
        cert_path,
        key_path,
        other_key_path,
    }
}

fn config(
    dir: &std::path::Path,
    tls_port: u16,
    plain_port: u16,
    cert: &std::path::Path,
    key: &std::path::Path,
) -> std::path::PathBuf {
    let path = dir.join("homeserver.yaml");
    std::fs::write(
        &path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n\
             \x20   - port: {tls_port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
             \x20     tls:\n        certificate_path: {cert:?}\n        private_key_path: {key:?}\n\
             \x20   - port: {plain_port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n",
            dir.join("data"),
            dir.join("media"),
        ),
    )
    .unwrap();
    path
}

/// A TLS handshake with the listener offering `alpn`, returning what the server chose.
async fn negotiated_alpn(port: u16, ca_pem: &str, alpn: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.to_vec();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let name = rustls_pki_types::ServerName::try_from("127.0.0.1").unwrap();
    let tls = connector.connect(name, tcp).await.unwrap();
    tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec)
}

fn spawn(config_path: &std::path::Path) -> HsProcess {
    HsProcess(
        std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
            .args(["serve", "-c"])
            .arg(config_path)
            .env_remove("RUST_LOG")
            .env_remove("HS_DATA_DIR")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("the hs binary should start"),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listener_with_tls_is_served_as_https_by_the_binary_itself() {
    let dir = tempfile::tempdir().unwrap();
    let pki = mint(dir.path());
    let tls_port = free_port();
    let plain_port = free_port();
    let config_path = config(
        dir.path(),
        tls_port,
        plain_port,
        &pki.cert_path,
        &pki.key_path,
    );
    let mut hs = spawn(&config_path);

    let trusting = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(pki.ca_pem.as_bytes()).unwrap())
        .build()
        .unwrap();
    let https = format!("https://127.0.0.1:{tls_port}");
    let http = format!("http://127.0.0.1:{plain_port}");

    // Generous: a debug build's first boot on a loaded disk has taken well over a minute.
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if let Ok(response) = trusting.get(format!("{https}/health/live")).send().await
            && response.status().is_success()
        {
            break;
        }
        if let Ok(Some(status)) = hs.0.try_wait() {
            panic!("hs exited before it was live: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "hs was not live over TLS within 300s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The client API over HTTPS.
    let versions = trusting
        .get(format!("{https}/_matrix/client/versions"))
        .send()
        .await
        .unwrap();
    assert_eq!(versions.status(), 200);
    // And the listener offers HTTP/2 through ALPN: a raw handshake that asks for h2 gets it.
    assert_eq!(
        negotiated_alpn(
            tls_port,
            &pki.ca_pem,
            &[b"h2".to_vec(), b"http/1.1".to_vec()]
        )
        .await,
        Some(b"h2".to_vec())
    );
    let body: serde_json::Value = versions.json().await.unwrap();
    assert!(body["versions"].as_array().is_some_and(|v| !v.is_empty()));
    let ready = trusting
        .get(format!("{https}/health/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);
    // The federation resource is on it too, so a server with nothing in front can be the
    // `.well-known`-delegated or SRV target itself.
    let key = trusting
        .get(format!("{https}/_matrix/key/v2/server"))
        .send()
        .await
        .unwrap();
    assert_eq!(key.status(), 200);

    // A client that does not trust the CA is refused by the handshake: the server's
    // certificate is the private CA's, nothing else.
    let distrusting = reqwest::Client::new();
    let refused = distrusting
        .get(format!("{https}/_matrix/client/versions"))
        .send()
        .await;
    assert!(
        refused.is_err(),
        "an untrusted CA should fail the handshake"
    );

    // Plaintext at the TLS port is not an HTTP conversation; and it costs the next client
    // nothing (the failed handshake is its own task).
    let plaintext_at_tls = distrusting
        .get(format!(
            "http://127.0.0.1:{tls_port}/_matrix/client/versions"
        ))
        .send()
        .await;
    assert!(
        plaintext_at_tls.is_err(),
        "plaintext on the TLS port should fail"
    );
    let after = trusting
        .get(format!("{https}/_matrix/client/versions"))
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 200);

    // The plaintext listener beside it is untouched.
    let plain = distrusting
        .get(format!("{http}/_matrix/client/versions"))
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), 200);

    // Several handshakes at once, as a browser opens them: all served.
    let client = Arc::new(trusting);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        let url = format!("{https}/health/live");
        tasks.push(tokio::spawn(async move {
            client.get(url).send().await.unwrap().status()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), 200);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_that_does_not_match_the_certificate_stops_the_start_naming_the_listener() {
    let dir = tempfile::tempdir().unwrap();
    let pki = mint(dir.path());
    let tls_port = free_port();
    let plain_port = free_port();
    let config_path = config(
        dir.path(),
        tls_port,
        plain_port,
        &pki.cert_path,
        &pki.other_key_path,
    );
    let mut hs = spawn(&config_path);
    let deadline = Instant::now() + Duration::from_secs(300);
    let status = loop {
        if let Some(status) = hs.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "hs should have refused to start with a mismatched key"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        !status.success(),
        "hs should exit with an error, got {status}"
    );
    let mut stderr = String::new();
    std::io::Read::read_to_string(hs.0.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(
        stderr.contains(&format!("127.0.0.1:{tls_port}")) && stderr.contains("declares TLS"),
        "the error should name the listener and the TLS material: {stderr}"
    );
}
