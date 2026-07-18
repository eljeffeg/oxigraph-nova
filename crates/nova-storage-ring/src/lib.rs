//! `oxigraph-nova-storage-ring` — storage backends for Oxigraph Nova.
//!
//! ## Crate split
//!
//! Production **six-order LOUDS** [`LoudsStore`] / CompactLTJ / delta / snapshot
//! lives in [`oxigraph_nova_storage_louds`]. This crate:
//!
//! 1. **Re-exports** that production surface so existing
//!    `oxigraph_nova_storage_ring::LoudsStore` (and related) imports stay green
//!    without a mass rewrite of dependents.
//! 2. Owns the **cyclic QWT Ring pilot**: `NOVARNG1` + D2 braided intersection
//!    under feature `cyclic-ring-pilot`, including [`cyclic_ring::RingStore`].
//!    Not the default SPARQL backend (`nova-store` still pins `LoudsStore`).
//!
//! ## Naming
//!
//! | Name | Meaning |
//! |------|---------|
//! | `LoudsStore` | Production six-order LOUDS CompactLTJ store |
//! | `RingStore` (`cyclic_ring`) | Cyclic QWT pilot store (feature-gated) |
//! | "braided" | Algorithm adjective for D2 multi-range intersection — not a product name |
//!
//! Long-term, dependents should import LOUDS from `oxigraph-nova-storage-louds`
//! and keep this crate for the Ring pilot only.

// Temporary compatibility: re-export the full production LOUDS surface
// (LoudsStore, LoudsTrie, SortOrder, wal, modules cltj/delta/louds/ring/store, …).
pub use oxigraph_nova_storage_louds::*;

/// Cyclic QWT Ring pilot: `NOVARNG1` + D2 braided intersection + `RingStore`.
/// Feature `cyclic-ring-pilot`. Not on the default `LoudsStore` SPARQL path.
#[cfg(feature = "cyclic-ring-pilot")]
pub mod cyclic_ring;
