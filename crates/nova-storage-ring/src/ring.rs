//! Ring index — full CompactLTJ LOUDS-trie implementation.
//!
//! Faithful implementation of Hogan et al. (SIGMOD'21 / TODS'24)
//! "Worst-Case Optimal Graph Joins in Almost No Space", using the
//! space and time optimal storage from:
//!
//! > Arroyuelo, Navarro, Gómez-Brandón et al. (VLDB Journal 2025)
//! > "CompactLTJ: Space and Time Efficient Leapfrog Triejoin on Graph Databases"
//!
//! ## Implementation
//!
//! Uses **six LOUDS height-3 tries** (one per SPO ordering) from
//! [`cltj::CltjData`].  Navigation is O(1) per step and O(log ℓ) per seek
//! (exponential search in the flat label array).  `contains()` and
//! `match_triples()` use binary search on the stored SPO-sorted array.
//!
//! ## Design
//!
//! A `GraphRing` stores the triples of one named graph (or the default graph)
//! as **six LOUDS tries** via [`CltjData`]:
//!
//! | Ordering | Depth 0 | Depth 1 | Depth 2 |
//! |---|---|---|---|
//! | SPO | S | P | O |
//! | SOP | S | O | P |
//! | PSO | P | S | O |
//! | POS | P | O | S |
//! | OPS | O | P | S |
//! | OSP | O | S | P |
//!
//! Each depth level is navigated in O(1) via LOUDS `child`/`degree`/`access`.
//! The `TrieIterator` interface consumed by LFTJ is implemented by
//! [`CltjTrieIter`] in `cltj.rs`.

use crate::cltj::{CltjData, build_cltj_data};
use oxigraph_nova_core::{EmptyTrieIter, TrieIterator};
use std::collections::HashMap;
use std::sync::Arc;

// ── Ordering enum ─────────────────────────────────────────────────────────────

/// Which of the six SPO-family orderings a trie iterator uses.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Spo,
    Sop,
    Pso,
    Pos,
    Ops,
    Osp,
}

impl SortOrder {
    /// Convert a `(col0, col1, col2)` triple back to canonical `[s, p, o]`.
    #[inline]
    pub fn to_spo(self, col0: u64, col1: u64, col2: u64) -> [u64; 3] {
        match self {
            SortOrder::Spo => [col0, col1, col2],
            SortOrder::Sop => [col0, col2, col1],
            SortOrder::Pso => [col1, col0, col2],
            SortOrder::Pos => [col2, col0, col1],
            SortOrder::Ops => [col2, col1, col0],
            SortOrder::Osp => [col1, col2, col0],
        }
    }
}

// ── RingData ──────────────────────────────────────────────────────────────────

/// All immutable data for one graph's Ring index.
struct RingData {
    /// SPO-sorted global triples for `spo_triples()` / `match_triples()` /
    /// `contains()`.
    spo: Vec<[u64; 3]>,

    /// Six LOUDS tries (one per ordering) — O(1) navigation per step.
    cltj: CltjData,
}

// ── LFTJ ordering selection ───────────────────────────────────────────────────

/// Choose the Ring ordering for a Leapfrog Triejoin scan.
///
/// `bound_fields` — field indices (0=S, 1=P, 2=O) that are bound (constant).
/// `target_field` — the variable field we are scanning for.
fn choose_array_for_lftj(bound_fields: &[usize], target_field: usize) -> SortOrder {
    let ignored: Vec<usize> = (0..3_usize)
        .filter(|&f| f != target_field && !bound_fields.contains(&f))
        .collect();
    let mut ord = bound_fields.to_vec();
    ord.push(target_field);
    ord.extend_from_slice(&ignored);
    debug_assert_eq!(ord.len(), 3);
    match (ord[0], ord[1], ord[2]) {
        (0, 1, 2) => SortOrder::Spo,
        (0, 2, 1) => SortOrder::Sop,
        (1, 0, 2) => SortOrder::Pso,
        (1, 2, 0) => SortOrder::Pos,
        (2, 1, 0) => SortOrder::Ops,
        (2, 0, 1) => SortOrder::Osp,
        _ => unreachable!("invalid field ordering {:?}", ord),
    }
}

// ── GraphRing ─────────────────────────────────────────────────────────────────

/// The Ring index for a single named graph (or the default graph).
///
/// Immutable after construction.  `RingStore` wraps it in `Arc` so it can be
/// swapped out atomically during an LSM merge without blocking readers.
pub struct GraphRing {
    /// Number of distinct triples stored in this graph.
    pub n: usize,
    data: Option<Arc<RingData>>,
}

impl GraphRing {
    /// All triples in SPO order (for testing / serialisation).
    pub fn spo_triples(&self) -> Vec<[u64; 3]> {
        match &self.data {
            None => Vec::new(),
            Some(d) => d.spo.clone(),
        }
    }

    /// Depth-0 `TrieIterator` for the given ordering.
    pub fn trie_iter(&self, ordering: SortOrder) -> Box<dyn TrieIterator> {
        match &self.data {
            None => Box::new(EmptyTrieIter),
            Some(d) => d.cltj.trie_iter(ordering),
        }
    }

    /// `true` if the triple `(s, p, o)` exists in this graph.
    pub fn contains(&self, s: u64, p: u64, o: u64) -> bool {
        let data = match &self.data {
            None => return false,
            Some(d) => d,
        };
        data.spo.binary_search(&[s, p, o]).is_ok()
    }

    /// Return all triples matching the optional `s`/`p`/`o` pattern.
    pub fn match_triples(&self, s: Option<u64>, p: Option<u64>, o: Option<u64>) -> Vec<[u64; 3]> {
        let data = match &self.data {
            None => return Vec::new(),
            Some(d) => d,
        };
        let (lo, hi) = spo_range(&data.spo, s, p);
        data.spo[lo..hi]
            .iter()
            .filter(|&&[sg, pg, og]| {
                s.is_none_or(|sv| sv == sg)
                    && p.is_none_or(|pv| pv == pg)
                    && o.is_none_or(|ov| ov == og)
            })
            .copied()
            .collect()
    }

    /// Estimate the number of distinct values for `target_field` given the other
    /// bound fields — used by the adaptive VEO predictor in LFTJ.
    ///
    /// Returns the global vocabulary size of `target_field` (0=S, 1=P, 2=O) as
    /// a conservative upper bound on the actual distinct-value count.  Using
    /// vocab size rather than exact per-node counts avoids LOUDS traversal while
    /// still giving correct relative ordering between fields (e.g. predicates
    /// have a much smaller vocab than subjects, so VEO correctly prefers
    /// iterating predicate variables first).
    ///
    /// When `n_bound > 0` bound fields are present, the estimate is scaled by
    /// `1 / (1 + n_bound)` to reflect that bound fields reduce fan-out, without
    /// requiring per-value traversal.
    pub fn estimate_count(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> u64 {
        let data = match &self.data {
            None => return 0,
            Some(d) => d,
        };
        let vocab = data.cltj.vocab_size_for_field(target_field);
        let n_bound = (s.is_some() as u64) + (p.is_some() as u64) + (o.is_some() as u64);
        // Divide by (n_bound + 1) so that each additional bound field reduces
        // the estimate, giving a useful relative ordering without exact counts.
        (vocab as u64) / (n_bound + 1)
    }

    /// Return a `TrieIterator` at the appropriate depth for an LFTJ join scan.
    ///
    /// `s`/`p`/`o` — bound values (constant); `target_field` — 0=S, 1=P, 2=O.
    /// The returned iterator yields values for the target field, with all bound
    /// fields already satisfied.
    pub fn join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator> {
        let data = match &self.data {
            None => return Box::new(EmptyTrieIter),
            Some(d) => d,
        };

        let mut bound: Vec<usize> = Vec::with_capacity(2);
        if s.is_some() && target_field != 0 {
            bound.push(0);
        }
        if p.is_some() && target_field != 1 {
            bound.push(1);
        }
        if o.is_some() && target_field != 2 {
            bound.push(2);
        }
        bound.sort_unstable();

        let sort_order = choose_array_for_lftj(&bound, target_field);
        let field_vals = [s, p, o];
        let bound_vals: Vec<u64> = bound.iter().map(|&f| field_vals[f].unwrap()).collect();

        let mut it: Box<dyn TrieIterator> = data.cltj.trie_iter(sort_order);
        if it.at_end() {
            return it;
        }

        for val in bound_vals {
            it.seek(val);
            if it.at_end() || it.key() != val {
                return Box::new(EmptyTrieIter);
            }
            it = it.open();
            if it.at_end() {
                return Box::new(EmptyTrieIter);
            }
        }

        it
    }
}

// ── SPO range helper ──────────────────────────────────────────────────────────

/// Return the `[lo, hi)` range in the SPO-sorted array for a given `(s, p)` prefix.
fn spo_range(spo: &[[u64; 3]], s: Option<u64>, p: Option<u64>) -> (usize, usize) {
    match (s, p) {
        (Some(sv), Some(pv)) => {
            let lo = spo.partition_point(|&[sg, pg, _]| (sg, pg) < (sv, pv));
            let hi = spo.partition_point(|&[sg, pg, _]| (sg, pg) <= (sv, pv));
            (lo, hi)
        }
        (Some(sv), None) => {
            let lo = spo.partition_point(|&[sg, _, _]| sg < sv);
            let hi = spo.partition_point(|&[sg, _, _]| sg <= sv);
            (lo, hi)
        }
        _ => (0, spo.len()),
    }
}

// ── RingBuilder ───────────────────────────────────────────────────────────────

/// Accumulates `(s_id, p_id, o_id)` tuples and builds a `GraphRing`.
pub struct RingBuilder {
    triples: Vec<[u64; 3]>,
}

impl RingBuilder {
    pub fn new() -> Self {
        Self {
            triples: Vec::new(),
        }
    }

    pub fn add(&mut self, s: u64, p: u64, o: u64) {
        self.triples.push([s, p, o]);
    }

    /// Consume the builder and produce a `GraphRing`.
    ///
    /// O(n log n) construction (6 sorts + LOUDS trie construction per ordering).
    /// Deduplication is applied automatically.
    pub fn build(self) -> GraphRing {
        let mut triples = self.triples;
        triples.sort_unstable();
        triples.dedup();
        let n = triples.len();
        if n == 0 {
            return GraphRing { n: 0, data: None };
        }

        // ── Compact vocabulary ────────────────────────────────────────────────

        let build_vocab = |extract: fn(&[u64; 3]) -> u64| -> (Vec<u64>, HashMap<u64, usize>) {
            let mut vals: Vec<u64> = triples.iter().map(extract).collect();
            vals.sort_unstable();
            vals.dedup();
            let map: HashMap<u64, usize> = vals.iter().enumerate().map(|(i, &v)| (v, i)).collect();
            (vals, map)
        };

        let (orig_s, map_s) = build_vocab(|t| t[0]);
        let (orig_p, map_p) = build_vocab(|t| t[1]);
        let (orig_o, map_o) = build_vocab(|t| t[2]);

        // ── Build six LOUDS tries ─────────────────────────────────────────────

        let orig_s = Arc::new(orig_s);
        let orig_p = Arc::new(orig_p);
        let orig_o = Arc::new(orig_o);

        let cltj = build_cltj_data(
            &triples,
            &map_s,
            &map_p,
            &map_o,
            Arc::clone(&orig_s),
            Arc::clone(&orig_p),
            Arc::clone(&orig_o),
        );

        // orig_s/p/o and map_s/p/o are consumed by build_cltj_data above.
        // RingData only keeps the SPO-sorted array and the six LOUDS tries.
        let _ = (map_s, map_p, map_o); // explicitly consumed above
        let data = Arc::new(RingData { spo: triples, cltj });

        GraphRing {
            n,
            data: Some(data),
        }
    }
}

impl Default for RingBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn build_ring(triples: &[[u64; 3]]) -> GraphRing {
        let mut b = RingBuilder::new();
        for &[s, p, o] in triples {
            b.add(s, p, o);
        }
        b.build()
    }

    #[test]
    fn empty_ring() {
        let r = build_ring(&[]);
        assert_eq!(r.n, 0);
        assert!(r.match_triples(None, None, None).is_empty());
    }

    #[test]
    fn dedup_on_build() {
        let r = build_ring(&[[1, 2, 3], [1, 2, 3], [4, 5, 6]]);
        assert_eq!(r.n, 2);
    }

    #[test]
    fn full_wildcard() {
        let r = build_ring(&[[1, 2, 3], [1, 2, 4], [5, 6, 7]]);
        let mut m = r.match_triples(None, None, None);
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [1, 2, 4], [5, 6, 7]]);
    }

    #[test]
    fn subject_bound() {
        let r = build_ring(&[[1, 2, 3], [1, 2, 4], [5, 6, 7]]);
        let mut m = r.match_triples(Some(1), None, None);
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [1, 2, 4]]);
    }

    #[test]
    fn predicate_bound() {
        let r = build_ring(&[[1, 2, 3], [1, 3, 4], [5, 2, 7]]);
        let mut m = r.match_triples(None, Some(2), None);
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [5, 2, 7]]);
    }

    #[test]
    fn object_bound() {
        let r = build_ring(&[[1, 2, 3], [1, 2, 4], [5, 6, 3]]);
        let mut m = r.match_triples(None, None, Some(3));
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [5, 6, 3]]);
    }

    #[test]
    fn sp_bound() {
        let r = build_ring(&[[1, 2, 3], [1, 2, 4], [1, 3, 5], [5, 2, 6]]);
        let mut m = r.match_triples(Some(1), Some(2), None);
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [1, 2, 4]]);
    }

    #[test]
    fn so_bound() {
        let r = build_ring(&[[1, 2, 3], [1, 3, 3], [2, 2, 3]]);
        let mut m = r.match_triples(Some(1), None, Some(3));
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [1, 3, 3]]);
    }

    #[test]
    fn exact_triple() {
        let r = build_ring(&[[1, 2, 3], [4, 5, 6]]);
        assert_eq!(r.match_triples(Some(1), Some(2), Some(3)), vec![[1, 2, 3]]);
        assert!(r.match_triples(Some(1), Some(2), Some(99)).is_empty());
    }

    #[test]
    fn contains_check() {
        let r = build_ring(&[[1, 2, 3], [4, 5, 6]]);
        assert!(r.contains(1, 2, 3));
        assert!(r.contains(4, 5, 6));
        assert!(!r.contains(1, 2, 4));
    }

    #[test]
    fn spo_triples_roundtrip() {
        let triples = vec![[1u64, 2, 3], [1, 2, 4], [5, 6, 7]];
        let r = build_ring(&triples);
        let mut out = r.spo_triples();
        out.sort();
        assert_eq!(out, triples);
    }

    // ── TrieIterator tests ────────────────────────────────────────────────────

    #[test]
    fn trie_iter_depth0_key_seek_advance() {
        let r = build_ring(&[[1, 2, 10], [1, 3, 11], [2, 2, 12], [3, 4, 13]]);
        let mut it = r.trie_iter(SortOrder::Spo);
        assert!(!it.at_end());
        assert_eq!(it.key(), 1);
        it.advance();
        assert_eq!(it.key(), 2);
        it.advance();
        assert_eq!(it.key(), 3);
        it.advance();
        assert!(it.at_end());
    }

    #[test]
    fn trie_iter_seek() {
        let r = build_ring(&[[1, 2, 10], [3, 4, 11], [7, 8, 12]]);
        let mut it = r.trie_iter(SortOrder::Spo);
        it.seek(4);
        assert_eq!(it.key(), 7);
        it.seek(100);
        assert!(it.at_end());
    }

    #[test]
    fn trie_iter_open_depth1_depth2() {
        let r = build_ring(&[[1, 2, 10], [1, 3, 11], [2, 5, 12]]);
        let mut it0 = r.trie_iter(SortOrder::Spo);
        assert_eq!(it0.key(), 1);

        let mut it1 = it0.open();
        assert_eq!(it1.key(), 2);
        let mut it2 = it1.open();
        assert_eq!(it2.key(), 10);
        it2.advance();
        assert!(it2.at_end());

        it1.advance();
        assert_eq!(it1.key(), 3);
        it1.advance();
        assert!(it1.at_end());

        it0.advance();
        assert_eq!(it0.key(), 2);
        let it1b = it0.open();
        assert_eq!(it1b.key(), 5);
    }

    #[test]
    fn d1b_sop_ordering() {
        // SOP: fixed=s, iterate distinct o values
        let r = build_ring(&[[1, 2, 10], [1, 3, 20], [1, 4, 10], [2, 2, 30]]);
        let it0 = r.trie_iter(SortOrder::Sop);
        assert_eq!(it0.key(), 1); // subject 1

        let mut it1 = it0.open(); // distinct objects for s=1: {10, 20}
        assert_eq!(it1.key(), 10);
        it1.advance();
        assert_eq!(it1.key(), 20);
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn d1b_pso_ordering() {
        // PSO: fixed=p, iterate distinct s values
        let r = build_ring(&[[1, 5, 10], [2, 5, 20], [3, 5, 30], [1, 6, 40]]);
        let it0 = r.trie_iter(SortOrder::Pso);
        assert_eq!(it0.key(), 5); // first predicate

        let mut it1 = it0.open(); // distinct subjects for p=5: {1, 2, 3}
        assert_eq!(it1.key(), 1);
        it1.advance();
        assert_eq!(it1.key(), 2);
        it1.advance();
        assert_eq!(it1.key(), 3);
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn d1b_seek() {
        // SOP: seek on D1B secondary
        let r = build_ring(&[[1, 2, 10], [1, 3, 20], [1, 4, 30], [1, 5, 40]]);
        let it0 = r.trie_iter(SortOrder::Sop);
        assert_eq!(it0.key(), 1);
        let mut it1 = it0.open();
        // Objects for s=1: {10, 20, 30, 40}
        it1.seek(25); // should land on 30
        assert_eq!(it1.key(), 30);
        it1.seek(50); // past end
        assert!(it1.at_end());
    }

    #[test]
    fn join_scan_sop() {
        // join_scan with s bound, target=o (uses SOP ordering)
        let r = build_ring(&[[1, 2, 10], [1, 3, 20], [2, 2, 30]]);
        let mut it = r.join_scan(Some(1), None, None, 2);
        assert!(!it.at_end());
        assert_eq!(it.key(), 10);
        it.advance();
        assert_eq!(it.key(), 20);
        it.advance();
        assert!(it.at_end());
    }
}
