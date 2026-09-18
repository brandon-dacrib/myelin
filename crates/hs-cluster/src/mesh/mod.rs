//! The internal mesh: HTTP/2 over `hyper`, a pluggable [`auth::Authenticator`] (mutual TLS or a
//! shared secret), the forwarding [`envelope::Envelope`], and [`forwarder::Forwarder`] /
//! [`server::MeshServer`] for the client and server sides. See
//! `docs/rfcs/0001-cluster-ownership.md` sections 8, 9 and 11.

pub mod auth;
pub mod envelope;
pub mod forwarder;
pub mod idempotency;
pub mod server;
pub mod tls;

pub use auth::{
    AuthMode, Authenticator, MutualTlsAuthenticator, PeerIdentity, SharedSecretAuthenticator,
    TlsPeerInfo,
};
pub use envelope::{Envelope, IdempotencyKey, Reply, RequesterContext, ShardHandler};
pub use forwarder::Forwarder;
pub use idempotency::IdempotencyCache;
pub use server::{MeshDeps, MeshServer};
