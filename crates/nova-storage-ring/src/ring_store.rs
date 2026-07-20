//! In-memory cyclic QWT [`RingStore`] `QuadStore` (Phase 5).
//!
//! Wires term dictionary + live LSM delta + per-graph
//! [`BraidedGraphImage`] into [`QuadStore`] / [`LftjSource`].
//!
//! ## Scope
//!
//! - **In-memory only** — no WAL / MANIFEST / snapshot reopen.
//! - **Not** the default SPARQL backend (`nova-store` still pins
//!   `LoudsStore`).
//! - Compaction rebuilds each graph as a Braided Ring image from
//!   external `TermId` triples (dense remap via [`BraidedGraphImage`]),
//!   then materializes `NOVARNG1` mmap when `NOVA_RING_MMAP=1` (default).
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
    get_or_prepare_sp_expansion, get_or_prepare_two_hop, get_or_prepare_wedge,
    PhysicalOpPreparedPlanCache, PREPARED_PLAN_CACHE_CAP,
};
use crate::product_path::{
    SPARQL_PATH, bump_mmap_ok, log_mmap_fail_once, ring_counters_log_enabled, ring_d2_enabled,
    ring_image_dir, ring_mmap_enabled,
};
use crate::scan::{PreparedPredD1, PreparedSpObjectScanImpl, PredicateAdjacency};
use oxigraph_nova_core::{
    PreparedPredObjectIntersect, PreparedSpExpansion, PreparedSpObjectScan, PreparedTwoHop,
    PreparedWedge,
};

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use oxigraph_nova_core::{
    Dictionary, EmptyTrieIter, GRAPH_DEFAULT, GraphId, GraphName, LftjSource, NamedNode, Oxigraph,
    Quad, QuadOp, QuadStore, StoredQuad, Subject, Term, TermId, TrieIterator,
};
use oxigraph_nova_storage_louds::delta::Delta;
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;

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
        if self.map.len() >= self.cap {
            if let Some(old) = self.order.first().copied() {
                self.order.remove(0);
                self.map.remove(&old);
            }
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
}

impl RingStoreInner {
    fn new() -> Self {
        Self {
            dict: Dictionary::new(),
            graphs: HashMap::new(),
            delta: Delta::new(),
            named_graph_ids: HashSet::new(),
            compaction_count: 0,
        }
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
    /// W1: when `NOVA_RING_MMAP=1` (default), materialize `NOVARNG1` per graph
    /// so LFTJ can use mapped RDI / D2. Failure keeps heap-only (never fails compact).
    fn compact_locked(&mut self) -> Result<DecodeSnapshot, Oxigraph> {
        let mut per_graph: HashMap<GraphId, Vec<[u64; 3]>> = HashMap::new();

        // Cleanup previous image files when replacing graphs.
        let old_paths: Vec<_> = self
            .graphs
            .values()
            .filter_map(|g| g.image_path().map(|p| p.to_path_buf()))
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
            // Dedup is handled inside BraidedGraphImage::from_external_triples.
            let mut img = BraidedGraphImage::from_external_triples(&triples);
            if let Some(ref dir) = img_dir {
                // Unique path per compact (tests run in parallel; avoid clobber).
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

        self.dict.compact()?;
        self.graphs = new_graphs;
        self.delta.clear();
        self.compaction_count = self.compaction_count.saturating_add(1);
        // W3: freeze decode table for LFTJ (delta empty) — published outside mutex.
        let snap = self.build_decode_snapshot();

        for p in old_paths {
            let _ = std::fs::remove_file(p);
        }
        Ok(snap)
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
/// Feature `cyclic-ring-pilot`. Not wired into `nova-store` as the default
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
    /// Empty in-memory store.
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
        self.snapshot_version
            .fetch_add(1, AtomicOrdering::Relaxed);
    }

    /// Publish decode + graphs snapshots after a successful compact.
    fn publish_after_compact(&self, decode: DecodeSnapshot, graphs: GraphsSnapshot) {
        // Compact rebuilds images — invalidate any plans held against old images.
        self.physical_op_cache.lock().clear();
        self.sp_adj_cache.lock().clear();
        self.snapshot_version
            .fetch_add(1, AtomicOrdering::Relaxed);
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
    /// Returns the number of newly inserted quads (duplicates skipped).
    /// Intended for `nova_serve --file` / external harness loads.
    pub fn bulk_load(&self, quads: impl IntoIterator<Item = Quad>) -> Result<usize, Oxigraph> {
        // Writes dirty compact-time snapshots until republish.
        self.clear_compact_snaps();
        let mut inner = self.inner.lock();
        let mut count = 0usize;
        for quad in quads {
            if inner.apply_insert(&quad)? {
                count += 1;
            }
        }
        if count > 0 || !inner.delta.is_empty() {
            let snap = inner.compact_locked()?;
            let graphs: GraphsSnapshot = Arc::new(inner.graphs.clone());
            drop(inner);
            self.publish_after_compact(snap, graphs);
        }
        Ok(count)
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
        let adj = self.sp_adj_cache.lock().get_or_insert((ver, graph_id, predicate), || {
            PreparedSpObjectScanImpl::build_shared_adj(&img, predicate)
        });
        PreparedSpObjectScanImpl::prepare_with_shared_adj(img, predicate, adj)
            .map(|p| Box::new(p) as Box<dyn PreparedSpObjectScan>)
    }


    fn lftj_prepare_two_hop(
        &self,
        p1: u64,
        p2: u64,
        graph_id: u8,
    ) -> Option<Box<dyn PreparedTwoHop>> {
        let img = self.graph_from_snap(graph_id)?;
        let ver = self.snapshot_version.load(AtomicOrdering::Relaxed);
        get_or_prepare_two_hop(&self.physical_op_cache, ver, graph_id, img, p1, p2)
    }

    fn lftj_prepare_wedge(
        &self,
        predicate: u64,
        graph_id: u8,
    ) -> Option<Box<dyn PreparedWedge>> {
        let img = self.graph_from_snap(graph_id)?;
        let ver = self.snapshot_version.load(AtomicOrdering::Relaxed);
        get_or_prepare_wedge(&self.physical_op_cache, ver, graph_id, img, predicate)
    }

    fn lftj_prepare_sp_expansion(
        &self,
        p_filter: u64,
        o_filter: u64,
        p_expand: u64,
        graph_id: u8,
    ) -> Option<Box<dyn PreparedSpExpansion>> {
        let img = self.graph_from_snap(graph_id)?;
        let ver = self.snapshot_version.load(AtomicOrdering::Relaxed);
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
        self.inner.lock().apply_insert(quad)
    }

    fn remove(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        self.clear_compact_snaps();
        self.inner.lock().apply_remove(quad)
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
            self.inner.lock().apply_register_graph(graph)?;
        }
        Ok(())
    }

    fn extend_boxed(&self, quads: Box<dyn Iterator<Item = Quad> + '_>) -> Result<usize, Oxigraph> {
        self.clear_compact_snaps();
        let mut inner = self.inner.lock();
        let mut count = 0usize;
        for quad in quads {
            if inner.apply_insert(&quad)? {
                count += 1;
            }
        }
        Ok(count)
    }

    fn apply_batch(&self, ops: &[QuadOp]) -> Result<(usize, usize), Oxigraph> {
        if ops.is_empty() {
            return Ok((0, 0));
        }
        self.clear_compact_snaps();
        let mut inner = self.inner.lock();

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
        Ok((inserted, removed))
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
        assert!(
            store
                .lftj_join_scan(None, None, None, 0, 0)
                .is_none()
        );
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
        let mut it = store
            .lftj_join_scan(Some(s1_id), None, None, 2, 0)
            .unwrap();
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
        let (ins, rem) = store
            .apply_batch(&[QuadOp::Remove(q1.clone())])
            .unwrap();
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
}

