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
//! - [`routes`]: the authenticated `client/v1/media` handlers, the legacy `media/v3` handlers,
//!   and the `federation/v1/media` handlers other servers fetch this server's media through.
//! - [`router`]: wires [`routes`] into an `hs_http::router::Builder`.
//! - [`multipart`]: the `multipart/mixed` shape of MSC3916 federation media responses: the
//!   parser for what another server answers, the builder for what this one answers.
//! - [`remote`]: another server's media -- fetched over federation (with the legacy and redirect
//!   forms), held in the same store as local media, and counted. Read its module doc for the order
//!   a fetch tries things in and what the cache does with quarantine and purges.
//! - [`synapse_layout`]: a read-only adapter over Synapse's on-disk media-directory layout, for
//!   track 13's importer.
//! - [`preview`]: `GET .../preview_url` — OpenGraph extraction, the SSRF guard, and the response
//!   cache. Read this module's doc first: it states exactly what the SSRF guard does and does not
//!   defend against, per this crate's convention for a security control.
//! - [`retention`]: `media.remote_media_retention`, the hourly sweeper that deletes cached
//!   copies of other servers' media nobody asked for in that long.
//! - [`admin_source`]: the admin API's Media area (`hs_admin::media::MediaSource`) over the
//!   repository: listing, quarantine, protection, deletion.

#![warn(missing_docs)]

pub mod admin_source;
pub mod error;
pub mod id;
pub mod metadata;
pub mod multipart;
pub mod policy;
pub mod preview;
pub mod remote;
pub mod repository;
pub mod retention;
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
pub mod usage;

pub use error::MediaError;
pub use id::MediaId;
pub use repository::MediaRepository;
pub use state::MediaState;
