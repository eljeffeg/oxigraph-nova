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
//! Production evolution path:
//! 1. Split into `RwLock<Ring>` + `Mutex<Delta+Dict>` for concurrent reads.
//! 2. Background merge thread: when `delta.len() > COMPACT_THRESHOLD` (1M),
//!    snapshot the delta, release the lock, rebuild Ring, `Arc::swap`, clear delta.

use crate::delta::Delta;
use crate::ring::{GraphRing, RingBuilder};
use oxigraph_nova_core::{
    Dictionary, EmptyTrieIter, GRAPH_DEFAULT, GraphId, GraphName, NamedNode, Oxigraph, Quad,
    QuadStore, StoredQuad, Subject, Term, TermId,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

// ── Inner state ───────────────────────────────────────────────────────────────

struct RingStoreInner {
    /// Bidirectional Term ↔ TermId / GraphName ↔ GraphId mapping.
    dict: Dictionary,
    /// Per-graph Ring indexes (built by `compact()`).
    graphs: HashMap<GraphId, Arc<GraphRing>>,
    /// Live write buffer (inserts and tombstones since last compaction).
    delta: Delta,
    /// Named-graph IDs that have been explicitly registered (tracks empty graphs).
    named_graph_ids: HashSet<GraphId>,
}

impl RingStoreInner {
    fn new() -> Self {
        Self {
            dict: Dictionary::new(),
            graphs: HashMap::new(),
            delta: Delta::new(),
            named_graph_ids: HashSet::new(),
        }
    }
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
    /// Create an empty `RingStore`.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RingStoreInner::new()),
        }
    }

    /// Merge the delta into the Ring index.
    ///
    /// After compaction:
    /// - All triples live in the per-graph `GraphRing` sorted arrays.
    /// - The delta is cleared.
    /// - Queries via `quads_for_pattern` continue to work (now scanning only the Ring).
    ///
    /// **When to call:** After a bulk load, or when the delta grows large (> 1M triples).
    /// This can be triggered automatically by a background thread when needed.
    pub fn compact(&self) -> Result<(), Oxigraph> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;

        // Collect per-graph triple sets.
        // Start from what's already in the Ring (if any prior compaction ran).
        let mut per_graph: HashMap<GraphId, Vec<[u64; 3]>> = HashMap::new();
        for (&g_id, ring) in &inner.graphs {
            per_graph.insert(g_id, ring.spo_triples());
        }

        // Apply delta: inserts add, tombstones remove.
        for (&key, &is_insert) in inner.delta.iter() {
            let (g_id, s_id, p_id, o_id) = Dictionary::unpack_quad(key);
            let triples = per_graph.entry(g_id).or_default();
            let triple = [s_id.as_u64(), p_id.as_u64(), o_id.as_u64()];
            if is_insert {
                triples.push(triple);
            } else {
                triples.retain(|t| t != &triple);
            }
        }

        // Rebuild Ring for each graph.
        let mut new_graphs: HashMap<GraphId, Arc<GraphRing>> = HashMap::new();
        for (g_id, triples) in per_graph {
            // sort_unstable + dedup for O(n log n) deduplication.
            let mut t = triples;
            t.sort_unstable();
            t.dedup();
            if !t.is_empty() {
                let mut builder = RingBuilder::new();
                for [s, p, o] in t {
                    builder.add(s, p, o);
                }
                new_graphs.insert(g_id, Arc::new(builder.build()));
            }
        }

        inner.graphs = new_graphs;
        inner.delta.clear();
        Ok(())
    }

    /// Number of triples stored across all graphs (approximation during merge).
    pub fn triple_count(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        let ring_total: usize = inner.graphs.values().map(|r| r.n).sum();
        let delta_inserts = inner.delta.insert_count();
        let delta_tombstones = inner.delta.tombstone_count();
        ring_total.saturating_sub(delta_tombstones) + delta_inserts
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
    let s_term = dict.get_term(s_id)?;
    let p_term = dict.get_term(p_id)?;
    let o_term = dict.get_term(o_id)?;

    // Subject may be NamedNode, BlankNode, or (RDF-star) Triple — all are valid.
    // Literals are never legal RDF subjects; return None so they are silently skipped.
    let subject: Term = match s_term {
        Term::NamedNode(_) | Term::BlankNode(_) | Term::Triple(_) => s_term.clone(),
        Term::Literal(_) => return None,
    };
    let predicate: NamedNode = match p_term {
        Term::NamedNode(n) => n.clone(),
        _ => return None, // predicates must be IRIs
    };

    Some(StoredQuad {
        subject,
        predicate,
        object: o_term.clone(),
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
        inner.dict.get_term(tid).cloned()
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

        // Intern all terms and the graph name.
        let g_id = inner.dict.intern_graph(&quad.graph_name)?;
        let s_id = inner.dict.intern(&subject_to_term(&quad.subject))?;
        let p_id = inner.dict.intern_predicate(&quad.predicate)?;
        let o_id = inner.dict.intern(&quad.object)?;

        let key = Dictionary::pack_quad(g_id, s_id, p_id, o_id);

        // Delta takes priority.
        match inner.delta.get(key) {
            Some(true) => return Ok(false), // already a live insert in delta
            Some(false) => {
                // Was tombstoned — re-inserting revives it.
                inner.delta.insert_key(key);
                return Ok(true);
            }
            None => {}
        }

        // Check in the Ring (immutable sorted arrays).
        if let Some(ring) = inner.graphs.get(&g_id)
            && ring.contains(s_id.as_u64(), p_id.as_u64(), o_id.as_u64())
        {
            return Ok(false); // already present in Ring, not tombstoned
        }

        // New triple — add to delta.
        inner.delta.insert_key(key);

        // Track named graphs (for known_named_graphs enumeration).
        if let GraphName::NamedNode(_) = &quad.graph_name {
            inner.named_graph_ids.insert(g_id);
        }

        Ok(true)
    }

    fn remove(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;

        // If the term isn't in the dict, it can't be in the store.
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

        // Delta takes priority.
        match inner.delta.get(key) {
            Some(true) => {
                // In delta as insert — remove it (delete from delta entirely).
                // We remove the delta entry rather than tombstoning, because the
                // triple isn't in the Ring (it was only ever in the delta).
                inner.delta.tombstone_key(key); // mark as deleted
                return Ok(true);
            }
            Some(false) => return Ok(false), // already tombstoned
            None => {}
        }

        // Check Ring.
        if let Some(ring) = inner.graphs.get(&g_id)
            && ring.contains(s_id.as_u64(), p_id.as_u64(), o_id.as_u64())
        {
            // In Ring — add a tombstone to the delta.
            inner.delta.tombstone_key(key);
            return Ok(true);
        }

        Ok(false)
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
        let ring_total: usize = inner.graphs.values().map(|r| r.n).sum();
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
            let g_id = inner.dict.intern_graph(graph)?;
            inner.named_graph_ids.insert(g_id);
        }
        Ok(())
    }

    fn extend(&self, quads: impl IntoIterator<Item = Quad>) -> Result<usize, Oxigraph> {
        // Bulk insert: intern all terms under one lock acquisition per quad.
        // For very large bulk loads, callers should call `compact()` afterwards.
        let mut count = 0usize;
        for quad in quads {
            if self.insert(&quad)? {
                count += 1;
            }
        }
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
}
