//! ID-level LFTJ join/scan seam for Braided Ring (Phase 4b).
//!
//! Provides a [`TrieIterator`]-compatible `join_scan` over shared-alphabet
//! `u32` triples **without** dictionary, delta, or `QuadStore`.
//!
//! ## Contract (matches production LFTJ usage)
//!
//! `nova-query`'s LFTJ evaluator and property-path helpers call
//! `lftj_join_scan(s, p, o, target_field, graph)` once per active pattern at
//! each variable depth and then only use `key` / `seek` / `advance` / `at_end`
//! on that scan — they never call `open()` on the returned iterator. Depth
//! descent is re-expressed as a fresh scan with more bound fields.
//!
//! Therefore this seam returns a **flat** distinct-value iterator over the
//! target field under the optional bound-field filters. `open()` returns
//! [`EmptyTrieIter`] (same as the path-stub / SortedVecTrie filtered path).
//!
//! ## Correctness strategy
//!
//! Values are collected by filtering the shared-alphabet SPO enumeration
//! (heap or mmap), then sort+dedup. That matches the semantic contract of
//! `LftjSource::lftj_join_scan` and the oracle used by differentials. A later
//! phase can replace the materialize step with native RNV/RDI navigation
//! without changing this public API.
//!
//! Multi-range D2 intersection is exercised separately via
//! [`crate::cyclic_ring::facade::BraidedRingIndex::collect_intersection3`] —
//! it is the product triangle primitive, not the per-pattern scan.

use super::facade::BraidedRingIndex;
use oxigraph_nova_core::{EmptyTrieIter, TrieIterator};

/// Flat distinct-value iterator over one target field of a join scan.
///
/// `vals` is sorted ascending and deduplicated. Position is an index into
/// `vals`; exhausted when `pos >= vals.len()`.
pub struct BraidedJoinScan {
    vals: Vec<u64>,
    pos: usize,
}

impl BraidedJoinScan {
    #[inline]
    fn new(vals: Vec<u64>) -> Self {
        Self { vals, pos: 0 }
    }
}

impl TrieIterator for BraidedJoinScan {
    #[inline]
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
        let i = self.vals.partition_point(|&v| v < target);
        self.pos = i;
    }

    fn advance(&mut self) {
        if !self.at_end() {
            self.pos += 1;
        }
    }

    fn open(&self) -> Box<dyn TrieIterator> {
        // Flat scan: LFTJ re-opens via a fresh join_scan with more binds.
        Box::new(EmptyTrieIter)
    }

    #[inline]
    fn at_end(&self) -> bool {
        self.pos >= self.vals.len()
    }

    fn remaining_count(&self) -> u64 {
        self.vals.len().saturating_sub(self.pos) as u64
    }
}

impl BraidedRingIndex {
    /// ID-level `join_scan` equivalent of
    /// [`oxigraph_nova_core::LftjSource::lftj_join_scan`].
    ///
    /// `s` / `p` / `o`: `Some(id)` binds that field (shared-alphabet `u32`
    /// promoted to `u64` for the `TrieIterator` contract). `target_field`:
    /// 0=S, 1=P, 2=O — the field whose distinct values are iterated.
    ///
    /// Returns an always-exhausted iterator when no triples match (never
    /// `None`; callers that need "unsupported" use a higher-level store).
    pub fn join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator> {
        debug_assert!(target_field < 3, "target_field must be 0/1/2");
        let target_field = target_field.min(2);

        let triples = self.enumerate_spo();
        let mut vals: Vec<u64> = triples
            .iter()
            .filter(|t| {
                s.is_none_or(|sv| u64::from(t[0]) == sv)
                    && p.is_none_or(|pv| u64::from(t[1]) == pv)
                    && o.is_none_or(|ov| u64::from(t[2]) == ov)
            })
            .map(|t| u64::from(t[target_field]))
            .collect();
        vals.sort_unstable();
        vals.dedup();

        if vals.is_empty() {
            return Box::new(EmptyTrieIter);
        }
        Box::new(BraidedJoinScan::new(vals))
    }

    /// Collect all distinct target-field values from [`Self::join_scan`].
    ///
    /// Convenience for differentials / oracles (not on the LFTJ hot path).
    pub fn collect_join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Vec<u64> {
        let mut it = self.join_scan(s, p, o, target_field);
        let mut out = Vec::new();
        while !it.at_end() {
            out.push(it.key());
            it.advance();
        }
        out
    }

    /// Exact distinct count for the same filters as [`Self::join_scan`].
    ///
    /// Mirrors `LftjSource::lftj_real_count` semantics at the ID level.
    pub fn real_count(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> u64 {
        self.collect_join_scan(s, p, o, target_field).len() as u64
    }
}

// ── Oracle helpers (shared with facade::oracle) ─────────────────────────────

/// Sorted distinct target-field values under optional bound filters.
pub fn oracle_join_scan(
    triples: &[[u32; 3]],
    s: Option<u64>,
    p: Option<u64>,
    o: Option<u64>,
    target_field: usize,
) -> Vec<u64> {
    let tf = target_field.min(2);
    let mut vals: Vec<u64> = triples
        .iter()
        .filter(|t| {
            s.is_none_or(|sv| u64::from(t[0]) == sv)
                && p.is_none_or(|pv| u64::from(t[1]) == pv)
                && o.is_none_or(|ov| u64::from(t[2]) == ov)
        })
        .map(|t| u64::from(t[tf]))
        .collect();
    vals.sort_unstable();
    vals.dedup();
    vals
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::facade::BraidedRingIndex;
    use super::super::{Col, RowRange};

    fn sample() -> Vec<[u32; 3]> {
        vec![
            [0, 0, 1],
            [0, 0, 2],
            [0, 1, 1],
            [1, 0, 0],
            [1, 1, 2],
            [2, 0, 1],
            [2, 1, 0],
            [1, 0, 1],
        ]
    }

    #[test]
    fn join_scan_unbound_subject_matches_oracle() {
        let t = sample();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let got = idx.collect_join_scan(None, None, None, 0);
        let want = oracle_join_scan(&t, None, None, None, 0);
        assert_eq!(got, want);
        assert_eq!(got, vec![0, 1, 2]);
    }

    #[test]
    fn join_scan_bound_s_target_o() {
        let t = sample();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        // Objects under s=0: 1, 2 (and p=0 or 1)
        let got = idx.collect_join_scan(Some(0), None, None, 2);
        let want = oracle_join_scan(&t, Some(0), None, None, 2);
        assert_eq!(got, want);
        assert_eq!(got, vec![1, 2]);
    }

    #[test]
    fn join_scan_bound_sp_target_o() {
        let t = sample();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let got = idx.collect_join_scan(Some(0), Some(0), None, 2);
        let want = oracle_join_scan(&t, Some(0), Some(0), None, 2);
        assert_eq!(got, want);
        assert_eq!(got, vec![1, 2]);
    }

    #[test]
    fn join_scan_seek_and_remaining() {
        let t = sample();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let mut it = idx.join_scan(None, None, None, 0);
        assert_eq!(it.remaining_count(), 3);
        it.seek(1);
        assert_eq!(it.key(), 1);
        assert_eq!(it.remaining_count(), 2);
        it.seek(1); // no-op
        assert_eq!(it.key(), 1);
        it.advance();
        assert_eq!(it.key(), 2);
        it.advance();
        assert!(it.at_end());
        assert_eq!(it.remaining_count(), 0);
    }

    #[test]
    fn join_scan_empty_pattern() {
        let t = sample();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let it = idx.join_scan(Some(9), None, None, 0);
        assert!(it.at_end());

        assert_eq!(idx.real_count(Some(9), None, None, 0), 0);
    }

    #[test]
    fn join_scan_heap_mmap_parity() {
        let t = sample();
        let mut idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let path = std::env::temp_dir().join(format!(
            "braided_scan_parity_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        idx.materialize_mapped(&path).expect("mmap");
        let _ = std::fs::remove_file(&path);

        let cases: &[(Option<u64>, Option<u64>, Option<u64>, usize)] = &[
            (None, None, None, 0),
            (None, None, None, 1),
            (None, None, None, 2),
            (Some(0), None, None, 2),
            (Some(1), Some(0), None, 2),
            (None, Some(1), None, 0),
            (None, None, Some(1), 0),
            (Some(0), None, Some(1), 1),
        ];
        for &(s, p, o, tf) in cases {
            let heap_vals = {
                // Build heap-only twin for comparison of filter semantics
                // (both paths use enumerate_spo which prefers mmap when set).
                let h = BraidedRingIndex::from_shared_triples(&t, 3);
                h.collect_join_scan(s, p, o, tf)
            };
            let mapped_vals = idx.collect_join_scan(s, p, o, tf);
            let oracle = oracle_join_scan(&t, s, p, o, tf);
            assert_eq!(mapped_vals, oracle, "mapped vs oracle s={s:?} p={p:?} o={o:?} tf={tf}");
            assert_eq!(heap_vals, oracle, "heap vs oracle s={s:?} p={p:?} o={o:?} tf={tf}");
        }
    }

    #[test]
    fn multi_pattern_leapfrog_on_join_scans() {
        // Simulate LFTJ leapfrog on variable ?x where two patterns share ?x:
        //   (?x, 0, 1)  and  (?x, 1, 0)   → subjects that have both edges.
        let t = sample();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);

        // Pattern A: p=0, o=1, target=s
        let a = idx.collect_join_scan(None, Some(0), Some(1), 0);
        // Pattern B: p=1, o=0, target=s
        let b = idx.collect_join_scan(None, Some(1), Some(0), 0);
        let want_a = oracle_join_scan(&t, None, Some(0), Some(1), 0);
        let want_b = oracle_join_scan(&t, None, Some(1), Some(0), 0);
        assert_eq!(a, want_a);
        assert_eq!(b, want_b);

        // Manual leapfrog intersection of two sorted streams.
        let mut i = 0usize;
        let mut j = 0usize;
        let mut common = Vec::new();
        while i < a.len() && j < b.len() {
            match a[i].cmp(&b[j]) {
                std::cmp::Ordering::Equal => {
                    common.push(a[i]);
                    i += 1;
                    j += 1;
                }
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
            }
        }
        // From sample: s with (s,0,1): 0,1,2; s with (s,1,0): 2 → common {2}
        assert_eq!(common, vec![2]);
    }

    #[test]
    fn d2_three_subject_object_intersection_still_reaches_product_path() {
        // Product triangle (D2) is not join_scan itself, but multi-range
        // compatible scans must still reach D2 rather than only generic
        // leapfrog. Three subject lead-ranges on Col::O.
        let t = sample();
        let mut idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let path = std::env::temp_dir().join(format!(
            "braided_scan_d2_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        idx.materialize_mapped(&path).expect("mmap");
        let _ = std::fs::remove_file(&path);

        let r0 = idx.range_s(0);
        let r1 = idx.range_s(1);
        let r2 = idx.range_s(2);
        assert!(!r0.is_empty() && !r1.is_empty() && !r2.is_empty());

        // D2 product path
        let d2 = idx
            .collect_intersection3(Col::O, r0, r1, r2)
            .expect("mapped D2");

        // Generic leapfrog over three independent join_scans (objects under each s)
        let o0 = idx.collect_join_scan(Some(0), None, None, 2);
        let o1 = idx.collect_join_scan(Some(1), None, None, 2);
        let o2 = idx.collect_join_scan(Some(2), None, None, 2);
        let common = {
            let mut sets = [
                o0.into_iter().map(|v| v as u32).collect::<Vec<_>>(),
                o1.into_iter().map(|v| v as u32).collect::<Vec<_>>(),
                o2.into_iter().map(|v| v as u32).collect::<Vec<_>>(),
            ];
            for s in &mut sets {
                s.sort_unstable();
                s.dedup();
            }
            super::super::facade::oracle::sorted_common_symbols(&sets)
        };
        assert_eq!(d2, common, "D2 must match multi-scan object intersection");
        // dual_rnv oracle agreement on first successor
        let full_check = idx.intersection_next_value3(Col::O, r0, r1, r2, 0);
        let dual = idx.intersection_next_value3_dual_rnv(Col::O, r0, r1, r2, 0);
        assert_eq!(full_check, dual);
        if let Some(first) = d2.first().copied() {
            assert_eq!(full_check, Some(first));
        }
        let _ = RowRange::full(idx.n());
    }
}
