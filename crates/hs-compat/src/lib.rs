//! `hs-compat`: Synapse compatibility.
//!
//! Owned by track 13 (`docs/workstreams/13-config-compat-and-migration.md`).
//! Covers the `homeserver.yaml` translator ([`translate`]), the
//! shared-secret registration protocol ([`shared_secret`]), a first slice of the
//! `/_synapse/admin` surface forwarded onto the native admin API ([`admin_proxy`]), and (see
//! `docs/compat/*` for the parts that are analysis rather than code) the rest of the Synapse
//! admin-API surface and the online importer.

pub mod admin_proxy;
pub mod classification;
pub mod report;
pub mod shared_secret;
pub mod translate;

pub use admin_proxy::{AdminProxyState, router as admin_proxy_router};
pub use classification::{Classification, KeyInfo};
pub use report::{KeyOutcome, TranslationReport};
pub use translate::{TranslateError, TranslateOptions, translate};
