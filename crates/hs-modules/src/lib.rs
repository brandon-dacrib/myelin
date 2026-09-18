//! hs-modules: the extension system that replaces Synapse's Python modules.
//!
//! - [`hooks`]: the [`hooks::ModuleHooks`] trait every track calls, covering Synapse's eleven
//!   callback categories (see `docs/workstreams/15-admin-api-and-modules.md`).
//! - [`noop`]: [`noop::NoopHooks`] (a module that does nothing) and [`noop::ModuleChain`]
//!   (composes several modules with Synapse-like veto semantics).
//! - [`callback`]: the versioned JSON HTTP-callback wire protocol.
//! - [`client`]: [`client::HttpCallbackClient`], the reference implementation of
//!   [`hooks::ModuleHooks`] over that protocol.
//!
//! The `wasmtime` component-model host (`docs/workstreams/15-admin-api-and-modules.md`'s Phase 1
//! deliverable) is deliberately not built here yet: see
//! `docs/design/wasmtime-feasibility.md` for the feasibility verdict and why (a `wasmtime` build
//! was not attempted on this shared machine).

pub mod callback;
pub mod client;
pub mod hooks;
pub mod noop;

pub use hooks::ModuleHooks;
pub use noop::{ModuleChain, NoopHooks};
