//! A fake identity server over TLS, shared by the third-party invite tests
//! (`third_party_invites.rs`, `third_party_invites_federation.rs`).

#![allow(dead_code)]

use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub const PEPPER: &str = "pepper";

pub fn lookup_hash(address: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("{address} email {PEPPER}").as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
}

/// A fake identity server: `bob@example.org` is bound to bob, anything else is not, and every
/// invitation it stores gets token `tok1` and this key.
#[derive(Clone)]
pub struct FakeIdentityServer {
    pub key: Arc<ed25519_dalek::SigningKey>,
    pub base: String,
    pub stored: Arc<Mutex<Vec<Value>>>,
}

impl FakeIdentityServer {
    pub fn public_key(&self) -> String {
        b64(&self.key.verifying_key().to_bytes())
    }

    pub fn sign(&self, mxid: &str, token: &str) -> Value {
        let signed = json!({"mxid": mxid, "token": token});
        let canonical = hs_model::canonical::to_canonical_value(&signed, true).unwrap();
        let signature = ed25519_dalek::Signer::sign(&*self.key, &canonical.to_canonical_bytes());
        let mut signed = signed;
        signed["signatures"] = json!({"localhost": {"ed25519:0": b64(&signature.to_bytes())}});
        signed
    }

    pub fn router(&self) -> axum::Router {
        use axum::routing::{get, post};
        let lookup_server = self.clone();
        let store_server = self.clone();
        let valid_server = self.clone();
        axum::Router::new()
            .route(
                "/_matrix/identity/v2/hash_details",
                get(|| async { axum::Json(json!({"lookup_pepper": PEPPER, "algorithms": ["sha256"]})) }),
            )
            .route(
                "/_matrix/identity/v2/lookup",
                post(move |axum::Json(body): axum::Json<Value>| {
                    let _ = &lookup_server;
                    async move {
                        let mut mappings = serde_json::Map::new();
                        let bob = lookup_hash("bob@example.org");
                        for address in body["addresses"].as_array().unwrap() {
                            if address == &Value::String(bob.clone()) {
                                mappings.insert(bob.clone(), json!("@bob:example.org"));
                            }
                        }
                        axum::Json(json!({"mappings": mappings}))
                    }
                }),
            )
            .route(
                "/_matrix/identity/v2/store-invite",
                post(move |axum::Json(body): axum::Json<Value>| {
                    let server = store_server.clone();
                    async move {
                        server.stored.lock().unwrap().push(body);
                        let key = server.public_key();
                        axum::Json(json!({
                            "token": "tok1",
                            "display_name": "c...@e...",
                            "public_key": key,
                            "public_keys": [{
                                "public_key": key,
                                "key_validity_url": format!("{}/_matrix/identity/v2/pubkey/isvalid", server.base),
                            }],
                        }))
                    }
                }),
            )
            .route(
                "/_matrix/identity/v2/pubkey/isvalid",
                get(
                    move |axum::extract::Query(query): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| {
                        let server = valid_server.clone();
                        async move {
                            axum::Json(json!({"valid": query.get("public_key") == Some(&server.public_key())}))
                        }
                    },
                ),
            )
    }
}

/// Serves `router` over TLS on a free port, with a certificate for `localhost` the server under
/// test is told not to verify (as Sytest's own identity server needs).
pub async fn serve_tls(make: impl FnOnce(String) -> axum::Router) -> String {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let key_der = rustls_pki_types::PrivateKeyDer::Pkcs8(
        rustls_pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()),
    );
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key_der)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    // The identity server itself speaks plain HTTP on one port; a TLS-terminating proxy in
    // front of it is what the server under test talks to.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("https://localhost:{port}");
    let plain = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plain_port = plain.local_addr().unwrap().port();
    let router = make(base.clone());
    tokio::spawn(async move {
        let _ = axum::serve(plain, router).await;
    });
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(mut backend) =
                    tokio::net::TcpStream::connect(("127.0.0.1", plain_port)).await
                else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut backend).await;
            });
        }
    });
    base
}
