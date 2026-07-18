//! `oxigraph-nova-storage-ring` — storage backends for Oxigraph Nova.
//!
//! ## Crate split
//!
//! Production **six-order LOUDS** `RingStore` / CompactLTJ / delta / snapshot
//! now lives in [`oxigraph_nova_storage_louds`]. This crate:
//!
//! 1. **Re-exports** that production surface so existing
//!    `oxigraph_nova_storage_ring::RingStore` (and related) imports stay green
//!    without a mass rewrite of dependents.
//! 2. Owns the **Braided Ring pilot**: cyclic QWT + `NOVARNG1` + D2 intersection
//!    under feature `cyclic-ring-pilot`. Not on the default `RingStore` SPARQL path.
//!
//! Long-term, dependents should migrate to `oxigraph-nova-storage-louds` for
//! production LOUDS and keep this crate for Braided Ring only.

// Temporary compatibility: re-export the full production LOUDS surface
// (RingStore, LoudsTrie, SortOrder, wal, modules cltj/delta/louds/ring/store, …).
pub use oxigraph_nova_storage_louds::*;

/// Braided Ring pilot: cyclic QWT + `NOVARNG1` + D2 intersection.
/// Feature `cyclic-ring-pilot`. Not on the default `RingStore` query path.
#[cfg(feature = "cyclic-ring-pilot")]
pub mod cyclic_ring;
