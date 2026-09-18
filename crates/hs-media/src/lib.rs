//! `hs-media`: the media repository. Object storage first, authenticated media as the primary
//! path, Synapse-compatible behavior at the edges.
//!
//! Owned by track 09 (`docs/workstreams/09-media.md`). See `PLAN.md` section 4 (D6) and 8.1 item
//! 4 for this crate's place in the workspace.
//!
//! # Layout
//!
//! - [`security`]: the rules that keep decoders and the browser from turning stored media into
//!   code execution — inline-vs-attachment `Content-Disposition`, the `Content-Security-Policy`
//!   sent on every media response, `X-Content-Type-Options`, filename sanitization, and HTTP
//!   range parsing. Read this module first; it is the normative statement of what this crate
//!   promises never to do.
//! - [`id`]: media ID generation and shape validation (24 random characters, Synapse's alphabet).
//! - [`store`]: the object-store backend selection (`object_store`: local filesystem, S3).
//! - [`metadata`]: media and thumbnail metadata over `hs-tables`.
//! - [`policy`]: pluggable per-user and per-server upload limits.
//! - [`sniff`]: decode-time content verification (does the file actually look like the format the
//!   uploader claimed?) and the decompression-bomb defenses (dimension and allocation limits).
//! - [`thumbnail`]: crop/scale thumbnail generation, the default size table, animated-image and
//!   dynamic-thumbnail handling.
//! - [`repository`]: ties storage, metadata, policy and thumbnailing together into the upload,
//!   download and thumbnail operations the HTTP layer calls.
//! - [`state`]: the axum shared state (`MediaState`) and the `Requester` extractor bridge.
//! - [`routes`]: the authenticated `client/v1/media` handlers and the legacy `media/v3` handlers.
//! - [`router`]: wires [`routes`] into an `hs_http::router::Builder`.
//! - [`multipart`]: a `multipart/mixed` parser for MSC3916 federation media responses (used by
//!   track 06's federation client once it exists; see `docs/rfcs/0007-federation-media.md`).
//! - [`synapse_layout`]: a read-only adapter over Synapse's on-disk media-directory layout, for
//!   track 13's importer.

#![warn(missing_docs)]

pub mod error;
pub mod id;
pub mod metadata;
pub mod multipart;
pub mod policy;
pub mod repository;
pub mod router;
pub mod routes;
pub mod scanning;
pub mod security;
pub mod sniff;
pub mod state;
pub mod store;
pub mod synapse_layout;
#[cfg(feature = "test-fixtures")]
pub mod test_fixtures;
#[cfg(test)]
mod test_support;
pub mod thumbnail;

pub use error::MediaError;
pub use id::MediaId;
pub use repository::MediaRepository;
pub use state::MediaState;
