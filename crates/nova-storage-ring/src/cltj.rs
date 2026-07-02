//! CompactLTJ LOUDS-trie iterator and data structures.
//!
//! Reference: Arroyuelo, Navarro, Gómez-Brandón et al.
//! "CompactLTJ: Space and Time Efficient Leapfrog Triejoin on Graph Databases"
//! VLDB Journal, 2025.
//!
//! ## Design
//!
//! [`CltjData`] holds **six LOUDS height-3 tries**, one per SPO ordering
//! (SPO, SOP, PSO, POS, OPS, OSP).  Each trie stores the corresponding sort of
//! the dataset as a compact trie and exposes a [`CltjTrieIter`] that implements
//! the [`TrieIterator`] trait consumed by the LFTJ evaluator.
//!
//! Every trie navigation operation is O(1) (child, degree, access via LOUDS
//! rank/select), while seek uses exponential search O(log ℓ).  This replaces
//! the WaveletMatrix Ring's O(log σ) backward-search steps with O(1) per step.
//!
//! ## Paired build to reduce sort work
//!
//! The six orderings form **three natural pairs** by depth-0 field:
//!
//! | Pair   | Depth-0 | Primary | Secondary |
//! |--------|---------|---------|-----------|
//! | S-pair | Subject | SPO     | SOP       |
//! | P-pair | Pred.   | PSO     | POS       |
//! | O-pair | Object  | OPS     | OSP       |
//!
//! Within each pair, the depth-0 grouping is identical.  [`build_pair_shared_c0`]
//! exploits this: the primary ordering re-uses the already-sorted input, and the
//! secondary ordering performs only per-group (within-c0) re-sorts of c1↔c2.
//! Total sort work falls from 6 full O(N log N) sorts to 2 full sorts +
//! 3 sets of within-group sorts ≈ 4.5 equivalent sorts — roughly a 25% saving.
//!
//! Additionally, **SPO comes in pre-sorted** from the caller
//! (`build_cltj_data` receives `spo_sorted`), so the S-pair needs zero full
//! sorts: one free primary + one set of within-group secondary sorts.
//!
//! ## Vocabulary storage
//!
//! Each trie stores three vocabulary arrays (`vocab[0..2]`) as `Arc<Vec<u64>>`.
//! Using direct `Vec<u64>` indexing keeps `key()` at O(1) with no bit-unpacking
//! overhead (critical since it's called on every leapfrog step), and allows
//! `seek()` to use `partition_point` which the compiler can SIMD-vectorise.
//!
//! ## Structure per trie
//!
//! ```text
//! depth 0 → primary field  (e.g. S for SPO)
//! depth 1 → secondary field (e.g. P for SPO)
//! depth 2 → leaf field      (e.g. O for SPO)
//! ```
//!
//! Labels stored in L are **0-indexed compact local IDs**.  The three
//! vocabulary arrays (`vocab[0]`, `vocab[1]`, `vocab[2]`) map local → global
//! (u64 term IDs) for each depth.

use crate::louds::{LoudsTrie, build_louds_from_sorted};
use crate::ring::SortOrder;
use oxigraph_nova_core::{EmptyTrieIter, TrieIterator};
use std::sync::Arc;

// ── CltjTrie ──────────────────────────────────────────────────────────────────

/// A single LOUDS trie for one sort ordering, with vocabulary for global-ID
/// translation.
///
/// `vocab[d]` holds a sorted `Vec<u64>` of global term IDs for depth `d`.
/// Direct `Vec` indexing keeps `key()` at O(1) with no bit-unpacking, and
/// `seek()` can use `partition_point` which the compiler can SIMD-vectorise.
pub struct CltjTrie {
    /// LOUDS bitvector + label array.
    louds: LoudsTrie,
    /// `vocab[d]` = sorted global IDs for depth `d` (0, 1, or 2).
    /// `vocab[d][local_id]` → global TermId as `u64`.
    vocab: [Arc<Vec<u64>>; 3],
}

impl CltjTrie {
    /// Number of distinct values (vocabulary size) at depth `d` (0, 1, or 2).
    ///
    /// Used by [`CltjData::vocab_size_for_field`] to produce cardinality
    /// estimates for adaptive VEO in LFTJ without opening a TrieIterator.
    pub fn vocab_len(&self, d: usize) -> usize {
        if d < 3 { self.vocab[d].len() } else { 0 }
    }

    /// Create a depth-0 `CltjTrieIter` positioned at the first root child.
    ///
    /// Returns [`EmptyTrieIter`] if the trie is empty.
    pub fn iter_d0(self: &Arc<Self>) -> Box<dyn TrieIterator> {
        let degree = self.louds.root_degree();
        if degree == 0 {
            return Box::new(EmptyTrieIter);
        }
        Box::new(CltjTrieIter {
            trie: Arc::clone(self),
            hi: degree,
            pos: 1,
            depth: 0,
        })
    }
}

// ── CltjData ──────────────────────────────────────────────────────────────────

/// Six LOUDS tries (one per ordering) for a single named graph.
pub struct CltjData {
    tries: [Arc<CltjTrie>; 6],
}

impl CltjData {
    /// Index into `tries` for a given ordering.
    #[inline]
    fn idx(ord: SortOrder) -> usize {
        match ord {
            SortOrder::Spo => 0,
            SortOrder::Sop => 1,
            SortOrder::Pso => 2,
            SortOrder::Pos => 3,
            SortOrder::Ops => 4,
            SortOrder::Osp => 5,
        }
    }

    /// Depth-0 `CltjTrieIter` for the given ordering.
    pub fn trie_iter(&self, ord: SortOrder) -> Box<dyn TrieIterator> {
        self.tries[Self::idx(ord)].iter_d0()
    }

    /// Number of distinct global values for the given SPO field (0=S, 1=P, 2=O).
    ///
    /// Uses the SPO trie's vocabulary arrays, which contain all distinct values
    /// at each depth.  Returns a conservative upper bound on the cardinality of
    /// any scan targeting that field — used by adaptive VEO estimation in LFTJ.
    pub fn vocab_size_for_field(&self, field: usize) -> usize {
        // SPO trie: vocab[0]=subjects, vocab[1]=predicates, vocab[2]=objects.
        self.tries[Self::idx(SortOrder::Spo)].vocab_len(field)
    }
}

// ── Pair builder ──────────────────────────────────────────────────────────────

/// Build two LOUDS tries that share the same depth-0 (c0) grouping.
///
/// ## Arguments
///
/// * `primary_sorted` — triples `[c0, c1, c2]` already sorted by `(c0, c1, c2)`.
///   Used directly for the *primary* trie (no re-sort).
/// * `vocab_primary` — `[d0, d1, d2]` global-ID vocab for the primary ordering.
/// * `vocab_secondary` — `[d0, d1, d2]` vocab for the secondary ordering,
///   where d1 and d2 are swapped relative to the primary (e.g. SPO→SOP means
///   d1=O, d2=P).
///
/// ## Secondary sort strategy
///
/// The secondary trie indexes `[c0, c2, c1]` (c1 and c2 swapped).  Rather than
/// re-sorting the entire N-triple array, we reuse the depth-0 group boundaries
/// already implied by `primary_sorted` and sort only within each c0 group.
/// Each group sort is `O(g · log g)` where `g` is the group size; summed over
/// all groups this is `O(N log(N/G))` — cheaper than a full `O(N log N)` sort
/// by a constant factor proportional to the depth-0 branching factor.
fn build_pair_shared_c0(
    primary_sorted: &[[u32; 3]],
    vocab_primary: [Arc<Vec<u64>>; 3],
    vocab_secondary: [Arc<Vec<u64>>; 3],
) -> (Arc<CltjTrie>, Arc<CltjTrie>) {
    let empty_louds = || LoudsTrie::from_raw(&[], &[]);

    // ── Primary trie: input is already sorted by (c0, c1, c2) ────────────────
    let primary_louds = build_louds_from_sorted(primary_sorted).unwrap_or_else(empty_louds);

    // ── Secondary trie: [c0, c2, c1] sorted by (c0, c2, c1) ─────────────────
    //
    // Process each c0 group from the primary, swap c1↔c2, and sort within the
    // group.  The overall output vector is sorted by (c0, c2, c1) because c0
    // values are non-decreasing and within-group entries are sorted after swap.
    let mut secondary: Vec<[u32; 3]> = Vec::with_capacity(primary_sorted.len());
    let mut i = 0;
    while i < primary_sorted.len() {
        let c0 = primary_sorted[i][0];
        let end = i + primary_sorted[i..].partition_point(|t| t[0] == c0);
        let group_start = secondary.len();
        // Emit [c0, c2, c1] for every triple in this c0 group.
        for triple in &primary_sorted[i..end] {
            secondary.push([triple[0], triple[2], triple[1]]);
        }
        // Sort within the group by (c2, c1); c0 is constant, so sort_unstable
        // compares (c0, c2, c1) lexicographically — effectively (c2, c1).
        secondary[group_start..].sort_unstable();
        i = end;
    }
    // Dedup is safe here: secondary is sorted by (c0, c2, c1) overall because
    // c0 groups are emitted in non-decreasing c0 order, and each group is sorted.
    // Duplicates can only arise if c1 == c2 for some input triple (rare in RDF).
    secondary.dedup();
    let secondary_louds = build_louds_from_sorted(&secondary).unwrap_or_else(empty_louds);

    (
        Arc::new(CltjTrie {
            louds: primary_louds,
            vocab: vocab_primary,
        }),
        Arc::new(CltjTrie {
            louds: secondary_louds,
            vocab: vocab_secondary,
        }),
    )
}

// ── build_cltj_data ───────────────────────────────────────────────────────────

/// Build a [`CltjData`] from SPO-sorted global-ID triples.
///
/// ## Build strategy
///
/// Uses paired construction to minimise sort work:
///
/// | Pair   | Full sorts | Within-group sorts |
/// |--------|------------|--------------------|
/// | S-pair | 0 (SPO input is pre-sorted) | 1 (SOP: per-S sort of O↔P) |
/// | P-pair | 1 (PSO)    | 1 (POS: per-P sort of O↔S) |
/// | O-pair | 1 (OPS)    | 1 (OSP: per-O sort of S↔P) |
///
/// Total: **2 full sorts** instead of 6 — approximately a 25% reduction in build time.
pub fn build_cltj_data(
    spo_sorted: &[[u64; 3]],
    map_s: &std::collections::HashMap<u64, usize>,
    map_p: &std::collections::HashMap<u64, usize>,
    map_o: &std::collections::HashMap<u64, usize>,
    orig_s: Arc<Vec<u64>>,
    orig_p: Arc<Vec<u64>>,
    orig_o: Arc<Vec<u64>>,
) -> CltjData {
    // ── Map global IDs → compact local IDs in SPO order ───────────────────────
    //
    // `spo_sorted` is already sorted by (S, P, O) and deduplicated.
    // The resulting `ls_lp_lo` is therefore sorted by (ls, lp, lo) — i.e. in
    // SPO local-ID order — with no extra sort required for the S-pair primary.
    let ls_lp_lo: Vec<[u32; 3]> = spo_sorted
        .iter()
        .map(|&[s, p, o]| [map_s[&s] as u32, map_p[&p] as u32, map_o[&o] as u32])
        .collect();

    // ── S-pair: SPO (primary, free) + SOP (secondary, within-group) ──────────
    //
    // `ls_lp_lo` is the SPO-sorted local-ID array.  The SOP secondary swaps
    // depth-1 and depth-2 (P↔O) and re-sorts within each S group.
    let (spo_trie, sop_trie) = build_pair_shared_c0(
        &ls_lp_lo,
        [
            Arc::clone(&orig_s),
            Arc::clone(&orig_p),
            Arc::clone(&orig_o),
        ], // SPO: d0=S, d1=P, d2=O
        [
            Arc::clone(&orig_s),
            Arc::clone(&orig_o),
            Arc::clone(&orig_p),
        ], // SOP: d0=S, d1=O, d2=P
    );

    // ── P-pair: PSO (primary, 1 full sort) + POS (secondary, within-group) ───
    //
    // Remap columns to (lp, ls, lo) and sort to get PSO order.
    // POS is then built with within-P-group re-sorts of O↔S.
    let mut pso_sorted: Vec<[u32; 3]> = ls_lp_lo.iter().map(|&[ls, lp, lo]| [lp, ls, lo]).collect();
    pso_sorted.sort_unstable();
    pso_sorted.dedup();

    let (pso_trie, pos_trie) = build_pair_shared_c0(
        &pso_sorted,
        [
            Arc::clone(&orig_p),
            Arc::clone(&orig_s),
            Arc::clone(&orig_o),
        ], // PSO: d0=P, d1=S, d2=O
        [
            Arc::clone(&orig_p),
            Arc::clone(&orig_o),
            Arc::clone(&orig_s),
        ], // POS: d0=P, d1=O, d2=S
    );

    // ── O-pair: OPS (primary, 1 full sort) + OSP (secondary, within-group) ───
    //
    // Remap columns to (lo, lp, ls) and sort to get OPS order.
    // OSP is built with within-O-group re-sorts of S↔P.
    let mut ops_sorted: Vec<[u32; 3]> = ls_lp_lo.iter().map(|&[ls, lp, lo]| [lo, lp, ls]).collect();
    ops_sorted.sort_unstable();
    ops_sorted.dedup();

    let (ops_trie, osp_trie) = build_pair_shared_c0(
        &ops_sorted,
        [
            Arc::clone(&orig_o),
            Arc::clone(&orig_p),
            Arc::clone(&orig_s),
        ], // OPS: d0=O, d1=P, d2=S
        [
            Arc::clone(&orig_o),
            Arc::clone(&orig_s),
            Arc::clone(&orig_p),
        ], // OSP: d0=O, d1=S, d2=P
    );

    CltjData {
        tries: [spo_trie, sop_trie, pso_trie, pos_trie, ops_trie, osp_trie],
    }
}

// ── CltjTrieIter ──────────────────────────────────────────────────────────────

/// A [`TrieIterator`] backed by one LOUDS trie in [`CltjData`].
///
/// State: the current level is an inclusive label-position range `[lo, hi]`
/// within L.  The current position is `pos ∈ [lo, hi]`.  Exhausted when
/// `pos > hi`.
///
/// Navigation is O(1) per step (LOUDS rank/select), with O(log ℓ) seek via
/// exponential search — replacing the WaveletMatrix Ring's O(log σ) per step.
pub struct CltjTrieIter {
    /// The parent trie (shared between parent and child iterators).
    trie: Arc<CltjTrie>,
    /// Inclusive upper bound of the current node's children label-range in L.
    hi: usize,
    /// Current position within [lo, hi].  `pos > hi` means exhausted.
    pos: usize,
    /// Depth: 0 = primary field, 1 = secondary field, 2 = leaf field.
    depth: u8,
}

impl TrieIterator for CltjTrieIter {
    /// Return the global term ID at the current position.
    ///
    /// O(1): direct Vec index (local_id from LOUDS L, then vocab lookup).
    #[inline]
    fn key(&self) -> u64 {
        let local_id = self.trie.louds.label_at(self.pos) as usize;
        self.trie.vocab[self.depth as usize][local_id]
    }

    /// Advance to the first position where `key() >= target`.
    ///
    /// Uses `partition_point` on the vocab `&[u64]` slice for the local-ID
    /// binary search — the compiler can SIMD-vectorise this over contiguous
    /// memory.  Then `leap()` jumps to the matching L position.
    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        if target <= self.key() {
            return;
        }

        let vocab = &self.trie.vocab[self.depth as usize];
        // Binary search for first local_id where vocab[local_id] >= target.
        let local_target = vocab.partition_point(|&v| v < target);
        if local_target >= vocab.len() {
            // All vocab values are below target — exhaust the iterator.
            self.pos = self.hi + 1;
            return;
        }
        self.pos = self.trie.louds.leap(self.pos, self.hi, local_target as u32);
    }

    fn advance(&mut self) {
        if !self.at_end() {
            self.pos += 1;
        }
    }

    fn open(&self) -> Box<dyn TrieIterator> {
        if self.depth >= 2 {
            // Depth 2 = leaf level; no further descent.
            return Box::new(EmptyTrieIter);
        }
        // child T-position = select1(pos − 1)
        let child_v = self.trie.louds.child_from_label_pos(self.pos);
        let child_degree = self.trie.louds.degree(child_v);
        if child_degree == 0 {
            return Box::new(EmptyTrieIter);
        }
        let new_lo = child_v + 1;
        let new_hi = child_v + child_degree;
        Box::new(CltjTrieIter {
            trie: Arc::clone(&self.trie),
            hi: new_hi,
            pos: new_lo,
            depth: self.depth + 1,
        })
    }

    fn at_end(&self) -> bool {
        self.pos > self.hi
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a CltjData from a small set of global-ID triples and verify
    /// basic TrieIterator behaviour on the SPO ordering.
    fn make_cltj(triples: &[[u64; 3]]) -> CltjData {
        let mut all = triples.to_vec();
        all.sort_unstable();
        all.dedup();

        let mut orig_s: Vec<u64> = all.iter().map(|t| t[0]).collect();
        orig_s.sort_unstable();
        orig_s.dedup();
        let mut orig_p: Vec<u64> = all.iter().map(|t| t[1]).collect();
        orig_p.sort_unstable();
        orig_p.dedup();
        let mut orig_o: Vec<u64> = all.iter().map(|t| t[2]).collect();
        orig_o.sort_unstable();
        orig_o.dedup();

        let map_s: HashMap<u64, usize> = orig_s.iter().enumerate().map(|(i, &v)| (v, i)).collect();
        let map_p: HashMap<u64, usize> = orig_p.iter().enumerate().map(|(i, &v)| (v, i)).collect();
        let map_o: HashMap<u64, usize> = orig_o.iter().enumerate().map(|(i, &v)| (v, i)).collect();

        build_cltj_data(
            &all,
            &map_s,
            &map_p,
            &map_o,
            Arc::new(orig_s),
            Arc::new(orig_p),
            Arc::new(orig_o),
        )
    }

    #[test]
    fn cltj_spo_depth0_advance() {
        let cltj = make_cltj(&[[10, 20, 30], [10, 20, 40], [20, 30, 50]]);
        let mut it = cltj.trie_iter(SortOrder::Spo);
        assert!(!it.at_end());
        assert_eq!(it.key(), 10);
        it.advance();
        assert_eq!(it.key(), 20);
        it.advance();
        assert!(it.at_end());
    }

    #[test]
    fn cltj_spo_depth0_seek() {
        let cltj = make_cltj(&[[1, 2, 3], [3, 4, 5], [7, 8, 9]]);
        let mut it = cltj.trie_iter(SortOrder::Spo);
        it.seek(4);
        assert_eq!(it.key(), 7);
        it.seek(100);
        assert!(it.at_end());
    }

    #[test]
    fn cltj_spo_open_depth1() {
        let cltj = make_cltj(&[[1, 2, 10], [1, 3, 11], [2, 5, 12]]);
        let it0 = cltj.trie_iter(SortOrder::Spo);
        assert_eq!(it0.key(), 1);

        let mut it1 = it0.open();
        assert_eq!(it1.key(), 2); // first P for S=1
        it1.advance();
        assert_eq!(it1.key(), 3); // second P for S=1
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn cltj_spo_open_depth2() {
        let cltj = make_cltj(&[[1, 2, 10], [1, 2, 20], [1, 3, 30]]);
        let it0 = cltj.trie_iter(SortOrder::Spo);
        assert_eq!(it0.key(), 1);
        let it1 = it0.open();
        assert_eq!(it1.key(), 2); // P=2
        let mut it2 = it1.open();
        assert_eq!(it2.key(), 10);
        it2.advance();
        assert_eq!(it2.key(), 20);
        it2.advance();
        assert!(it2.at_end());
    }

    #[test]
    fn cltj_sop_ordering() {
        // SOP: primary=S, secondary=O, leaf=P
        let cltj = make_cltj(&[[1, 2, 10], [1, 3, 20], [1, 4, 10], [2, 2, 30]]);
        let it0 = cltj.trie_iter(SortOrder::Sop);
        assert_eq!(it0.key(), 1); // subject 1

        let mut it1 = it0.open(); // distinct O values for S=1: {10, 20}
        assert_eq!(it1.key(), 10);
        it1.advance();
        assert_eq!(it1.key(), 20);
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn cltj_pso_ordering() {
        // PSO: primary=P, secondary=S
        let cltj = make_cltj(&[[1, 5, 10], [2, 5, 20], [3, 5, 30], [1, 6, 40]]);
        let it0 = cltj.trie_iter(SortOrder::Pso);
        assert_eq!(it0.key(), 5); // first predicate

        let mut it1 = it0.open(); // distinct S values for P=5: {1, 2, 3}
        assert_eq!(it1.key(), 1);
        it1.advance();
        assert_eq!(it1.key(), 2);
        it1.advance();
        assert_eq!(it1.key(), 3);
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn cltj_seek_on_secondary() {
        // SOP: seek on secondary (O values)
        let cltj = make_cltj(&[[1, 2, 10], [1, 3, 20], [1, 4, 30], [1, 5, 40]]);
        let it0 = cltj.trie_iter(SortOrder::Sop);
        assert_eq!(it0.key(), 1);
        let mut it1 = it0.open(); // O values for S=1: {10, 20, 30, 40}
        it1.seek(25); // should land on 30
        assert_eq!(it1.key(), 30);
        it1.seek(50);
        assert!(it1.at_end());
    }

    #[test]
    fn cltj_join_scan_sop() {
        // Simulate join_scan: S=1 bound, target=O
        // Using CltjData::trie_iter(SOP) and descending to depth 1
        let cltj = make_cltj(&[[1, 2, 10], [1, 3, 20], [2, 2, 30]]);
        let mut it = cltj.trie_iter(SortOrder::Sop);
        // S=1 is the first (and only) S → seek to S=1
        it.seek(1);
        assert_eq!(it.key(), 1);
        let mut it1 = it.open(); // O values for S=1: {10, 20}
        assert_eq!(it1.key(), 10);
        it1.advance();
        assert_eq!(it1.key(), 20);
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn cltj_empty_trie() {
        let cltj = make_cltj(&[]);
        let it = cltj.trie_iter(SortOrder::Spo);
        assert!(it.at_end());
    }

    #[test]
    fn cltj_all_six_orderings_non_empty() {
        let cltj = make_cltj(&[[1, 2, 3], [1, 2, 4], [1, 3, 5], [2, 2, 6]]);
        for ord in [
            SortOrder::Spo,
            SortOrder::Sop,
            SortOrder::Pso,
            SortOrder::Pos,
            SortOrder::Ops,
            SortOrder::Osp,
        ] {
            let it = cltj.trie_iter(ord);
            assert!(!it.at_end(), "ordering {:?} should not be empty", ord);
        }
    }

    /// Verify PSO and POS orderings via the shared-pair builder.
    #[test]
    fn cltj_pos_ordering() {
        // POS: primary=P, secondary=O, leaf=S
        let cltj = make_cltj(&[[1, 5, 10], [2, 5, 20], [3, 5, 30], [4, 6, 10]]);
        let it0 = cltj.trie_iter(SortOrder::Pos);
        assert_eq!(it0.key(), 5); // first predicate

        let mut it1 = it0.open(); // distinct O values for P=5: {10, 20, 30}
        assert_eq!(it1.key(), 10);
        it1.advance();
        assert_eq!(it1.key(), 20);
        it1.advance();
        assert_eq!(it1.key(), 30);
        it1.advance();
        assert!(it1.at_end());
    }

    /// Verify OPS and OSP orderings via the shared-pair builder.
    #[test]
    fn cltj_osp_ordering() {
        // OSP: primary=O, secondary=S, leaf=P
        let cltj = make_cltj(&[[1, 2, 100], [3, 4, 100], [5, 6, 200]]);
        let it0 = cltj.trie_iter(SortOrder::Osp);
        assert_eq!(it0.key(), 100); // first object

        let mut it1 = it0.open(); // distinct S values for O=100: {1, 3}
        assert_eq!(it1.key(), 1);
        it1.advance();
        assert_eq!(it1.key(), 3);
        it1.advance();
        assert!(it1.at_end());
    }

    /// Cross-check: SPO and SOP each enumerate the same number of leaf triples.
    ///
    /// Uses only the public `TrieIterator` API (no internal field access).
    #[test]
    fn cltj_spo_sop_triple_count_matches() {
        let triples = &[[1u64, 2, 3], [1, 3, 4], [2, 2, 5], [2, 4, 3]];
        let cltj = make_cltj(triples);

        // Full depth-3 traversal via SPO.
        let mut spo_count = 0usize;
        let mut it0 = cltj.trie_iter(SortOrder::Spo);
        while !it0.at_end() {
            let mut it1 = it0.open();
            while !it1.at_end() {
                let mut it2 = it1.open();
                while !it2.at_end() {
                    spo_count += 1;
                    it2.advance();
                }
                it1.advance();
            }
            it0.advance();
        }
        assert_eq!(
            spo_count,
            triples.len(),
            "SPO traversal should see all triples"
        );

        // Full depth-3 traversal via SOP — must yield same count.
        let mut sop_count = 0usize;
        let mut it0 = cltj.trie_iter(SortOrder::Sop);
        while !it0.at_end() {
            let mut it1 = it0.open();
            while !it1.at_end() {
                let mut it2 = it1.open();
                while !it2.at_end() {
                    sop_count += 1;
                    it2.advance();
                }
                it1.advance();
            }
            it0.advance();
        }
        assert_eq!(
            sop_count,
            triples.len(),
            "SOP traversal should see all triples"
        );
    }
}
