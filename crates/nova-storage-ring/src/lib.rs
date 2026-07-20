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
//!    under feature `cyclic-ring-pilot`, including [`RingStore`].
//!    Not the default SPARQL backend (`nova-store` still pins `LoudsStore`).
//!
//! ## Naming
//!
//! | Name | Meaning |
//! |------|---------|
//! | `LoudsStore` | Production six-order LOUDS CompactLTJ store |
//! | `RingStore` | Cyclic QWT pilot store (feature-gated) |
//! | "braided" | Algorithm adjective for D2 multi-range intersection — not a product name |
//!
//! Pilot modules live at the crate root (no nested `cyclic_ring/` directory).
//! A thin [`cyclic_ring`] compatibility module re-exports the historical path.
//!
//! Long-term, dependents should import LOUDS from `oxigraph-nova-storage-louds`
//! and keep this crate for the Ring pilot only.

// Temporary compatibility: re-export the full production LOUDS surface
// (LoudsStore, LoudsTrie, SortOrder, wal, modules cltj/delta/louds/ring/store, …).
// Note: LOUDS already owns public modules `ring` and `store`; pilot modules use
// distinct names (`cyclic`, `ring_store`) to avoid collisions under the glob.
pub use oxigraph_nova_storage_louds::*;

// ── Cyclic QWT Ring pilot (`cyclic-ring-pilot`) ───────────────────────────────

/// Heap Ring A primitives (`CyclicRing`, `Col`, `RowRange`, counters).
#[cfg(feature = "cyclic-ring-pilot")]
pub mod cyclic;

/// E5.10 W0 — immutable mapped QWT (`NOVAQWT1`).
#[cfg(feature = "cyclic-ring-pilot")]
pub mod mapped_qwt;
/// E5.10 W1 — mmap-backed Ring A shell (`NOVARNG1` / `MappedRingA`).
#[cfg(feature = "cyclic-ring-pilot")]
pub mod mapped_ring;
/// Phase 4 ID-level facade + differential oracles (not QuadStore / not SPARQL).
#[cfg(feature = "cyclic-ring-pilot")]
pub mod facade;
/// Unified heap/mmap navigation view (Phase 1A single residency).
#[cfg(feature = "cyclic-ring-pilot")]
pub mod ring_nav;
/// Phase 4b ID-level LFTJ join/scan seam (`TrieIterator`, not QuadStore).
#[cfg(feature = "cyclic-ring-pilot")]
pub mod scan;
/// Phase 4b per-graph read-only canonical-ID image adapter.
#[cfg(feature = "cyclic-ring-pilot")]
pub mod image;
/// Phase 5 in-memory QuadStore: Dictionary + Delta + BraidedGraphImage.
#[cfg(feature = "cyclic-ring-pilot")]
pub mod ring_store;
/// E5.11 → SPARQL product wire: env flags + path counters (W0).
#[cfg(feature = "cyclic-ring-pilot")]
pub mod product_path;
/// E5.9B Phase 2/3 — column-local Huffman for C_p (`HuffColP`) +
/// [`PredicateColumn`] substrate on in-memory [`CyclicRing::c_p`].
/// Enabled by product `ring-backend` (Phase 1D default). Research builds may
/// enable `ring-huffman-cp` alone. Without the feature, C_p is plain QWT256
/// (`*_qwt_cp` builders / `ring-backend-qwt`). See e5.9b-qwt-substrate-matrix.md.

#[cfg(feature = "ring-huffman-cp")]
pub mod huff_cp;
#[cfg(feature = "ring-huffman-cp")]
pub use huff_cp::{HuffCpCounterSnapshot, HuffCpCounters, HuffColP};
/// E5.9B Phase 4 — mapped Huffman C_p section (`HQWA`) for NOVARNG1 v2.
#[cfg(feature = "ring-huffman-cp")]
pub mod mapped_hqwt;
#[cfg(feature = "ring-huffman-cp")]
pub use mapped_hqwt::{HQWA_MAGIC, HotHuffColumn, MappedHqwtSection, RNG_FLAG_HUFF_CP, build_hqwa_section};



#[cfg(feature = "cyclic-ring-pilot")]
pub use facade::BraidedRingIndex;
#[cfg(feature = "cyclic-ring-pilot")]
pub use image::{BraidedGraphImage, IdRemap};
#[cfg(feature = "cyclic-ring-pilot")]
pub use mapped_ring::{
    MappedColDistinctIter, MappedRingA, MappedRingError, open_novarng1_mmap, parse_header, write_novarng1_file, write_novarng1_v1,
};
#[cfg(feature = "cyclic-ring-pilot")]
pub use product_path::{SPARQL_PATH, SparqlPathCounters, SparqlPathSnapshot, ring_keep_heap, ring_mmap_enabled};
#[cfg(feature = "cyclic-ring-pilot")]
pub use cyclic::{
    Col, CounterSnapshot, CyclicRangeDistinctIter, CyclicRing, GlobalCounters, PredicateColumn,
    RingMemBreakdown, RowRange,
};
#[cfg(feature = "cyclic-ring-pilot")]
pub use ring_nav::RingRef;
#[cfg(all(feature = "cyclic-ring-pilot", any(test, feature = "diagnostics")))]
pub use cyclic::{Orientation, OrientationCounters, URing};
#[cfg(feature = "cyclic-ring-pilot")]
pub use scan::BraidedJoinScan;
#[cfg(feature = "cyclic-ring-pilot")]
pub use ring_store::RingStore;

/// Compatibility alias: historical `oxigraph_nova_storage_ring::cyclic_ring::*` path.
/// Prefer crate-root imports (`RingStore`, `mapped_qwt`, `cyclic`, …).
#[cfg(feature = "cyclic-ring-pilot")]
pub mod cyclic_ring {
    #[allow(unused_imports)]
    pub use crate::cyclic::*;

    pub use crate::facade;
    pub use crate::image;
    pub use crate::mapped_qwt;
    pub use crate::mapped_ring;
    pub use crate::product_path;
    pub use crate::ring_store as store;
    pub use crate::scan;
    pub use crate::{
        BraidedGraphImage, BraidedJoinScan, BraidedRingIndex, Col, CounterSnapshot,
        CyclicRangeDistinctIter, CyclicRing, GlobalCounters, IdRemap, MappedColDistinctIter, MappedRingA, MappedRingError,
        PredicateColumn, RingStore, RowRange, SPARQL_PATH, SparqlPathCounters, SparqlPathSnapshot,
        open_novarng1_mmap, parse_header, write_novarng1_file, write_novarng1_v1,
    };
    #[cfg(any(test, feature = "diagnostics"))]
    pub use crate::{Orientation, OrientationCounters, URing};
}
