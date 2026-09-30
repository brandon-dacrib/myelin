//! `PostgresBackend`'s `ssl_mode` against real servers: one with `ssl = on` and a self-signed
//! certificate, one without TLS at all. Needs both, and prints `SKIP` otherwise.
//!
//! - `HS_KV_TEST_POSTGRES_TLS_DSN`: a server with `ssl = on`.
//! - `HS_KV_TEST_POSTGRES_TLS_CERT`: the PEM of that server's certificate (its own trust anchor
//!   when self-signed). The certificate should name `localhost` and not the address the DSN
//!   uses, so `verify-full` can be seen to fail on the name while `verify-ca` passes.
//! - `HS_KV_TEST_POSTGRES_DSN`: a server with no TLS (the conformance suite's; defaults as
//!   there).
//!
//! Start the TLS server with something like:
//!
//! ```sh
//! openssl req -x509 -newkey rsa:2048 -nodes -keyout server.key -out server.crt -days 30 \
//!     -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost"
//! docker run -d --name hs-kv-pg-tls -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5463:5432 \
//!     -v $PWD/server.crt:/certs/server.crt:ro -v $PWD/server.key:/certs/server.key:ro \
//!     --entrypoint bash public.ecr.aws/docker/library/postgres:17 -c '
//!       install -o postgres -g postgres -m 600 /certs/server.key /var/lib/postgresql/server.key &&
//!       install -o postgres -g postgres -m 644 /certs/server.crt /var/lib/postgresql/server.crt &&
//!       exec docker-entrypoint.sh postgres -c ssl=on \
//!         -c ssl_cert_file=/var/lib/postgresql/server.crt \
//!         -c ssl_key_file=/var/lib/postgresql/server.key'
//! HS_KV_TEST_POSTGRES_TLS_DSN=postgres://postgres:hspg@127.0.0.1:5463/postgres \
//! HS_KV_TEST_POSTGRES_TLS_CERT=$PWD/server.crt \
//! HS_KV_TEST_POSTGRES_DSN=postgres://postgres:hspg@127.0.0.1:5462/postgres \
//!     cargo test -p hs-kv --test postgres_tls
//! ```

use std::path::PathBuf;

use hs_kv::KvBackend as _;
use hs_kv::postgres_backend::{PostgresBackend, PostgresOpenOptions};
use hs_kv::postgres_tls::{PgSslMode, PgTlsError, PgTlsOptions};

struct Servers {
    tls_dsn: String,
    cert: PathBuf,
    plain_dsn: String,
}

fn servers() -> Option<Servers> {
    let (Ok(tls_dsn), Ok(cert)) = (
        std::env::var("HS_KV_TEST_POSTGRES_TLS_DSN"),
        std::env::var("HS_KV_TEST_POSTGRES_TLS_CERT"),
    ) else {
        eprintln!(
            "SKIP: postgres_tls needs HS_KV_TEST_POSTGRES_TLS_DSN and HS_KV_TEST_POSTGRES_TLS_CERT \
             (see the test file's docs)"
        );
        return None;
    };
    let plain_dsn = std::env::var("HS_KV_TEST_POSTGRES_DSN")
        .unwrap_or_else(|_| "postgres://postgres:hskvtest@localhost:5433/postgres".to_owned());
    Some(Servers {
        tls_dsn,
        cert: PathBuf::from(cert),
        plain_dsn,
    })
}

fn options(mode: PgSslMode, root_cert: Option<PathBuf>) -> PostgresOpenOptions {
    PostgresOpenOptions {
        schema: format!(
            "hs_kv_tls_{}_{}",
            std::process::id(),
            mode.as_str().replace('-', "_")
        ),
        pool_size: 2,
        tls: PgTlsOptions { mode, root_cert },
    }
}

/// Opens, does one round trip through a keyspace, drops the schema, and says whether the session
/// was encrypted.
fn round_trip(dsn: &str, options: &PostgresOpenOptions) -> Result<bool, hs_kv::KvError> {
    let (backend, info) = PostgresBackend::open_with_info(dsn, options)?;
    let ks = backend.keyspace("tls_probe")?;
    let mut txn = backend.begin()?;
    {
        use hs_kv::KvWrite as _;
        txn.put(&ks, b"k", b"v")?;
    }
    assert!(backend.commit(txn)?.is_ok());
    let snap = backend.snapshot();
    {
        use hs_kv::KvRead as _;
        assert_eq!(snap.get(&ks, b"k")?.as_deref(), Some(&b"v"[..]));
    }
    drop(snap);
    backend.drop_schema_for_test()?;
    Ok(info.encrypted)
}

fn tls_error(err: &hs_kv::KvError) -> Option<&PgTlsError> {
    std::error::Error::source(err).and_then(|source| source.downcast_ref::<PgTlsError>())
}

/// A hostname that resolves to the same server as `dsn`'s address but is not what the
/// certificate names, or the reverse: `dsn` with its host swapped for `host`.
fn with_host(dsn: &str, host: &str) -> String {
    let mut config: postgres::Config = dsn.parse().expect("a postgres:// DSN");
    config.host(host);
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let user = config.get_user().unwrap_or("postgres");
    let password = String::from_utf8_lossy(config.get_password().unwrap_or_default()).into_owned();
    let db = config.get_dbname().unwrap_or("postgres");
    format!("postgres://{user}:{password}@{host}:{port}/{db}")
}

#[test]
fn every_mode_against_a_tls_server_and_a_plain_one() {
    let Some(servers) = servers() else {
        return;
    };
    let cert = Some(servers.cert.clone());

    // The TLS server: disable is in the clear, everything else is encrypted.
    assert!(!round_trip(&servers.tls_dsn, &options(PgSslMode::Disable, None)).unwrap());
    assert!(round_trip(&servers.tls_dsn, &options(PgSslMode::Prefer, None)).unwrap());
    assert!(round_trip(&servers.tls_dsn, &options(PgSslMode::Require, None)).unwrap());
    assert!(
        round_trip(
            &servers.tls_dsn,
            &options(PgSslMode::VerifyCa, cert.clone())
        )
        .unwrap()
    );

    // verify-full: the certificate names `localhost`, so it verifies by that name and not by
    // the IP address literal.
    let by_name = with_host(&servers.tls_dsn, "localhost");
    assert!(round_trip(&by_name, &options(PgSslMode::VerifyFull, cert.clone())).unwrap());
    let by_ip = with_host(&servers.tls_dsn, "127.0.0.1");
    let err = round_trip(&by_ip, &options(PgSslMode::VerifyFull, cert.clone())).unwrap_err();
    let tls = tls_error(&err).unwrap_or_else(|| panic!("expected a TLS error, got {err}"));
    assert_eq!(tls.mode, PgSslMode::VerifyFull);
    assert!(err.to_string().contains("ssl_mode = verify-full"), "{err}");
    // ... while verify-ca does not care about the name.
    assert!(round_trip(&by_ip, &options(PgSslMode::VerifyCa, cert.clone())).unwrap());

    // The verify modes against a certificate that is not in the trust store: a self-signed cert
    // with the platform's roots (no file) is rejected.
    let err = round_trip(&servers.tls_dsn, &options(PgSslMode::VerifyCa, None)).unwrap_err();
    let tls = tls_error(&err).unwrap_or_else(|| panic!("expected a TLS error, got {err}"));
    assert_eq!(tls.mode, PgSslMode::VerifyCa);

    // The plain server: disable and prefer connect in the clear; the rest refuse, naming the
    // mode and the reason.
    assert!(!round_trip(&servers.plain_dsn, &options(PgSslMode::Disable, None)).unwrap());
    assert!(!round_trip(&servers.plain_dsn, &options(PgSslMode::Prefer, None)).unwrap());
    for mode in [
        PgSslMode::Require,
        PgSslMode::VerifyCa,
        PgSslMode::VerifyFull,
    ] {
        let err = round_trip(&servers.plain_dsn, &options(mode, cert.clone())).unwrap_err();
        let tls =
            tls_error(&err).unwrap_or_else(|| panic!("{mode}: expected a TLS error, got {err}"));
        assert_eq!(tls.mode, mode);
        assert!(
            tls.detail.contains("server does not support TLS"),
            "{mode}: {err}"
        );
    }
}
