//! Ring-accelerated property path evaluation.
//!
//! For simple-predicate paths (`PPE::NamedNode(p)`), the Ring index provides
//! O(degree) neighbor enumeration via [`Dataset::lftj_join_scan`], avoiding the
//! O(total-triples) full-scan that `find_quads` requires.
//!
//! ## Principled approach
//!
//! Reference: "BWT Indexes for Optimal Joins"
//! (OASIcs, Arroyuelo/Navarro et al.), §Property Paths / RPQs.
//!
//! For a path `p+` or `p*` over a Ring-indexed dataset:
//!
//! 1. Enumerate sources via `lftj_join_scan(None, Some(p_id), None, 0, ag)` —
//!    PSO ordering — iterating subjects with predicate `p` in O(|S_p|).
//! 2. For each source `s`, enumerate successors via
//!    `lftj_join_scan(Some(s), Some(p_id), None, 2, ag)` — SPO ordering —
//!    iterating objects in O(deg(s, p)).
//! 3. BFS from each source accumulates the full transitive closure.
//!
//! Total work is O(|closure|), which is ≪ O(n) when predicate `p` appears on
//! only a fraction of all graph triples.  The Ring's LOUDS tries provide O(1)
//! navigation per step and O(log ℓ) seek via exponential search, so each
//! adjacency query is considerably faster than a `find_quads` round-trip.

use crate::dataset::{Dataset, GraphSelector};
use anyhow::Result;
use oxrdf::{NamedNode, Term};
use std::collections::{HashSet, VecDeque};

// ── Pair enumeration ──────────────────────────────────────────────────────────

/// Enumerate all `(subject, object)` pairs for a single named-node predicate,
/// using the Ring's LOUDS trie iterators instead of a full `find_quads` scan.
///
/// ## Access pattern
///
/// - `lftj_join_scan(None, Some(p_id), None, 0, ag)` — PSO ordering — yields
///   all subjects that have predicate `p` in O(|S_p|).
/// - For each subject `s`, `lftj_join_scan(Some(s), Some(p_id), None, 2, ag)` —
///   SPO ordering — yields all objects for `(s, p)` in O(deg(s, p)).
///
/// Returns `None` if LFTJ is unavailable (e.g. predicate not in dictionary),
/// so the caller can fall through to the generic `find_quads` path.
pub fn ring_pairs_for_pred<D: Dataset>(
    dataset: &D,
    pred: &NamedNode,
    ag: &GraphSelector,
) -> Option<Result<Vec<(Term, Term)>>> {
    let p_id = dataset.lftj_intern_term(&Term::NamedNode(pred.clone()), ag)?;

    // Subjects with predicate p — PSO depth-1 iterator within P=p_id.
    let mut s_iter = dataset.lftj_join_scan(None, Some(p_id), None, 0, ag)?;

    let mut pairs: Vec<(Term, Term)> = Vec::new();

    while !s_iter.at_end() {
        let s_id = s_iter.key();
        let s_term = match dataset.lftj_decode_term(s_id) {
            Some(t) => t,
            None => {
                s_iter.advance();
                continue;
            }
        };

        // Objects for (s, p) — SPO depth-2 iterator within S=s_id, P=p_id.
        let mut o_iter = dataset.lftj_join_scan(Some(s_id), Some(p_id), None, 2, ag)?;

        while !o_iter.at_end() {
            let o_id = o_iter.key();
            if let Some(o_term) = dataset.lftj_decode_term(o_id) {
                pairs.push((s_term.clone(), o_term));
            }
            o_iter.advance();
        }

        s_iter.advance();
    }

    Some(Ok(pairs))
}

// ── Node enumeration ──────────────────────────────────────────────────────────

/// Enumerate all subject IDs that have predicate `pred_id` in the active graph.
///
/// Uses `lftj_join_scan(None, Some(pred_id), None, 0, ag)` — PSO ordering —
/// O(|S_p|) rather than O(n total triples).
///
/// Returns `None` if LFTJ is unavailable.
pub fn ring_subjects_for_pred<D: Dataset>(
    dataset: &D,
    pred_id: u64,
    ag: &GraphSelector,
) -> Option<Result<Vec<u64>>> {
    let mut iter = dataset.lftj_join_scan(None, Some(pred_id), None, 0, ag)?;
    let mut ids: Vec<u64> = Vec::new();
    while !iter.at_end() {
        ids.push(iter.key());
        iter.advance();
    }
    Some(Ok(ids))
}

/// Enumerate all distinct node IDs (subjects ∪ objects) in the active graph.
///
/// Used by `ZeroOrMore` transitive closure to generate identity pairs `(x, x)`
/// for every graph node, including nodes not connected by the path predicate.
///
/// - Subjects: `lftj_join_scan(None, None, None, 0, ag)` → SPO depth-0, O(|S|).
/// - Objects:  `lftj_join_scan(None, None, None, 2, ag)` → OPS depth-0, O(|O|).
///
/// Returns `None` if LFTJ is unavailable.
pub fn ring_all_node_ids<D: Dataset>(dataset: &D, ag: &GraphSelector) -> Option<Result<Vec<u64>>> {
    let mut s_iter = dataset.lftj_join_scan(None, None, None, 0, ag)?;
    let mut o_iter = dataset.lftj_join_scan(None, None, None, 2, ag)?;

    let mut ids: HashSet<u64> = HashSet::new();
    while !s_iter.at_end() {
        ids.insert(s_iter.key());
        s_iter.advance();
    }
    while !o_iter.at_end() {
        ids.insert(o_iter.key());
        o_iter.advance();
    }
    Some(Ok(ids.into_iter().collect()))
}

// ── Transitive closure ────────────────────────────────────────────────────────

/// BFS transitive closure for a single predicate, using Ring TrieIterators for
/// lazy neighbor enumeration.
///
/// Each BFS step calls `lftj_join_scan(Some(curr_id), Some(pred_id), None, 2, ag)`
/// — SPO ordering — to enumerate successors in O(deg(curr, p)) rather than
/// scanning a pre-materialised adjacency HashMap.
///
/// ## Parameters
///
/// - `pred_id` — interned predicate ID.
/// - `start_ids` — BFS starting node IDs (subjects only for `+`, all nodes for `*`).
/// - `include_identity` — `true` for `ZeroOrMore`; emits `(x, x)` for every
///   start node even when no outgoing edge exists.
///
/// Returns `None` if Ring neighbor enumeration becomes unavailable mid-BFS
/// (caller falls back to the generic HashMap-BFS).
pub fn ring_bfs_transitive<D: Dataset>(
    dataset: &D,
    pred_id: u64,
    start_ids: &[u64],
    include_identity: bool,
    ag: &GraphSelector,
) -> Option<Result<Vec<(Term, Term)>>> {
    let mut result: Vec<(Term, Term)> = Vec::new();
    // (start_id, reachable_id) deduplication across all BFS roots.
    let mut global_seen: HashSet<(u64, u64)> = HashSet::new();

    for &start_id in start_ids {
        let start_term = match dataset.lftj_decode_term(start_id) {
            Some(t) => t,
            None => continue,
        };

        // Identity pair for ZeroOrMore (*).
        if include_identity && global_seen.insert((start_id, start_id)) {
            result.push((start_term.clone(), start_term.clone()));
        }

        // BFS: accumulate all nodes reachable from start_id via pred_id.
        //
        // `visited` prevents re-expanding a node that's already been processed
        // from this root (loop / cycle safety). We still check `global_seen` for
        // each (start, nbr) pair because a neighbor may be reachable from this
        // root without being newly visited (e.g. it was already visited before we
        // reach it via a different path through a cycle).
        let mut visited: HashSet<u64> = HashSet::new();
        visited.insert(start_id);
        let mut queue: VecDeque<u64> = VecDeque::new();
        queue.push_back(start_id);

        while let Some(curr_id) = queue.pop_front() {
            let mut nbr_iter = dataset.lftj_join_scan(Some(curr_id), Some(pred_id), None, 2, ag)?;

            while !nbr_iter.at_end() {
                let nbr_id = nbr_iter.key();

                // Only expand unseen nodes to avoid infinite BFS in cycles.
                if visited.insert(nbr_id) {
                    queue.push_back(nbr_id);
                }

                // Emit every (start, nbr) pair the first time we see it —
                // including (start, start) when a cycle brings us back (correct
                // per SPARQL ALP semantics for `+`).
                if global_seen.insert((start_id, nbr_id))
                    && let Some(nbr_term) = dataset.lftj_decode_term(nbr_id)
                {
                    result.push((start_term.clone(), nbr_term));
                }

                nbr_iter.advance();
            }
        }
    }

    Some(Ok(result))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{GraphSelector, QuadIter, QuadPattern};
    use anyhow::Result;
    use oxigraph_nova_core::{GraphName, Term, TrieIterator};
    use oxrdf::NamedNode;

    // ── Stub Dataset for path unit tests ─────────────────────────────────────

    /// Minimal LFTJ-capable dataset for testing property path helpers.
    ///
    /// Stores triples as `[s_id, p_id, o_id]` and implements `lftj_join_scan`
    /// with a sorted `Vec` filter — semantically equivalent to what the Ring
    /// provides, without requiring the full LOUDS trie stack.
    struct PathStub {
        triples: Vec<[u64; 3]>,
        dict: Vec<(String, u64)>,
    }

    impl PathStub {
        fn new(triples: Vec<[u64; 3]>, dict: Vec<(&str, u64)>) -> Self {
            let mut t = triples;
            t.sort_unstable();
            t.dedup();
            Self {
                triples: t,
                dict: dict.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            }
        }

        fn id_of(&self, uri: &str) -> Option<u64> {
            self.dict.iter().find(|(k, _)| k == uri).map(|(_, v)| *v)
        }
    }

    // ── Trivial TrieIterator over sorted Vec<u64> ─────────────────────────────

    struct VecIter {
        vals: Vec<u64>,
        pos: usize,
    }

    impl TrieIterator for VecIter {
        fn key(&self) -> u64 {
            self.vals[self.pos]
        }
        fn seek(&mut self, t: u64) {
            while self.pos < self.vals.len() && self.vals[self.pos] < t {
                self.pos += 1;
            }
        }
        fn open(&self) -> Box<dyn TrieIterator> {
            Box::new(VecIter {
                vals: vec![],
                pos: 0,
            })
        }
        fn at_end(&self) -> bool {
            self.pos >= self.vals.len()
        }
    }

    impl Dataset for PathStub {
        fn find_quads<'a>(&'a self, _: &QuadPattern) -> Result<QuadIter<'a>> {
            Ok(Box::new(std::iter::empty()))
        }
        fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
            Ok(Box::new(std::iter::empty()))
        }
        fn supports_lftj(&self) -> bool {
            true
        }
        fn lftj_has_delta(&self) -> bool {
            false
        }

        fn lftj_intern_term(&self, term: &Term, _: &GraphSelector) -> Option<u64> {
            if let Term::NamedNode(n) = term {
                self.id_of(n.as_str())
            } else {
                None
            }
        }

        fn lftj_decode_term(&self, id: u64) -> Option<Term> {
            self.dict
                .iter()
                .find(|(_, v)| *v == id)
                .map(|(k, _)| Term::NamedNode(NamedNode::new_unchecked(k.clone())))
        }

        fn lftj_join_scan(
            &self,
            s: Option<u64>,
            p: Option<u64>,
            o: Option<u64>,
            target_field: usize,
            _: &GraphSelector,
        ) -> Option<Box<dyn TrieIterator>> {
            let mut vals: Vec<u64> = self
                .triples
                .iter()
                .filter(|t| {
                    s.is_none_or(|sv| t[0] == sv)
                        && p.is_none_or(|pv| t[1] == pv)
                        && o.is_none_or(|ov| t[2] == ov)
                })
                .map(|t| t[target_field])
                .collect();
            vals.sort_unstable();
            vals.dedup();
            Some(Box::new(VecIter { vals, pos: 0 }))
        }
    }

    // ── Fixture ───────────────────────────────────────────────────────────────

    fn n(s: &str) -> NamedNode {
        NamedNode::new_unchecked(s)
    }

    /// Graph: a→b→c→d (chain) with shortcut a→c, all via predicate p (id=20).
    fn chain_stub() -> PathStub {
        PathStub::new(
            vec![[1, 20, 2], [2, 20, 3], [3, 20, 4], [1, 20, 3]],
            vec![
                ("http://ex/a", 1),
                ("http://ex/b", 2),
                ("http://ex/c", 3),
                ("http://ex/d", 4),
                ("http://ex/p", 20),
            ],
        )
    }

    // ── ring_pairs_for_pred ───────────────────────────────────────────────────

    #[test]
    fn ring_pairs_for_pred_basic() {
        let ds = chain_stub();
        let pred = n("http://ex/p");
        let pairs = ring_pairs_for_pred(&ds, &pred, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // Direct edges: (a,b), (a,c), (b,c), (c,d)
        assert_eq!(pairs.len(), 4);
    }

    #[test]
    fn ring_pairs_for_pred_unknown_pred_returns_none() {
        let ds = chain_stub();
        let pred = n("http://ex/unknown");
        // lftj_intern_term returns None → ring_pairs_for_pred returns None (caller uses fallback)
        assert!(ring_pairs_for_pred(&ds, &pred, &GraphSelector::Default).is_none());
    }

    // ── ring_subjects_for_pred ────────────────────────────────────────────────

    #[test]
    fn ring_subjects_for_pred_basic() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let mut subjs = ring_subjects_for_pred(&ds, p_id, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        subjs.sort_unstable();
        // Subjects with predicate p: a(1), b(2), c(3)
        assert_eq!(subjs, vec![1, 2, 3]);
    }

    // ── ring_all_node_ids ─────────────────────────────────────────────────────

    #[test]
    fn ring_all_node_ids_basic() {
        let ds = chain_stub();
        let mut ids = ring_all_node_ids(&ds, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        ids.sort_unstable();
        // Subjects ∪ Objects: a(1), b(2), c(3), d(4)
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    // ── ring_bfs_transitive — OneOrMore (+) ───────────────────────────────────

    #[test]
    fn ring_bfs_one_or_more_chain() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let subjs = ring_subjects_for_pred(&ds, p_id, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        let pairs = ring_bfs_transitive(&ds, p_id, &subjs, false, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // Transitive closure of {a,b,c} via p+:
        // (a,b),(a,c),(a,d),(b,c),(b,d),(c,d) = 6
        assert_eq!(pairs.len(), 6);
    }

    #[test]
    fn ring_bfs_one_or_more_no_identity() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let subjs = ring_subjects_for_pred(&ds, p_id, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        let pairs = ring_bfs_transitive(&ds, p_id, &subjs, false, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // No identity pairs in OneOrMore
        let identity_count = pairs.iter().filter(|(s, o)| s == o).count();
        assert_eq!(identity_count, 0);
    }

    // ── ring_bfs_transitive — ZeroOrMore (*) ──────────────────────────────────

    #[test]
    fn ring_bfs_zero_or_more() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let all_ids = ring_all_node_ids(&ds, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        let pairs = ring_bfs_transitive(&ds, p_id, &all_ids, true, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // Identity pairs: (a,a),(b,b),(c,c),(d,d) = 4
        let identity_count = pairs.iter().filter(|(s, o)| s == o).count();
        assert_eq!(identity_count, 4);
        // Non-identity reachable: (a,b),(a,c),(a,d),(b,c),(b,d),(c,d) = 6
        assert_eq!(pairs.len(), 10);
    }

    // ── ring_bfs_transitive — cycles ──────────────────────────────────────────

    #[test]
    fn ring_bfs_cycle_two_nodes() {
        // Graph: a→b→a (2-cycle via predicate p=20)
        let ds = PathStub::new(
            vec![[1, 20, 2], [2, 20, 1]],
            vec![("http://ex/a", 1), ("http://ex/b", 2), ("http://ex/p", 20)],
        );
        let p_id = ds.id_of("http://ex/p").unwrap();
        let subjs = ring_subjects_for_pred(&ds, p_id, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        let pairs = ring_bfs_transitive(&ds, p_id, &subjs, false, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // (a→b), (a→b→a i.e. a,a), (b→a), (b→a→b i.e. b,b) = 4
        // Cycles: (a,a) and (b,b) are valid for + per SPARQL ALP semantics.
        assert_eq!(pairs.len(), 4);
    }

    #[test]
    fn ring_bfs_empty_graph() {
        let ds = PathStub::new(vec![], vec![("http://ex/p", 20)]);
        let p_id = 20u64;
        let subjs = ring_subjects_for_pred(&ds, p_id, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        assert!(subjs.is_empty());
        let pairs = ring_bfs_transitive(&ds, p_id, &subjs, false, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        assert!(pairs.is_empty());
    }
}
