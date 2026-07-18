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
//! - Compaction rebuilds each graph as a heap Braided Ring image from
//!   external `TermId` triples (dense remap via [`BraidedGraphImage`]).
//! - LFTJ is supported **only when the delta is empty** (same contract as
//!   LOUDS: joins run on the fully compacted index).
//!
//! Pattern scans always merge ring ∪ delta \ tombstones and are correct
//! with a non-empty delta.
//!
//! "Braided" in related types/docs is the D2 intersection algorithm, not
//! this store's product name.

use super::image::BraidedGraphImage;
use oxigraph_nova_core::{
    Dictionary, EmptyTrieIter, GRAPH_DEFAULT, GraphId, GraphName, LftjSource, NamedNode, Oxigraph,
    Quad, QuadOp, QuadStore, StoredQuad, Subject, Term, TermId, TrieIterator,
};
use oxigraph_nova_storage_louds::delta::Delta;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
    fn compact_locked(&mut self) -> Result<(), Oxigraph> {
        let mut per_graph: HashMap<GraphId, Vec<[u64; 3]>> = HashMap::new();

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

        let mut new_graphs = HashMap::new();
        for (g_id, triples) in per_graph {
            if triples.is_empty() {
                continue;
            }
            // Dedup is handled inside BraidedGraphImage::from_external_triples.
            new_graphs.insert(g_id, Arc::new(BraidedGraphImage::from_external_triples(&triples)));
        }

        self.dict.compact()?;
        self.graphs = new_graphs;
        self.delta.clear();
        self.compaction_count = self.compaction_count.saturating_add(1);
        Ok(())
    }
}

fn image_contains(img: &BraidedGraphImage, s: u64, p: u64, o: u64) -> bool {
    // Enumerate is fine at pilot scale; compact path already holds the lock.
    img.enumerate_spo_external()
        .into_iter()
        .any(|t| t == [s, p, o])
}

fn image_match_triples(
    img: &BraidedGraphImage,
    s: Option<u64>,
    p: Option<u64>,
    o: Option<u64>,
) -> Vec<[u64; 3]> {
    img.enumerate_spo_external()
        .into_iter()
        .filter(|t| {
            s.is_none_or(|sv| t[0] == sv)
                && p.is_none_or(|pv| t[1] == pv)
                && o.is_none_or(|ov| t[2] == ov)
        })
        .collect()
}

// ── Public store ──────────────────────────────────────────────────────────────

/// In-memory cyclic QWT Ring store: Dictionary + Delta + per-graph
/// [`BraidedGraphImage`].
///
/// Feature `cyclic-ring-pilot`. Not wired into `nova-store` as the default
/// backend (that remains `LoudsStore`).
pub struct RingStore {
    inner: Mutex<RingStoreInner>,
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
        }
    }

    /// Merge delta into Braided Ring images and clear the delta.
    pub fn compact(&self) -> Result<(), Oxigraph> {
        self.inner.lock().compact_locked()
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
        let mut inner = self.inner.lock();
        let mut count = 0usize;
        for quad in quads {
            if inner.apply_insert(&quad)? {
                count += 1;
            }
        }
        if count > 0 || !inner.delta.is_empty() {
            inner.compact_locked()?;
        }
        Ok(count)
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
        self.lftj_real_count(s, p, o, target_field, graph_id)
            .unwrap_or(0)
    }

    fn lftj_join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
        graph_id: u8,
    ) -> Option<Box<dyn TrieIterator>> {
        let inner = self.inner.lock();
        // LFTJ only on fully compacted state (delta must be empty).
        if !inner.delta.is_empty() {
            return None;
        }
        let g_id = GraphId(graph_id);
        match inner.graphs.get(&g_id) {
            None => Some(Box::new(EmptyTrieIter)),
            Some(img) => {
                let vals = img.join_scan_external(s, p, o, target_field);
                if vals.is_empty() {
                    Some(Box::new(EmptyTrieIter))
                } else {
                    Some(Box::new(BraidedExternalScan::new(vals)))
                }
            }
        }
    }

    fn lftj_real_count(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
        graph_id: u8,
    ) -> Option<u64> {
        let inner = self.inner.lock();
        if !inner.delta.is_empty() {
            return None;
        }
        let g_id = GraphId(graph_id);
        match inner.graphs.get(&g_id) {
            None => Some(0),
            Some(img) => {
                Some(img.join_scan_external(s, p, o, target_field).len() as u64)
            }
        }
    }

    fn lftj_has_delta(&self) -> bool {
        !self.inner.lock().delta.is_empty()
    }
}

/// Flat external-ID scan for store-level LFTJ (same shape as Phase 4b).
struct BraidedExternalScan {
    vals: Vec<u64>,
    pos: usize,
}

impl BraidedExternalScan {
    fn new(vals: Vec<u64>) -> Self {
        Self { vals, pos: 0 }
    }
}

impl TrieIterator for BraidedExternalScan {
    fn key(&self) -> u64 {
        self.vals[self.pos]
    }

    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        if self.vals[self.pos] >= target {
            return;
        }
        self.pos = self.vals.partition_point(|&v| v < target);
    }

    fn advance(&mut self) {
        if !self.at_end() {
            self.pos += 1;
        }
    }

    fn open(&self) -> Box<dyn TrieIterator> {
        Box::new(EmptyTrieIter)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.vals.len()
    }

    fn remaining_count(&self) -> u64 {
        self.vals.len().saturating_sub(self.pos) as u64
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
        self.inner.lock().apply_insert(quad)
    }

    fn remove(&self, quad: &Quad) -> Result<bool, Oxigraph> {
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

