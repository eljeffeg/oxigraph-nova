//! `oxigraph-nova-storage-ring` — Ring index storage backend for Oxigraph Nova.
//!
//! ## Architecture
//!
//! This crate provides the **Ring index** — a compact, ordered trie over integer
//! term IDs — combined with an **LSM delta** for live writes and a **`RingStore`**
//! that wires both into the `QuadStore` trait.
//!
//! ### Components
//!
//! | Module | Purpose |
//! |---|---|
//! | [`ring`] | `GraphRing` (6 LOUDS tries), `RingBuilder`, `CltjTrieIter` |
//! | [`delta`] | `Delta` — `BTreeMap<u128, bool>` absorbing live writes |
//! | [`store`] | `RingStore` — `QuadStore` impl wiring Ring + Dictionary + delta |
//!
//! ### Memory layout (per triple)
//!
//! | Tier | Bytes/triple |
//! |---|---|
//! | `MemoryStore` | ~200+ (heap-allocated Term strings) |
//! | `RingStore` delta only | 16 bytes (u128 BTreeMap key) |
//! | `RingStore` after `compact()` | ~12 bytes (6 LOUDS tries @ ⌈log₂ σ⌉ bits/label) |
//!
//! ### Workflow
//!
//! 1. Create a `RingStore::new()`.
//! 2. Insert quads via `QuadStore::insert()` — they land in the delta BTreeMap.
//! 3. Call `RingStore::compact()` to build/rebuild the Ring from delta + existing Ring.
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
//! `RingStore` implements the same `QuadStore` trait as `MemoryStore`. Swapping backends
//! requires only changing the concrete type at construction — the evaluator and HTTP server
//! are completely unaffected.

pub mod cltj;
pub mod delta;
pub mod louds;
pub mod ring;
mod snapshot;
pub mod store;

// Generic WAL/MANIFEST/dict-persistence machinery now lives in
// `oxigraph-nova-storage-common`, reusable by any `QuadStore` backend.
// Re-exported here so existing callers (`nova_serve.rs`, tests, benches)
// that referred to `oxigraph_nova_storage_ring::wal`/`WalRecord`/`WalWriter`
// keep working unchanged.
pub use louds::LoudsMemBreakdown;
pub use oxigraph_nova_storage_common::{WalRecord, WalWriter, wal};
pub use ring::SortOrder;
pub use store::{MemoryBreakdown, PerOrderingBreakdown, RingStore, SyncPolicy};
