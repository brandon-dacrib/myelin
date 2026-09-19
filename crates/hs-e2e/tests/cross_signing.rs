//! Signature-verification tests for `POST /keys/device_signing/upload` and
//! `POST /keys/signatures/upload` (`crates/hs-e2e/src/routes/cross_signing.rs`), added when those
//! two endpoints started cryptographically verifying signatures instead of storing whatever they
//! were handed. Composition mirrors `tests/scenario.rs`.

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use hs_auth::state::AuthState;
use hs_e2e::state::E2eState;
use hs_e2e::store::tables::TablesE2eStore;
use hs_kv::memory::MemoryBackend;
use hs_model::signing::{SigningKeyPair, sign_bytes, to_signable_object};
use hs_testkit::Scenario;
use serde_json::{Value, json};

fn obj1(k: impl Into<String>, v: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(k.into(), v);
    Value::Object(map)
}

fn app() -> axum::Router {
    let auth_state = AuthState::in_memory();
    let backend = MemoryBackend::new();
    let store = TablesE2eStore::open(backend).expect("open e2e store");
    let e2e_state: E2eState<MemoryBackend> = E2eState::new(auth_state.clone(), Arc::new(store));
    let (e2e_router, _manifest) = hs_e2e::routes::router::<MemoryBackend>();

    hs_auth::routes::router()
        .with_state(auth_state)
        .merge(e2e_router.with_state(e2e_state))
}

/// Computes the exact bytes `hs_model::signing::verify_object` checks for `value` (canonical JSON
/// with `signatures`/`unsigned` stripped) and signs them with `key`, base64-encoding the result
/// the same way the spec's signed JSON does (unpadded, standard alphabet).
fn sign_canonical(value: &Value, key: &SigningKeyPair) -> String {
    let mut canonical = to_signable_object(value).expect("value canonicalizes for signing");
    canonical.remove("signatures");
    canonical.remove("unsigned");
    let bytes = hs_model::canonical::CanonicalJsonValue::Object(canonical).to_canonical_bytes();
    STANDARD_NO_PAD.encode(sign_bytes(&bytes, key).to_bytes())
}

/// A cross-signing key object (`{"user_id", "usage", "keys": {"<key_id>": "<pubkey>"}}`, no
/// `signatures` yet) plus the key ID and keypair it was built from.
struct CrossSigningFixture {
    key_id: String,
    keypair: SigningKeyPair,
    json: Value,
}

fn make_cross_signing_key(user_id: &str, usage: &str, version: &str) -> CrossSigningFixture {
    let keypair = SigningKeyPair::generate(version);
    let key_id = format!("ed25519:{}", keypair.verifying_key_base64());
    let json = json!({
        "user_id": user_id,
        "usage": [usage],
        "keys": { key_id.clone(): keypair.verifying_key_base64() },
    });
    CrossSigningFixture {
        key_id,
        keypair,
        json,
    }
}

/// `fixture` signed by `signer_id`'s `signing_key`, with the resulting `signatures` field
/// attached (any placeholder pre-existing `signatures` is discarded, matching how a client
/// would build the object once).
fn signed_by(
    mut fixture_json: Value,
    signer_id: &str,
    signing_key_id: &str,
    signing_key: &SigningKeyPair,
) -> Value {
    let signature = sign_canonical(&fixture_json, signing_key);
    fixture_json["signatures"] = json!({ signer_id: { signing_key_id: signature } });
    fixture_json
}

/// A full happy-path bootstrap: registers `name`, uploads a master key and a self-signing key
/// genuinely signed by it. Returns the user id and both fixtures for further use by the caller
/// (e.g. signing a device key, or building a user-signing key too).
async fn bootstrap_cross_signing(
    scenario: &mut Scenario,
    name: &str,
) -> (String, CrossSigningFixture, CrossSigningFixture) {
    scenario
        .register(name, name, "correct horse battery staple 42")
        .await
        .assert_ok();
    let user_id = scenario.session(name).unwrap().user_id.clone().unwrap();

    let master = make_cross_signing_key(&user_id, "master", "master");
    let ssk = make_cross_signing_key(&user_id, "self_signing", "ssk");
    let signed_ssk = signed_by(ssk.json.clone(), &user_id, &master.key_id, &master.keypair);

    scenario
        .send(
            Some(name),
            Method::POST,
            "/keys/device_signing/upload",
            Some(json!({
                "master_key": master.json,
                "self_signing_key": signed_ssk,
            })),
        )
        .await
        .assert_ok();

    (user_id, master, ssk)
}

/// Adds a genuinely-signed user-signing key to a user who already has a master key stored (as
/// [`bootstrap_cross_signing`] leaves them), via a second `/keys/device_signing/upload` call.
async fn add_user_signing_key(
    scenario: &mut Scenario,
    name: &str,
    user_id: &str,
    master: &CrossSigningFixture,
) -> CrossSigningFixture {
    let usk = make_cross_signing_key(user_id, "user_signing", "usk");
    let signed_usk = signed_by(usk.json.clone(), user_id, &master.key_id, &master.keypair);
    scenario
        .send(
            Some(name),
            Method::POST,
            "/keys/device_signing/upload",
            Some(obj1("user_signing_key", signed_usk)),
        )
        .await
        .assert_ok();
    usk
}

/// The unprefixed public-key value inside a cross-signing key fixture's `keys` map -- the form
/// `/keys/signatures/upload` addresses that key by at the top level.
fn unprefixed_key_id(fixture: &CrossSigningFixture) -> String {
    fixture
        .json
        .get("keys")
        .unwrap()
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

/// The master key alone (no self/user-signing key) verifies with no signature at all -- it is the
/// trust root, not signed by anything.
#[tokio::test]
async fn device_signing_upload_accepts_a_bare_master_key_with_no_signature() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    let alice_id = scenario.session("alice").unwrap().user_id.clone().unwrap();
    let master = make_cross_signing_key(&alice_id, "master", "master");

    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/device_signing/upload",
            Some(obj1("master_key", master.json)),
        )
        .await
        .assert_ok();
}

/// The full happy path: master key plus a self-signing key genuinely signed by it, both in the
/// same request, is accepted.
#[tokio::test]
async fn device_signing_upload_accepts_a_genuinely_signed_self_signing_key() {
    let mut scenario = Scenario::new(app());
    bootstrap_cross_signing(&mut scenario, "alice").await;
}

/// A self-signing key whose claimed master-key signature was actually produced by a different
/// key entirely must be rejected with `M_INVALID_SIGNATURE`, and must not be stored (a follow-up
/// bare upload of the same master key, with no self-signing key this time, must still succeed --
/// proving the first request's rejection did not partially apply).
#[tokio::test]
async fn device_signing_upload_rejects_a_self_signing_key_signed_by_the_wrong_key() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    let alice_id = scenario.session("alice").unwrap().user_id.clone().unwrap();

    let master = make_cross_signing_key(&alice_id, "master", "master");
    let impostor = SigningKeyPair::generate("impostor");
    let ssk = make_cross_signing_key(&alice_id, "self_signing", "ssk");
    // Signed by `impostor`, but claiming (via the key ID) to be signed by `master`.
    let bogus_ssk = signed_by(ssk.json.clone(), &alice_id, &master.key_id, &impostor);

    let response = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/device_signing/upload",
            Some(json!({
                "master_key": master.json,
                "self_signing_key": bogus_ssk,
            })),
        )
        .await;
    response.assert_matrix_error(StatusCode::BAD_REQUEST, "M_INVALID_SIGNATURE");
}

/// Uploading a self-signing key with no master key anywhere -- not in this request, not
/// previously stored -- is `M_MISSING_PARAM`.
#[tokio::test]
async fn device_signing_upload_rejects_self_signing_key_with_no_master_key_available() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    let alice_id = scenario.session("alice").unwrap().user_id.clone().unwrap();
    let master = make_cross_signing_key(&alice_id, "master", "master");
    let ssk = make_cross_signing_key(&alice_id, "self_signing", "ssk");
    let signed_ssk = signed_by(ssk.json.clone(), &alice_id, &master.key_id, &master.keypair);

    // Note: no `master_key` field at all, and none was ever uploaded before this.
    let response = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/device_signing/upload",
            Some(obj1("self_signing_key", signed_ssk)),
        )
        .await;
    response.assert_matrix_error(StatusCode::BAD_REQUEST, "M_MISSING_PARAM");
}

/// A self-signing key can be verified against a master key uploaded in an *earlier* request, per
/// `cross_signing.yaml`'s "or by the user's most recently uploaded master signing key if no
/// master signing key is included in the request".
#[tokio::test]
async fn device_signing_upload_verifies_against_a_previously_stored_master_key() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    let alice_id = scenario.session("alice").unwrap().user_id.clone().unwrap();
    let master = make_cross_signing_key(&alice_id, "master", "master");

    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/device_signing/upload",
            Some(obj1("master_key", master.json.clone())),
        )
        .await
        .assert_ok();

    let ssk = make_cross_signing_key(&alice_id, "self_signing", "ssk");
    let signed_ssk = signed_by(ssk.json.clone(), &alice_id, &master.key_id, &master.keypair);
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/device_signing/upload",
            // No `master_key` in this request -- must fall back to the one stored above.
            Some(obj1("self_signing_key", signed_ssk)),
        )
        .await
        .assert_ok();
}

/// `/keys/signatures/upload`: a device key can only be signed by its own user -- another user's
/// attempt to sign it is reported as a per-item `M_INVALID_SIGNATURE` failure (a 200 overall, per
/// the spec's response shape), not a request-level error.
#[tokio::test]
async fn signatures_upload_rejects_a_device_signed_by_a_different_user() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("bob", "bob", "hunter2official")
        .await
        .assert_ok();
    let alice_id = scenario.session("alice").unwrap().user_id.clone().unwrap();
    let alice_device = scenario
        .session("alice")
        .unwrap()
        .device_id
        .clone()
        .unwrap();

    let device_keys = json!({
        "user_id": alice_id,
        "device_id": alice_device,
        "algorithms": ["m.olm.v1.curve25519-aes-sha2"],
        "keys": { format!("ed25519:{alice_device}"): "aliceEd25519Key" },
        "signatures": {},
    });
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/upload",
            Some(obj1("device_keys", device_keys.clone())),
        )
        .await
        .assert_ok();

    // Bob has no legitimate way to sign alice's device, but tries anyway with a real keypair (so
    // this is testing the "wrong signer" rule, not just "bad signature").
    let forged_key = SigningKeyPair::generate("forged");
    let forged_sig = sign_canonical(&device_keys, &forged_key);
    let response = scenario
        .send(
            Some("bob"),
            Method::POST,
            "/keys/signatures/upload",
            Some(obj1(
                alice_id.clone(),
                obj1(
                    alice_device.clone(),
                    json!({
                        "user_id": alice_id,
                        "device_id": alice_device,
                        "algorithms": ["m.olm.v1.curve25519-aes-sha2"],
                        "keys": { format!("ed25519:{alice_device}"): "aliceEd25519Key" },
                        "signatures": { scenario.session("bob").unwrap().user_id.clone().unwrap(): { "ed25519:forged": forged_sig } },
                    }),
                ),
            )),
        )
        .await;
    response.assert_ok();
    let bob_id = scenario.session("bob").unwrap().user_id.clone().unwrap();
    let failure = &response.json["failures"][&alice_id][&alice_device];
    assert_eq!(
        failure["errcode"], "M_INVALID_SIGNATURE",
        "bob signing alice's device must be a per-item M_INVALID_SIGNATURE failure, not \
         silently merged: {:?}",
        response.json
    );
    let _ = bob_id; // only used to build the request above
}

/// `/keys/signatures/upload`: another user's master key can be signed, and merged, when the
/// signature resolves to the signer's *user-signing* key -- the one legitimate way to vouch for
/// someone else's identity in this model.
#[tokio::test]
async fn signatures_upload_accepts_another_users_master_key_signed_by_the_user_signing_key() {
    let mut scenario = Scenario::new(app());
    let (bob_id, bob_master, _bob_ssk) = bootstrap_cross_signing(&mut scenario, "bob").await;
    let (alice_id, alice_master, _alice_ssk) =
        bootstrap_cross_signing(&mut scenario, "alice").await;

    // Give alice a user-signing key too, genuinely signed by her master key.
    let usk = add_user_signing_key(&mut scenario, "alice", &alice_id, &alice_master).await;

    // Alice signs bob's master key with her (real) user-signing key.
    let bob_master_key_id = unprefixed_key_id(&bob_master);
    let signed_bob_master = signed_by(
        bob_master.json.clone(),
        &alice_id,
        &usk.key_id,
        &usk.keypair,
    );

    let response = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/signatures/upload",
            Some(obj1(
                bob_id.clone(),
                obj1(bob_master_key_id, signed_bob_master),
            )),
        )
        .await;
    response.assert_ok();
    assert!(
        response.json["failures"].as_object().unwrap().is_empty(),
        "a signature by alice's real user-signing key over bob's master key must succeed: {:?}",
        response.json
    );
}

/// The same scenario, but alice signs bob's master key with her *self-signing* key instead of her
/// user-signing key -- structurally a real signature by a real key of alice's, but not the one
/// key that is allowed to vouch for another user's identity, so it must still be rejected.
///
/// This is the mutation-style check the review asked for: it starts from the accepting case above
/// and flips exactly one thing (which of alice's own keys signs bob's master key) to confirm the
/// verifier's asymmetric treatment of "signing another user's master key" is actually enforced,
/// not just "some signature by some real key of the signer's" as `verify_submitted_entries_by_signer`
/// would (correctly) accept for every *other* target in this file.
#[tokio::test]
async fn signatures_upload_rejects_another_users_master_key_signed_by_the_wrong_key_type() {
    let mut scenario = Scenario::new(app());
    let (bob_id, bob_master, _bob_ssk) = bootstrap_cross_signing(&mut scenario, "bob").await;
    let (alice_id, alice_master, alice_ssk) = bootstrap_cross_signing(&mut scenario, "alice").await;
    // Alice genuinely has a user-signing key too -- this proves the rejection below is about
    // *which* of alice's real keys signed bob's master key, not merely that she lacks one.
    let _alice_usk = add_user_signing_key(&mut scenario, "alice", &alice_id, &alice_master).await;

    let bob_master_key_id = unprefixed_key_id(&bob_master);
    // Alice's self-signing key, not her user-signing key, signs bob's master key.
    let signed_bob_master = signed_by(
        bob_master.json.clone(),
        &alice_id,
        &alice_ssk.key_id,
        &alice_ssk.keypair,
    );

    let response = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/signatures/upload",
            Some(obj1(bob_id, obj1(bob_master_key_id, signed_bob_master))),
        )
        .await;
    response.assert_ok();
    let failure = &response.json["failures"];
    let failure = failure.as_object().unwrap();
    assert!(
        !failure.is_empty(),
        "a self-signing-key signature over another user's master key must be rejected \
         (only a user-signing key may make this claim): {:?}",
        response.json
    );
    for by_key in failure.values() {
        for entry in by_key.as_object().unwrap().values() {
            assert_eq!(entry["errcode"], "M_INVALID_SIGNATURE");
        }
    }
}
