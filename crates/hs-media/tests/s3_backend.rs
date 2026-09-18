//! Exercises the S3-compatible `object_store` backend (`hs_media::store::build` with
//! `MediaStorageBackend::S3`) against a *real* S3-compatible endpoint (MinIO, R2, actual S3, ...),
//! configured entirely through environment variables. Skips cleanly (prints why, exits `Ok`)
//! whenever those variables are not set, since this workspace's shared CI/dev environment cannot
//! assume network access to an S3-compatible service is available — `docs/workstreams/09-media.md`'s
//! day-one work item asks for exactly this shape ("skip S3 tests cleanly when no credentials are
//! present").
//!
//! To actually run this test, e.g. against a local MinIO:
//! ```text
//! docker run -p 9000:9000 -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
//!     minio/minio server /data
//! HS_MEDIA_TEST_S3_BUCKET=test HS_MEDIA_TEST_S3_ENDPOINT=http://localhost:9000 \
//! HS_MEDIA_TEST_S3_ACCESS_KEY_ID=minioadmin HS_MEDIA_TEST_S3_SECRET_ACCESS_KEY=minioadmin \
//!     cargo test -p hs-media --test s3_backend -- --ignored --nocapture
//! ```
//! (bucket must already exist; this test does not create it). Not `--ignored` by default so it is
//! still collected and its skip message is visible in a normal `cargo test -p hs-media` run.

use hs_config::media::MediaStorageBackend;
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

#[tokio::test]
async fn put_get_round_trip_against_a_real_s3_compatible_endpoint() {
    let (Some(bucket), Some(endpoint), Some(access_key_id), Some(secret_access_key)) = (
        env("HS_MEDIA_TEST_S3_BUCKET"),
        env("HS_MEDIA_TEST_S3_ENDPOINT"),
        env("HS_MEDIA_TEST_S3_ACCESS_KEY_ID"),
        env("HS_MEDIA_TEST_S3_SECRET_ACCESS_KEY"),
    ) else {
        eprintln!(
            "skipping: set HS_MEDIA_TEST_S3_BUCKET, HS_MEDIA_TEST_S3_ENDPOINT, \
             HS_MEDIA_TEST_S3_ACCESS_KEY_ID and HS_MEDIA_TEST_S3_SECRET_ACCESS_KEY to run this \
             test against a real S3-compatible endpoint (see this file's module doc)"
        );
        return;
    };

    let config = MediaStorageBackend::S3 {
        bucket,
        region: env("HS_MEDIA_TEST_S3_REGION"),
        endpoint: Some(endpoint),
        access_key_id: Some(access_key_id),
        secret_access_key: secret_access_key.into(),
        secret_access_key_file: None,
    };
    let store = hs_media::store::build(&config).expect("building the S3 object store");

    let key = ObjectPath::from(format!("hs-media-test/{}", hs_media::MediaId::generate()));
    let payload = b"hs-media S3 backend integration test".to_vec();

    store
        .put(&key, payload.clone().into())
        .await
        .expect("PUT to the S3-compatible endpoint");

    let fetched = store
        .get(&key)
        .await
        .expect("GET from the S3-compatible endpoint")
        .bytes()
        .await
        .expect("reading the GET body");
    assert_eq!(fetched.as_ref(), payload.as_slice());

    let range = store
        .get_range(&key, 0..10)
        .await
        .expect("ranged GET from the S3-compatible endpoint");
    assert_eq!(range.as_ref(), &payload[0..10]);

    store
        .delete(&key)
        .await
        .expect("DELETE from the S3-compatible endpoint");
}
