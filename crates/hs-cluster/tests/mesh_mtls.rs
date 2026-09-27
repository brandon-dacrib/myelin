//! The mesh over mutual TLS, end to end over a real socket: a [`hs_cluster::mesh::MeshServer`]
//! serving with a certificate from a private CA, a [`hs_cluster::mesh::Forwarder`] whose
//! certificate the same CA issued completes a forward, and one whose certificate a *different* CA
//! issued is refused at the handshake. A third case, the same CA but a name outside the
//! configured SAN suffix, is refused by the authenticator after the handshake.
//!
//! `docs/rfcs/0001-cluster-ownership.md` section 11 specifies mutual TLS as the production mesh
//! authentication; until this file the TLS code (`hs_cluster::mesh::tls`) was tested only for
//! loading and SAN extraction, never for a real handshake between the two halves of the mesh.
//! The certificates are minted with `rcgen` the way `hs-federation`'s own in-process TLS test
//! does, so no Docker, no `openssl` binary and no fixtures on disk are needed.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use hs_cluster::mesh::tls::TlsMaterial;
use hs_cluster::mesh::{
    AuthMode, Envelope, Forwarder, IdempotencyCache, IdempotencyKey, MeshDeps, MeshServer,
    MutualTlsAuthenticator, Reply, ShardHandler,
};
use hs_cluster::metrics::ClusterMetrics;
use hs_cluster::ownership::SingleNode;
use hs_cluster::{Fence, Generation, ReplicaId, ShardId, ShardKind};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair, SanType,
};
use tokio::sync::watch;

/// A private certificate authority: a self-signed CA certificate and the key that signs with it.
struct PrivateCa {
    name: String,
    params: CertificateParams,
    key: KeyPair,
    cert_pem: String,
}

impl PrivateCa {
    fn new(name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("empty SAN list");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, name);
        params.distinguished_name = dn;
        let key = KeyPair::generate().expect("CA key");
        let cert_pem = params.self_signed(&key).expect("self-signed CA").pem();
        Self {
            name: name.to_owned(),
            params,
            key,
            cert_pem,
        }
    }

    /// Issues a leaf certificate for `dns_name` (plus `127.0.0.1` as an IP SAN, which is what
    /// the forwarder dials and therefore what `rustls` verifies the server's certificate
    /// against). Returns `(certificate PEM, private key PEM)`.
    fn issue(&self, dns_name: &str) -> (String, String) {
        let mut params = CertificateParams::new(vec![dns_name.to_owned()]).expect("SAN");
        params
            .subject_alt_names
            .push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, dns_name);
        params.distinguished_name = dn;
        let key = KeyPair::generate().expect("leaf key");
        let issuer = Issuer::from_params(&self.params, &self.key);
        let cert = params.signed_by(&key, &issuer).expect("signed leaf");
        (cert.pem(), key.serialize_pem())
    }

    /// Writes a leaf for `dns_name` and this CA's certificate into `dir`, returning the three
    /// paths `TlsMaterial::load` wants, in its order: CA, certificate, key.
    fn material_on_disk(&self, dir: &Path, dns_name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let (cert_pem, key_pem) = self.issue(dns_name);
        let stem = format!("{}-{dns_name}", self.name);
        let ca = dir.join(format!("{stem}-ca.pem"));
        let cert = dir.join(format!("{stem}-cert.pem"));
        let key = dir.join(format!("{stem}-key.pem"));
        std::fs::write(&ca, &self.cert_pem).expect("write ca");
        std::fs::write(&cert, cert_pem).expect("write cert");
        std::fs::write(&key, key_pem).expect("write key");
        (ca, cert, key)
    }
}

/// A `ShardHandler` that echoes the envelope's payload back with `200` and counts how many
/// envelopes ever reached it: for the refusal cases the count staying at zero is the proof that
/// the refusal happened in front of the handler, not after it.
struct Echo(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl ShardHandler for Echo {
    async fn handle(&self, env: Envelope, _fence: Fence) -> Reply {
        self.0.fetch_add(1, Ordering::SeqCst);
        Reply {
            status: 200,
            payload: env.payload,
        }
    }
}

/// A port nobody is listening on at the moment of the call. `MeshServer` binds the address it
/// is given only inside `serve`, so a test has to pick the port first; the gap in which another
/// process could take it is the same one every in-process server test in this workspace lives
/// with.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0")
        .local_addr()
        .expect("local_addr")
        .port()
}

struct RunningServer {
    addr: String,
    handled: Arc<AtomicUsize>,
    shutdown: watch::Sender<bool>,
}

/// Starts a mutual-TLS mesh server presenting `material`, accepting peers whose certificate
/// chains to the CA in `material` and carries a DNS SAN ending in `peer_san_suffix`.
async fn spawn_mtls_server(material: TlsMaterial, peer_san_suffix: &str) -> RunningServer {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let handled = Arc::new(AtomicUsize::new(0));
    let deps = Arc::new(MeshDeps {
        authenticator: Arc::new(MutualTlsAuthenticator::new(Some(
            peer_san_suffix.to_owned(),
        ))),
        ownership: SingleNode::new(ReplicaId::new(addr.clone())),
        handler: Arc::new(Echo(handled.clone())),
        idempotency: Arc::new(IdempotencyCache::new(Duration::from_secs(5), 64)),
        in_flight: Arc::new(tokio::sync::Semaphore::new(16)),
        nudge: None,
    });
    let server = MeshServer::new(addr.clone(), Some(&material)).expect("server config");
    let (shutdown, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        server
            .serve(deps, shutdown_rx)
            .await
            .expect("mesh server bound");
    });
    // Until the listener is bound a client's connect is refused outright; wait for it.
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    RunningServer {
        addr,
        handled,
        shutdown,
    }
}

/// A forwarder dialing `server_addr` in mutual-TLS mode with `material`.
fn mtls_forwarder(
    server_addr: &str,
    material: &TlsMaterial,
    (ca, cert, key): &(PathBuf, PathBuf, PathBuf),
) -> Forwarder {
    Forwarder::new(
        AuthMode::MutualTls {
            ca_file: ca.clone(),
            cert_file: cert.clone(),
            key_file: key.clone(),
            peer_san_suffix: None,
        },
        Some(material),
        3,
        2,
        Duration::from_millis(5),
        SingleNode::new(ReplicaId::new(server_addr.to_owned())),
        Arc::new(ClusterMetrics::new()),
    )
    .expect("forwarder with TLS material")
}

fn envelope(payload: &'static [u8]) -> Envelope {
    Envelope {
        shard: ShardId::new(ShardKind::Room, 7),
        route: "test.echo".into(),
        idempotency_key: IdempotencyKey::generate(),
        requester: serde_json::Value::Null,
        deadline: Duration::from_secs(5),
        origin: ReplicaId::new("origin:1"),
        origin_generation: Generation(1),
        hops: 0,
        traceparent: None,
        payload: Bytes::from_static(payload),
    }
}

#[tokio::test]
async fn a_peer_with_a_certificate_from_the_same_ca_completes_a_forward() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = PrivateCa::new("mesh-ca");
    let server_paths = ca.material_on_disk(dir.path(), "hs-0.mesh.test");
    let client_paths = ca.material_on_disk(dir.path(), "hs-1.mesh.test");
    let server_material =
        TlsMaterial::load(&server_paths.0, &server_paths.1, &server_paths.2).expect("server");
    let client_material =
        TlsMaterial::load(&client_paths.0, &client_paths.1, &client_paths.2).expect("client");

    let server = spawn_mtls_server(server_material, ".mesh.test").await;
    let forwarder = mtls_forwarder(&server.addr, &client_material, &client_paths);

    let reply = forwarder
        .forward(envelope(b"hello over mtls"))
        .await
        .expect("a forward between two replicas of the same CA succeeds");
    assert_eq!(reply.status, 200);
    assert_eq!(reply.payload.as_ref(), b"hello over mtls");
    assert_eq!(server.handled.load(Ordering::SeqCst), 1);

    let _ = server.shutdown.send(true);
}

#[tokio::test]
async fn a_peer_with_a_certificate_from_another_ca_is_refused_at_the_handshake() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = PrivateCa::new("mesh-ca");
    let other_ca = PrivateCa::new("someone-elses-ca");
    let server_paths = ca.material_on_disk(dir.path(), "hs-0.mesh.test");
    // A certificate with exactly the right name, signed by the wrong authority: the name is
    // not what the mesh trusts, the chain is.
    let intruder_paths = other_ca.material_on_disk(dir.path(), "hs-9.mesh.test");
    let server_material =
        TlsMaterial::load(&server_paths.0, &server_paths.1, &server_paths.2).expect("server");
    let intruder_material =
        TlsMaterial::load(&intruder_paths.0, &intruder_paths.1, &intruder_paths.2)
            .expect("intruder");

    let server = spawn_mtls_server(server_material, ".mesh.test").await;
    let forwarder = mtls_forwarder(&server.addr, &intruder_material, &intruder_paths);

    // The handshake fails on both sides (the server does not trust the client's chain, the
    // client does not trust the server's), which the forwarder sees as a transport failure and
    // retries until its attempts are spent; the handler behind the listener never runs.
    let error = forwarder
        .forward(envelope(b"let me in"))
        .await
        .expect_err("a certificate from another CA must not complete a forward");
    assert!(
        error.to_string().contains("exhausted"),
        "expected the forwarder to give up after retrying a failed handshake, got: {error}"
    );
    assert_eq!(
        server.handled.load(Ordering::SeqCst),
        0,
        "the mesh handler must never see a request from outside the CA"
    );

    // And the other direction: a legitimate client does not accept a server from the other CA
    // either (the intruder cannot impersonate an owner and receive forwards).
    let intruder_server = spawn_mtls_server(
        TlsMaterial::load(&intruder_paths.0, &intruder_paths.1, &intruder_paths.2).expect("i"),
        ".mesh.test",
    )
    .await;
    let legit_paths = ca.material_on_disk(dir.path(), "hs-1.mesh.test");
    let legit_material =
        TlsMaterial::load(&legit_paths.0, &legit_paths.1, &legit_paths.2).expect("legit");
    let forwarder = mtls_forwarder(&intruder_server.addr, &legit_material, &legit_paths);
    forwarder
        .forward(envelope(b"are you an owner?"))
        .await
        .expect_err("a legitimate replica must not forward to a server outside the CA");
    assert_eq!(intruder_server.handled.load(Ordering::SeqCst), 0);

    let _ = server.shutdown.send(true);
    let _ = intruder_server.shutdown.send(true);
}

#[tokio::test]
async fn a_peer_outside_the_san_suffix_is_refused_after_the_handshake() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = PrivateCa::new("mesh-ca");
    let server_paths = ca.material_on_disk(dir.path(), "hs-0.mesh.test");
    // Same CA, so the handshake itself succeeds; but the name is not one of this cluster's
    // pods, which the authenticator's SAN-suffix check catches.
    let stranger_paths = ca.material_on_disk(dir.path(), "something-else.example.org");
    let server_material =
        TlsMaterial::load(&server_paths.0, &server_paths.1, &server_paths.2).expect("server");
    let stranger_material =
        TlsMaterial::load(&stranger_paths.0, &stranger_paths.1, &stranger_paths.2)
            .expect("stranger");

    let server = spawn_mtls_server(server_material, ".mesh.test").await;
    let forwarder = mtls_forwarder(&server.addr, &stranger_material, &stranger_paths);

    let outcome = forwarder.forward(envelope(b"hi")).await;
    match outcome {
        Ok(reply) => assert_eq!(
            reply.status, 401,
            "a wrong-name peer must be answered with 401 from the authenticator, not served"
        ),
        Err(error) => {
            let text = error.to_string();
            assert!(text.contains("401"), "expected a 401 refusal, got: {text}");
        }
    }
    assert_eq!(
        server.handled.load(Ordering::SeqCst),
        0,
        "the authenticator must refuse a wrong-name peer before the handler runs"
    );

    let _ = server.shutdown.send(true);
}

#[tokio::test]
async fn a_plaintext_shared_secret_client_cannot_reach_an_mtls_server() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = PrivateCa::new("mesh-ca");
    let server_paths = ca.material_on_disk(dir.path(), "hs-0.mesh.test");
    let server_material =
        TlsMaterial::load(&server_paths.0, &server_paths.1, &server_paths.2).expect("server");
    let server = spawn_mtls_server(server_material, ".mesh.test").await;

    let plaintext = Forwarder::new(
        AuthMode::SharedSecret {
            secret: "not-the-mesh-auth".into(),
        },
        None,
        3,
        2,
        Duration::from_millis(5),
        SingleNode::new(ReplicaId::new(server.addr.clone())),
        Arc::new(ClusterMetrics::new()),
    )
    .expect("plaintext forwarder");
    plaintext
        .forward(envelope(b"plaintext"))
        .await
        .expect_err("a plaintext client gets no HTTP/2 session from a TLS listener");
    assert_eq!(server.handled.load(Ordering::SeqCst), 0);

    let _ = server.shutdown.send(true);
}
