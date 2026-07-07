//! Whole-store persistent snapshot format: a single ε-serde-serializable
//! [`StoreSnapshot`] capturing every graph's compacted [`GraphRing`] (via
//! [`RingSnapshot`]).
//!
//! ## Design
//!
//! - **One file, whole-store.**  Rather than one file per graph (which would
//!   need a manifest to enumerate them), a single
//!   snapshot captures all graphs as parallel `graph_ids: Vec<u8>` /
//!   `rings: Vec<RingSnapshot>` vectors (avoiding any need for `HashMap` or
//!   tuple ε-serde support, neither of which epserde provides directly).
//!
//! - **Dictionary persistence (see `dict_snapshot.rs`).**  The
//!   `RingSnapshot`s embed raw `u64` term IDs assigned by the `Dictionary` at
//!   compaction time. A full-WAL-replay model needs no separate persistence,
//!   because replaying every record from byte 0 reconstructs a
//!   byte-identical `Dictionary` deterministically. Tail-only WAL replay
//!   changes this: `open()` replays only the **tail** WAL
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
//!
//! - **"Always mapped".**  [`StoreSnapshot::round_trip_and_maybe_save`] is
//!   used by `RingStore::compact()` (and `bulk_load()`) to replace the
//!   freshly-built `Arc<GraphRing>` map with the result of serializing it
//!   and immediately deserializing it back — so the servable representation
//!   is always literally "what ε-serde deserialized", matching what will
//!   later be loaded from disk. Since ε-serde's on-disk byte format
//!   is identical for both loading strategies (`deserialize_full`'s heap copy
//!   or `load_mmap`'s zero-copy mapping), the loading strategy can be
//!   changed later with **zero** file-format migration — this is exactly
//!   what Phase 3 of the mmap'd ε-serde snapshot plan (CLAUDE.md item 14)
//!   does. This round-trip happens in
//!   `compact()`/`bulk_load()`, never on the query hot path
//!   (`quads_for_pattern`/`join_scan`), so it cannot affect query latency.
//!
//! - **Snapshot writing is purely additive.**  `compact()` continues to
//!   clear the delta and swap in the new `Ring` exactly as before; writing
//!   the snapshot file (for persistent stores) and doing the in-memory
//!   round-trip (for all stores) are both side effects layered on top with
//!   no behavioural change to `RingStore`'s query or write semantics.
//!
//! ## Phase 1 of the mmap'd ε-serde snapshot plan (CLAUDE.md item 14)
//!
//! This file's `nova.snapshot.<gen>` on-disk format is **uncompressed**
//! (no zstd) — a deliberate, documented reversal of the earlier zstd-on-
//! snapshot optimisation. This reintroduces an on-disk size regression
//! (roughly 2.3 MiB -> 43-45 MiB on the disk benchmark dataset), but it is
//! a prerequisite for Phase 3 ("map after compact"): mmap-based zero-copy
//! loading is only possible against a file whose bytes are byte-identical
//! to the in-memory ε-serde layout, which zstd compression would break.
//! Still roughly 9x smaller than Oxigraph's 416.5 MiB RocksDB directory on
//! the same dataset. The dictionary file (`nova.dict.<gen>`, see
//! `dict_snapshot.rs`) is unaffected by this — it keeps its zstd
//! compression until Phase 4. In-memory mode (`path: None`) is also
//! unaffected, since `round_trip_and_maybe_save`'s file-writing branch is
//! skipped entirely when `path` is `None`.

use crate::ring::{GraphRing, GraphRingHandle, MappedGraphRing, RingSnapshot};
use epserde::Epserde;
use epserde::deser::{Deserialize, Flags};
use epserde::ser::Serialize;
use oxigraph_nova_core::{GraphId, Oxigraph};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;


/// Whole-store ε-serde-serializable snapshot: one [`RingSnapshot`] per graph,
/// keyed by the parallel `graph_ids` vector (index-aligned with `rings`).
///
/// Generic over `Rings` (default `Vec<RingSnapshot>`) so that a future
/// mmap'd load can substitute ε-serde's borrowed `DeserType<Vec<RingSnapshot>>`
/// form here with **zero extra code** — mirrors the "bare generic parameter
/// with a default" pattern used for `CltjSnapshot`'s `Tries = [LoudsCore; 6]`
/// (Phase 3.3c probe, CLAUDE.md item 14).
#[derive(Epserde)]
pub(crate) struct StoreSnapshot<Rings = Vec<RingSnapshot>> {
    pub(crate) graph_ids: Vec<u8>,
    pub(crate) rings: Rings,
}


impl StoreSnapshot {
    /// Build a snapshot from a per-graph `Arc<GraphRing>` map, consuming each
    /// `Arc` (requires unique ownership — true for a freshly-built `graphs`
    /// map that has not yet been installed into `RingStoreInner`/shared).
    ///
    /// `pub(crate)` (rather than private) so that `ring.rs`'s
    /// `MappedGraphRing` tests can drive the exact same snapshot-construction
    /// path used in production instead of hand-rolling a parallel struct.
    pub(crate) fn from_graphs(graphs: HashMap<GraphId, Arc<GraphRing>>) -> Self {
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

        // Phase 1 (see module docs above): write the raw ε-serde bytes
        // directly, with NO zstd compression.
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
    /// per-graph `Arc<GraphRing>` map. Returns an empty map if `path`
    /// doesn't exist (fresh store, or a persistent store that has never
    /// been compacted).
    ///
    /// Retained (alongside `load_mmap_from_file` below, which is what
    /// `RingStore::open`/`commit_compaction` actually use now) as a
    /// full-heap-copy alternative and for its existing regression test
    /// coverage.
    pub(crate) fn load_from_file(
        path: &Path,
    ) -> Result<HashMap<GraphId, Arc<GraphRing>>, Oxigraph> {
        if !path.exists() {
            return Ok(HashMap::new());
        }
        // The file on disk is raw, uncompressed ε-serde bytes (see Phase 1
        // module docs above) — read it directly and deserialize via
        // `deserialize_full`.
        let buf = std::fs::read(path)
            .map_err(|e| Oxigraph::Storage(format!("snapshot read failed: {e}")))?;
        let mut cursor = std::io::Cursor::new(&buf[..]);
        let snap = unsafe {
            StoreSnapshot::deserialize_full(&mut cursor)
                .map_err(|e| Oxigraph::Storage(format!("snapshot load failed: {e}")))?
        };
        Ok(snap.into_graphs())
    }

    /// Load a `StoreSnapshot` from `path` via zero-copy `load_mmap` (rather
    /// than `deserialize_full`'s full heap copy) and wrap each graph in a
    /// [`GraphRingHandle::Mapped`]. Returns an empty map if `path` doesn't
    /// exist (fresh store, or a persistent store that has never been
    /// compacted).
    ///
    /// Used by `RingStore::open()` (so a reopened persistent store's rings
    /// are zero-copy mapped from the moment they're loaded, not just after
    /// the next `compact()`) and by [`Self::write_and_load_mmap`] (right
    /// after writing a fresh snapshot generation during `commit_compaction`).
    pub(crate) fn load_mmap_from_file(
        path: &Path,
    ) -> Result<HashMap<GraphId, GraphRingHandle>, Oxigraph> {
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let mem = Arc::new(unsafe {
            StoreSnapshot::load_mmap(path, Flags::empty())
                .map_err(|e| Oxigraph::Storage(format!("snapshot load_mmap failed: {e}")))?
        });
        let graph_ids: Vec<u8> = mem.uncase().graph_ids.iter().copied().collect();
        let mut out = HashMap::with_capacity(graph_ids.len());
        for (i, g) in graph_ids.into_iter().enumerate() {
            let mapped = MappedGraphRing::new(Arc::clone(&mem), i);
            out.insert(GraphId(g), GraphRingHandle::Mapped(Arc::new(mapped)));
        }
        Ok(out)
    }

    /// Consume `graphs`, serializing it once, atomically writing the bytes
    /// to `path` (write to a `.tmp` sibling, then rename), then `load_mmap`
    /// the just-written file back in — so the servable representation is a
    /// genuine zero-copy view of exactly the bytes on disk, never a
    /// redundant owned heap copy. Used by `commit_compaction`'s
    /// persistent-store branch (see this module's doc comment, "Phase 3").
    pub(crate) fn write_and_load_mmap(
        graphs: HashMap<GraphId, Arc<GraphRing>>,
        path: &Path,
    ) -> Result<HashMap<GraphId, GraphRingHandle>, Oxigraph> {
        let snap = Self::from_graphs(graphs);
        let mut buf: Vec<u8> = Vec::new();
        unsafe {
            snap.serialize(&mut buf)
                .map_err(|e| Oxigraph::Storage(format!("snapshot serialize failed: {e}")))?;
        }

        let tmp_path = {
            let mut s = path.as_os_str().to_os_string();
            s.push(".tmp");
            std::path::PathBuf::from(s)
        };
        std::fs::write(&tmp_path, &buf)
            .map_err(|e| Oxigraph::Storage(format!("snapshot write failed: {e}")))?;
        std::fs::rename(&tmp_path, path)
            .map_err(|e| Oxigraph::Storage(format!("snapshot rename failed: {e}")))?;

        Self::load_mmap_from_file(path)
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

    /// Phase 3.3c step 1 (probe): real `load_mmap` round-trip of a whole
    /// [`StoreSnapshot`] (not just a single [`crate::cltj::CltjSnapshot`] as
    /// `cltj_snapshot_load_mmap_probe` in `cltj.rs` already proved) to a temp
    /// file, confirming that the generic-with-defaults threading added to
    /// [`StoreSnapshot`]/[`RingSnapshot`] this session actually produces a
    /// navigable, zero-copy `DeserType` view all the way down through
    /// `StoreSnapshot -> RingSnapshot -> CltjSnapshot`, with vocab slices and
    /// tries still directly inspectable/borrowed at the innermost layer.
    ///
    /// This is deliberately scoped to *inspecting* the borrowed view (not yet
    /// constructing a navigable `GraphRing<BorrowedLouds, VocabRepr>` from
    /// it) — that reconstruction is genuinely new code (an `Option A`/`Option
    /// D` concern for Step 2/3 of the mmap'd ε-serde snapshot plan, CLAUDE.md
    /// item 14), whereas this probe's job is only to de-risk the type-level
    /// plumbing added in this step.
    #[test]
    fn store_snapshot_load_mmap_probe() {
        use epserde::deser::Flags;

        let mut graphs: HashMap<GraphId, Arc<GraphRing>> = HashMap::new();
        graphs.insert(
            GRAPH_DEFAULT,
            Arc::new(build_ring(&[[1, 2, 3], [1, 2, 4], [5, 6, 7]])),
        );
        graphs.insert(GraphId(2), Arc::new(build_ring(&[[10, 20, 30]])));

        let snap = StoreSnapshot::from_graphs(graphs);

        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("nova_store_mmap_probe_{pid}_{n}.snap"));
        let _ = std::fs::remove_file(&path);

        {
            let mut f = std::fs::File::create(&path).expect("create temp file");
            unsafe {
                snap.serialize(&mut f).expect("serialize StoreSnapshot");
            }
        }

        // Zero-copy mmap load: `mem_case` owns the mmap'd backing memory;
        // `.uncase()` yields a borrowed `DeserType<StoreSnapshot>` tied to
        // `mem_case`'s lifetime.
        let mem_case = unsafe {
            <StoreSnapshot>::load_mmap(&path, Flags::empty()).expect("load_mmap StoreSnapshot")
        };
        let view: &epserde::deser::DeserType<StoreSnapshot> = mem_case.uncase();

        assert_eq!(view.graph_ids.len(), 2);
        assert_eq!(view.rings.len(), 2);

        // Every non-empty graph's `RingSnapshot.cltj` should still expose a
        // navigable-looking `CltjSnapshot` view (borrowed vocab slices, 6
        // tries) even through this extra `StoreSnapshot`/`RingSnapshot`
        // wrapping — proving the generic threading survives the additional
        // nesting introduced this step. `cltj` is a bare `CltjSnapshot` (not
        // `Option`-wrapped — see `RingSnapshot`'s doc comment); `n == 0` is
        // the empty-graph sentinel, so only check vocab non-emptiness for
        // non-empty graphs (an empty graph's `cltj` is the valid-but-empty
        // placeholder from `CltjSnapshot::empty()`).
        for ring in view.rings.iter() {
            if ring.n > 0 {
                let cltj = &ring.cltj;
                assert!(!cltj.vocab_s.is_empty());
                assert!(!cltj.vocab_p.is_empty());
                assert!(!cltj.vocab_o.is_empty());
                assert_eq!(cltj.tries.len(), 6);
            }
        }

        let _ = std::fs::remove_file(&path);
    }
}

