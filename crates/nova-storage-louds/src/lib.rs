//! `oxigraph-nova-storage-louds` — production six-order LOUDS LoudsStore for Oxigraph Nova.
//!
//! ## Architecture
//!
//! This crate provides the **Ring index** — a compact, ordered trie over integer
//! term IDs — combined with an **LSM delta** for live writes and a **`LoudsStore`**
//! that wires both into the `QuadStore` trait.
//!
//! ### Components
//!
//! | Module | Purpose |
//! |---|---|
//! | [`ring`] | `GraphRing` (6 LOUDS tries), `RingBuilder`, `CltjTrieIter` |
//! | [`delta`] | `Delta` — `BTreeMap<u128, bool>` absorbing live writes |
//! | [`store`] | `LoudsStore` — `QuadStore` impl wiring Ring + Dictionary + delta |
//! | [`louds`] | CompactLTJ LOUDS height-3 trie substrate |
//! | [`cltj`] | Six-order LOUDS-trie iterators and data structures |
//!
//! ### Memory layout (per triple)
//!
//! | Tier | Bytes/triple |
//! |---|---|
//! | `MemoryStore` | ~200+ (heap-allocated Term strings) |
//! | `LoudsStore` delta only | 16 bytes (u128 BTreeMap key) |
//! | `LoudsStore` after `compact()` | ~12 bytes (6 LOUDS tries @ ⌈log₂ σ⌉ bits/label) |
//!
//! ### Workflow
//!
//! 1. Create a `LoudsStore::new()`.
//! 2. Insert quads via `QuadStore::insert()` — they land in the delta BTreeMap.
//! 3. Call `LoudsStore::compact()` to build/rebuild the Ring from delta + existing Ring.
//!    After compaction, the delta is cleared and all triples live in the Ring's sorted arrays.
//! 4. Queries via `QuadStore::quads_for_pattern()` merge Ring + delta (always correct,
//!    whether or not `compact()` has been called).
//!
//! A background thread can call `compact()` when the delta crosses a threshold
//! (1M triples by default), with an atomic `Arc::swap` so queries see no downtime
//! during rebuilds.
//!
//! ### Backend compatibility
//!
//! `LoudsStore` implements the same `QuadStore` trait as `MemoryStore`. Swapping backends
//! requires only changing the concrete type at construction — the evaluator and HTTP server
//! are completely unaffected.
//!
//! ### Crate split note
//!
//! Production LOUDS lives here. The Braided Ring pilot (cyclic QWT / `NOVARNG1` / D2)
//! lives in `oxigraph-nova-storage-ring`, which currently re-exports this crate so
//! existing `oxigraph_nova_storage_ring::LoudsStore` imports stay green.

pub mod cltj;
pub mod delta;
#[cfg(feature = "fulltext")]
mod fulltext;
pub mod louds;
pub mod ring;
mod snapshot;
pub mod store;

// Generic WAL/MANIFEST/dict-persistence machinery now lives in
// `oxigraph-nova-storage-common`, reusable by any `QuadStore` backend.
// Re-exported here so existing callers (`nova_serve.rs`, tests, benches)
// that referred to `oxigraph_nova_storage_ring::wal`/`WalRecord`/`WalWriter`
// keep working unchanged via the ring crate's re-export of this crate.
pub use louds::{LoudsMemBreakdown, LoudsTrie, build_louds_from_sorted};
pub use oxigraph_nova_storage_common::{WalRecord, WalWriter, wal};
pub use ring::SortOrder;
pub use store::{
    MemoryBreakdown, PerOrderingBreakdown, LoudsStore, SyncPolicy, compaction_count,
    compaction_duration_seconds_total,
};
