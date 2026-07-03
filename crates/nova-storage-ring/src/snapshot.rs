//! Whole-store persistent snapshot format (item 1b in `CLAUDE.md`'s "What's
//! Next"): a single ε-serde-serializable [`StoreSnapshot`] capturing every
//! graph's compacted [`GraphRing`] (via [`RingSnapshot`]).
//!
//! ## Design
//!
//! - **One file, whole-store.**  Rather than one file per graph (which would
//!   need a manifest to enumerate them — deferred to item 1c), a single
//!   snapshot captures all graphs as parallel `graph_ids: Vec<u8>` /
//!   `rings: Vec<RingSnapshot>` vectors (avoiding any need for `HashMap` or
//!   tuple ε-serde support, neither of which epserde provides directly).
//!
//! - **Dictionary persistence (see `dict_snapshot.rs`).**  The
//!   `RingSnapshot`s embed raw `u64` term IDs assigned by the `Dictionary` at
//!   compaction time. Under item 1b's original full-WAL-replay model this
//!   needed no separate persistence, because replaying every record from
//!   byte 0 reconstructs a byte-identical `Dictionary` deterministically.
//!   Item 1c changed this: `open()` now replays only the **tail** WAL
//!   segment(s) on top of a loaded snapshot generation (see `manifest.rs`),
//!   so a fresh, empty `Dictionary` used during replay would reassign
//!   `TermId`s starting from `0`, colliding with the ID space the loaded
//!   `RingSnapshot`s already assume. The fix is `dict_snapshot.rs`: the
//!   `Dictionary`'s state (every term, in `TermId` order, plus the
//!   `GraphId ↔ GraphName` mapping) is persisted alongside every snapshot
//!   generation (`nova.dict.<gen>`) and reconstructed via
//!   `Dictionary::rebuild` *before* the WAL tail is replayed, so replay's
//!   `intern()` calls only ever append new terms after the snapshot's
//!   high-water-mark.

//! - **"Always mapped".**  [`StoreSnapshot::round_trip_and_maybe_save`] is
//!   used by `RingStore::compact()` (and `bulk_load()`) to replace the
//!   freshly-built `Arc<GraphRing>` map with the result of serializing it
//!   and immediately deserializing it back — so the servable representation
//!   is always literally "what ε-serde deserialized", matching what will
//!   later be loaded from disk (Option A: `deserialize_full`) or eventually
//!   mmap'd (Option B: `deserialize_eps`/`mmap`, deferred — see `CLAUDE.md`).
//!   Since ε-serde's on-disk byte format is identical for both loading
//!   strategies, Option B can be added later with **zero** file-format
//!   migration.  This round-trip happens in `compact()`/`bulk_load()`, never
//!   on the query hot path (`quads_for_pattern`/`join_scan`), so it cannot
//!   affect query latency.
//!
//! - **Snapshot writing is purely additive.**  `compact()` continues to
//!   clear the delta and swap in the new `Ring` exactly as before; writing
//!   the snapshot file (for persistent stores) and doing the in-memory
//!   round-trip (for all stores) are both side effects layered on top with
//!   no behavioural change to `RingStore`'s query or write semantics.

use crate::ring::{GraphRing, RingSnapshot};
use epserde::Epserde;
use epserde::deser::Deserialize;
use epserde::ser::Serialize;
use oxigraph_nova_core::{GraphId, Oxigraph};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Whole-store ε-serde-serializable snapshot: one [`RingSnapshot`] per graph,
/// keyed by the parallel `graph_ids` vector (index-aligned with `rings`).
#[derive(Epserde)]
pub(crate) struct StoreSnapshot {
    graph_ids: Vec<u8>,
    rings: Vec<RingSnapshot>,
}

impl StoreSnapshot {
    /// Build a snapshot from a per-graph `Arc<GraphRing>` map, consuming each
    /// `Arc` (requires unique ownership — true for a freshly-built `graphs`
    /// map that has not yet been installed into `RingStoreInner`/shared).
    fn from_graphs(graphs: HashMap<GraphId, Arc<GraphRing>>) -> Self {
        let mut graph_ids = Vec::with_capacity(graphs.len());
        let mut rings = Vec::with_capacity(graphs.len());
        for (g_id, ring) in graphs {
            let ring = Arc::try_unwrap(ring)
                .unwrap_or_else(|_| panic!("StoreSnapshot::from_graphs: GraphRing Arc is shared"));
            graph_ids.push(g_id.as_u8());
            rings.push(ring.into_snapshot());
        }
        StoreSnapshot { graph_ids, rings }
    }

    /// Reconstruct the per-graph `Arc<GraphRing>` map from a loaded snapshot.
    fn into_graphs(self) -> HashMap<GraphId, Arc<GraphRing>> {
        self.graph_ids
            .into_iter()
            .zip(self.rings)
            .map(|(g, snap)| (GraphId(g), Arc::new(GraphRing::from_snapshot(snap))))
            .collect()
    }

    /// Consume `graphs`, serializing it once into an in-memory buffer, then:
    ///
    /// 1. If `path` is `Some`, atomically write the serialized bytes to that
    ///    file (write to a `.tmp` sibling, then rename — so a crash mid-write
    ///    never leaves a half-written snapshot file on disk).
    /// 2. Deserialize the same buffer back into a fresh `Arc<GraphRing>` map
    ///    ("always mapped" — see module docs) and return it.
    ///
    /// Used by `RingStore::compact()`/`bulk_load()` right after building a
    /// fresh `new_graphs` map.
    pub(crate) fn round_trip_and_maybe_save(
        graphs: HashMap<GraphId, Arc<GraphRing>>,
        path: Option<&Path>,
    ) -> Result<HashMap<GraphId, Arc<GraphRing>>, Oxigraph> {
        let snap = Self::from_graphs(graphs);
        let mut buf: Vec<u8> = Vec::new();
        unsafe {
            snap.serialize(&mut buf)
                .map_err(|e| Oxigraph::Storage(format!("snapshot serialize failed: {e}")))?;
        }

        if let Some(path) = path {
            let tmp_path = {
                let mut s = path.as_os_str().to_os_string();
                s.push(".tmp");
                std::path::PathBuf::from(s)
            };
            std::fs::write(&tmp_path, &buf)
                .map_err(|e| Oxigraph::Storage(format!("snapshot write failed: {e}")))?;
            std::fs::rename(&tmp_path, path)
                .map_err(|e| Oxigraph::Storage(format!("snapshot rename failed: {e}")))?;
        }

        let mut cursor = std::io::Cursor::new(&buf[..]);
        let snap2 = unsafe {
            StoreSnapshot::deserialize_full(&mut cursor)
                .map_err(|e| Oxigraph::Storage(format!("snapshot deserialize failed: {e}")))?
        };
        Ok(snap2.into_graphs())
    }

    /// Load a `StoreSnapshot` from `path` (if it exists) and reconstruct the
    /// per-graph `Arc<GraphRing>` map.  Returns an empty map if `path`
    /// doesn't exist (fresh store, or a persistent store that has never
    /// been compacted).
    pub(crate) fn load_from_file(
        path: &Path,
    ) -> Result<HashMap<GraphId, Arc<GraphRing>>, Oxigraph> {
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let snap = unsafe {
            StoreSnapshot::load_full(path)
                .map_err(|e| Oxigraph::Storage(format!("snapshot load failed: {e}")))?
        };
        Ok(snap.into_graphs())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::RingBuilder;
    use oxigraph_nova_core::GRAPH_DEFAULT;

    fn build_ring(triples: &[[u64; 3]]) -> GraphRing {
        let mut b = RingBuilder::new();
        for &[s, p, o] in triples {
            b.add(s, p, o);
        }
        b.build()
    }

    #[test]
    fn round_trip_in_memory_preserves_contents() {
        let mut graphs: HashMap<GraphId, Arc<GraphRing>> = HashMap::new();
        graphs.insert(
            GRAPH_DEFAULT,
            Arc::new(build_ring(&[[1, 2, 3], [1, 2, 4], [5, 6, 7]])),
        );
        graphs.insert(GraphId(2), Arc::new(build_ring(&[[10, 20, 30]])));

        let round_tripped = StoreSnapshot::round_trip_and_maybe_save(graphs, None).unwrap();

        assert_eq!(round_tripped.len(), 2);
        let dg = &round_tripped[&GRAPH_DEFAULT];
        assert_eq!(dg.n, 3);
        assert!(dg.contains(1, 2, 3));
        assert!(dg.contains(1, 2, 4));
        assert!(dg.contains(5, 6, 7));
        assert!(!dg.contains(1, 2, 5));

        let g2 = &round_tripped[&GraphId(2)];
        assert_eq!(g2.n, 1);
        assert!(g2.contains(10, 20, 30));
    }

    #[test]
    fn round_trip_handles_empty_graph() {
        let mut graphs: HashMap<GraphId, Arc<GraphRing>> = HashMap::new();
        graphs.insert(GRAPH_DEFAULT, Arc::new(build_ring(&[])));

        let round_tripped = StoreSnapshot::round_trip_and_maybe_save(graphs, None).unwrap();
        let dg = &round_tripped[&GRAPH_DEFAULT];
        assert_eq!(dg.n, 0);
        assert!(dg.match_triples(None, None, None).is_empty());
    }

    #[test]
    fn save_and_load_from_file() {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("nova_snapshot_test_{pid}_{n}.snap"));
        let _ = std::fs::remove_file(&path);

        let mut graphs: HashMap<GraphId, Arc<GraphRing>> = HashMap::new();
        graphs.insert(GRAPH_DEFAULT, Arc::new(build_ring(&[[1, 2, 3], [4, 5, 6]])));

        let _ = StoreSnapshot::round_trip_and_maybe_save(graphs, Some(&path)).unwrap();
        assert!(path.exists());

        let loaded = StoreSnapshot::load_from_file(&path).unwrap();
        let dg = &loaded[&GRAPH_DEFAULT];
        assert_eq!(dg.n, 2);
        assert!(dg.contains(1, 2, 3));
        assert!(dg.contains(4, 5, 6));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_missing_file_is_empty() {
        let path = std::env::temp_dir().join("nova_snapshot_test_definitely_missing.snap");
        let _ = std::fs::remove_file(&path);
        let loaded = StoreSnapshot::load_from_file(&path).unwrap();
        assert!(loaded.is_empty());
    }
}
