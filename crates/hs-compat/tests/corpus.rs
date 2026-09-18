//! Runs the translator over the fixture corpus in `crates/hs-compat/testdata/`
//! (Docker generator output, an Ansible-role style file, a Helm-chart
//! style file, a NixOS-module style file, a minimal file, and a
//! kitchen-sink file exercising every key classified `Mapped` or
//! `Mapped (diff)` that can coexist in one valid file — see that file's
//! header comment for the handful that can't and are covered by unit
//! tests in `src/translate.rs` instead).
//!
//! Paths are relative to the crate root, matching `cargo test`'s working
//! directory.

use hs_compat::translate::{TranslateOptions, translate};
use hs_config::listeners::ListenerResource;
use hs_config::storage::StorageConfig;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("testdata/{name}"))
        .unwrap_or_else(|e| panic!("reading testdata/{name}: {e}"))
}

#[test]
fn minimal_translates_with_no_override_needed() {
    let (config, report) = translate(&fixture("minimal.yaml"), TranslateOptions::default())
        .expect("minimal.yaml should translate cleanly");
    assert_eq!(config.server.server_name, "minimal.example.org");
    assert_eq!(
        config.auth.registration_shared_secret.as_str(),
        Some("minimal-shared-secret")
    );
    assert!(!report.has_blocking());
}

#[test]
fn minimal_fails_without_override_is_not_the_case() {
    // Sanity check that "no unsupported keys" really does mean the default
    // (fail-closed) options succeed, not just the permissive ones.
    let result = translate(
        &fixture("minimal.yaml"),
        TranslateOptions {
            allow_unsupported: false,
        },
    );
    assert!(result.is_ok());
}

#[test]
fn docker_generator_output_needs_the_override_and_then_translates() {
    let yaml = fixture("docker.yaml");
    let blocked = translate(&yaml, TranslateOptions::default());
    assert!(
        blocked.is_err(),
        "docker.yaml sets pid_file/form_secret/trusted_key_servers, all unsupported"
    );

    let (config, report) = translate(
        &yaml,
        TranslateOptions {
            allow_unsupported: true,
        },
    )
    .unwrap();
    assert_eq!(config.server.server_name, "docker.example.org");
    assert_eq!(
        config.auth.registration_shared_secret.as_str(),
        Some("generated-docker-shared-secret")
    );
    assert_eq!(
        config.auth.session_secret.as_str(),
        Some("generated-docker-macaroon-key")
    );
    // sqlite3 has no native backend; falls back to the embedded default
    // rather than erroring.
    assert!(matches!(config.storage, StorageConfig::Embedded(_)));
    assert!(report.outcomes.iter().any(|o| o.key == "pid_file"));
    assert!(report.outcomes.iter().any(|o| o.key == "form_secret"));
    assert!(
        report
            .outcomes
            .iter()
            .any(|o| o.key == "trusted_key_servers")
    );
}

#[test]
fn ansible_style_output_translates_rate_limits_and_database() {
    let yaml = fixture("ansible.yaml");
    let (config, report) = translate(
        &yaml,
        TranslateOptions {
            allow_unsupported: true,
        },
    )
    .unwrap();
    assert_eq!(config.server.server_name, "ansible.example.org");
    assert_eq!(config.rate_limits.login.per_second, 0.17);
    assert_eq!(config.rate_limits.login.burst_count, 3);
    match &config.storage {
        StorageConfig::Postgres(p) => assert_eq!(p.host, "matrix-postgres"),
        other => panic!("expected Postgres, got {other:?}"),
    }
    assert!(!config.auth.enable_registration);
    // retention/turn_uris/user_directory/allowed_local_3pids are unsupported.
    assert!(report.blocking().count() >= 4);
}

#[test]
fn helm_style_output_translates_mas_delegation_and_s3_media() {
    let yaml = fixture("helm.yaml");
    let (config, report) = translate(
        &yaml,
        TranslateOptions {
            allow_unsupported: true,
        },
    )
    .unwrap();
    let mas = config
        .auth
        .mas_delegation
        .expect("matrix_authentication_service.enabled: true");
    assert_eq!(mas.endpoint, "http://mas.matrix.svc.cluster.local:8080");
    assert_eq!(
        mas.shared_secret.as_str(),
        Some("helm-generated-mas-shared-secret")
    );
    // media_storage_providers (S3) is a documented no-op: media_store_path
    // still drives the (unused-by-S3-in-practice) local fallback path, but
    // the important thing is translation does not error on the block.
    assert!(
        report
            .outcomes
            .iter()
            .any(|o| o.key == "media_storage_providers")
    );
    // redis/instance_map/federation_metrics_domains are unsupported.
    assert!(report.outcomes.iter().any(|o| o.key == "redis"));
    assert!(report.outcomes.iter().any(|o| o.key == "instance_map"));
    assert!(report.has_blocking());
}

#[test]
fn nixos_style_output_translates_oidc_and_federation_policy() {
    let yaml = fixture("nixos.yaml");
    let (config, report) = translate(
        &yaml,
        TranslateOptions {
            allow_unsupported: true,
        },
    )
    .unwrap();
    assert_eq!(config.auth.oidc_providers.len(), 1);
    assert_eq!(config.auth.oidc_providers[0].idp_id, "keycloak");
    assert_eq!(
        config.federation.domain_allowlist.as_deref(),
        Some(&["trusted-partner.example.org".to_string()][..])
    );
    assert!(report.outcomes.iter().any(|o| o.key == "turn_uris"));
    assert!(
        report
            .outcomes
            .iter()
            .any(|o| o.key == "enable_registration_without_verification")
    );
}

#[test]
fn kitchen_sink_translates_every_mapped_key_with_no_override() {
    let yaml = fixture("kitchen-sink.yaml");
    let (config, report) = translate(&yaml, TranslateOptions::default())
        .expect("kitchen-sink.yaml sets only Mapped/Mapped(diff) keys");
    assert!(
        !report.has_blocking(),
        "kitchen sink must not contain unsupported keys: {:#?}",
        report.outcomes
    );

    // Spot-check across every section touched.
    assert_eq!(config.server.server_name, "kitchen-sink.example.org");
    assert_eq!(
        config.server.public_baseurl.as_deref(),
        Some("https://ks.example.org/")
    );
    assert_eq!(
        config.server.admin_contact.as_deref(),
        Some("mailto:admin@example.org")
    );
    assert!(config.server.report_stats);
    assert_eq!(
        config.server.signing_key_path.to_str(),
        Some("/etc/hs/signing.key")
    );

    assert_eq!(config.listeners.listeners.len(), 2);
    let main = &config.listeners.listeners[0];
    assert_eq!(main.port, 8448);
    assert!(
        main.tls.is_some(),
        "tls: true with global cert/key paths set"
    );
    // enable_media_repo: false stripped `media` from every listener.
    assert!(!main.resources.contains(&ListenerResource::Media));
    assert!(main.resources.contains(&ListenerResource::Client));
    assert!(main.resources.contains(&ListenerResource::Federation));

    match &config.storage {
        StorageConfig::Postgres(p) => {
            assert_eq!(p.host, "db.internal");
            assert_eq!(p.pool_size, 15);
        }
        other => panic!("expected Postgres, got {other:?}"),
    }

    assert_eq!(
        config.federation.domain_allowlist.as_deref(),
        Some(&["a.example.org".to_string(), "b.example.org".to_string()][..])
    );
    assert!(!config.federation.verify_certificates);
    assert!(config.federation.allow_public_rooms_over_federation);
    assert!(config.federation.allow_device_name_lookup_over_federation);
    assert_eq!(
        config.federation.client_timeout,
        hs_config::Duration::from_secs(45)
    );
    assert_eq!(
        config.federation.max_retry_backoff,
        hs_config::Duration::from_secs(90)
    );

    assert_eq!(config.rate_limits.message.burst_count, 12);
    assert_eq!(config.rate_limits.login.per_second, 0.4);
    assert_eq!(config.rate_limits.joins_local.burst_count, 20);
    assert_eq!(config.rate_limits.joins_remote.per_second, 0.02);
    assert_eq!(config.rate_limits.third_party_id_validation.burst_count, 8);
    assert_eq!(config.rate_limits.federation.burst_count, 80);

    assert_eq!(config.media.max_upload_size, hs_config::ByteSize::mib(75));
    assert_eq!(config.media.thumbnail_sizes.len(), 2);
    assert!(config.media.url_preview_enabled);
    // Not asserting the final url_preview_ip_range_blocklist value: both
    // `ip_range_blacklist` and `url_preview_ip_range_blacklist` write it,
    // and only the report (checked below) is order-independent.
    assert!(
        !config.media.allow_legacy_unauthenticated_media,
        "enable_authenticated_media: true inverts to false"
    );
    assert_eq!(
        config.media.remote_media_retention,
        Some(hs_config::Duration::from_days(14))
    );

    assert!(config.auth.enable_registration);
    assert_eq!(
        config.auth.registration_shared_secret.as_str(),
        Some("registration-shared-secret-from-file")
    );
    assert_eq!(
        config.auth.session_secret.as_str(),
        Some("macaroon-secret-from-file")
    );
    assert_eq!(
        config.auth.refresh_token_lifetime,
        Some(hs_config::Duration::from_days(365))
    );
    assert_eq!(
        config.auth.access_token_lifetime,
        hs_config::Duration::from_hours(12)
    );
    assert!(config.auth.password.enabled);
    assert_eq!(config.auth.password.policy.minimum_length, 10);
    assert!(config.auth.password.policy.require_symbol);
    assert_eq!(config.auth.oidc_providers.len(), 1);
    assert_eq!(config.auth.oidc_providers[0].idp_id, "kitchen-idp");
    assert_eq!(config.auth.oidc_providers[0].scopes.len(), 3);

    assert_eq!(config.appservices.registration_files.len(), 2);

    assert!(config.telemetry.metrics.enabled);
    assert_eq!(
        config.telemetry.sentry.as_ref().unwrap().dsn.as_str(),
        Some("https://examplekey@sentry.example.org/1")
    );
    // opentracing: {enabled: true} is a documented no-op (see translate.rs);
    // Synapse's Jaeger-agent config has no OTLP endpoint to translate to.

    // Every key in the file appears in the report exactly once, and none
    // of them are unsupported/unrecognized.
    let source: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
    let top_level_keys = source
        .as_mapping()
        .unwrap()
        .keys()
        .filter_map(|k| k.as_str())
        .count();
    assert_eq!(report.outcomes.len(), top_level_keys);
}
