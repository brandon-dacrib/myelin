//! The key server of the real binary: the deprecated `GET /_matrix/key/v2/server/{keyId}`
//! answers the same document as `/server`, and the notary `/_matrix/key/v2/query` (both
//! spellings) answers another real server's keys co-signed by this one. Two `hs serve` instances
//! over plain HTTP, named by IP literal, as in `federation_two_servers.rs`.

use serde_json::{Value, json};

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(port: u16, data_dir: &std::path::Path) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: \"127.0.0.1:{port}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         federation:\n  ip_range_blocklist: []\n"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

struct Server {
    handle: hs_cli::serve::ServeHandle,
    base: String,
    name: String,
    _dir: tempfile::TempDir,
}

async fn start(port: u16) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let handle = hs_cli::serve::spawn_serve(
        config(port, dir.path()),
        hs_cli::serve::ServeOptions {
            federation_scheme: Some("http"),
            ..Default::default()
        },
    )
    .await
    .expect("the server boots");
    Server {
        base: handle.base_url(),
        name: format!("127.0.0.1:{port}"),
        handle,
        _dir: dir,
    }
}

/// Checks `doc` carries a valid signature by `signer`, against the key `signer` publishes.
async fn assert_signed_by(client: &reqwest::Client, doc: &Value, signer: &Server) {
    let published: Value = client
        .get(format!("{}/_matrix/key/v2/server", signer.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (key_id, key) = published["verify_keys"]
        .as_object()
        .unwrap()
        .iter()
        .next()
        .unwrap();
    let verifying_key =
        hs_model::signing::verifying_key_from_base64(key["key"].as_str().unwrap()).unwrap();
    let object = hs_model::signing::to_signable_object(doc).unwrap();
    hs_model::signing::verify_object(&object, &signer.name, key_id, &verifying_key)
        .unwrap_or_else(|e| panic!("{doc} is not signed by {}: {e}", signer.name));
}

#[tokio::test]
async fn the_key_id_spelling_and_the_notary_answer_on_the_real_binary() {
    let a = start(reserve_port()).await;
    let b = start(reserve_port()).await;
    let client = reqwest::Client::new();

    // The deprecated key-id spelling: the same document, whatever key it names. It was not
    // routed at all, and Sytest's federation client asks it first.
    let plain: Value = client
        .get(format!("{}/_matrix/key/v2/server", a.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key_id = plain["verify_keys"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    let response = client
        .get(format!("{}/_matrix/key/v2/server/{key_id}", a.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let by_id: Value = response.json().await.unwrap();
    assert_eq!(by_id["verify_keys"], plain["verify_keys"]);
    assert_signed_by(&client, &by_id, &a).await;

    // The notary: A answers B's keys, fetched from B, co-signed by A and still signed by B.
    let b_keys: Value = client
        .get(format!("{}/_matrix/key/v2/server", b.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let b_key_id = b_keys["verify_keys"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    let by_post = client
        .post(format!("{}/_matrix/key/v2/query", a.base))
        .json(&json!({"server_keys": {&b.name: {&b_key_id: {"minimum_valid_until_ts": 0}}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(by_post.status(), reqwest::StatusCode::OK);
    let by_post: Value = by_post.json().await.unwrap();
    let by_get: Value = client
        .get(format!("{}/_matrix/key/v2/query/{}", a.base, b.name))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for answer in [&by_post, &by_get] {
        let docs = answer["server_keys"].as_array().unwrap();
        assert_eq!(docs.len(), 1, "{answer}");
        assert_eq!(docs[0]["server_name"], b.name.as_str());
        assert_eq!(docs[0]["verify_keys"], b_keys["verify_keys"]);
        assert_signed_by(&client, &docs[0], &a).await;
        assert_signed_by(&client, &docs[0], &b).await;
    }

    // A server nobody answers for is left out.
    let unreachable: Value = client
        .post(format!("{}/_matrix/key/v2/query", a.base))
        .json(&json!({"server_keys": {format!("127.0.0.1:{}", reserve_port()): {}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(unreachable["server_keys"], json!([]));

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}
