//! Oxigraph-compatible RocksDB backend for Oxigraph Nova.
//!
//! Wraps upstream [`oxigraph::store::Store`] so Nova can open an existing
//! Oxigraph on-disk data directory and serve it through Nova's SPARQL stack
//! (`nova-query` evaluator + product surfaces).
//!
//! Registered as `"rocksdb"` via [`inventory`] / [`BackendFactory`].
//! Enable with product feature `rocksdb-backend`.
//!
//! ## Compatibility
//!
//! - **On-disk format:** identical to Oxigraph 0.5.x RocksDB stores (`oxversion` 2).
//! - **Drop-in path:** stop `oxigraph serve --location D`, start
//!   `nova_serve --backend rocksdb --location D`.
//! - **Query engine:** Nova's evaluator (not Oxigraph's nested-loop engine).
//! - **LFTJ:** not accelerated (`LftjSource` defaults); pattern scans use
//!   Oxigraph's multi-index RocksDB iterators.

#![deny(unsafe_code)]

mod store;

pub use store::RocksDbStore;

/// Keep this crate (and its `inventory::submit!` [`BackendFactory`](oxigraph_nova_core::BackendFactory))
/// linked into the final binary.
///
/// Product library crates (`nova-store`, `nova-server`, `nova-mcp`) call this
/// once under `#[cfg(feature = "rocksdb-backend")]`. Binaries then inherit
/// registration transitively — no per-binary `use … as _` force-links.
#[inline(never)]
pub fn ensure_registered() {
    // No runtime work: inventory registration is link-time. Referencing a
    // public item from this crate keeps the object file (and submit!) alive.
    let _ = std::any::type_name::<RocksDbStore>();
}
