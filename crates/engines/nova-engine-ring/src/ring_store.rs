//! Cyclic QWT [`RingStore`] `QuadStore` (Phase 5).
//!
//! Wires term dictionary + live LSM delta + per-graph
//! [`BraidedGraphImage`] into [`QuadStore`] / [`LftjSource`].
//!
//! ## Scope
//!
//! - **Mutable lifecycle parity with LOUDS** — `new()` is pure in-memory;
//!   `open(dir)` enables WAL + MANIFEST + generation-numbered per-graph
//!   `NOVARNG1` images + dict snapshots via `nova-storage`.
//! - **Not** the default SPARQL backend (`nova-store` still pins
//!   `LoudsStore`).
//! - Compaction rebuilds each graph as a Braided Ring image from
//!   external `TermId` triples (dense remap via [`BraidedGraphImage`]),
//!   then materializes `NOVARNG1` mmap. Persistent stores write durable
//!   generation files (`nova.ring.<gen>.<gid>` + `nova.ringmap.<gen>.<gid>`)
//!   and rotate the WAL under an atomic MANIFEST commit.
//! - LFTJ is supported **only when the delta is empty** (same contract as
//!   LOUDS: joins run on the fully compacted index).
//!
//! Pattern scans always merge ring ∪ delta \ tombstones and are correct
//! with a non-empty delta.
//!
//! "Braided" in related types/docs is the D2 intersection algorithm, not
//! this store's product name.

use crate::image::BraidedGraphImage;
use crate::prepared_plan_cache::{
    PREPARED_PLAN_CACHE_CAP, PhysicalOpPreparedPlanCache, get_or_prepare_k_chain,
    get_or_prepare_sp_expansion, get_or_prepare_star, get_or_prepare_two_hop, get_or_prepare_wedge,
};
use crate::product_path::{
    SPARQL_PATH, bump_mmap_ok, log_mmap_fail_once, ring_counters_log_enabled, ring_d2_enabled,
    ring_image_dir, ring_mmap_enabled,
};
use crate::scan::{PredicateAdjacency, PreparedPredD1, PreparedSpObjectScanImpl};
use oxigraph_nova_core::{
    Dictionary, EmptyTrieIter, GRAPH_DEFAULT, GraphId, GraphName, LftjSource, NamedNode, Oxigraph,
    PhysicalShape, PreparedPhysicalOperator, PreparedPredObjectIntersect, PreparedSpObjectScan,
    Quad, QuadOp, QuadStore, StorageEngine, StoredQuad, Subject, SyncPolicy, Term, TermId,
    TrieIterator,
};
use oxigraph_nova_engine_louds::delta::Delta;
#[cfg(feature = "fulltext")]
use oxigraph_nova_engine_louds::fulltext;
#[cfg(feature = "fulltext")]
use oxigraph_nova_fulltext::FulltextIndex;
use oxigraph_nova_storage::dict_snapshot;
use oxigraph_nova_storage::manifest::{self, Manifest};
use oxigraph_nova_storage::wal::{self, WalRecord, WalWriter};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering, Ordering};
use std::time::Duration;

/// Default delta-size threshold that triggers automatic inline compaction for
/// a persistent (`RingStore::open`) store. In-memory (`RingStore::new`) stores
/// never auto-compact.
const DEFAULT_AUTO_COMPACT_THRESHOLD: usize = 1_000_000;

/// How many quads `RingStore::bulk_load_with_progress` consumes between each
/// `on_progress` callback invocation. Matches LOUDS /
/// upstream Oxigraph's order of magnitude.
const PROGRESS_REPORT_INTERVAL: usize = 1_000_000;

/// Background flusher thread for `SyncPolicy::Interval` (mirrors LOUDS).
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

/// Cross-request SP→O adjacency tables (K9.4), keyed by
/// (snapshot_version, graph_id, predicate). Built once on cold miss; warm
/// HTTP 2join reuses `Arc` without the ~50–100 ms universe walk.
struct SpAdjCache {
    /// Cap keeps memory bounded (one entry ≈ universe × sizeof Option<RowRange>).
    cap: usize,
    map: HashMap<(u64, u8, u64), Arc<PredicateAdjacency>>,
    order: Vec<(u64, u8, u64)>,
}

impl SpAdjCache {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            map: HashMap::new(),
            order: Vec::new(),
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    fn get_or_insert(
        &mut self,
        key: (u64, u8, u64),
        build: impl FnOnce() -> Option<Arc<PredicateAdjacency>>,
    ) -> Option<Arc<PredicateAdjacency>> {
        if let Some(a) = self.map.get(&key) {
            return Some(Arc::clone(a));
        }
        let a = build()?;
        if self.map.len() >= self.cap
            && let Some(old) = self.order.first().copied()
        {
            self.order.remove(0);
            self.map.remove(&old);
        }
        self.map.insert(key, Arc::clone(&a));
        self.order.push(key);
        Some(a)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[inline]
fn subject_to_term(s: &Subject) -> Term {
    Term::from(s.clone())
}

fn decode_stored_quad(
    dict: &Dictionary,
    g_id: GraphId,
    s_id: TermId,
    p_id: TermId,
    o_id: TermId,
) -> Option<StoredQuad> {
    let graph_name = dict.get_graph(g_id)?.clone();
    let s_term = dict.get_term_arc(s_id)?;
    let p_term = dict.get_term_arc(p_id)?;
    let o_term = dict.get_term_arc(o_id)?;
    match s_term.as_ref() {
        Term::NamedNode(_) | Term::BlankNode(_) | Term::Triple(_) => {}
        Term::Literal(_) => return None,
    };
    let predicate: NamedNode = match p_term.as_ref() {
        Term::NamedNode(n) => n.clone(),
        _ => return None,
    };
    Some(StoredQuad {
        subject: s_term,
        predicate,
        object: o_term,
        graph_name,
    })
}

// ── Inner state ───────────────────────────────────────────────────────────────

/// W3 decode table: TermId → Arc<Term> frozen at compact time.
type DecodeSnapshot = Arc<Vec<Option<Arc<Term>>>>;

struct RingStoreInner {
    dict: Dictionary,
    /// Per-graph compacted Braided Ring images (external TermId coordinates).
    graphs: HashMap<GraphId, Arc<BraidedGraphImage>>,
    delta: Delta,
    named_graph_ids: HashSet<GraphId>,
    compaction_count: u64,

    /// WAL writer for the currently-active segment. Present only for
    /// persistent stores opened via [`RingStore::open`].
    wal: Option<WalWriter>,
    /// Data directory root for persistent stores (`None` for in-memory).
    data_dir: Option<PathBuf>,
    /// Generation of the currently-installed ring snapshot (`0` = none).
    snapshot_gen: u64,
    /// Segment number of the currently-active WAL file.
    wal_seq: u64,
    /// Delta-size threshold that triggers automatic inline compaction.
    auto_compact_threshold: usize,
    /// WAL durability policy (see [`SyncPolicy`]).
    sync_policy: SyncPolicy,
    /// Background flusher for `SyncPolicy::Interval`.
    flusher: Option<Flusher>,
    /// Opt-in Tantivy full-text index (see LOUDS `fulltext` glue). Present
    /// only when the `fulltext` cargo feature is enabled AND
    /// [`RingStore::enable_fulltext`] has been called.
    #[cfg(feature = "fulltext")]
    fulltext: Option<Arc<FulltextIndex>>,
}

impl RingStoreInner {
    fn new() -> Self {
        Self {
            dict: Dictionary::new(),
            graphs: HashMap::new(),
            delta: Delta::new(),
            named_graph_ids: HashSet::new(),
            compaction_count: 0,
            wal: None,
            data_dir: None,
            snapshot_gen: 0,
            wal_seq: 1,
            auto_compact_threshold: DEFAULT_AUTO_COMPACT_THRESHOLD,
            sync_policy: SyncPolicy::default(),
            flusher: None,
            #[cfg(feature = "fulltext")]
            fulltext: None,
        }
    }

    /// Append `record` to the WAL if this store is persistent.
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

    /// Append a batch of records with a single fsync (or none under Interval).
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

    /// (Re)spawn the background flusher for `SyncPolicy::Interval`.
    fn respawn_flusher(&mut self) {
        self.flusher = None;
        if let (SyncPolicy::Interval(interval), Some(w)) = (self.sync_policy, &self.wal)
            && let Ok(file) = w.try_clone_file()
        {
            self.flusher = Some(Flusher::spawn(file, interval));
        }
    }

    /// Auto-compact when a persistent store's delta crosses the threshold.
    fn maybe_auto_compact(&mut self) -> Result<Option<DecodeSnapshot>, Oxigraph> {
        if self.data_dir.is_some() && self.delta.len() >= self.auto_compact_threshold {
            return Ok(Some(self.compact_locked()?));
        }
        Ok(None)
    }

    /// Build an immutable TermId→Term table for lock-free LFTJ decode.
    fn build_decode_snapshot(&self) -> DecodeSnapshot {
        // After compact, dictionary high-water is stable until the next write.
        let n = self.dict.len();
        let mut table = Vec::with_capacity(n);
        for raw in 0..n as u64 {
            let tid = match TermId::new(raw) {
                Ok(t) => t,
                Err(_) => {
                    table.push(None);
                    continue;
                }
            };
            table.push(self.dict.get_term_arc(tid));
        }
        Arc::new(table)
    }

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

        if let Some(img) = self.graphs.get(&g_id)
            && image_contains(img, s_id.as_u64(), p_id.as_u64(), o_id.as_u64())
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

        if let Some(img) = self.graphs.get(&g_id)
            && image_contains(img, s_id.as_u64(), p_id.as_u64(), o_id.as_u64())
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

    /// Merge ring ∪ delta into new per-graph Braided images; clear delta.
    ///
    /// For in-memory stores: materialize temp `NOVARNG1` when `NOVA_RING_MMAP=1`.
    /// For persistent stores: crash-safe commit (write generation files + dict
    /// snapshot + WAL rotation + atomic MANIFEST), matching LOUDS.
    fn compact_locked(&mut self) -> Result<DecodeSnapshot, Oxigraph> {
        let mut per_graph: HashMap<GraphId, Vec<[u64; 3]>> = HashMap::new();

        // Ephemeral temp-image paths from the previous in-memory compact
        // (persistent generation files are cleaned by MANIFEST orphan cleanup).
        let old_temp_paths: Vec<_> = self
            .graphs
            .values()
            .filter_map(|g| {
                let p = g.image_path()?;
                // Only delete ephemeral temp images, never durable generation files.
                let name = p.file_name()?.to_string_lossy();
                if name.starts_with("g") && name.ends_with(".novarng1") {
                    Some(p.to_path_buf())
                } else {
                    None
                }
            })
            .collect();

        for (&g_id, img) in &self.graphs {
            per_graph.insert(g_id, img.enumerate_spo_external());
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

        // Incremental Tantivy indexing: walk exactly the delta entries this
        // compaction is already merging (before the delta is cleared below).
        // Same non-propagation policy as LOUDS: a Tantivy I/O error must not
        // abort the core Ring/Dictionary compaction — log + disable fulltext.
        #[cfg(feature = "fulltext")]
        if let Err(e) = self.index_delta_into_fulltext() {
            eprintln!(
                "[RingStore] fulltext indexing failed during compaction; \
                 disabling full-text search until enable_fulltext is called again: {e}"
            );
            self.fulltext = None;
        }

        // Compact the dictionary first so TermIds are dense for the remap.
        self.dict.compact()?;

        match self.data_dir.clone() {
            None => {
                // ── In-memory path ──────────────────────────────────────────
                let do_mmap = ring_mmap_enabled();
                let img_dir = if do_mmap {
                    let d = ring_image_dir();
                    let _ = std::fs::create_dir_all(&d);
                    Some(d)
                } else {
                    None
                };

                let mut new_graphs = HashMap::new();
                for (g_id, triples) in per_graph {
                    if triples.is_empty() {
                        continue;
                    }
                    let mut img = BraidedGraphImage::from_external_triples(&triples);
                    if let Some(ref dir) = img_dir {
                        let path = dir.join(format!(
                            "g{}_{}_{}.novarng1",
                            g_id.as_u8(),
                            self.compaction_count,
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos())
                                .unwrap_or(0)
                        ));
                        match img.materialize_mapped(&path) {
                            Ok(()) => bump_mmap_ok(),
                            Err(e) => log_mmap_fail_once(&e),
                        }
                    }
                    new_graphs.insert(g_id, Arc::new(img));
                }
                self.graphs = new_graphs;
            }
            Some(dir) => {
                // ── Persistent crash-safe commit ────────────────────────────
                let new_gen = self.snapshot_gen + 1;
                let new_seq = self.wal_seq + 1;

                // 1. Build + materialize each graph to durable generation paths.
                let mut new_graphs: HashMap<GraphId, Arc<BraidedGraphImage>> = HashMap::new();
                for (g_id, triples) in per_graph {
                    if triples.is_empty() {
                        continue;
                    }
                    let mut img = BraidedGraphImage::from_external_triples(&triples);
                    let img_path = manifest::ring_image_path(&dir, new_gen, g_id.as_u8());
                    let remap_path = manifest::ring_remap_path(&dir, new_gen, g_id.as_u8());
                    img.materialize_mapped(&img_path).map_err(|e| {
                        Oxigraph::Storage(format!(
                            "failed to materialize ring image {}: {e}",
                            img_path.display()
                        ))
                    })?;
                    img.write_remap(&remap_path).map_err(|e| {
                        Oxigraph::Storage(format!(
                            "failed to write ring remap {}: {e}",
                            remap_path.display()
                        ))
                    })?;
                    bump_mmap_ok();
                    new_graphs.insert(g_id, Arc::new(img));
                }

                // 1b. Persist the Dictionary alongside this generation.
                let dict_path = manifest::dict_path(&dir, new_gen);
                self.dict = dict_snapshot::write_and_load_mmap(&self.dict, &dict_path)?;

                // 2. Rotate the WAL to a fresh empty segment.
                let new_seg_path = manifest::wal_segment_path(&dir, new_seq);
                let new_wal = WalWriter::create_or_open(&new_seg_path).map_err(|e| {
                    Oxigraph::Storage(format!("failed to open new WAL segment: {e}"))
                })?;

                // 3. Atomic commit point: MANIFEST now points at (new_gen, new_seq).
                let manifest_path = dir.join(manifest::MANIFEST_FILE_NAME);
                let m = Manifest {
                    snapshot_gen: new_gen,
                    wal_seq: new_seq,
                };
                m.save(&manifest_path)?;

                // 4. Best-effort orphan cleanup.
                manifest::cleanup_orphans(&dir, new_gen, new_seq);

                self.graphs = new_graphs;
                self.wal = Some(new_wal);
                self.snapshot_gen = new_gen;
                self.wal_seq = new_seq;
                self.respawn_flusher();
            }
        }

        self.delta.clear();
        self.compaction_count = self.compaction_count.saturating_add(1);
        let snap = self.build_decode_snapshot();

        // Finalize Tantivy only after the Ring/Dictionary/WAL commit succeeded
        // (same non-propagation policy as LOUDS).
        #[cfg(feature = "fulltext")]
        if self.fulltext.is_some()
            && let Err(e) = self.finalize_fulltext_commit()
        {
            eprintln!(
                "[RingStore] fulltext commit failed after successful compaction; \
                 disabling full-text search until enable_fulltext is called again: {e}"
            );
            self.fulltext = None;
        }

        for p in old_temp_paths {
            let _ = std::fs::remove_file(p);
        }
        Ok(snap)
    }

    /// Add/remove literal-object documents for every entry currently in
    /// `self.delta`. No-op if fulltext isn't enabled. Does **not** call
    /// `FulltextIndex::commit` — that happens in `finalize_fulltext_commit`.
    #[cfg(feature = "fulltext")]
    fn index_delta_into_fulltext(&self) -> Result<(), Oxigraph> {
        let Some(ft) = &self.fulltext else {
            return Ok(());
        };
        for (&key, &is_insert) in self.delta.iter() {
            let (g_id, s_id, p_id, o_id) = Dictionary::unpack_quad(key);
            if is_insert {
                fulltext::index_quad_insert(ft, &self.dict, g_id, s_id, p_id, o_id)?;
            } else {
                fulltext::index_quad_remove(ft, g_id, s_id, p_id, o_id)?;
            }
        }
        Ok(())
    }

    /// Commit the pending Tantivy writer and — for a persistent store —
    /// record the now-current `snapshot_gen` in the generation marker.
    #[cfg(feature = "fulltext")]
    fn finalize_fulltext_commit(&self) -> Result<(), Oxigraph> {
        let Some(ft) = &self.fulltext else {
            return Ok(());
        };
        ft.commit()
            .map_err(|e| Oxigraph::Storage(format!("fulltext commit failed: {e}")))?;
        if let Some(dir) = &self.data_dir {
            fulltext::write_marker(dir, self.snapshot_gen)?;
        }
        Ok(())
    }
}

fn image_contains(img: &BraidedGraphImage, s: u64, p: u64, o: u64) -> bool {
    // Lead-range SP + O RNV — O(log σ), not full-graph enumerate.
    img.contains_external(s, p, o)
}

fn image_match_triples(
    img: &BraidedGraphImage,
    s: Option<u64>,
    p: Option<u64>,
    o: Option<u64>,
) -> Vec<[u64; 3]> {
    // Indexed lead-range walk (bound P → range_p only). Full enumerate only
    // when the pattern is fully unbound.
    img.match_triples_external(s, p, o)
}

// ── Public store ──────────────────────────────────────────────────────────────

/// Snapshot of compacted graph images for lock-free LFTJ / VEO (B).
///
/// Published outside `inner` when delta is empty so estimate/real_count/
/// join_scan never contend on the store mutex during leapfrog probes.
type GraphsSnapshot = Arc<HashMap<GraphId, Arc<BraidedGraphImage>>>;

/// In-memory cyclic QWT Ring store: Dictionary + Delta + per-graph
/// [`BraidedGraphImage`].
///
/// Feature `cyclic-ring`. Not wired into `nova-store` as the default
/// backend (that remains `LoudsStore`).
///
/// ## W3 decode path
///
/// After compact, [`Self::decode_snap`] holds an `Arc` term table published
/// **outside** the store mutex. LFTJ solution emit only needs a short
/// `RwLock` read (no contention with join_scan navigation on `inner`).
///
/// ## B graphs snapshot
///
/// After compact, [`Self::graphs_snap`] holds the same per-graph images as
/// `inner.graphs` so VEO / join_scan can Arc-clone a graph without locking
/// `inner`. Cleared on any write (delta non-empty).
pub struct RingStore {
    inner: Mutex<RingStoreInner>,
    /// W3: lock-free-ish decode when delta empty (None while dirty / pre-compact).
    decode_snap: RwLock<Option<DecodeSnapshot>>,
    /// B: compacted graphs when delta empty (None while dirty / pre-compact).
    graphs_snap: RwLock<Option<GraphsSnapshot>>,
    /// Phase L: reusable prepared physical operators (two-hop, wedge, …).
    physical_op_cache: Arc<Mutex<PhysicalOpPreparedPlanCache>>,
    /// Cross-request SP→O K9.4 adjacency (warm HTTP 2join).
    sp_adj_cache: Mutex<SpAdjCache>,
    /// Bumped on write/compact so cache keys cannot outlive store identity.
    snapshot_version: AtomicU64,
}

impl Default for RingStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RingStore {
    /// Empty in-memory store (no WAL / disk persistence).
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RingStoreInner::new()),
            decode_snap: RwLock::new(None),
            graphs_snap: RwLock::new(None),
            physical_op_cache: Arc::new(Mutex::new(PhysicalOpPreparedPlanCache::new(
                PREPARED_PLAN_CACHE_CAP,
            ))),
            // A few fixed predicates (P131 / related / …) cover RESULTS_MEM shapes.
            sp_adj_cache: Mutex::new(SpAdjCache::new(16)),
            snapshot_version: AtomicU64::new(0),
        }
    }

    /// Open (or create) a persistent `RingStore` rooted at `dir`.
    ///
    /// Recovery procedure (same as LOUDS):
    /// 1. Load `nova.manifest` (absent ⇒ fresh: `snapshot_gen=0`, `wal_seq=1`).
    /// 2. If `snapshot_gen > 0`, load dict snapshot + every
    ///    `nova.ring.<gen>.<gid>` / `nova.ringmap.<gen>.<gid>` pair.
    /// 3. Replay WAL segments with `seq >= wal_seq` in ascending order.
    /// 4. Open the active segment for appending; cleanup orphans.
    pub fn open(dir: &Path) -> Result<Self, Oxigraph> {
        std::fs::create_dir_all(dir)?;
        let manifest_path = dir.join(manifest::MANIFEST_FILE_NAME);
        let m = Manifest::load(&manifest_path, dir)?;

        let mut inner = RingStoreInner::new();
        if m.snapshot_gen > 0 {
            // Reconstruct Dictionary first so TermId space is stable for replay.
            let dict_path = manifest::dict_path(dir, m.snapshot_gen);
            inner.dict = dict_snapshot::load_mmap_from_file(&dict_path)?;

            // Load every per-graph ring image for this generation.
            let prefix = format!("nova.ring.{}.", m.snapshot_gen);
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if let Some(gid_s) = name.strip_prefix(&prefix)
                        && let Ok(gid_u8) = gid_s.parse::<u8>()
                    {
                        let img_path = entry.path();
                        let remap_path = manifest::ring_remap_path(dir, m.snapshot_gen, gid_u8);
                        let img = BraidedGraphImage::open_mapped_with_remap(&img_path, &remap_path)
                            .map_err(|e| {
                                Oxigraph::Storage(format!(
                                    "failed to open ring image {}: {e}",
                                    img_path.display()
                                ))
                            })?;
                        let g_id = GraphId(gid_u8);
                        // Named graphs present in the compacted tier.
                        if !g_id.is_default() {
                            inner.named_graph_ids.insert(g_id);
                        }
                        inner.graphs.insert(g_id, Arc::new(img));
                    }
                }
            }
        }

        // Discover + replay WAL segments at or after the MANIFEST's wal_seq.
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

        let active_seq = segments.last().map(|(seq, _)| *seq).unwrap_or(m.wal_seq);
        let active_path = manifest::wal_segment_path(dir, active_seq);
        inner.wal = Some(
            WalWriter::create_or_open(&active_path)
                .map_err(|e| Oxigraph::Storage(format!("failed to open WAL for writing: {e}")))?,
        );
        inner.data_dir = Some(dir.to_path_buf());
        inner.snapshot_gen = m.snapshot_gen;
        inner.wal_seq = active_seq;
        inner.respawn_flusher();

        manifest::cleanup_orphans(dir, inner.snapshot_gen, inner.wal_seq);

        let store = Self {
            inner: Mutex::new(inner),
            decode_snap: RwLock::new(None),
            graphs_snap: RwLock::new(None),
            physical_op_cache: Arc::new(Mutex::new(PhysicalOpPreparedPlanCache::new(
                PREPARED_PLAN_CACHE_CAP,
            ))),
            sp_adj_cache: Mutex::new(SpAdjCache::new(16)),
            snapshot_version: AtomicU64::new(0),
        };

        // If we loaded a snapshot with empty delta, publish compact-time snaps
        // so LFTJ is immediately available (same as post-compact).
        {
            let inner = store.inner.lock();
            if inner.delta.is_empty() && !inner.graphs.is_empty() {
                let decode = inner.build_decode_snapshot();
                let graphs: GraphsSnapshot = Arc::new(inner.graphs.clone());
                drop(inner);
                store.publish_after_compact(decode, graphs);
            }
        }

        Ok(store)
    }

    /// Set the WAL durability policy. No-op for in-memory stores.
    pub fn set_sync_policy(&self, policy: SyncPolicy) {
        let mut inner = self.inner.lock();
        inner.sync_policy = policy;
        inner.respawn_flusher();
    }

    /// Explicitly fsync the active WAL file. No-op for in-memory stores.
    pub fn flush_wal(&self) -> Result<(), Oxigraph> {
        let inner = self.inner.lock();
        if let Some(w) = &inner.wal {
            w.sync()
                .map_err(|e| Oxigraph::Storage(format!("WAL flush failed: {e}")))?;
        }
        Ok(())
    }

    /// On-disk path of the WAL segment a fresh `open(dir)` would use (segment 1).
    pub fn wal_path(dir: &Path) -> PathBuf {
        manifest::wal_segment_path(dir, 1)
    }

    /// Configure the auto-compact delta-size threshold (persistent stores only).
    pub fn set_auto_compact_threshold(&self, threshold: usize) {
        let mut inner = self.inner.lock();
        inner.auto_compact_threshold = threshold;
    }

    /// Mirrors upstream Oxigraph's `Store::backup(destination)` and
    /// [`oxigraph_nova_engine_louds::LoudsStore::backup`]: after this call
    /// returns, [`RingStore::open`]`(destination)` recovers exactly the same
    /// data as the source store had at the moment `backup` was called, and
    /// the backup is a fully independent store thereafter (no shared file
    /// handles or further coupling to the source).
    ///
    /// Only valid for a **persistent** store (opened via [`RingStore::open`]);
    /// returns an error for an in-memory (`RingStore::new()`) store, since
    /// there is no on-disk state to copy.
    ///
    /// ## Consistency
    ///
    /// The entire operation runs under the single `Mutex<RingStoreInner>` lock
    /// (the same lock every read/write already serialises through), so no
    /// concurrent write or compaction can rotate the generation/WAL-segment
    /// files out from under the copy. Before copying, the active WAL segment
    /// is explicitly `fsync`ed (regardless of the current [`SyncPolicy`]) so
    /// the backup never misses a write that had already been acknowledged to
    /// a caller.
    ///
    /// Only the files the current MANIFEST actually references are copied
    /// (the MANIFEST itself, the current generation's `nova.dict.<gen>`,
    /// every `nova.ring.<gen>.<gid>` / `nova.ringmap.<gen>.<gid>` pair for
    /// graphs present in the compacted tier, and the active `nova.wal.<seq>`
    /// segment) — any orphaned older-generation files still sitting in the
    /// source directory (pending best-effort cleanup) are intentionally not
    /// copied.
    pub fn backup(&self, destination: &Path) -> Result<(), Oxigraph> {
        let inner = self.inner.lock();

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
            let dict_src = manifest::dict_path(&dir, inner.snapshot_gen);
            if dict_src.exists() {
                std::fs::copy(
                    &dict_src,
                    manifest::dict_path(destination, inner.snapshot_gen),
                )?;
            }

            // Copy every per-graph ring image + remap for the live generation.
            // Prefer the in-memory graph set (authoritative for which gids
            // exist at this generation); fall back to directory scan if the
            // compacted tier is somehow empty while snapshot_gen > 0.
            let mut gids: Vec<u8> = inner.graphs.keys().map(|g| g.as_u8()).collect();
            if gids.is_empty() {
                let prefix = format!("nova.ring.{}.", inner.snapshot_gen);
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for entry in rd.flatten() {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        if let Some(gid_s) = name.strip_prefix(&prefix)
                            && let Ok(gid_u8) = gid_s.parse::<u8>()
                        {
                            gids.push(gid_u8);
                        }
                    }
                }
            }
            gids.sort_unstable();
            gids.dedup();

            for gid in gids {
                let img_src = manifest::ring_image_path(&dir, inner.snapshot_gen, gid);
                if img_src.exists() {
                    std::fs::copy(
                        &img_src,
                        manifest::ring_image_path(destination, inner.snapshot_gen, gid),
                    )?;
                }
                let remap_src = manifest::ring_remap_path(&dir, inner.snapshot_gen, gid);
                if remap_src.exists() {
                    std::fs::copy(
                        &remap_src,
                        manifest::ring_remap_path(destination, inner.snapshot_gen, gid),
                    )?;
                }
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

    #[inline]
    fn publish_decode(&self, snap: Option<DecodeSnapshot>) {
        *self.decode_snap.write() = snap;
    }

    #[inline]
    fn clear_decode(&self) {
        *self.decode_snap.write() = None;
    }

    #[inline]
    fn publish_graphs(&self, snap: Option<GraphsSnapshot>) {
        *self.graphs_snap.write() = snap;
    }

    #[inline]
    fn clear_graphs_snap(&self) {
        *self.graphs_snap.write() = None;
    }

    /// Clear both outside-mutex snapshots (any live write dirties compact state).
    #[inline]
    fn clear_compact_snaps(&self) {
        self.clear_decode();
        self.clear_graphs_snap();
        // Phase L: drop prepared physical ops keyed by the prior snapshot generation.
        self.physical_op_cache.lock().clear();
        self.sp_adj_cache.lock().clear();
        self.snapshot_version.fetch_add(1, AtomicOrdering::Relaxed);
    }

    /// Publish decode + graphs snapshots after a successful compact.
    fn publish_after_compact(&self, decode: DecodeSnapshot, graphs: GraphsSnapshot) {
        // Compact rebuilds images — invalidate any plans held against old images.
        self.physical_op_cache.lock().clear();
        self.sp_adj_cache.lock().clear();
        self.snapshot_version.fetch_add(1, AtomicOrdering::Relaxed);
        self.publish_decode(Some(decode));
        self.publish_graphs(Some(graphs));
    }

    /// Try to take a graph image from the lock-free snapshot (delta empty).
    #[inline]
    fn graph_from_snap(&self, graph_id: u8) -> Option<Arc<BraidedGraphImage>> {
        let guard = self.graphs_snap.read();
        let snap = guard.as_ref()?;
        snap.get(&GraphId(graph_id)).map(Arc::clone)
    }

    /// Merge delta into Braided Ring images and clear the delta.
    pub fn compact(&self) -> Result<(), Oxigraph> {
        let (snap, graphs) = {
            let mut inner = self.inner.lock();
            let snap = inner.compact_locked()?;
            let graphs: GraphsSnapshot = Arc::new(inner.graphs.clone());
            (snap, graphs)
        };
        self.publish_after_compact(snap, graphs);
        Ok(())
    }

    /// Live delta entry count (inserts + tombstones).
    pub fn delta_len(&self) -> usize {
        self.inner.lock().delta.len()
    }

    /// Number of successful [`Self::compact`] calls on this store.
    pub fn compaction_count(&self) -> u64 {
        self.inner.lock().compaction_count
    }

    /// Compacted triple count across all graphs (excludes uncompacted delta).
    pub fn ring_triple_count(&self) -> usize {
        self.inner
            .lock()
            .graphs
            .values()
            .map(|g| g.n() as usize)
            .sum()
    }

    /// Total live triple count (ring ∪ delta, same as [`QuadStore::len`]).
    pub fn triple_count(&self) -> usize {
        self.len().unwrap_or(0)
    }

    /// Bulk-insert quads into the delta, then compact into Braided images.
    ///
    /// Returns the number of **input quads consumed** (same unit as LOUDS /
    /// upstream Oxigraph `BulkLoader::on_progress` — not "new inserts only").
    /// Intended for `nova_serve --file` / external harness loads.
    ///
    /// For a persistent store every insert is WAL-logged (batch) before the
    /// in-memory apply, then a crash-safe compact commits a new generation.
    pub fn bulk_load(&self, quads: impl IntoIterator<Item = Quad>) -> Result<usize, Oxigraph> {
        self.bulk_load_with_progress(quads, None)
    }

    /// Same as [`RingStore::bulk_load`], additionally invoking `on_progress`
    /// (if given) periodically with the number of quads consumed so far —
    /// mirroring LOUDS / upstream Oxigraph's `BulkLoader::on_progress`.
    ///
    /// The callback fires every [`PROGRESS_REPORT_INTERVAL`] quads, plus once
    /// more at the end if the final total didn't land on the interval boundary
    /// (including the empty-input case).
    pub fn bulk_load_with_progress(
        &self,
        quads: impl IntoIterator<Item = Quad>,
        mut on_progress: Option<&mut dyn FnMut(u64)>,
    ) -> Result<usize, Oxigraph> {
        // Writes dirty compact-time snapshots until republish.
        self.clear_compact_snaps();

        // Stream through the input once: collect for WAL batch + progress.
        let mut collected: Vec<Quad> = Vec::new();
        let mut consumed = 0usize;
        for quad in quads {
            consumed += 1;
            if let Some(cb) = on_progress.as_deref_mut()
                && consumed.is_multiple_of(PROGRESS_REPORT_INTERVAL)
            {
                cb(consumed as u64);
            }
            collected.push(quad);
        }
        if let Some(cb) = on_progress
            && !consumed.is_multiple_of(PROGRESS_REPORT_INTERVAL)
        {
            cb(consumed as u64);
        }

        let mut inner = self.inner.lock();
        if !collected.is_empty() {
            let records: Vec<WalRecord> = collected
                .iter()
                .map(|q| WalRecord::InsertQuad(q.clone()))
                .collect();
            inner.wal_append_batch(records.iter())?;
        }
        let mut new_inserts = 0usize;
        for quad in &collected {
            if inner.apply_insert(quad)? {
                new_inserts += 1;
            }
        }
        if new_inserts > 0 || !inner.delta.is_empty() {
            let snap = inner.compact_locked()?;
            let graphs: GraphsSnapshot = Arc::new(inner.graphs.clone());
            drop(inner);
            self.publish_after_compact(snap, graphs);
        }
        Ok(consumed)
    }

    /// Turn on Tantivy-backed full-text indexing for this store (same contract
    /// as [`oxigraph_nova_engine_louds::LoudsStore::enable_fulltext`]).
    ///
    /// Literal objects are indexed incrementally on every compaction. For a
    /// persistent store, a stale/missing generation marker triggers a one-time
    /// full rebuild from compacted SPO triples.
    ///
    /// Quads still sitting in the (not-yet-compacted) delta are intentionally
    /// **not** indexed here — they will be picked up by the next `compact()`.
    #[cfg(feature = "fulltext")]
    pub fn enable_fulltext(&self) -> Result<(), Oxigraph> {
        let mut inner = self.inner.lock();

        if inner.fulltext.is_some() {
            return Ok(()); // already enabled
        }

        let (ft, needs_rebuild) = match &inner.data_dir {
            None => (
                FulltextIndex::create_in_ram()
                    .map_err(|e| Oxigraph::Storage(format!("fulltext init failed: {e}")))?,
                true, // no marker concept for in-memory stores — always (re)build
            ),
            Some(dir) => {
                let dir = dir.clone();
                let ft = FulltextIndex::open_or_create(&fulltext::index_dir(&dir))
                    .map_err(|e| Oxigraph::Storage(format!("fulltext open failed: {e}")))?;
                let up_to_date = fulltext::read_marker(&dir) == Some(inner.snapshot_gen);
                (ft, !up_to_date)
            }
        };

        if needs_rebuild {
            fulltext::rebuild_from_spo_triples(
                &ft,
                &inner.dict,
                inner
                    .graphs
                    .iter()
                    .map(|(&g_id, img)| (g_id, img.enumerate_spo_external())),
            )?;
            ft.commit()
                .map_err(|e| Oxigraph::Storage(format!("fulltext commit failed: {e}")))?;
            if let Some(dir) = inner.data_dir.clone() {
                fulltext::write_marker(&dir, inner.snapshot_gen)?;
            }
        }

        inner.fulltext = Some(Arc::new(ft));
        Ok(())
    }

    /// Whether every non-empty compacted graph currently has a mapped image open.
    pub fn all_graphs_mapped(&self) -> bool {
        let inner = self.inner.lock();
        !inner.graphs.is_empty() && inner.graphs.values().all(|g| g.has_mapped())
    }

    /// Default-graph Braided image when delta is empty (Phase-2 cost harness).
    ///
    /// Returns `None` while dirty / pre-compact. Diagnostic only — not a
    /// stable product API for query engines (use `LftjSource` instead).
    pub fn default_graph_image(&self) -> Option<Arc<BraidedGraphImage>> {
        self.graph_from_snap(GRAPH_DEFAULT.as_u8())
    }

    /// Snapshot of SPARQL path counters (W0).
    pub fn sparql_path_snapshot(&self) -> super::product_path::SparqlPathSnapshot {
        SPARQL_PATH.snapshot()
    }
}

// ── LftjSource ────────────────────────────────────────────────────────────────

impl LftjSource for RingStore {
    fn supports_lftj(&self) -> bool {
        true
    }

    fn supports_veo_estimates(&self) -> bool {
        // Exact real_count is available; estimates use distinct-value counts.
        true
    }

    fn lftj_intern_term(&self, term: &Term) -> Option<u64> {
        let inner = self.inner.lock();
        inner.dict.get_id(term).map(|id| id.as_u64())
    }

    fn lftj_decode_term(&self, id: u64) -> Option<Term> {
        SPARQL_PATH.decode_calls.fetch_add(1, Ordering::Relaxed);
        // W3: compact-time snapshot lives outside `inner` — never take the
        // store mutex on the hot LFTJ emit path when delta is empty.
        {
            let guard = self.decode_snap.read();
            if let Some(ref table) = *guard {
                let idx = id as usize;
                if let Some(Some(arc)) = table.get(idx) {
                    return Some(arc.as_ref().clone());
                }
                // Snapshot present but id missing: term not in dict.
                return None;
            }
        }
        // No snapshot (delta dirty / pre-compact): fall back under store lock.
        let inner = self.inner.lock();
        let tid = TermId::new(id).ok()?;
        inner.dict.get_term_arc(tid).map(|arc| arc.as_ref().clone())
    }

    fn lftj_graph_id(&self, graph: &GraphName) -> Option<u8> {
        let inner = self.inner.lock();
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
        // B: VEO probes use graphs_snap — no store mutex on the hot path.
        // Absence of snap means delta dirty / pre-compact → same as LOUDS "0".
        match self.graph_from_snap(graph_id) {
            None => 0,
            Some(img) => img.estimate_count_external(s, p, o, target_field),
        }
    }

    fn lftj_join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
        graph_id: u8,
    ) -> Option<Box<dyn TrieIterator>> {
        // B: graphs_snap present iff delta empty (cleared on every write).
        SPARQL_PATH.join_scan_open.fetch_add(1, Ordering::Relaxed);
        // None snap ⇒ delta non-empty or never compacted → LFTJ unavailable.
        if self.graphs_snap.read().is_none() {
            return None;
        }
        let img = self.graph_from_snap(graph_id);
        let out = match img {
            None => Some(Box::new(EmptyTrieIter) as Box<dyn TrieIterator>),
            Some(img) => {
                let _ = ring_d2_enabled();
                Some(BraidedGraphImage::join_scan_streaming(
                    img,
                    s,
                    p,
                    o,
                    target_field,
                ))
            }
        };
        // NEVER log per open: path_2hop opens millions of scans; eprintln each
        // time turns a ~0.5s query into multi-second I/O (observed 3.6M lines).
        // Counters still accumulate; call `log_sparql_path_counters()` explicitly
        // or rely on sparse sampling below when NOVA_RING_COUNTERS=1.
        if ring_counters_log_enabled() {
            let n = SPARQL_PATH.join_scan_open.load(Ordering::Relaxed);
            // Log first open, then every 1_000_000 opens (diagnostic only).
            if n == 1 || n.is_multiple_of(1_000_000) {
                let s = SPARQL_PATH.snapshot();
                eprintln!(
                    "nova-ring path: open={} mapped_rdi={} heap_rnv={} middle={} d2_calls={} d2_hits={} decode={}",
                    s.join_scan_open,
                    s.path_mapped_rdi,
                    s.path_heap_rnv,
                    s.path_middle_runs,
                    s.d2_calls,
                    s.d2_hits,
                    s.decode_calls
                );
            }
        }
        out
    }

    fn lftj_real_count(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
        graph_id: u8,
    ) -> Option<u64> {
        // VEO is called for every candidate at every depth. Exact distinct
        // walks were the 2join hang; use the same cheap estimate LOUDS uses
        // via estimate_count (vocab/range heuristic). B: no inner mutex.
        if self.graphs_snap.read().is_none() {
            return None;
        }
        match self.graph_from_snap(graph_id) {
            None => Some(0),
            Some(img) => Some(img.estimate_count_external(s, p, o, target_field)),
        }
    }

    fn lftj_has_delta(&self) -> bool {
        // Snap cleared on write and republished only after compact ⇒ equivalent
        // to delta non-empty without locking inner on the LFTJ gate.
        self.graphs_snap.read().is_none() && {
            // Empty store before first compact also has no snap; check delta.
            !self.inner.lock().delta.is_empty()
        }
    }

    fn lftj_multi_subject_object_intersect(
        &self,
        subjects: &[u64],
        _predicate: Option<u64>,
        graph_id: u8,
    ) -> Option<Box<dyn TrieIterator>> {
        // W4b: D1 (2 subjects) / D2 (≥3) product path — only when compacted + mmap.
        if !ring_d2_enabled() || subjects.len() < 2 {
            return None;
        }
        let img = self.graph_from_snap(graph_id)?;
        BraidedGraphImage::multi_subject_object_intersect(img, subjects)
    }

    fn lftj_prepare_pred_object_intersect(
        &self,
        predicate: u64,
        graph_id: u8,
    ) -> Option<Box<dyn PreparedPredObjectIntersect>> {
        let img = self.graph_from_snap(graph_id)?;
        PreparedPredD1::prepare(img, predicate)
            .map(|p| Box::new(p) as Box<dyn PreparedPredObjectIntersect>)
    }

    fn lftj_prepare_sp_object_scan(
        &self,
        predicate: u64,
        graph_id: u8,
    ) -> Option<Box<dyn PreparedSpObjectScan>> {
        let img = self.graph_from_snap(graph_id)?;
        let ver = self.snapshot_version.load(AtomicOrdering::Relaxed);
        // Warm HTTP 2join: reuse K9.4 adj across requests. Cold miss builds once
        // (amortized over warmup + timed iters). Never rebuild per evaluate.
        let adj = self
            .sp_adj_cache
            .lock()
            .get_or_insert((ver, graph_id, predicate), || {
                PreparedSpObjectScanImpl::build_shared_adj(&img, predicate)
            });
        PreparedSpObjectScanImpl::prepare_with_shared_adj(img, predicate, adj)
            .map(|p| Box::new(p) as Box<dyn PreparedSpObjectScan>)
    }

    fn lftj_prepare_shape(
        &self,
        shape: PhysicalShape,
        graph_id: u8,
    ) -> Option<Box<dyn PreparedPhysicalOperator>> {
        let img = self.graph_from_snap(graph_id)?;
        let ver = self.snapshot_version.load(AtomicOrdering::Relaxed);
        match shape {
            PhysicalShape::TwoHop { p1, p2 } => {
                get_or_prepare_two_hop(&self.physical_op_cache, ver, graph_id, img, p1, p2)
            }
            PhysicalShape::Wedge { predicate } => {
                get_or_prepare_wedge(&self.physical_op_cache, ver, graph_id, img, predicate)
            }
            PhysicalShape::SpExpansion {
                p_filter,
                o_filter,
                p_expand,
            } => {
                // Warm expand adj from SpAdjCache (same table as prepared SP scan).
                let adj = self
                    .sp_adj_cache
                    .lock()
                    .get_or_insert((ver, graph_id, p_expand), || {
                        PreparedSpObjectScanImpl::build_shared_adj(&img, p_expand)
                    });
                get_or_prepare_sp_expansion(
                    &self.physical_op_cache,
                    ver,
                    graph_id,
                    img,
                    p_filter,
                    o_filter,
                    p_expand,
                    adj,
                )
            }
            PhysicalShape::KChain { p1, p2, p3 } => {
                get_or_prepare_k_chain(&self.physical_op_cache, ver, graph_id, img, p1, p2, p3)
            }
            PhysicalShape::Star { p1, p2, p3 } => {
                get_or_prepare_star(&self.physical_op_cache, ver, graph_id, img, p1, p2, p3)
            }
        }
    }
}

// ── TextSearch ────────────────────────────────────────────────────────────────

#[cfg(feature = "fulltext")]
impl oxigraph_nova_core::TextSearch for RingStore {
    fn search(
        &self,
        query: &str,
        predicate_id: Option<u64>,
        limit: usize,
    ) -> Vec<oxigraph_nova_core::TextMatch> {
        let inner = self.inner.lock();
        match &inner.fulltext {
            Some(ft) => ft.search(query, predicate_id, limit),
            None => Vec::new(),
        }
    }

    fn text_search_ready(&self) -> bool {
        self.inner.lock().fulltext.is_some()
    }
}

// ── QuadStore ─────────────────────────────────────────────────────────────────

impl QuadStore for RingStore {
    fn delta_len(&self) -> Option<usize> {
        Some(RingStore::delta_len(self))
    }

    fn compaction_count(&self) -> Option<u64> {
        Some(RingStore::compaction_count(self))
    }

    fn insert(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        // Any live write invalidates compact-time decode + graphs snapshots.
        self.clear_compact_snaps();
        let mut inner = self.inner.lock();
        // Log intent durably BEFORE applying (same discipline as LOUDS).
        inner.wal_append(&WalRecord::InsertQuad(quad.clone()))?;
        let result = inner.apply_insert(quad)?;
        if let Some(snap) = inner.maybe_auto_compact()? {
            let graphs: GraphsSnapshot = Arc::new(inner.graphs.clone());
            drop(inner);
            self.publish_after_compact(snap, graphs);
        }
        Ok(result)
    }

    fn remove(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        self.clear_compact_snaps();
        let mut inner = self.inner.lock();
        inner.wal_append(&WalRecord::RemoveQuad(quad.clone()))?;
        let result = inner.apply_remove(quad)?;
        if let Some(snap) = inner.maybe_auto_compact()? {
            let graphs: GraphsSnapshot = Arc::new(inner.graphs.clone());
            drop(inner);
            self.publish_after_compact(snap, graphs);
        }
        Ok(result)
    }

    fn quads_for_pattern(
        &self,
        subject: Option<&Term>,
        predicate: Option<&NamedNode>,
        object: Option<&Term>,
        graph_name: Option<&GraphName>,
    ) -> Result<Box<dyn Iterator<Item = Result<StoredQuad, Oxigraph>> + '_>, Oxigraph> {
        let inner = self.inner.lock();

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

        let search_graphs: Vec<GraphId> = match graph_name {
            Some(gn) => match inner.dict.get_graph_id(gn) {
                None => return Ok(Box::new(std::iter::empty())),
                Some(id) => vec![id],
            },
            None => {
                let mut gids: HashSet<GraphId> = inner.graphs.keys().copied().collect();
                gids.insert(GRAPH_DEFAULT);
                for &gid in &inner.named_graph_ids {
                    gids.insert(gid);
                }
                for (&key, _) in inner.delta.iter() {
                    let (gid, _, _, _) = Dictionary::unpack_quad(key);
                    gids.insert(gid);
                }
                gids.into_iter().collect()
            }
        };

        let mut results: Vec<StoredQuad> = Vec::new();
        let mut seen: HashSet<u128> = HashSet::new();

        for g_id in search_graphs {
            let (lo, hi) = Delta::graph_range(g_id, s_id, p_id, o_id);

            for (&key, &is_insert) in inner.delta.range_inclusive(lo, hi) {
                let (dk_g, dk_s, dk_p, dk_o) = Dictionary::unpack_quad(key);
                if dk_g != g_id {
                    continue;
                }
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
                seen.insert(key);
                if is_insert
                    && let Some(sq) = decode_stored_quad(&inner.dict, g_id, dk_s, dk_p, dk_o)
                {
                    results.push(sq);
                }
            }

            if let Some(img) = inner.graphs.get(&g_id) {
                let matches = image_match_triples(
                    img,
                    s_id.map(|i| i.as_u64()),
                    p_id.map(|i| i.as_u64()),
                    o_id.map(|i| i.as_u64()),
                );
                for [rs, rp, ro] in matches {
                    let key = Dictionary::pack_quad(
                        g_id,
                        TermId::new(rs).unwrap_or_else(|_| TermId::new(0).unwrap()),
                        TermId::new(rp).unwrap_or_else(|_| TermId::new(0).unwrap()),
                        TermId::new(ro).unwrap_or_else(|_| TermId::new(0).unwrap()),
                    );
                    if seen.contains(&key) {
                        continue;
                    }
                    if let (Ok(ts), Ok(tp), Ok(to)) =
                        (TermId::new(rs), TermId::new(rp), TermId::new(ro))
                        && let Some(sq) = decode_stored_quad(&inner.dict, g_id, ts, tp, to)
                    {
                        results.push(sq);
                    }
                }
            }
        }

        Ok(Box::new(results.into_iter().map(Ok)))
    }

    fn len(&self) -> Result<usize, Oxigraph> {
        let inner = self.inner.lock();
        let ring_total: usize = inner.graphs.values().map(|r| r.n() as usize).sum();
        let delta_inserts = inner.delta.insert_count();
        let delta_tombstones = inner.delta.tombstone_count();
        Ok(ring_total.saturating_sub(delta_tombstones) + delta_inserts)
    }

    fn contains(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        let inner = self.inner.lock();

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
        match inner.delta.get(key) {
            Some(true) => return Ok(true),
            Some(false) => return Ok(false),
            None => {}
        }
        if let Some(img) = inner.graphs.get(&g_id) {
            return Ok(image_contains(
                img,
                s_id.as_u64(),
                p_id.as_u64(),
                o_id.as_u64(),
            ));
        }
        Ok(false)
    }

    fn known_named_graphs(
        &self,
    ) -> Result<Box<dyn Iterator<Item = Result<GraphName, Oxigraph>> + '_>, Oxigraph> {
        let inner = self.inner.lock();
        let mut seen: HashSet<u8> = HashSet::new();
        let mut graphs: Vec<GraphName> = Vec::new();

        for &gid in &inner.named_graph_ids {
            if seen.insert(gid.as_u8())
                && let Some(gn) = inner.dict.get_graph(gid)
            {
                graphs.push(gn.clone());
            }
        }
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
            let mut inner = self.inner.lock();
            inner.wal_append(&WalRecord::RegisterGraph(graph.clone()))?;
            inner.apply_register_graph(graph)?;
        }
        Ok(())
    }

    fn extend_boxed(&self, quads: Box<dyn Iterator<Item = Quad> + '_>) -> Result<usize, Oxigraph> {
        let quads: Vec<Quad> = quads.into_iter().collect();
        if quads.is_empty() {
            return Ok(0);
        }
        self.clear_compact_snaps();
        let mut inner = self.inner.lock();

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
        if let Some(snap) = inner.maybe_auto_compact()? {
            let graphs: GraphsSnapshot = Arc::new(inner.graphs.clone());
            drop(inner);
            self.publish_after_compact(snap, graphs);
        }
        Ok(count)
    }

    fn apply_batch(&self, ops: &[QuadOp]) -> Result<(usize, usize), Oxigraph> {
        if ops.is_empty() {
            return Ok((0, 0));
        }
        self.clear_compact_snaps();
        let mut inner = self.inner.lock();

        let records: Vec<WalRecord> = ops
            .iter()
            .map(|op| match op {
                QuadOp::Insert(q) => WalRecord::InsertQuad(q.clone()),
                QuadOp::Remove(q) => WalRecord::RemoveQuad(q.clone()),
            })
            .collect();
        inner.wal_append_batch(records.iter())?;

        let mut inserted = 0usize;
        let mut removed = 0usize;
        for op in ops {
            match op {
                QuadOp::Insert(q) => {
                    if inner.apply_insert(q)? {
                        inserted += 1;
                    }
                }
                QuadOp::Remove(q) => {
                    if inner.apply_remove(q)? {
                        removed += 1;
                    }
                }
            }
        }
        if let Some(snap) = inner.maybe_auto_compact()? {
            let graphs: GraphsSnapshot = Arc::new(inner.graphs.clone());
            drop(inner);
            self.publish_after_compact(snap, graphs);
        }
        Ok((inserted, removed))
    }
}

// ── StorageEngine (product lifecycle + registry) ──────────────────────────────

impl StorageEngine for RingStore {
    fn engine_name(&self) -> &'static str {
        "ring"
    }

    fn compact(&self) -> Result<(), Oxigraph> {
        RingStore::compact(self)
    }

    fn backup(&self, destination: &Path) -> Result<(), Oxigraph> {
        RingStore::backup(self, destination)
    }

    fn set_sync_policy(&self, policy: SyncPolicy) {
        RingStore::set_sync_policy(self, policy)
    }

    fn set_auto_compact_threshold(&self, threshold: usize) {
        RingStore::set_auto_compact_threshold(self, threshold)
    }

    fn flush_wal(&self) -> Result<(), Oxigraph> {
        RingStore::flush_wal(self)
    }

    fn triple_count(&self) -> usize {
        RingStore::triple_count(self)
    }

    fn bulk_load_boxed(
        &self,
        quads: Box<dyn Iterator<Item = Quad> + '_>,
        on_progress: Option<&mut dyn FnMut(u64)>,
    ) -> Result<usize, Oxigraph> {
        self.bulk_load_with_progress(quads, on_progress)
    }

    #[cfg(feature = "fulltext")]
    fn enable_fulltext(&self) -> Result<(), Oxigraph> {
        RingStore::enable_fulltext(self)
    }

    #[cfg(feature = "fulltext")]
    fn as_text_search(self: Arc<Self>) -> Option<Arc<dyn oxigraph_nova_core::TextSearch>> {
        Some(self as Arc<dyn oxigraph_nova_core::TextSearch>)
    }
}

fn ring_new_in_memory() -> Arc<dyn StorageEngine> {
    Arc::new(RingStore::new())
}

fn ring_open(path: &Path) -> Result<Arc<dyn StorageEngine>, Oxigraph> {
    Ok(Arc::new(RingStore::open(path)?))
}

inventory::submit! {
    oxigraph_nova_core::BackendFactory {
        name: "ring",
        description: "Cyclic-QWT RingStore (NOVARNG1 / D2 braided)",
        new_in_memory: ring_new_in_memory,
        open: ring_open,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{LftjSource, Literal};

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
        let q = make_quad("http://s", "http://p", "o1", dg());
        assert!(store.insert(&q).unwrap());
        assert!(!store.insert(&q).unwrap());
        assert!(store.contains(&q).unwrap());
        assert_eq!(store.len().unwrap(), 1);
        assert_eq!(store.delta_len(), 1);
        assert_eq!(store.ring_triple_count(), 0);
    }

    #[test]
    fn remove_from_delta() {
        let store = RingStore::new();
        let q = make_quad("http://s", "http://p", "o1", dg());
        store.insert(&q).unwrap();
        assert!(store.remove(&q).unwrap());
        assert!(!store.contains(&q).unwrap());
        assert_eq!(store.len().unwrap(), 0);
    }

    #[test]
    fn insert_compact_contains_ring() {
        let store = RingStore::new();
        let q1 = make_quad("http://s1", "http://p", "a", dg());
        let q2 = make_quad("http://s1", "http://p", "b", dg());
        let q3 = make_quad("http://s2", "http://p", "a", dg());
        store.insert(&q1).unwrap();
        store.insert(&q2).unwrap();
        store.insert(&q3).unwrap();
        assert_eq!(store.delta_len(), 3);
        store.compact().unwrap();
        assert_eq!(store.delta_len(), 0);
        assert_eq!(store.ring_triple_count(), 3);
        assert!(store.contains(&q1).unwrap());
        assert!(store.contains(&q2).unwrap());
        assert!(store.contains(&q3).unwrap());
        assert_eq!(store.len().unwrap(), 3);
        assert_eq!(store.compaction_count(), 1);
        // P0 / W1: mmap open after compact when NOVA_RING_MMAP defaults on.
        if crate::product_path::ring_mmap_enabled() {
            assert!(
                store.all_graphs_mapped(),
                "compact should materialize mapped NOVARNG1 when NOVA_RING_MMAP=1"
            );
        }
    }

    #[test]
    fn decode_snapshot_used_after_compact() {
        let store = RingStore::new();
        store
            .insert(&make_quad("http://s", "http://p", "o", dg()))
            .unwrap();
        store.compact().unwrap();
        let o = lit("o");
        // After compact, decode must still work (snapshot path).
        let id = store.lftj_intern_term(&o).unwrap();
        assert!(
            store.decode_snap.read().is_some(),
            "W3: compact must publish decode_snap outside mutex"
        );
        let decoded = store.lftj_decode_term(id).unwrap();
        assert_eq!(decoded, o);
        // Write clears snapshot.
        store
            .insert(&make_quad("http://s2", "http://p", "o2", dg()))
            .unwrap();
        assert!(store.decode_snap.read().is_none());
    }

    #[test]
    fn remove_from_ring_via_tombstone() {
        let store = RingStore::new();
        let q = make_quad("http://s", "http://p", "o", dg());
        store.insert(&q).unwrap();
        store.compact().unwrap();
        assert_eq!(store.delta_len(), 0);
        assert!(store.remove(&q).unwrap());
        assert!(!store.contains(&q).unwrap());
        assert_eq!(store.len().unwrap(), 0);
        // Compact again drops the tombstoned triple from the image.
        store.compact().unwrap();
        assert_eq!(store.ring_triple_count(), 0);
        assert_eq!(store.delta_len(), 0);
    }

    #[test]
    fn re_insert_after_remove() {
        let store = RingStore::new();
        let q = make_quad("http://s", "http://p", "o", dg());
        store.insert(&q).unwrap();
        store.compact().unwrap();
        store.remove(&q).unwrap();
        assert!(store.insert(&q).unwrap());
        assert!(store.contains(&q).unwrap());
        store.compact().unwrap();
        assert!(store.contains(&q).unwrap());
        assert_eq!(store.ring_triple_count(), 1);
    }

    #[test]
    fn quads_for_pattern_wildcard_and_bound() {
        let store = RingStore::new();
        let q1 = make_quad("http://s1", "http://p1", "a", dg());
        let q2 = make_quad("http://s1", "http://p2", "b", dg());
        let q3 = make_quad("http://s2", "http://p1", "c", dg());
        store.insert(&q1).unwrap();
        store.insert(&q2).unwrap();
        store.insert(&q3).unwrap();
        store.compact().unwrap();

        let all: Vec<_> = store
            .quads_for_pattern(None, None, None, None)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(all.len(), 3);

        let s1 = Term::NamedNode(NamedNode::new_unchecked("http://s1"));
        let s1_only: Vec<_> = store
            .quads_for_pattern(Some(&s1), None, None, None)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(s1_only.len(), 2);

        let p1 = pred("http://p1");
        let p1_only: Vec<_> = store
            .quads_for_pattern(None, Some(&p1), None, None)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(p1_only.len(), 2);
    }

    #[test]
    fn quads_mixed_ring_and_delta() {
        let store = RingStore::new();
        let q1 = make_quad("http://s", "http://p", "ring", dg());
        let q2 = make_quad("http://s", "http://p", "delta", dg());
        store.insert(&q1).unwrap();
        store.compact().unwrap();
        store.insert(&q2).unwrap();
        assert_eq!(store.len().unwrap(), 2);
        let all: Vec<_> = store
            .quads_for_pattern(None, None, None, None)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn named_graphs_tracked() {
        let store = RingStore::new();
        let g = ng("http://g");
        store.register_named_graph(&g).unwrap();
        let known: Vec<_> = store
            .known_named_graphs()
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(known, vec![g.clone()]);
        let q = make_quad("http://s", "http://p", "o", g.clone());
        store.insert(&q).unwrap();
        store.compact().unwrap();
        let in_g: Vec<_> = store
            .quads_for_pattern(None, None, None, Some(&g))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(in_g.len(), 1);
    }

    #[test]
    fn lftj_disabled_while_delta_nonempty() {
        let store = RingStore::new();
        let q = make_quad("http://s", "http://p", "o", dg());
        store.insert(&q).unwrap();
        assert!(store.lftj_has_delta());
        assert!(store.lftj_join_scan(None, None, None, 0, 0).is_none());
        store.compact().unwrap();
        assert!(!store.lftj_has_delta());
        let scan = store.lftj_join_scan(None, None, None, 0, 0).unwrap();
        assert!(!scan.at_end() || store.len().unwrap() == 0);
    }

    #[test]
    fn lftj_join_scan_after_compact() {
        let store = RingStore::new();
        store
            .insert(&make_quad("http://s1", "http://p", "a", dg()))
            .unwrap();
        store
            .insert(&make_quad("http://s1", "http://p", "b", dg()))
            .unwrap();
        store
            .insert(&make_quad("http://s2", "http://p", "a", dg()))
            .unwrap();
        store.compact().unwrap();

        assert!(store.supports_lftj());
        let s1 = Term::NamedNode(NamedNode::new_unchecked("http://s1"));
        let s1_id = store.lftj_intern_term(&s1).unwrap();
        let mut it = store.lftj_join_scan(Some(s1_id), None, None, 2, 0).unwrap();
        let mut objs = Vec::new();
        while !it.at_end() {
            objs.push(it.key());
            it.advance();
        }
        assert_eq!(objs.len(), 2);

        // Decode back to terms.
        let terms: Vec<_> = objs
            .iter()
            .filter_map(|&id| store.lftj_decode_term(id))
            .collect();
        assert_eq!(terms.len(), 2);

        let count = store
            .lftj_real_count(Some(s1_id), None, None, 2, 0)
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn apply_batch_mixed() {
        let store = RingStore::new();
        let q1 = make_quad("http://s", "http://p", "a", dg());
        let q2 = make_quad("http://s", "http://p", "b", dg());
        let (ins, rem) = store
            .apply_batch(&[QuadOp::Insert(q1.clone()), QuadOp::Insert(q2.clone())])
            .unwrap();
        assert_eq!((ins, rem), (2, 0));
        let (ins, rem) = store.apply_batch(&[QuadOp::Remove(q1.clone())]).unwrap();
        assert_eq!((ins, rem), (0, 1));
        assert!(!store.contains(&q1).unwrap());
        assert!(store.contains(&q2).unwrap());
    }

    #[test]
    fn differential_pattern_vs_sorted_oracle() {
        // Ground truth: sorted SPO multiset after insert+compact.
        let store = RingStore::new();
        let quads = [
            make_quad("http://a", "http://p1", "x", dg()),
            make_quad("http://a", "http://p1", "y", dg()),
            make_quad("http://b", "http://p1", "x", dg()),
            make_quad("http://b", "http://p2", "z", dg()),
            make_quad("http://a", "http://p2", "x", dg()),
        ];
        for q in &quads {
            store.insert(q).unwrap();
        }
        store.compact().unwrap();

        let mut from_store: Vec<(String, String, String)> = store
            .quads_for_pattern(None, None, None, None)
            .unwrap()
            .map(|r| {
                let sq = r.unwrap();
                (
                    sq.subject.to_string(),
                    sq.predicate.as_str().to_string(),
                    sq.object.to_string(),
                )
            })
            .collect();
        from_store.sort();

        let mut oracle: Vec<(String, String, String)> = quads
            .iter()
            .map(|q| {
                (
                    Term::from(q.subject.clone()).to_string(),
                    q.predicate.as_str().to_string(),
                    q.object.to_string(),
                )
            })
            .collect();
        oracle.sort();
        assert_eq!(from_store, oracle);

        // Bound subject pattern: s=a has three objects {x, y, x}.
        let a = Term::NamedNode(NamedNode::new_unchecked("http://a"));
        let mut bound: Vec<_> = store
            .quads_for_pattern(Some(&a), None, None, None)
            .unwrap()
            .map(|r| r.unwrap().object.to_string())
            .collect();
        bound.sort();
        assert_eq!(bound.len(), 3);
        assert_eq!(
            bound,
            vec![
                lit("x").to_string(),
                lit("x").to_string(),
                lit("y").to_string()
            ]
        );
    }

    // ── Persistent lifecycle (WAL / MANIFEST / reopen) ────────────────────────

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("nova_ring_persist_{pid}_{n}_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn open_reopen_round_trip() {
        let dir = temp_dir("roundtrip");
        let q1 = make_quad("http://ex/s1", "http://ex/p", "a", dg());
        let q2 = make_quad("http://ex/s2", "http://ex/p", "b", dg());

        {
            let store = RingStore::open(&dir).unwrap();
            store.set_sync_policy(SyncPolicy::Always);
            assert!(store.insert(&q1).unwrap());
            assert!(store.insert(&q2).unwrap());
            assert_eq!(store.len().unwrap(), 2);
            store.flush_wal().unwrap();
        }

        {
            let store = RingStore::open(&dir).unwrap();
            assert_eq!(store.len().unwrap(), 2);
            assert!(store.contains(&q1).unwrap());
            assert!(store.contains(&q2).unwrap());

            // Subsequent writes on the reopened store must also persist.
            let q3 = make_quad("http://ex/s3", "http://ex/p", "c", dg());
            store.set_sync_policy(SyncPolicy::Always);
            assert!(store.insert(&q3).unwrap());
            store.flush_wal().unwrap();
        }

        {
            let store = RingStore::open(&dir).unwrap();
            assert_eq!(store.len().unwrap(), 3);
            assert!(store.contains(&q1).unwrap());
            assert!(store.contains(&q2).unwrap());
            let q3 = make_quad("http://ex/s3", "http://ex/p", "c", dg());
            assert!(store.contains(&q3).unwrap());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_reopen_after_compact_uses_snapshot() {
        let dir = temp_dir("snapshot_roundtrip");
        let q1 = make_quad("http://ex/s1", "http://ex/p", "a", dg());
        let q2 = make_quad("http://ex/s2", "http://ex/p", "b", dg());

        {
            let store = RingStore::open(&dir).unwrap();
            store.set_sync_policy(SyncPolicy::Always);
            store.insert(&q1).unwrap();
            store.insert(&q2).unwrap();
            store.compact().unwrap();
            assert_eq!(store.delta_len(), 0);
            assert_eq!(store.ring_triple_count(), 2);
            // Durable generation files must exist.
            assert!(manifest::ring_image_path(&dir, 1, 0).exists());
            assert!(manifest::ring_remap_path(&dir, 1, 0).exists());
            assert!(manifest::dict_path(&dir, 1).exists());
            assert!(dir.join(manifest::MANIFEST_FILE_NAME).exists());
        }

        {
            let store = RingStore::open(&dir).unwrap();
            assert_eq!(store.len().unwrap(), 2);
            assert!(store.contains(&q1).unwrap());
            assert!(store.contains(&q2).unwrap());
            // Compacted reopen should have empty delta and LFTJ available.
            assert_eq!(store.delta_len(), 0);
            assert!(!store.lftj_has_delta());
            assert!(store.all_graphs_mapped());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_reopen_after_compact_then_wal_tail() {
        let dir = temp_dir("compact_then_tail");
        let q1 = make_quad("http://ex/s1", "http://ex/p", "a", dg());
        let q2 = make_quad("http://ex/s2", "http://ex/p", "b", dg());
        let q3 = make_quad("http://ex/s3", "http://ex/p", "c", dg());

        {
            let store = RingStore::open(&dir).unwrap();
            store.set_sync_policy(SyncPolicy::Always);
            store.insert(&q1).unwrap();
            store.insert(&q2).unwrap();
            store.compact().unwrap();
            // Post-compact write lives only in the new WAL segment.
            store.insert(&q3).unwrap();
            store.flush_wal().unwrap();
            assert_eq!(store.len().unwrap(), 3);
            assert_eq!(store.delta_len(), 1);
        }

        {
            let store = RingStore::open(&dir).unwrap();
            assert_eq!(store.len().unwrap(), 3);
            assert!(store.contains(&q1).unwrap());
            assert!(store.contains(&q2).unwrap());
            assert!(store.contains(&q3).unwrap());
            // q3 still in the delta after reopen (not yet compacted).
            assert_eq!(store.delta_len(), 1);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compact_rotates_wal_and_cleans_orphans() {
        let dir = temp_dir("wal_rotate");
        {
            let store = RingStore::open(&dir).unwrap();
            store.set_sync_policy(SyncPolicy::Always);
            store
                .insert(&make_quad("http://s", "http://p", "o", dg()))
                .unwrap();
            // Segment 1 exists after first write.
            assert!(manifest::wal_segment_path(&dir, 1).exists());
            store.compact().unwrap();
            // Compact rotates to segment 2; orphan cleanup drops segment 1.
            assert!(!manifest::wal_segment_path(&dir, 1).exists());
            assert!(manifest::wal_segment_path(&dir, 2).exists());
            assert!(manifest::ring_image_path(&dir, 1, 0).exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn named_graph_survives_reopen() {
        let dir = temp_dir("named_graph");
        let g = ng("http://g");
        let q = make_quad("http://s", "http://p", "o", g.clone());
        {
            let store = RingStore::open(&dir).unwrap();
            store.set_sync_policy(SyncPolicy::Always);
            store.register_named_graph(&g).unwrap();
            store.insert(&q).unwrap();
            store.compact().unwrap();
        }
        {
            let store = RingStore::open(&dir).unwrap();
            assert!(store.contains(&q).unwrap());
            let known: Vec<_> = store
                .known_named_graphs()
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert!(known.contains(&g));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_survives_reopen_via_wal() {
        let dir = temp_dir("remove_wal");
        let q = make_quad("http://s", "http://p", "o", dg());
        {
            let store = RingStore::open(&dir).unwrap();
            store.set_sync_policy(SyncPolicy::Always);
            store.insert(&q).unwrap();
            store.compact().unwrap();
            assert!(store.remove(&q).unwrap());
            store.flush_wal().unwrap();
            assert!(!store.contains(&q).unwrap());
        }
        {
            let store = RingStore::open(&dir).unwrap();
            assert!(!store.contains(&q).unwrap());
            assert_eq!(store.len().unwrap(), 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_round_trips_into_independent_store() {
        let src_dir = temp_dir("backup_src");
        let dst_dir = temp_dir("backup_dst");
        let _ = std::fs::remove_dir_all(&dst_dir);

        let g = ng("http://ex/g");
        let q1 = make_quad("http://ex/s1", "http://ex/p", "a", dg());
        let q2 = make_quad("http://ex/s2", "http://ex/p", "b", g.clone());

        let store = RingStore::open(&src_dir).unwrap();
        store.set_sync_policy(SyncPolicy::Always);
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

        // Named graph from the compacted tier must survive the backup.
        let known: Vec<_> = restored
            .known_named_graphs()
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(known.contains(&g));

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
    fn bulk_load_with_progress_reports_final_count() {
        let store = RingStore::new();
        let quads = [
            make_quad("http://s1", "http://p", "a", dg()),
            make_quad("http://s2", "http://p", "b", dg()),
            make_quad("http://s3", "http://p", "c", dg()),
        ];
        let mut reported = Vec::new();
        let count = store
            .bulk_load_with_progress(quads, Some(&mut |n| reported.push(n)))
            .unwrap();
        assert_eq!(count, 3);
        // Final callback always fires when count is not a multiple of INTERVAL.
        assert_eq!(reported, vec![3]);
        assert_eq!(store.len().unwrap(), 3);
        assert_eq!(store.delta_len(), 0);
    }

    #[cfg(feature = "fulltext")]
    #[test]
    fn fulltext_search_finds_indexed_literal() {
        use oxigraph_nova_core::TextSearch;
        let store = RingStore::new();
        store.enable_fulltext().unwrap();
        store
            .insert(&make_quad("http://s", "http://p", "hello world", dg()))
            .unwrap();
        // Indexing is compaction-eventually-consistent.
        store.compact().unwrap();
        assert!(store.text_search_ready());
        let hits = store.search("hello", None, 10);
        assert_eq!(hits.len(), 1);
    }

    #[cfg(feature = "fulltext")]
    #[test]
    fn enable_fulltext_is_idempotent() {
        use oxigraph_nova_core::TextSearch;
        let store = RingStore::new();
        store.enable_fulltext().unwrap();
        store.enable_fulltext().unwrap();
        assert!(store.text_search_ready());
    }
}
