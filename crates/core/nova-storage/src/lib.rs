//! `oxigraph-nova-storage` — generic, backend-agnostic
//! persistence machinery shared by any `QuadStore` implementation that wants
//! WAL + MANIFEST + `Dictionary` durability.
//!
//! ## Why this crate exists
//!
//! `nova-engine-ring`'s WAL framing/replay engine, MANIFEST commit
//! protocol, and `Dictionary`-persistence format have no dependency on the
//! LOUDS/CLTJ/Ring index structures themselves — they operate purely on
//! RDF quads/terms and abstract snapshot-generation/WAL-segment numbers.
//! Pulling them out into their own crate means a hypothetical alternative
//! `QuadStore` backend (e.g. a RocksDB-backed one) can reuse this exact
//! crash-safe durability machinery without reimplementing it or depending
//! on `nova-engine-ring`'s succinct-trie-specific code.
//!
//! ### Components
//!
//! | Module | Purpose |
//! |---|---|
//! | [`wal`] | Write-ahead log: crash-safe framing/replay engine + RDF quad/term byte encoders |
//! | [`manifest`] | The crash-safe commit point tying a snapshot generation to a WAL segment |
//! | [`dict_snapshot`] | Persistence for `oxigraph_nova_core::Dictionary` (term/graph interning state) |
//! | [`dict_lz4`] | lz4_flex block-container codec for `nova.dict.<gen>` |
//!
//! ### What's deliberately NOT here
//!
//! The actual index/snapshot format (e.g. `nova-engine-ring`'s ε-serde
//! `StoreSnapshot`/`RingSnapshot`, or a RocksDB backend's native SST files)
//! is backend-specific and stays in that backend's own crate. This crate
//! only provides the generic surrounding machinery: durable intent logging,
//! crash-safe generation/segment bookkeeping, and dictionary persistence.

pub mod dict_lz4;
pub mod dict_snapshot;
pub mod manifest;
pub mod wal;

pub use dict_lz4::{DEFAULT_BLOCK_SIZE as DICT_LZ4_DEFAULT_BLOCK_SIZE, MAGIC as DICT_LZ4_MAGIC};
pub use manifest::{MANIFEST_FILE_NAME, Manifest};
pub use wal::{WalRecord, WalWriter};
