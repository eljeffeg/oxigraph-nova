//! `RingStore` — `QuadStore` implementation backed by the Ring index + LSM delta.
//!
//! ## Concurrency model
//!
//! A single `Mutex<RingStoreInner>` serialises all reads and writes.  This is
//! intentionally simple — the POC goal is correctness and memory efficiency, not
//! maximum write throughput.  The mutex is held only while working with in-memory
//! data structures (dictionary lookups, delta writes, Ring scans), and released
//! before returning results.  For the W3C test suite datasets (< 10 K triples each)
//! this is completely fine.
//!
//! Automatic compaction (see `maybe_auto_compact`) runs **inline, under the
//! same lock**, when the delta crosses `auto_compact_threshold` for a
//! persistent store. This is simple and correct but means a large delta can
//! stall all other readers/writers for the duration of the merge + snapshot
//! serialize + fsync. This is a known, accepted limitation for now.
//!
//! Production evolution path:
//! 1. Split into `RwLock<Ring>` + `Mutex<Delta+Dict>` for concurrent reads.
//! 2. Background merge thread: snapshot the delta, release the lock, rebuild
//!    Ring, `Arc::swap`, clear delta — so compaction never blocks readers or
//!    writers. This would replace the current inline `maybe_auto_compact`.
//!
//! ## Isolation semantics
//!
//! Every **single** `QuadStore` call (`insert`, `remove`, `contains`,
//! `quads_for_pattern`, ...) is atomic: it acquires `Mutex<RingStoreInner>`
//! exactly once and computes its entire result under that one critical
//! section, so no other thread's write can be observed "half-applied" within
//! one call.
//!
//! However, `RingStore` does **not** provide "repeatable read"/fixed-snapshot
//! isolation across *multiple* calls that together implement one logical
//! SPARQL operation:
//!
//! - A multi-triple-pattern `SELECT` query issues one `quads_for_pattern`
//!   call per pattern (or, for the LFTJ path, a sequence of seek/estimate
//!   calls); each is its own lock acquisition, so a concurrent writer's
//!   commit that lands between two of those calls **is** visible to the
//!   later one. There is no query-wide snapshot.
//! - `DELETE/INSERT ... WHERE` (`nova-query`'s `execute_update`) evaluates
//!   the WHERE clause fully into an in-memory `Vec<Solution>` first (itself
//!   made up of possibly many individual lock acquisitions), then issues one
//!   `remove`/`insert` call *per matched solution row* — again, each a
//!   separate lock acquisition. A concurrent reader can therefore observe
//!   the Update partially applied (some rows' deletes/inserts done, others
//!   not yet).
//!
//! This is an accepted, documented limitation (not upstream Oxigraph's
//! documented "repeatable read" guarantee) — see
//! `store::tests::multi_call_scan_does_not_get_a_whole_query_repeatable_read_snapshot`

//! for a deterministic (non-racy, `thread::join`-ordered) test that proves
//! this gap directly, and `oxigraph_nova_query::update`'s module doc comment
//! for the `Update`-atomicity side of the same limitation. Providing true
//! snapshot isolation would require either an MVCC scheme or holding the
//! single mutex for an entire multi-call operation (which would serialize
//! all concurrent queries against the store, defeating the point of
//! `RwLock`-style read concurrency) — out of scope for the current
//! single-`Mutex` design; see the "Production evolution path" above for the
//! direction a fix would take.

use crate::delta::Delta;
use crate::louds::LoudsMemBreakdown;
use crate::ring::{GraphRing, GraphRingHandle, RingBuilder, SortOrder};
use crate::snapshot::StoreSnapshot;
use oxigraph_nova_core::{
    Dictionary, EmptyTrieIter, GRAPH_DEFAULT, GraphId, GraphName, NamedNode, Oxigraph, Quad,
    QuadStore, StoredQuad, Subject, Term, TermId,
};
use oxigraph_nova_storage_common::dict_snapshot;
use oxigraph_nova_storage_common::manifest::{self, Manifest};
use oxigraph_nova_storage_common::wal::{self, WalRecord, WalWriter};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default delta-size threshold (number of live entries) that triggers an
/// automatic inline compaction for a persistent (`RingStore::open`) store.
/// See `maybe_auto_compact`. In-memory (`RingStore::new`) stores never
/// auto-compact (no `data_dir` to persist to).
const DEFAULT_AUTO_COMPACT_THRESHOLD: usize = 1_000_000;

/// Configurable WAL durability policy — the user-facing knob for trading a
/// small durability window for much higher write throughput (see
/// `wal.rs`'s "## Fsync policy" module docs).
///
/// - `Always`: every `insert`/`remove`/`extend` call's WAL record(s) are
///   `fsync`ed before the call returns. Every acknowledged write is
///   durable, at the cost of an fsync's latency on every single write.
/// - `Interval(d)` (default: 500ms — see `Default` impl below): WAL records
///   are written but **not** fsynced by the writer thread; a background
///   thread fsyncs the WAL file every `d` (group commit / "periodic
///   fsync"). Writes return immediately after the in-memory apply, without
///   waiting on any disk I/O at all — this is the much higher-throughput
///   default for both in-process and `nova_serve` usage. The cost: on a
///   crash, any writes made since the last background flush (at most `d`
///   old) are lost (not corrupted — `wal::replay`'s torn-tail handling
///   covers this exactly the same way it covers any other incomplete
///   write). This is the "group commit" pattern used by most production
///   databases (e.g. RocksDB's default WAL mode, MongoDB's periodic
///   journal commit) — `Always` remains available as an explicit opt-in
///   for callers that need zero durability window and can accept the
///   latency cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPolicy {
    Always,
    Interval(Duration),
}

/// Default durability policy: `Interval(500ms)` — group-commit fsync every
/// 500 milliseconds rather than on every single write. This is the
/// optimized, recommended default for both library callers and
/// `nova_serve`; see the module docs above for the trade-off. Use
/// `RingStore::set_sync_policy(SyncPolicy::Always)` to opt into per-write
/// fsync durability instead.
impl Default for SyncPolicy {
    fn default() -> Self {
        SyncPolicy::Interval(Duration::from_millis(500))
    }
}

/// Background flusher thread for `SyncPolicy::Interval`: periodically calls
/// `fsync` (`File::sync_data`) on a cloned handle of the currently-active
/// WAL file. Runs independently of the `Mutex<RingStoreInner>` — `fsync`
/// from one thread while another thread concurrently `write`s to the same
/// fd is safe (the kernel serializes them), so the flusher never needs to
/// acquire the store's lock.
///
/// A new `Flusher` is spawned every time the active WAL file changes (WAL
/// rotation during `commit_compaction`, or initial `RingStore::open`), and
/// the previous one is stopped (via `stop`) and joined on `Drop`.
struct Flusher {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Flusher {
    fn spawn(file: std::fs::File, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                // Best-effort: an fsync error here (e.g. disk full) is not
                // actionable from a background thread; the next foreground
                // write/compaction will surface any persistent I/O failure.
                let _ = file.sync_data();
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Flusher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ── Inner state ───────────────────────────────────────────────────────────────

struct RingStoreInner {
    /// Bidirectional Term ↔ TermId / GraphName ↔ GraphId mapping.
    dict: Dictionary,
    /// Per-graph Ring indexes (built by `compact()`), each either the owned
    /// in-memory form or a zero-copy `load_mmap`'d form — see
    /// [`GraphRingHandle`].
    graphs: HashMap<GraphId, GraphRingHandle>,

    /// Live write buffer (inserts and tombstones since last compaction).
    delta: Delta,
    /// Named-graph IDs that have been explicitly registered (tracks empty graphs).
    named_graph_ids: HashSet<GraphId>,
    /// WAL writer for the *currently active* segment, present only for
    /// persistent stores opened via [`RingStore::open`]. `None` for
    /// `RingStore::new()` (pure in-memory, used by unit tests and
    /// benchmarks that must not touch disk).
    wal: Option<WalWriter>,

    /// Data directory root, present only for persistent stores opened via
    /// [`RingStore::open`] (see `crate::manifest`). `None` for `RingStore::new()`.
    data_dir: Option<PathBuf>,
    /// Generation number of the currently-installed snapshot
    /// (`nova.snapshot.<snapshot_gen>`), `0` if none has been committed yet.
    /// Kept in sync with the MANIFEST every time `commit_compaction` runs.
    snapshot_gen: u64,
    /// Segment number of the currently-active WAL file
    /// (`nova.wal.<wal_seq>`), matching `wal`'s open file.
    wal_seq: u64,
    /// Delta-size threshold that triggers automatic inline compaction (see
    /// `maybe_auto_compact`). Irrelevant for in-memory stores (`data_dir ==
    /// None` gates auto-compaction off regardless of this value).
    auto_compact_threshold: usize,
    /// Current WAL durability policy (see [`SyncPolicy`]). Defaults to
    /// `Interval(500ms)`. Irrelevant for in-memory stores (`wal.is_none()`).
    sync_policy: SyncPolicy,

    /// Background flusher thread for `SyncPolicy::Interval`, `None` under
    /// `Always` or for in-memory stores. Replaced whenever the active WAL
    /// file changes.
    flusher: Option<Flusher>,
}

impl RingStoreInner {
    fn new() -> Self {
        Self {
            dict: Dictionary::new(),
            graphs: HashMap::new(),
            delta: Delta::new(),
            named_graph_ids: HashSet::new(),
            wal: None,
            data_dir: None,
            snapshot_gen: 0,
            wal_seq: 1,
            auto_compact_threshold: DEFAULT_AUTO_COMPACT_THRESHOLD,
            sync_policy: SyncPolicy::default(),
            flusher: None,
        }
    }

    /// Append `record` to the WAL if this store is persistent, respecting
    /// `sync_policy`: `Always` fsyncs immediately (via `WalWriter::append`);
    /// `Interval` writes without fsyncing (`append_no_sync`), relying on the
    /// background `Flusher` to fsync periodically.
    fn wal_append(&mut self, record: &WalRecord) -> Result<(), Oxigraph> {
        if let Some(w) = &mut self.wal {
            match self.sync_policy {
                SyncPolicy::Always => w
                    .append(record)
                    .map_err(|e| Oxigraph::Storage(format!("WAL append failed: {e}")))?,
                SyncPolicy::Interval(_) => w
                    .append_no_sync(record)
                    .map_err(|e| Oxigraph::Storage(format!("WAL append failed: {e}")))?,
            }
        }
        Ok(())
    }

    /// Append a batch of records with a single `fsync` (or none, under
    /// `Interval` policy) — used by `RingStore::extend` for multi-quad bulk
    /// inserts / SPARQL `INSERT DATA` with many triples.
    fn wal_append_batch<'a>(
        &mut self,
        records: impl IntoIterator<Item = &'a WalRecord>,
    ) -> Result<(), Oxigraph> {
        if let Some(w) = &mut self.wal {
            match self.sync_policy {
                SyncPolicy::Always => w
                    .append_batch(records)
                    .map_err(|e| Oxigraph::Storage(format!("WAL append_batch failed: {e}")))?,
                SyncPolicy::Interval(_) => {
                    for record in records {
                        w.append_no_sync(record)
                            .map_err(|e| Oxigraph::Storage(format!("WAL append failed: {e}")))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// (Re)spawn the background flusher thread for `SyncPolicy::Interval`
    /// against the currently-active WAL file, dropping (stopping/joining)
    /// any previous one. No-op for in-memory stores or `SyncPolicy::Always`.
    fn respawn_flusher(&mut self) {
        self.flusher = None; // drop stops+joins the old thread, if any
        if let (SyncPolicy::Interval(interval), Some(w)) = (self.sync_policy, &self.wal)
            && let Ok(file) = w.try_clone_file()
        {
            self.flusher = Some(Flusher::spawn(file, interval));
        }
    }

    /// Apply an already-durable `InsertQuad`/`RemoveQuad`/`RegisterGraph`
    /// record to in-memory state, **without** touching the WAL (used both by
    /// normal writes after `wal_append` and by WAL replay at startup, where
    /// re-appending would be redundant/wrong).
    fn apply_insert(&mut self, quad: &Quad) -> Result<bool, Oxigraph> {
        let g_id = self.dict.intern_graph(&quad.graph_name)?;
        let s_id = self.dict.intern(&subject_to_term(&quad.subject))?;
        let p_id = self.dict.intern_predicate(&quad.predicate)?;
        let o_id = self.dict.intern(&quad.object)?;

        let key = Dictionary::pack_quad(g_id, s_id, p_id, o_id);

        match self.delta.get(key) {
            Some(true) => return Ok(false),
            Some(false) => {
                self.delta.insert_key(key);
                return Ok(true);
            }
            None => {}
        }

        if let Some(ring) = self.graphs.get(&g_id)
            && ring.contains(s_id.as_u64(), p_id.as_u64(), o_id.as_u64())
        {
            return Ok(false);
        }

        self.delta.insert_key(key);

        if let GraphName::NamedNode(_) = &quad.graph_name {
            self.named_graph_ids.insert(g_id);
        }

        Ok(true)
    }

    fn apply_remove(&mut self, quad: &Quad) -> Result<bool, Oxigraph> {
        let g_id = match self.dict.get_graph_id(&quad.graph_name) {
            None => return Ok(false),
            Some(id) => id,
        };
        let s_id = match self.dict.get_id(&subject_to_term(&quad.subject)) {
            None => return Ok(false),
            Some(id) => id,
        };
        let p_id = match self.dict.get_id(&Term::NamedNode(quad.predicate.clone())) {
            None => return Ok(false),
            Some(id) => id,
        };
        let o_id = match self.dict.get_id(&quad.object) {
            None => return Ok(false),
            Some(id) => id,
        };

        let key = Dictionary::pack_quad(g_id, s_id, p_id, o_id);

        match self.delta.get(key) {
            Some(true) => {
                self.delta.tombstone_key(key);
                return Ok(true);
            }
            Some(false) => return Ok(false),
            None => {}
        }

        if let Some(ring) = self.graphs.get(&g_id)
            && ring.contains(s_id.as_u64(), p_id.as_u64(), o_id.as_u64())
        {
            self.delta.tombstone_key(key);
            return Ok(true);
        }

        Ok(false)
    }

    fn apply_register_graph(&mut self, graph: &GraphName) -> Result<(), Oxigraph> {
        if let GraphName::NamedNode(_) = graph {
            let g_id = self.dict.intern_graph(graph)?;
            self.named_graph_ids.insert(g_id);
        }
        Ok(())
    }

    /// Build the merged `new_graphs` map (Ring ∪ delta_inserts \ tombstones)
    /// and commit it — used by `RingStore::compact()` and by
    /// `maybe_auto_compact`. Assumes `self` is already locked (called with
    /// `&mut self` from inside the single `Mutex<RingStoreInner>` critical
    /// section — never re-enters the lock).
    fn compact_locked(&mut self) -> Result<(), Oxigraph> {
        let mut per_graph: HashMap<GraphId, Vec<[u64; 3]>> = HashMap::new();
        for (&g_id, ring) in &self.graphs {
            per_graph.insert(g_id, ring.spo_triples());
        }

        for (&key, &is_insert) in self.delta.iter() {
            let (g_id, s_id, p_id, o_id) = Dictionary::unpack_quad(key);
            let triples = per_graph.entry(g_id).or_default();
            let triple = [s_id.as_u64(), p_id.as_u64(), o_id.as_u64()];
            if is_insert {
                triples.push(triple);
            } else {
                triples.retain(|t| t != &triple);
            }
        }

        let new_graphs = build_graphs_from_triples(per_graph);
        // In-memory Front-Coding compaction of the Dictionary  — runs on the same
        // cadence as the Ring trie rebuild above, regardless of persistence mode.
        self.dict.compact()?;
        self.commit_compaction(new_graphs, true)
    }

    /// If this is a persistent store and the delta has crossed
    /// `auto_compact_threshold`, compact inline (see module docs for the
    /// "stalls writers" trade-off this accepts). No-op otherwise (including
    /// always for in-memory stores, since `data_dir == None`). Auto-compaction
    /// cannot be disabled for a persistent store — only its threshold is
    /// configurable (see `RingStore::set_auto_compact_threshold`) — so that
    /// memory-efficient defaults can't be accidentally left off.
    fn maybe_auto_compact(&mut self) -> Result<(), Oxigraph> {
        if self.data_dir.is_some() && self.delta.len() >= self.auto_compact_threshold {
            self.compact_locked()?;
        }
        Ok(())
    }

    /// Crash-safe compaction commit: serialize `new_graphs` to a
    /// fresh snapshot generation, rotate the WAL to a fresh segment, commit
    /// both atomically via the MANIFEST, delete now-obsolete snapshot/WAL
    /// files, then install the round-tripped graphs.
    ///
    /// For an in-memory store (`data_dir.is_none()`) this only performs the
    /// in-memory ε-serde round-trip (see `snapshot.rs`'s "always mapped"
    /// design) — no disk I/O at all.
    ///
    /// `clear_delta`: `compact()`'s merge already folds delta into
    /// `new_graphs`, so it must clear the delta afterward. `bulk_load()`
    /// never touches the delta in the first place (see its doc comment), so
    /// it passes `false` to avoid clearing entries that were never part of
    /// this merge.
    fn commit_compaction(
        &mut self,
        new_graphs: HashMap<GraphId, Arc<GraphRing>>,
        clear_delta: bool,
    ) -> Result<(), Oxigraph> {
        match self.data_dir.clone() {
            None => {
                // In-memory store: no disk to map from, so keep the owned
                // ε-serde round-trip (see `snapshot.rs` module docs,
                // "Always mapped") and wrap each graph as `Owned`.
                let round_tripped = StoreSnapshot::round_trip_and_maybe_save(new_graphs, None)?;
                self.graphs = round_tripped
                    .into_iter()
                    .map(|(g, ring)| (g, GraphRingHandle::Owned(ring)))
                    .collect();
            }
            Some(dir) => {
                let new_gen = self.snapshot_gen + 1;
                let new_seq = self.wal_seq + 1;

                // 1. Serialize the new snapshot generation to disk, then
                //    `load_mmap` it back in — the servable representation
                //    becomes a genuine zero-copy view of exactly the bytes
                //    just written (see `snapshot.rs`'s `write_and_load_mmap`
                //    doc comment).
                let snap_path = manifest::snapshot_path(&dir, new_gen);
                let graphs = StoreSnapshot::write_and_load_mmap(new_graphs, &snap_path)?;

                // 1b. Persist the Dictionary alongside this snapshot
                //     generation (see `dict_snapshot.rs` module docs for why
                //     this is required under tail-only WAL replay). Like the
                //     Ring snapshot above, the servable dictionary becomes a
                //     genuine zero-copy `load_mmap`'d view of exactly the
                //     bytes just written.
                let dict_path = manifest::dict_path(&dir, new_gen);
                self.dict = dict_snapshot::write_and_load_mmap(&self.dict, &dict_path)?;

                // 2. Rotate the WAL: new writes land in a fresh empty segment.

                let new_seg_path = manifest::wal_segment_path(&dir, new_seq);
                let new_wal = WalWriter::create_or_open(&new_seg_path).map_err(|e| {
                    Oxigraph::Storage(format!("failed to open new WAL segment: {e}"))
                })?;

                // 3. Single atomic commit point: MANIFEST now points at
                //    (new_gen, new_seq). Before this line, a crash leaves the
                //    previous MANIFEST (and thus the previous consistent
                //    state) untouched — the new snapshot/segment are simply
                //    orphans that `open()`/cleanup will delete later.
                let manifest_path = dir.join(manifest::MANIFEST_FILE_NAME);
                let m = Manifest {
                    snapshot_gen: new_gen,
                    wal_seq: new_seq,
                };
                m.save(&manifest_path)?;

                // 4. Delete now-obsolete files (best-effort; see
                //    `cleanup_orphans` doc comment).
                manifest::cleanup_orphans(&dir, new_gen, new_seq);

                self.graphs = graphs;
                self.wal = Some(new_wal);
                self.snapshot_gen = new_gen;
                self.wal_seq = new_seq;
                // The WAL file changed (rotated) — re-spawn the background
                // flusher (if any) against the new file.
                self.respawn_flusher();
            }
        }
        if clear_delta {
            self.delta.clear();
        }

        Ok(())
    }
}

/// Sort + dedup per-graph triples and build a `RingBuilder`-based
/// `Arc<GraphRing>` map. Shared by `compact_locked` and `bulk_load`.
fn build_graphs_from_triples(
    per_graph: HashMap<GraphId, Vec<[u64; 3]>>,
) -> HashMap<GraphId, Arc<GraphRing>> {
    let mut new_graphs: HashMap<GraphId, Arc<GraphRing>> = HashMap::new();
    for (g_id, triples) in per_graph {
        if triples.is_empty() {
            continue;
        }
        // Hand the already-collected Vec straight to the builder — `build()`
        // performs its own sort_unstable+dedup, so there is no need to
        // sort/dedup here first, nor to copy element-by-element via `add()`.
        // This avoids one full-size transient Vec allocation per graph per
        // compaction/bulk-load.
        let builder = RingBuilder::from_vec(triples);
        new_graphs.insert(g_id, Arc::new(builder.build()));
    }
    new_graphs
}

// ── RingStore ─────────────────────────────────────────────────────────────────

/// A `QuadStore` implementation backed by:
///
/// - A `Dictionary` mapping RDF terms to 40-bit integer IDs.
/// - Per-graph `GraphRing` indexes (six sorted arrays, built by `compact()`).
/// - An LSM delta `BTreeMap<u128, bool>` absorbing live writes.
///
/// ## Usage
///
/// ```rust,no_run
/// use oxigraph_nova_core::{GraphName, Literal, NamedNode, Quad, QuadStore, Term};
/// use oxigraph_nova_storage_ring::RingStore;
/// use std::sync::Arc;
///
/// let store = Arc::new(RingStore::new());
///
/// // Inserts go into the delta BTreeMap.
/// let quad = Quad::new(
///     oxrdf::NamedNode::new_unchecked("http://ex/s"),
///     oxrdf::NamedNode::new_unchecked("http://ex/p"),
///     Term::Literal(Literal::new_simple_literal("hello")),
///     GraphName::DefaultGraph,
/// );
/// store.insert(&quad).unwrap();
///
/// // Optional: compact the delta into the Ring for better scan performance.
/// store.compact().unwrap();
/// ```
pub struct RingStore {
    inner: Mutex<RingStoreInner>,
}

impl RingStore {
    /// Create an empty, purely in-memory `RingStore` with **no** WAL/disk
    /// persistence — writes are never logged to disk. Used by unit tests,
    /// benchmarks, and any caller that explicitly wants an ephemeral store.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RingStoreInner::new()),
        }
    }

    /// Open (or create) a persistent `RingStore` rooted at `dir` (created if
    /// it doesn't exist), using the MANIFEST + generation-numbered
    /// snapshot + segment-numbered WAL scheme.
    ///
    /// ## Recovery procedure
    ///
    /// 1. Load `nova.manifest` (absent ⇒ fresh store: `snapshot_gen = 0`,
    ///    `wal_seq = 1`).
    /// 2. If `snapshot_gen > 0`, load `nova.snapshot.<snapshot_gen>` to
    ///    pre-populate `graphs`.
    /// 3. Replay every `nova.wal.<seq>` segment with `seq >= wal_seq`, in
    ///    ascending order — **not** the entire WAL history, since everything
    ///    in earlier segments is already reflected in the loaded snapshot
    ///    (this keeps startup cost O(tail) instead of O(total history)).
    /// 4. Open the highest such segment (or a fresh one at `wal_seq` if none
    ///    exist yet) for appending, and delete any now-provably-obsolete
    ///    snapshot/segment files (best-effort; see `manifest::cleanup_orphans`).
    ///
    /// Replaying a WAL record already reflected in the loaded snapshot is a
    /// safe no-op: `apply_insert`/`apply_remove` both check
    /// `ring.contains(...)` before touching the delta (see `snapshot.rs`
    /// module docs for the full argument — still true here even though the
    /// tail-only replay makes this redundancy rare in practice, e.g. a
    /// segment that was rotated-into but had zero records appended before a
    /// crash).
    pub fn open(dir: &Path) -> Result<Self, Oxigraph> {
        std::fs::create_dir_all(dir)?;
        let manifest_path = dir.join(manifest::MANIFEST_FILE_NAME);
        let m = Manifest::load(&manifest_path, dir)?;

        let mut inner = RingStoreInner::new();
        if m.snapshot_gen > 0 {
            let snap_path = manifest::snapshot_path(dir, m.snapshot_gen);
            inner.graphs = StoreSnapshot::load_mmap_from_file(&snap_path)?;

            // Reconstruct the Dictionary's exact TermId/GraphId assignments
            // from this snapshot generation BEFORE replaying the WAL tail,
            // so replay's intern() calls only ever append new terms after
            // the snapshot's high-water-mark (see `dict_snapshot.rs` module
            // docs for why this is required). The compacted tier comes back
            // as a genuine zero-copy `load_mmap`'d view of the on-disk file.
            let dict_path = manifest::dict_path(dir, m.snapshot_gen);
            inner.dict = dict_snapshot::load_mmap_from_file(&dict_path)?;
        }

        // Discover every WAL segment at or after the MANIFEST's recorded
        // `wal_seq`, replay them in ascending order.
        let mut segments: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|entry| {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        name.strip_prefix("nova.wal.")
                            .and_then(|s| s.parse::<u64>().ok())
                            .filter(|&seq| seq >= m.wal_seq)
                            .map(|seq| (seq, entry.path()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        segments.sort_by_key(|(seq, _)| *seq);

        for (_, path) in &segments {
            wal::replay(path, |record| {
                // Errors during replay of a single record are logged and
                // skipped rather than aborting the whole replay — a bad
                // Dictionary/GraphId-space-exhaustion error on one record
                // should not prevent recovering the rest of a large store.
                let result = match &record {
                    WalRecord::InsertQuad(q) => inner.apply_insert(q).map(|_| ()),
                    WalRecord::RemoveQuad(q) => inner.apply_remove(q).map(|_| ()),
                    WalRecord::RegisterGraph(g) => inner.apply_register_graph(g),
                };
                if let Err(e) = result {
                    eprintln!("[RingStore::open] WAL replay: skipping record due to error: {e}");
                }
            })
            .map_err(|e| Oxigraph::Storage(format!("WAL replay I/O error: {e}")))?;
        }

        // The active segment is the highest one found, or a fresh segment at
        // `m.wal_seq` if none exist yet (brand-new store, or a MANIFEST that
        // was just committed by `commit_compaction` immediately before a
        // crash that prevented the new segment file from being created —
        // extremely unlikely given creation happens before the MANIFEST
        // write, but `create_or_open` handles it correctly either way).
        let active_seq = segments.last().map(|(seq, _)| *seq).unwrap_or(m.wal_seq);
        let active_path = manifest::wal_segment_path(dir, active_seq);
        inner.wal = Some(
            WalWriter::create_or_open(&active_path)
                .map_err(|e| Oxigraph::Storage(format!("failed to open WAL for writing: {e}")))?,
        );

        inner.data_dir = Some(dir.to_path_buf());
        inner.snapshot_gen = m.snapshot_gen;
        inner.wal_seq = active_seq;

        // Best-effort cleanup of anything strictly older than what we just
        // committed to using (handles orphans left by a crash between
        // creating a new snapshot/segment and committing the MANIFEST that
        // references them, or between committing the MANIFEST and the
        // previous run's cleanup step).
        manifest::cleanup_orphans(dir, inner.snapshot_gen, inner.wal_seq);

        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    /// Set the WAL durability policy (see [`SyncPolicy`]). Default is
    /// `Interval(500ms)`. Switching to `Interval(d)` spawns a background
    /// flusher thread against the currently-active WAL file; switching to
    /// `Always` stops it. No-op for in-memory stores (no WAL to flush).
    pub fn set_sync_policy(&self, policy: SyncPolicy) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.sync_policy = policy;
            inner.respawn_flusher();
        }
    }

    /// Explicitly fsync the active WAL file right now, regardless of
    /// `SyncPolicy`. Useful before a clean shutdown under
    /// `SyncPolicy::Interval` to guarantee all acknowledged writes are
    /// durable. No-op for in-memory stores.
    pub fn flush_wal(&self) -> Result<(), Oxigraph> {
        if let Ok(inner) = self.inner.lock()
            && let Some(w) = &inner.wal
        {
            w.sync()
                .map_err(|e| Oxigraph::Storage(format!("WAL flush failed: {e}")))?;
        }
        Ok(())
    }

    /// The on-disk path of the WAL segment `open(dir)` would create/use for
    /// a **fresh** directory (segment `1`). Exposed for tests that need to
    /// interact with the WAL file directly (e.g. to simulate a torn write)
    /// before any compaction has rotated it.
    pub fn wal_path(dir: &Path) -> PathBuf {
        manifest::wal_segment_path(dir, 1)
    }

    /// Create a consistent, restorable copy of this store's on-disk state
    /// into `destination` (created if it doesn't exist).
    ///
    /// Mirrors upstream Oxigraph's `Store::backup(destination)`: after this
    /// call returns, `RingStore::open(destination)` recovers exactly the
    /// same data as the source store had at the moment `backup` was called,
    /// and the backup is a fully independent store thereafter (no shared
    /// file handles or further coupling to the source).
    ///
    /// Only valid for a **persistent** store (opened via [`RingStore::open`]);
    /// returns an error for an in-memory (`RingStore::new()`) store, since
    /// there is no on-disk state to copy.
    ///
    /// ## Consistency
    ///
    /// The entire operation runs under the single `Mutex<RingStoreInner>`
    /// lock (the same lock every read/write already serialises through), so
    /// no concurrent write or compaction can rotate the snapshot/WAL-segment
    /// files out from under the copy. Before copying, the active WAL
    /// segment is explicitly `fsync`ed (regardless of the current
    /// [`SyncPolicy`]) so the backup never misses a write that had already
    /// been acknowledged to a caller.
    ///
    /// Only the files the current MANIFEST actually references are copied
    /// (the MANIFEST itself, the current snapshot generation's
    /// `nova.snapshot.<gen>` + `nova.dict.<gen>`, and the active
    /// `nova.wal.<seq>` segment) — any orphaned older-generation files
    /// still sitting in the source directory (pending best-effort cleanup)
    /// are intentionally not copied.
    pub fn backup(&self, destination: &Path) -> Result<(), Oxigraph> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;

        let dir = inner.data_dir.clone().ok_or_else(|| {
            Oxigraph::Storage(
                "RingStore::backup requires a persistent store (opened via RingStore::open); \
                 an in-memory store (RingStore::new()) has no on-disk state to copy"
                    .to_string(),
            )
        })?;

        // Make sure every acknowledged write is actually durable before
        // copying, regardless of the current SyncPolicy (Interval policy
        // writes are not fsynced until the background flusher runs).
        if let Some(w) = &inner.wal {
            w.sync()
                .map_err(|e| Oxigraph::Storage(format!("WAL flush before backup failed: {e}")))?;
        }

        std::fs::create_dir_all(destination)?;

        let manifest_src = dir.join(manifest::MANIFEST_FILE_NAME);
        if manifest_src.exists() {
            std::fs::copy(
                &manifest_src,
                destination.join(manifest::MANIFEST_FILE_NAME),
            )?;
        }

        if inner.snapshot_gen > 0 {
            let snap_src = manifest::snapshot_path(&dir, inner.snapshot_gen);
            if snap_src.exists() {
                std::fs::copy(
                    &snap_src,
                    manifest::snapshot_path(destination, inner.snapshot_gen),
                )?;
            }
            let dict_src = manifest::dict_path(&dir, inner.snapshot_gen);
            if dict_src.exists() {
                std::fs::copy(
                    &dict_src,
                    manifest::dict_path(destination, inner.snapshot_gen),
                )?;
            }
        }

        let wal_src = manifest::wal_segment_path(&dir, inner.wal_seq);
        if wal_src.exists() {
            std::fs::copy(
                &wal_src,
                manifest::wal_segment_path(destination, inner.wal_seq),
            )?;
        }

        Ok(())
    }

    /// Merge the delta into the Ring index.
    ///
    /// After compaction:
    /// - All triples live in the per-graph `GraphRing` sorted arrays.
    /// - The delta is cleared.
    /// - Queries via `quads_for_pattern` continue to work (now scanning only the Ring).
    /// - For a persistent store, a new snapshot generation + WAL segment are
    ///   committed via the MANIFEST (see `commit_compaction`), and obsolete
    ///   snapshot/segment files are deleted.
    ///
    /// **When to call:** After a bulk load, or manually/administratively.
    /// For persistent stores this also happens automatically once the delta
    /// crosses a configurable threshold — see `set_auto_compact_threshold`.
    pub fn compact(&self) -> Result<(), Oxigraph> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;
        inner.compact_locked()
    }

    /// Configure the delta-size threshold (number of live entries) that
    /// triggers automatic inline compaction for a persistent store. Default
    /// is 1,000,000. Has no effect on in-memory (`RingStore::new()`) stores.
    pub fn set_auto_compact_threshold(&self, threshold: usize) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.auto_compact_threshold = threshold;
        }
    }

    /// Bulk-load quads directly into the Ring, **bypassing the delta
    /// `BTreeMap` entirely**.
    ///
    /// Intended for initial dataset loads (e.g. `nova_serve`'s startup path)
    /// where every quad is known to be a fresh insert and there is no need to
    /// absorb writes into an LSM delta first.  Interning + per-graph triple
    /// collection happens directly against `Vec<[u64;3]>` buffers, then each
    /// graph's `RingBuilder` is built once — avoiding the O(n) `BTreeMap<u128,
    /// bool>` node-allocation overhead (~4-8x the logical 17 bytes/entry) that
    /// `insert()` + `compact()` would otherwise incur for the same data.
    ///
    /// Any quads already present in `self` (from a prior `insert`/`compact`)
    /// are preserved and merged in via `spo_triples()`, so this is safe to
    /// call on a non-empty store, but for the common "fresh store, one big
    /// load" case it will not have to touch `spo_triples()`/`per_graph`
    /// merging at all (no existing graphs ⇒ that loop is a no-op).
    ///
    /// For a persistent store, this also commits a new snapshot
    /// generation + WAL segment via the MANIFEST (see `commit_compaction`),
    /// same as `compact()` — so a bulk-loaded dataset is never left
    /// WAL-only/un-snapshotted.
    pub fn bulk_load(&self, quads: impl IntoIterator<Item = Quad>) -> Result<usize, Oxigraph> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;

        // Start from whatever's already compacted into the Ring (usually
        // empty for a fresh store — this loop is then a no-op).
        let mut per_graph: HashMap<GraphId, Vec<[u64; 3]>> = HashMap::new();
        for (&g_id, ring) in &inner.graphs {
            per_graph.insert(g_id, ring.spo_triples());
        }

        let mut count = 0usize;
        for quad in quads {
            let g_id = inner.dict.intern_graph(&quad.graph_name)?;
            let s_id = inner.dict.intern(&subject_to_term(&quad.subject))?;
            let p_id = inner.dict.intern_predicate(&quad.predicate)?;
            let o_id = inner.dict.intern(&quad.object)?;

            per_graph
                .entry(g_id)
                .or_default()
                .push([s_id.as_u64(), p_id.as_u64(), o_id.as_u64()]);

            if let GraphName::NamedNode(_) = &quad.graph_name {
                inner.named_graph_ids.insert(g_id);
            }
            count += 1;
        }

        let new_graphs = build_graphs_from_triples(per_graph);
        // Same as `compact_locked`: fold the newly-interned terms into the
        // Front-Coded compacted tier before committing, so a bulk load ends
        // up in the same memory-efficient state as an inline-compacted
        // incrementally-built store. Without this call,
        // `bulk_load()` — the path used by `nova_serve --file`, and thus by
        // every external comparative benchmark — would leave the entire
        // dictionary sitting in its uncompacted delta tier forever, since
        // nothing else ever triggers compaction for a one-shot bulk load.
        inner.dict.compact()?;
        inner.commit_compaction(new_graphs, false)?;
        Ok(count)
    }

    /// Number of triples stored across all graphs (approximation during merge).
    pub fn triple_count(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        let ring_total: usize = inner.graphs.values().map(|r| r.n()).sum();

        let delta_inserts = inner.delta.insert_count();
        let delta_tombstones = inner.delta.tombstone_count();
        ring_total.saturating_sub(delta_tombstones) + delta_inserts
    }

    /// Real per-component memory breakdown across all graphs' Ring indexes
    /// plus the dictionary, for memory-breakdown diagnostics (see
    /// `benches/external/RESULTS.md`).
    ///
    /// Returned counts are **measured** heap byte totals (via `mem_dbg`'s
    /// `MemSize` trait and direct capacity accounting), not theoretical
    /// estimates — see `GraphRing::mem_size_bytes` / `Dictionary::mem_size_bytes`.
    pub fn memory_breakdown(&self) -> MemoryBreakdown {
        let inner = self.inner.lock().unwrap();
        let ring_bytes: usize = inner.graphs.values().map(|r| r.mem_size_bytes()).sum();
        let dict_bytes = inner.dict.mem_size_bytes();
        let triple_count: usize = inner.graphs.values().map(|r| r.n()).sum();
        MemoryBreakdown {
            ring_bytes,
            dict_bytes,
            triple_count,
        }
    }

    /// Per-ordering (SPO/SOP/PSO/POS/OPS/OSP) memory breakdown, summed across
    /// all graphs.  Also returns the summed redundant
    /// `spo: Vec<[u64;3]>` bytes and the summed deduped-vocab bytes (both
    /// summed across graphs, since dedup only applies within one graph's
    /// six tries).
    pub fn per_ordering_breakdown(&self) -> PerOrderingBreakdown {
        let inner = self.inner.lock().unwrap();
        let orders = [
            SortOrder::Spo,
            SortOrder::Sop,
            SortOrder::Pso,
            SortOrder::Pos,
            SortOrder::Ops,
            SortOrder::Osp,
        ];
        let mut per_order: [LoudsMemBreakdown; 6] = Default::default();
        let mut vocab_undeduped: [usize; 6] = Default::default();
        let mut spo_bytes_total = 0usize;
        let mut vocab_deduped_total = 0usize;

        for ring in inner.graphs.values() {
            if let Some(breakdown) = ring.mem_breakdown_per_ordering() {
                for (i, (_ord, b, vocab_u)) in breakdown.iter().enumerate() {
                    per_order[i].t_bytes += b.t_bytes;
                    per_order[i].l_bytes += b.l_bytes;
                    per_order[i].sidecar_bytes += b.sidecar_bytes;
                    vocab_undeduped[i] += vocab_u;
                }
            }
            let (spo_bytes, vocab_deduped) = ring.spo_and_vocab_bytes();
            spo_bytes_total += spo_bytes;
            vocab_deduped_total += vocab_deduped;
        }

        PerOrderingBreakdown {
            orders,
            per_order,
            vocab_undeduped,
            spo_bytes_total,
            vocab_deduped_total,
        }
    }
}

/// Per-ordering memory breakdown across all graphs.
///
/// See [`RingStore::per_ordering_breakdown`].
#[derive(Clone, Debug)]
pub struct PerOrderingBreakdown {
    /// The six orderings, in fixed order (index-aligned with `per_order` and
    /// `vocab_undeduped`).
    pub orders: [SortOrder; 6],
    /// T/L/sidecar breakdown per ordering, summed across all graphs.
    pub per_order: [LoudsMemBreakdown; 6],
    /// Undeduped vocab bytes per ordering (i.e. as if each ordering's three
    /// vocab arrays were NOT shared with the other five orderings), summed
    /// across all graphs.  Useful for seeing "what would this ordering cost
    /// alone" but double/triple-counts shared allocations — see
    /// `vocab_deduped_total` for the real total.
    pub vocab_undeduped: [usize; 6],
    /// Always `0` — there is no redundant `spo: Vec<[u64;3]>` raw copy;
    /// retained for print-layout stability in `nova_serve.rs`.
    pub spo_bytes_total: usize,

    /// Summed deduped vocab bytes across all graphs (3 unique allocations
    /// per graph, regardless of the 18 references across six tries).
    pub vocab_deduped_total: usize,
}

/// Real, measured per-component memory breakdown for a [`RingStore`].
///
/// See [`RingStore::memory_breakdown`].
#[derive(Copy, Clone, Debug)]
pub struct MemoryBreakdown {
    /// Total bytes across all graphs' `GraphRing` indexes (six-LOUDS-trie
    /// CompactLTJ structure, Arc-deduped vocab; no redundant
    /// `spo: Vec<[u64;3]>` raw copy is kept).
    pub ring_bytes: usize,

    /// Total bytes in the `Dictionary` (`id_to_term` + `term_to_id` + side
    /// tables) — every term's string content counted per the real oxrdf
    /// `Term` representation (no `Arc` sharing).
    pub dict_bytes: usize,
    /// Total number of triples currently in the Ring (post-compaction).
    pub triple_count: usize,
}

impl MemoryBreakdown {
    /// Grand total: `ring_bytes + dict_bytes`.
    pub fn total_bytes(&self) -> usize {
        self.ring_bytes + self.dict_bytes
    }

    /// Grand total bytes per triple (0.0 if `triple_count == 0`).
    pub fn bytes_per_triple(&self) -> f64 {
        if self.triple_count == 0 {
            0.0
        } else {
            self.total_bytes() as f64 / self.triple_count as f64
        }
    }
}

impl Default for RingStore {
    fn default() -> Self {
        Self::new()
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Convert an `oxrdf::NamedOrBlankNode` (Subject) to `Term` for dictionary lookup.
#[inline]
fn subject_to_term(s: &Subject) -> Term {
    Term::from(s.clone())
}

/// Decode `(GraphId, TermId × 3)` → [`StoredQuad`] using the dictionary.
///
/// Returns `None` if any ID is invalid or the predicate is not a `NamedNode`
/// (which should never happen for correctly-inserted data).
///
/// Unlike the old `decode_quad → Quad` path, this function preserves
/// `Term::Triple(...)` subjects so that RDF-star triples are not silently
/// dropped when scanning the Ring.
fn decode_stored_quad(
    dict: &Dictionary,
    g_id: GraphId,
    s_id: TermId,
    p_id: TermId,
    o_id: TermId,
) -> Option<StoredQuad> {
    let graph_name = dict.get_graph(g_id)?.clone();
    // Fetch `Arc<Term>` (cheap refcount bump) instead of `.clone()`-ing the
    // dereferenced `Term` (deep String copy) on every matched row.
    let s_term = dict.get_term_arc(s_id)?;
    let p_term = dict.get_term_arc(p_id)?;
    let o_term = dict.get_term_arc(o_id)?;

    // Subject may be NamedNode, BlankNode, or (RDF-star) Triple — all are valid.
    // Literals are never legal RDF subjects; return None so they are silently skipped.
    match s_term.as_ref() {
        Term::NamedNode(_) | Term::BlankNode(_) | Term::Triple(_) => {}
        Term::Literal(_) => return None,
    };
    let predicate: NamedNode = match p_term.as_ref() {
        Term::NamedNode(n) => n.clone(),
        _ => return None, // predicates must be IRIs
    };

    Some(StoredQuad {
        subject: s_term,
        predicate,
        object: o_term,
        graph_name,
    })
}

// ── QuadStore impl ────────────────────────────────────────────────────────────

impl QuadStore for RingStore {
    // ── LFTJ capability ───────────────────────────────────────────────────────

    fn supports_lftj(&self) -> bool {
        true
    }

    fn supports_veo_estimates(&self) -> bool {
        // RingStore provides vocab-size estimates (non-u64::MAX) via
        // GraphRing::estimate_count → CltjData::vocab_size_for_field.
        // This gates the VEO sort in lftj_step so it only fires for CLTJ backends.
        true
    }

    fn lftj_intern_term(&self, term: &Term) -> Option<u64> {
        let inner = self.inner.lock().ok()?;
        inner.dict.get_id(term).map(|id| id.as_u64())
    }

    fn lftj_decode_term(&self, id: u64) -> Option<Term> {
        let inner = self.inner.lock().ok()?;
        let tid = oxigraph_nova_core::TermId::new(id).ok()?;
        inner.dict.get_term_arc(tid).map(|arc| arc.as_ref().clone())
    }

    fn lftj_graph_id(&self, graph: &GraphName) -> Option<u8> {
        let inner = self.inner.lock().ok()?;
        inner.dict.get_graph_id(graph).map(|gid| gid.as_u8())
    }

    fn lftj_estimate_count(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
        graph_id: u8,
    ) -> u64 {
        let inner = match self.inner.lock().ok() {
            Some(i) => i,
            None => return u64::MAX,
        };
        let g_id = GraphId(graph_id);
        match inner.graphs.get(&g_id) {
            None => 0,
            Some(ring) => ring.estimate_count(s, p, o, target_field),
        }
    }

    fn lftj_join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
        graph_id: u8,
    ) -> Option<Box<dyn oxigraph_nova_core::TrieIterator>> {
        let inner = self.inner.lock().ok()?;
        let g_id = GraphId(graph_id);
        // If the graph has no Ring entry, return an always-exhausted iterator
        // (graph exists in dict but has no compacted triples → empty scan is correct).
        match inner.graphs.get(&g_id) {
            None => Some(Box::new(EmptyTrieIter)),
            Some(ring) => Some(ring.join_scan(s, p, o, target_field)),
        }
    }

    fn lftj_has_delta(&self) -> bool {
        match self.inner.lock() {
            Ok(inner) => !inner.delta.is_empty(),
            Err(_) => false,
        }
    }

    // ─────────────────────────────────────────────────────────────────────────

    fn insert(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;

        // Log intent durably BEFORE applying, so a crash between the two can
        // always be recovered by replaying the WAL (see wal.rs module docs).
        inner.wal_append(&WalRecord::InsertQuad(quad.clone()))?;
        let result = inner.apply_insert(quad)?;
        inner.maybe_auto_compact()?;
        Ok(result)
    }

    fn remove(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;

        inner.wal_append(&WalRecord::RemoveQuad(quad.clone()))?;
        let result = inner.apply_remove(quad)?;
        inner.maybe_auto_compact()?;
        Ok(result)
    }

    fn quads_for_pattern(
        &self,
        subject: Option<&Term>,
        predicate: Option<&NamedNode>,
        object: Option<&Term>,
        graph_name: Option<&GraphName>,
    ) -> Result<Box<dyn Iterator<Item = Result<StoredQuad, Oxigraph>> + '_>, Oxigraph> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;

        // ── Encode filter terms (None if not in dict → return empty immediately) ──

        // Subject is now Option<&Term> — handles NamedNode, BlankNode, and Triple
        // (quoted-triple subject patterns).  If the term is not in the dictionary,
        // no triples can match, so return empty immediately.
        let s_id: Option<TermId> = match subject {
            None => None,
            Some(sv) => match inner.dict.get_id(sv) {
                None => return Ok(Box::new(std::iter::empty())),
                Some(id) => Some(id),
            },
        };

        let p_id: Option<TermId> = match predicate {
            None => None,
            Some(pv) => match inner.dict.get_id(&Term::NamedNode(pv.clone())) {
                None => return Ok(Box::new(std::iter::empty())),
                Some(id) => Some(id),
            },
        };

        let o_id: Option<TermId> = match object {
            None => None,
            Some(ov) => match inner.dict.get_id(ov) {
                None => return Ok(Box::new(std::iter::empty())),
                Some(id) => Some(id),
            },
        };

        // ── Determine graph IDs to search ────────────────────────────────────

        let search_graphs: Vec<GraphId> = match graph_name {
            Some(gn) => match inner.dict.get_graph_id(gn) {
                None => return Ok(Box::new(std::iter::empty())),
                Some(id) => vec![id],
            },
            None => {
                // All known graphs: those in the Ring + those with delta entries.
                let mut gids: HashSet<GraphId> = inner.graphs.keys().copied().collect();
                gids.insert(GRAPH_DEFAULT);
                for &gid in &inner.named_graph_ids {
                    gids.insert(gid);
                }
                // Also discover any graphs that appeared only via delta keys.
                for (&key, _) in inner.delta.iter() {
                    let (gid, _, _, _) = Dictionary::unpack_quad(key);
                    gids.insert(gid);
                }
                gids.into_iter().collect()
            }
        };

        // ── Scan Ring + delta, collect results ───────────────────────────────

        let mut results: Vec<StoredQuad> = Vec::new();
        // `seen` tracks delta keys so Ring results don't re-emit them.
        let mut seen: HashSet<u128> = HashSet::new();

        for g_id in search_graphs {
            let (lo, hi) = Delta::graph_range(g_id, s_id, p_id, o_id);

            // 1. Delta entries for this graph (may be inserts or tombstones).
            for (&key, &is_insert) in inner.delta.range_inclusive(lo, hi) {
                let (dk_g, dk_s, dk_p, dk_o) = Dictionary::unpack_quad(key);
                if dk_g != g_id {
                    continue;
                }
                // Post-filter for fields not covered by the range bounds.
                if let Some(sv) = s_id
                    && dk_s != sv
                {
                    continue;
                }
                if let Some(pv) = p_id
                    && dk_p != pv
                {
                    continue;
                }
                if let Some(ov) = o_id
                    && dk_o != ov
                {
                    continue;
                }

                seen.insert(key); // track tombstones too — suppresses Ring result
                if is_insert
                    && let Some(sq) = decode_stored_quad(&inner.dict, g_id, dk_s, dk_p, dk_o)
                {
                    results.push(sq);
                }
            }

            // 2. Ring entries not already handled by the delta.
            if let Some(ring) = inner.graphs.get(&g_id) {
                let matches = ring.match_triples(
                    s_id.map(|i| i.as_u64()),
                    p_id.map(|i| i.as_u64()),
                    o_id.map(|i| i.as_u64()),
                );
                for [rs, rp, ro] in matches {
                    let key = Dictionary::pack_quad(
                        g_id,
                        TermId::new(rs).unwrap_or(TermId::new(0).unwrap()),
                        TermId::new(rp).unwrap_or(TermId::new(0).unwrap()),
                        TermId::new(ro).unwrap_or(TermId::new(0).unwrap()),
                    );
                    if seen.contains(&key) {
                        continue; // tombstone or already-emitted delta insert
                    }
                    if let Some(sq) = decode_stored_quad(
                        &inner.dict,
                        g_id,
                        TermId::new(rs).unwrap(),
                        TermId::new(rp).unwrap(),
                        TermId::new(ro).unwrap(),
                    ) {
                        results.push(sq);
                    }
                }
            }
        }

        Ok(Box::new(results.into_iter().map(Ok)))
    }

    fn len(&self) -> Result<usize, Oxigraph> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;
        let ring_total: usize = inner.graphs.values().map(|r| r.n()).sum();
        let delta_inserts = inner.delta.insert_count();
        let delta_tombstones = inner.delta.tombstone_count();
        Ok(ring_total.saturating_sub(delta_tombstones) + delta_inserts)
    }

    fn contains(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;

        let g_id = match inner.dict.get_graph_id(&quad.graph_name) {
            None => return Ok(false),
            Some(id) => id,
        };
        let s_id = match inner.dict.get_id(&subject_to_term(&quad.subject)) {
            None => return Ok(false),
            Some(id) => id,
        };
        let p_id = match inner.dict.get_id(&Term::NamedNode(quad.predicate.clone())) {
            None => return Ok(false),
            Some(id) => id,
        };
        let o_id = match inner.dict.get_id(&quad.object) {
            None => return Ok(false),
            Some(id) => id,
        };

        let key = Dictionary::pack_quad(g_id, s_id, p_id, o_id);

        // Delta is authoritative if the key appears there.
        match inner.delta.get(key) {
            Some(true) => return Ok(true),
            Some(false) => return Ok(false), // tombstoned
            None => {}
        }

        // Fall through to Ring.
        if let Some(ring) = inner.graphs.get(&g_id) {
            return Ok(ring.contains(s_id.as_u64(), p_id.as_u64(), o_id.as_u64()));
        }

        Ok(false)
    }

    fn known_named_graphs(
        &self,
    ) -> Result<Box<dyn Iterator<Item = Result<GraphName, Oxigraph>> + '_>, Oxigraph> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;

        let mut seen: HashSet<u8> = HashSet::new();
        let mut graphs: Vec<GraphName> = Vec::new();

        // Graphs tracked via named_graph_ids (set during insert).
        for &gid in &inner.named_graph_ids {
            if seen.insert(gid.as_u8())
                && let Some(gn) = inner.dict.get_graph(gid)
            {
                graphs.push(gn.clone());
            }
        }

        // Any graphs found only in the Ring (from prior compactions).
        for &gid in inner.graphs.keys() {
            if gid.is_default() {
                continue;
            }
            if seen.insert(gid.as_u8())
                && let Some(gn) = inner.dict.get_graph(gid)
            {
                graphs.push(gn.clone());
            }
        }

        Ok(Box::new(graphs.into_iter().map(Ok)))
    }

    fn register_named_graph(&self, graph: &GraphName) -> Result<(), Oxigraph> {
        if let GraphName::NamedNode(_) = graph {
            let mut inner = self
                .inner
                .lock()
                .map_err(|e| Oxigraph::Storage(e.to_string()))?;
            inner.wal_append(&WalRecord::RegisterGraph(graph.clone()))?;
            inner.apply_register_graph(graph)?;
        }
        Ok(())
    }

    /// Bulk insert: acquires the lock **once** for the entire batch (not
    /// once per quad), applies every quad in-memory, and — for a persistent
    /// store — writes every resulting `WalRecord::InsertQuad` in a single
    /// `append_batch` call (one `fsync` for the whole batch instead of one
    /// per quad; see `wal.rs`'s "## Fsync policy" module docs). This is the
    /// path taken by SPARQL `INSERT DATA` with multiple triples.
    ///
    /// Durability ordering: every quad about to be applied — whether or not
    /// it turns out to already exist (`apply_insert` is idempotent, see its
    /// doc comment) — is logged to the WAL *before* any of them are applied
    /// to in-memory state, exactly mirroring the single-quad `insert()`'s
    /// "log intent durably BEFORE applying" discipline, just batched.
    fn extend(&self, quads: impl IntoIterator<Item = Quad>) -> Result<usize, Oxigraph> {
        let quads: Vec<Quad> = quads.into_iter().collect();
        if quads.is_empty() {
            return Ok(0);
        }

        let mut inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;

        // Log every quad's intent durably BEFORE applying any of them (one
        // batched fsync for the whole set, instead of one per quad).
        let records: Vec<WalRecord> = quads
            .iter()
            .map(|q| WalRecord::InsertQuad(q.clone()))
            .collect();
        inner.wal_append_batch(records.iter())?;

        let mut count = 0usize;
        for quad in &quads {
            if inner.apply_insert(quad)? {
                count += 1;
            }
        }
        inner.maybe_auto_compact()?;
        Ok(count)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{Literal, NamedNode};

    fn nn(s: &str) -> Subject {
        Subject::NamedNode(NamedNode::new_unchecked(s))
    }
    fn pred(s: &str) -> NamedNode {
        NamedNode::new_unchecked(s)
    }
    fn lit(s: &str) -> Term {
        Term::Literal(Literal::new_simple_literal(s))
    }
    fn dg() -> GraphName {
        GraphName::DefaultGraph
    }
    fn ng(s: &str) -> GraphName {
        GraphName::NamedNode(NamedNode::new_unchecked(s))
    }

    fn make_quad(s: &str, p: &str, o: &str, g: GraphName) -> Quad {
        Quad::new(nn(s), pred(p), lit(o), g)
    }

    #[test]
    fn insert_and_contains_delta_only() {
        let store = RingStore::new();
        let q = make_quad("http://ex/s", "http://ex/p", "hello", dg());
        assert!(store.insert(&q).unwrap());
        assert!(!store.insert(&q).unwrap()); // duplicate
        assert!(store.contains(&q).unwrap());
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn remove_from_delta() {
        let store = RingStore::new();
        let q = make_quad("http://ex/s", "http://ex/p", "hello", dg());
        store.insert(&q).unwrap();
        assert!(store.remove(&q).unwrap());
        assert!(!store.contains(&q).unwrap());
        assert_eq!(store.len().unwrap(), 0);
        assert!(!store.remove(&q).unwrap()); // already gone
    }

    #[test]
    fn insert_compact_contains_ring() {
        let store = RingStore::new();
        let q = make_quad("http://ex/s", "http://ex/p", "hello", dg());
        store.insert(&q).unwrap();
        store.compact().unwrap();
        // After compact, triple is in Ring, delta is empty.
        assert!(store.contains(&q).unwrap());
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn remove_from_ring_via_tombstone() {
        let store = RingStore::new();
        let q = make_quad("http://ex/s", "http://ex/p", "hello", dg());
        store.insert(&q).unwrap();
        store.compact().unwrap();
        assert!(store.remove(&q).unwrap());
        assert!(!store.contains(&q).unwrap());
        assert_eq!(store.len().unwrap(), 0);
    }

    #[test]
    fn re_insert_after_remove() {
        let store = RingStore::new();
        let q = make_quad("http://ex/s", "http://ex/p", "v", dg());
        store.insert(&q).unwrap();
        store.compact().unwrap();
        store.remove(&q).unwrap();
        assert!(store.insert(&q).unwrap()); // tombstone revived
        assert!(store.contains(&q).unwrap());
    }

    #[test]
    fn quads_for_pattern_wildcard() {
        let store = RingStore::new();
        let p = pred("http://ex/p");
        store
            .insert(&Quad::new(nn("http://ex/s1"), p.clone(), lit("a"), dg()))
            .unwrap();
        store
            .insert(&Quad::new(nn("http://ex/s2"), p.clone(), lit("b"), dg()))
            .unwrap();
        store.compact().unwrap();
        let res: Vec<_> = store
            .quads_for_pattern(None, Some(&p), None, None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn quads_mixed_ring_and_delta() {
        let store = RingStore::new();
        let p = pred("http://ex/p");
        store
            .insert(&Quad::new(nn("http://ex/s1"), p.clone(), lit("a"), dg()))
            .unwrap();
        store.compact().unwrap(); // s1 in Ring
        store
            .insert(&Quad::new(nn("http://ex/s2"), p.clone(), lit("b"), dg()))
            .unwrap(); // s2 in delta
        let res: Vec<_> = store
            .quads_for_pattern(None, Some(&p), None, None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn named_graphs_tracked() {
        let store = RingStore::new();
        let g = ng("http://ex/g");
        store
            .insert(&Quad::new(
                nn("http://ex/s"),
                pred("http://ex/p"),
                lit("v"),
                g.clone(),
            ))
            .unwrap();
        let known: Vec<_> = store
            .known_named_graphs()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(known.contains(&g));
    }

    #[test]
    fn quads_for_pattern_named_graph() {
        let store = RingStore::new();
        let g = ng("http://ex/g");
        let p = pred("http://ex/p");
        store
            .insert(&Quad::new(
                nn("http://ex/s"),
                p.clone(),
                lit("in_g"),
                g.clone(),
            ))
            .unwrap();
        store
            .insert(&Quad::new(nn("http://ex/s"), p.clone(), lit("in_dg"), dg()))
            .unwrap();

        let res_g: Vec<_> = store
            .quads_for_pattern(None, None, None, Some(&g))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(res_g.len(), 1);

        let res_dg: Vec<_> = store
            .quads_for_pattern(None, None, None, Some(&dg()))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(res_dg.len(), 1);
    }

    #[test]
    fn is_empty_and_len() {
        let store = RingStore::new();
        assert!(store.is_empty().unwrap());
        let q = make_quad("http://ex/s", "http://ex/p", "v", dg());
        store.insert(&q).unwrap();
        assert!(!store.is_empty().unwrap());
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn compact_multiple_graphs() {
        let store = RingStore::new();
        let p = pred("http://ex/p");
        let g1 = ng("http://ex/g1");
        let g2 = ng("http://ex/g2");
        store
            .insert(&Quad::new(
                nn("http://ex/s"),
                p.clone(),
                lit("a"),
                g1.clone(),
            ))
            .unwrap();
        store
            .insert(&Quad::new(
                nn("http://ex/s"),
                p.clone(),
                lit("b"),
                g2.clone(),
            ))
            .unwrap();
        store.compact().unwrap();
        assert_eq!(store.len().unwrap(), 2);
        let res: Vec<_> = store
            .quads_for_pattern(None, None, None, None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(res.len(), 2);
    }

    // ── RingStore::open() persistence round-trip ────────────────────────────

    fn temp_dir(name: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("nova_ringstore_test_{pid}_{n}_{name}"))
    }

    #[test]
    fn open_reopen_round_trip() {
        let dir = temp_dir("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);

        let g = ng("http://ex/g");
        let q1 = make_quad("http://ex/s1", "http://ex/p", "a", dg());
        let q2 = make_quad("http://ex/s2", "http://ex/p", "b", g.clone());

        {
            let store = RingStore::open(&dir).unwrap();
            store.insert(&q1).unwrap();
            store.insert(&q2).unwrap();
            store.remove(&q1).unwrap();
            store.insert(&q1).unwrap(); // re-insert after remove
            assert_eq!(store.len().unwrap(), 2);
        } // store dropped — WAL file remains on disk

        {
            let store = RingStore::open(&dir).unwrap();
            assert_eq!(store.len().unwrap(), 2);
            assert!(store.contains(&q1).unwrap());
            assert!(store.contains(&q2).unwrap());

            // Subsequent writes on the reopened store must also persist.
            let q3 = make_quad("http://ex/s3", "http://ex/p", "c", dg());
            store.insert(&q3).unwrap();
            assert_eq!(store.len().unwrap(), 3);
        }

        {
            let store = RingStore::open(&dir).unwrap();
            assert_eq!(store.len().unwrap(), 3);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_fresh_dir_is_empty() {
        let dir = temp_dir("fresh");
        let _ = std::fs::remove_dir_all(&dir);
        let store = RingStore::open(&dir).unwrap();
        assert!(store.is_empty().unwrap());
        assert!(RingStore::wal_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_reopen_after_compact_uses_snapshot() {
        let dir = temp_dir("snapshot_roundtrip");
        let _ = std::fs::remove_dir_all(&dir);

        let g = ng("http://ex/g");
        let q1 = make_quad("http://ex/s1", "http://ex/p", "a", dg());
        let q2 = make_quad("http://ex/s2", "http://ex/p", "b", g.clone());
        let q3 = make_quad("http://ex/s3", "http://ex/p", "c", dg());

        {
            let store = RingStore::open(&dir).unwrap();
            store.insert(&q1).unwrap();
            store.insert(&q2).unwrap();
            store.compact().unwrap(); // writes nova.snapshot.1, rotates WAL, commits MANIFEST
            store.insert(&q3).unwrap(); // lands back in delta, post-snapshot
            assert_eq!(store.len().unwrap(), 3);
        }

        // The generation-1 snapshot file must exist after compact().
        assert!(manifest::snapshot_path(&dir, 1).exists());
        // The MANIFEST must exist and point at generation 1 / segment 2.
        assert!(dir.join(manifest::MANIFEST_FILE_NAME).exists());

        {
            // Reopen: loads snapshot gen 1 (q1, q2) then replays only the
            // post-compaction segment (q3) — not the whole WAL history.
            let store = RingStore::open(&dir).unwrap();
            assert_eq!(store.len().unwrap(), 3);
            assert!(store.contains(&q1).unwrap());
            assert!(store.contains(&q2).unwrap());
            assert!(store.contains(&q3).unwrap());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compact_rotates_wal_and_deletes_old_segment() {
        let dir = temp_dir("wal_rotation");
        let _ = std::fs::remove_dir_all(&dir);

        let q1 = make_quad("http://ex/s1", "http://ex/p", "a", dg());
        let store = RingStore::open(&dir).unwrap();
        store.insert(&q1).unwrap();

        let old_segment = manifest::wal_segment_path(&dir, 1);
        assert!(old_segment.exists());

        store.compact().unwrap();

        // Old segment (fully covered by the new snapshot) must be gone;
        // a new segment must exist.
        assert!(!old_segment.exists());
        assert!(manifest::wal_segment_path(&dir, 2).exists());
        assert!(manifest::snapshot_path(&dir, 1).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multiple_compactions_leave_only_latest_generation() {
        let dir = temp_dir("multi_compact");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RingStore::open(&dir).unwrap();
        for i in 0..5 {
            let q = make_quad(&format!("http://ex/s{i}"), "http://ex/p", "v", dg());
            store.insert(&q).unwrap();
            store.compact().unwrap();
        }

        assert_eq!(store.len().unwrap(), 5);

        // Only the latest snapshot generation (5) and latest WAL segment (6)
        // should remain; everything older was cleaned up.
        for generation in 1..5 {
            assert!(!manifest::snapshot_path(&dir, generation).exists());
        }

        assert!(manifest::snapshot_path(&dir, 5).exists());
        for seq in 1..6 {
            assert!(!manifest::wal_segment_path(&dir, seq).exists());
        }
        assert!(manifest::wal_segment_path(&dir, 6).exists());

        // Reopen must still see all 5 triples via just the latest snapshot.
        drop(store);
        let store2 = RingStore::open(&dir).unwrap();
        assert_eq!(store2.len().unwrap(), 5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_compact_triggers_at_threshold() {
        let dir = temp_dir("auto_compact");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RingStore::open(&dir).unwrap();
        store.set_auto_compact_threshold(3);

        for i in 0..3 {
            let q = make_quad(&format!("http://ex/s{i}"), "http://ex/p", "v", dg());
            store.insert(&q).unwrap();
        }

        // The third insert should have triggered an automatic compaction:
        // a snapshot generation should now exist and the delta should have
        // been folded into it.
        assert!(manifest::snapshot_path(&dir, 1).exists());
        assert_eq!(store.len().unwrap(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extend_single_lock_and_batch_fsync_round_trips() {
        let dir = temp_dir("extend_batch");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RingStore::open(&dir).unwrap();
        let quads: Vec<Quad> = (0..50)
            .map(|i| make_quad(&format!("http://ex/s{i}"), "http://ex/p", "v", dg()))
            .collect();
        let count = store.extend(quads.clone()).unwrap();
        assert_eq!(count, 50);
        assert_eq!(store.len().unwrap(), 50);

        drop(store);
        let store2 = RingStore::open(&dir).unwrap();
        assert_eq!(store2.len().unwrap(), 50);
        for q in &quads {
            assert!(store2.contains(q).unwrap());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_policy_interval_writes_are_eventually_durable() {
        let dir = temp_dir("interval_policy");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RingStore::open(&dir).unwrap();
        store.set_sync_policy(SyncPolicy::Interval(Duration::from_millis(20)));

        let q = make_quad("http://ex/s", "http://ex/p", "v", dg());
        store.insert(&q).unwrap();

        // Give the background flusher a chance to run at least once.
        std::thread::sleep(Duration::from_millis(100));

        // Explicit flush to be certain, then reopen and confirm durability.
        store.flush_wal().unwrap();
        drop(store);

        let store2 = RingStore::open(&dir).unwrap();
        assert!(store2.contains(&q).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Isolation semantics ──────────────────────────────────────────────────
    //
    // `RingStore` gives per-call atomicity for free (each `insert`/`remove`/
    // `quads_for_pattern` call takes the single `Mutex<RingStoreInner>` once
    // and returns a result computed entirely under that one lock
    // acquisition), but it does **not** give "repeatable read" isolation
    // across *multiple* calls that together make up one logical
    // query/Update. This test deterministically demonstrates that: a write
    // committed strictly between two `quads_for_pattern` calls belonging to
    // the same hypothetical multi-pattern query **is** visible to the second
    // call, even though — under a true "fixed snapshot for the whole
    // operation" guarantee (the semantics upstream Oxigraph documents) — it
    // should not be. See the module doc comment above for the full write-up
    // of why this is an accepted, documented limitation rather than a bug.
    #[test]
    fn multi_call_scan_does_not_get_a_whole_query_repeatable_read_snapshot() {
        let store = Arc::new(RingStore::new());
        let p = pred("http://ex/p");
        let quad_a = Quad::new(nn("http://ex/a"), p.clone(), lit("a"), dg());
        let quad_b = Quad::new(nn("http://ex/b"), p.clone(), lit("b"), dg());

        store.insert(&quad_a).unwrap();

        // First "triple pattern" scan of a logical multi-pattern query,
        // matching only quad_a (quad_b doesn't exist yet).
        let result1: Vec<_> = store
            .quads_for_pattern(None, Some(&p), Some(&lit("a")), None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(result1.len(), 1);

        // A concurrent writer commits quad_b *between* the two scans that
        // conceptually belong to the same logical query. `.join()` gives a
        // real happens-before edge, so this is deterministic, not a timing
        // race.
        let store2 = Arc::clone(&store);
        let quad_b2 = quad_b.clone();
        std::thread::spawn(move || {
            store2.insert(&quad_b2).unwrap();
        })
        .join()
        .unwrap();

        // Second "triple pattern" scan of the *same* logical query. Under a
        // real repeatable-read/fixed-snapshot guarantee, quad_b — committed
        // after the logical query began — must NOT be visible here. It
        // demonstrably is, because each `quads_for_pattern` call is its own
        // independent lock acquisition with no cross-call snapshot.
        let result2: Vec<_> = store
            .quads_for_pattern(None, Some(&p), Some(&lit("b")), None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            result2.len(),
            1,
            "quad_b, inserted strictly between the two scans, is visible to the second \
             scan — proving RingStore does NOT provide a repeatable-read/fixed-snapshot \
             guarantee across multiple store calls belonging to one logical query or Update"
        );
    }

    #[test]
    fn open_survives_torn_tail() {
        let dir = temp_dir("torn");
        let _ = std::fs::remove_dir_all(&dir);

        let q1 = make_quad("http://ex/s1", "http://ex/p", "a", dg());
        let q2 = make_quad("http://ex/s2", "http://ex/p", "b", dg());

        {
            let store = RingStore::open(&dir).unwrap();
            store.insert(&q1).unwrap();
            store.insert(&q2).unwrap();
        }

        // Simulate a crash mid-write: truncate the last few bytes of the
        // (still-fresh, un-rotated) WAL segment.
        let wal_path = RingStore::wal_path(&dir);
        let full_len = std::fs::metadata(&wal_path).unwrap().len();
        {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&wal_path)
                .unwrap();
            file.set_len(full_len - 5).unwrap();
        }

        let store = RingStore::open(&dir).unwrap();
        assert_eq!(store.len().unwrap(), 1);
        assert!(store.contains(&q1).unwrap());
        assert!(!store.contains(&q2).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_round_trips_into_independent_store() {
        let src_dir = temp_dir("backup_src");
        let dst_dir = temp_dir("backup_dst");
        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&dst_dir);

        let g = ng("http://ex/g");
        let q1 = make_quad("http://ex/s1", "http://ex/p", "a", dg());
        let q2 = make_quad("http://ex/s2", "http://ex/p", "b", g.clone());

        let store = RingStore::open(&src_dir).unwrap();
        store.insert(&q1).unwrap();
        store.insert(&q2).unwrap();
        store.compact().unwrap();
        // A post-compaction write, still in the WAL tail (not yet snapshotted).
        let q3 = make_quad("http://ex/s3", "http://ex/p", "c", dg());
        store.insert(&q3).unwrap();

        store.backup(&dst_dir).unwrap();

        // Backup must be independently openable and contain everything.
        let restored = RingStore::open(&dst_dir).unwrap();
        assert_eq!(restored.len().unwrap(), 3);
        assert!(restored.contains(&q1).unwrap());
        assert!(restored.contains(&q2).unwrap());
        assert!(restored.contains(&q3).unwrap());

        // The backup must be independent: further writes to the source
        // must not affect the backup, and vice versa.
        let q4 = make_quad("http://ex/s4", "http://ex/p", "d", dg());
        store.insert(&q4).unwrap();
        assert!(!restored.contains(&q4).unwrap());

        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&dst_dir);
    }

    #[test]
    fn backup_of_in_memory_store_errors() {
        let store = RingStore::new();
        let dst_dir = temp_dir("backup_in_memory_dst");
        let _ = std::fs::remove_dir_all(&dst_dir);
        assert!(store.backup(&dst_dir).is_err());
    }

    #[test]
    fn extend_torn_batch_tail_replays_none_of_the_batch() {
        // A batch's records are each independently framed, so a torn write
        // mid-batch behaves exactly like any other torn write: replay stops
        // at the first bad frame. If the tear happens to land such that NO
        // record in the batch is fully intact, none of the batch survives —
        // this test picks a small batch and truncates enough bytes to tear
        // through all of it, confirming no partial/corrupt state results.
        let dir = temp_dir("extend_torn");
        let _ = std::fs::remove_dir_all(&dir);

        let q_before = make_quad("http://ex/before", "http://ex/p", "v", dg());
        {
            let store = RingStore::open(&dir).unwrap();
            store.insert(&q_before).unwrap();
        }

        let before_len = std::fs::metadata(RingStore::wal_path(&dir)).unwrap().len();

        {
            let store = RingStore::open(&dir).unwrap();
            let quads: Vec<Quad> = (0..10)
                .map(|i| make_quad(&format!("http://ex/batch{i}"), "http://ex/p", "v", dg()))
                .collect();
            store.extend(quads).unwrap();
        }

        let full_len = std::fs::metadata(RingStore::wal_path(&dir)).unwrap().len();
        assert!(full_len > before_len);

        // Truncate everything the batch wrote, minus a few bytes, so the
        // batch's tail is torn but the pre-existing record is untouched.
        {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(RingStore::wal_path(&dir))
                .unwrap();
            file.set_len(before_len + 5).unwrap();
        }

        let store = RingStore::open(&dir).unwrap();
        assert!(store.contains(&q_before).unwrap());
        assert_eq!(
            store.len().unwrap(),
            1,
            "torn batch tail must not partially apply — only the pre-existing record survives"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
