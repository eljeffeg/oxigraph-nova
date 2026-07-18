//! ID-level LFTJ join/scan seam for Braided Ring (Phase 4b / native fix).
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
//! ## Correctness strategy (native navigation)
//!
//! Prefer **lead-range + RDI / range-restricted walks** over the cyclic Ring A
//! last columns — never full-graph [`BraidedRingIndex::enumerate_spo`] on the
//! LFTJ hot path (that was O(N) per open and melted real SPARQL loads).
//!
//! Ring A tables / last columns:
//! - T_spo ordered (s,p,o), last = C_o, lead(S) partitions by subject
//! - T_osp ordered (o,s,p), last = C_p, lead(O) partitions by object
//! - T_pos ordered (p,o,s), last = C_s, lead(P) partitions by predicate
//!
//! When the target field is the **last column** of the table whose lead is a
//! bound attribute, use [`CyclicRing::range_distinct_iter`] (RDI). Middle
//! attributes are recovered via one LF step (`F` + `access`) over the lead
//! range only (O(|range| log σ), not O(N)).
//!
//! Multi-range D2 intersection is separate via
//! [`crate::cyclic_ring::facade::BraidedRingIndex::collect_intersection3`].

use super::facade::BraidedRingIndex;
use super::{Col, CyclicRing, RowRange};
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

// ── Native collection helpers (heap Ring A) ───────────────────────────────────

#[inline]
fn as_u32(id: Option<u64>) -> Option<u32> {
    id.map(|v| v as u32)
}

/// Collect distinct symbols via RDI over a row range on a last column.
fn rdi_distinct(ring: &CyclicRing, col: Col, r: RowRange) -> Vec<u64> {
    if r.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut it = ring.range_distinct_iter(col, r);
    while let Some((sym, _cnt)) = it.next() {
        out.push(u64::from(sym));
    }
    // RDI yields sorted ascending already.
    out
}

/// Distinct values of the middle field under a lead-range, via one LF step.
///
/// For T_spo lead(S)=R: middle p = C_p[F_o(i)] for i ∈ R.
/// For T_osp lead(O)=R: middle s = C_s[F_p(i)] for i ∈ R.
/// For T_pos lead(P)=R: middle o = C_o[F_s(i)] for i ∈ R.
fn middle_via_lf(
    ring: &CyclicRing,
    lead_last_col: Col,
    middle_last_col: Col,
    r: RowRange,
) -> Vec<u64> {
    if r.is_empty() {
        return Vec::new();
    }
    let mut vals: Vec<u64> = Vec::with_capacity(r.len() as usize);
    for i in r.start..r.end {
        let i_next = ring.f(lead_last_col, i);
        let mid = ring.access(middle_last_col, i_next);
        vals.push(u64::from(mid));
    }
    vals.sort_unstable();
    vals.dedup();
    vals
}

/// Distinct last-column values under lead range, filtered by a middle bind.
///
/// e.g. bound s + bound p → objects: for i in lead(S,s), keep o=C_o[i] where
/// C_p[F_o(i)] == p.
fn last_col_filtered_by_middle(
    ring: &CyclicRing,
    lead_last_col: Col,
    middle_last_col: Col,
    middle_bind: u32,
    r: RowRange,
) -> Vec<u64> {
    if r.is_empty() {
        return Vec::new();
    }
    let mut vals: Vec<u64> = Vec::new();
    for i in r.start..r.end {
        let i_next = ring.f(lead_last_col, i);
        let mid = ring.access(middle_last_col, i_next);
        if mid == middle_bind {
            let last = ring.access(lead_last_col, i);
            vals.push(u64::from(last));
        }
    }
    vals.sort_unstable();
    vals.dedup();
    vals
}

/// Native join_scan collection on heap Ring A.
///
/// Returns sorted distinct target-field values in dense shared-alphabet IDs.
fn collect_join_scan_native(
    ring: &CyclicRing,
    s: Option<u32>,
    p: Option<u32>,
    o: Option<u32>,
    target_field: usize,
) -> Vec<u64> {
    let n = ring.n();
    if n == 0 {
        return Vec::new();
    }
    let full = RowRange::full(n);

    // Reject out-of-universe binds early.
    let universe = ring.universe;
    let ok = |v: Option<u32>| v.is_none_or(|x| x < universe);
    if !ok(s) || !ok(p) || !ok(o) {
        return Vec::new();
    }

    match (s, p, o, target_field) {
        // ── Fully unbound: distinct values of one role via lead partitions ──
        // lead(S) non-empty ⇒ symbol appears as subject, etc.
        (None, None, None, 0) => {
            // Distinct subjects: symbols with non-empty lead_range(S, ·).
            // Equivalent to distinct first-column of T_spo = A_s partitions.
            // Cheapest: RDI over full T_pos last column C_s? C_s is last of T_pos
            // (p,o,s) so full range of C_s = all s multiset → distinct subjects.
            rdi_distinct(ring, Col::S, full)
        }
        (None, None, None, 1) => rdi_distinct(ring, Col::P, full),
        (None, None, None, 2) => rdi_distinct(ring, Col::O, full),

        // ── Single bind ─────────────────────────────────────────────────────
        // Bound S, target O: RDI on C_o over lead(S,s)
        (Some(sv), None, None, 2) => rdi_distinct(ring, Col::O, ring.range_s(sv)),
        // Bound S, target P: middle via LF
        (Some(sv), None, None, 1) => middle_via_lf(ring, Col::O, Col::P, ring.range_s(sv)),
        // Bound S, target S: either empty or {s}
        (Some(sv), None, None, 0) => {
            if ring.range_s(sv).is_empty() {
                Vec::new()
            } else {
                vec![u64::from(sv)]
            }
        }

        // Bound P, target S: RDI on C_s over lead(P,p)
        (None, Some(pv), None, 0) => rdi_distinct(ring, Col::S, ring.range_p(pv)),
        // Bound P, target O: middle via LF on T_pos (last C_s, middle o via F_s → C_o)
        (None, Some(pv), None, 2) => middle_via_lf(ring, Col::S, Col::O, ring.range_p(pv)),
        (None, Some(pv), None, 1) => {
            if ring.range_p(pv).is_empty() {
                Vec::new()
            } else {
                vec![u64::from(pv)]
            }
        }

        // Bound O, target P: RDI on C_p over lead(O,o)
        (None, None, Some(ov), 1) => rdi_distinct(ring, Col::P, ring.range_o(ov)),
        // Bound O, target S: middle via LF on T_osp (last C_p, middle s via F_p → C_s)
        (None, None, Some(ov), 0) => middle_via_lf(ring, Col::P, Col::S, ring.range_o(ov)),
        (None, None, Some(ov), 2) => {
            if ring.range_o(ov).is_empty() {
                Vec::new()
            } else {
                vec![u64::from(ov)]
            }
        }

        // ── Two binds ───────────────────────────────────────────────────────
        // Bound S+P, target O: last col C_o filtered by middle p under lead(S)
        (Some(sv), Some(pv), None, 2) => {
            last_col_filtered_by_middle(ring, Col::O, Col::P, pv, ring.range_s(sv))
        }
        // Bound S+O, target P: for i in lead(S,s) with C_o[i]==o, p = C_p[F_o(i)]
        (Some(sv), None, Some(ov), 1) => {
            let r = ring.range_s(sv);
            if r.is_empty() {
                return Vec::new();
            }
            let mut vals = Vec::new();
            for i in r.start..r.end {
                if ring.access(Col::O, i) == ov {
                    let i_osp = ring.f(Col::O, i);
                    vals.push(u64::from(ring.access(Col::P, i_osp)));
                }
            }
            vals.sort_unstable();
            vals.dedup();
            vals
        }
        // Bound P+O, target S: RDI on C_s over lead(P), filter by middle o
        (None, Some(pv), Some(ov), 0) => {
            last_col_filtered_by_middle(ring, Col::S, Col::O, ov, ring.range_p(pv))
        }

        // Bound S+P, target S/P — existence check
        (Some(sv), Some(pv), None, 0) => {
            if last_col_filtered_by_middle(ring, Col::O, Col::P, pv, ring.range_s(sv)).is_empty() {
                Vec::new()
            } else {
                vec![u64::from(sv)]
            }
        }
        (Some(sv), Some(pv), None, 1) => {
            if last_col_filtered_by_middle(ring, Col::O, Col::P, pv, ring.range_s(sv)).is_empty() {
                Vec::new()
            } else {
                vec![u64::from(pv)]
            }
        }

        // Bound S+O target S/O
        (Some(sv), None, Some(ov), 0) => {
            let r = ring.range_s(sv);
            let mut hit = false;
            for i in r.start..r.end {
                if ring.access(Col::O, i) == ov {
                    hit = true;
                    break;
                }
            }
            if hit {
                vec![u64::from(sv)]
            } else {
                Vec::new()
            }
        }
        (Some(sv), None, Some(ov), 2) => {
            let r = ring.range_s(sv);
            let mut hit = false;
            for i in r.start..r.end {
                if ring.access(Col::O, i) == ov {
                    hit = true;
                    break;
                }
            }
            if hit {
                vec![u64::from(ov)]
            } else {
                Vec::new()
            }
        }

        // Bound P+O target P/O
        (None, Some(pv), Some(ov), 1) => {
            if last_col_filtered_by_middle(ring, Col::S, Col::O, ov, ring.range_p(pv)).is_empty() {
                Vec::new()
            } else {
                vec![u64::from(pv)]
            }
        }
        (None, Some(pv), Some(ov), 2) => {
            if last_col_filtered_by_middle(ring, Col::S, Col::O, ov, ring.range_p(pv)).is_empty() {
                Vec::new()
            } else {
                vec![u64::from(ov)]
            }
        }

        // ── Three binds: existence only ─────────────────────────────────────
        (Some(sv), Some(pv), Some(ov), tf) => {
            let r = ring.range_s(sv);
            let mut hit = false;
            for i in r.start..r.end {
                if ring.access(Col::O, i) != ov {
                    continue;
                }
                let i_osp = ring.f(Col::O, i);
                if ring.access(Col::P, i_osp) == pv {
                    hit = true;
                    break;
                }
            }
            if !hit {
                return Vec::new();
            }
            match tf {
                0 => vec![u64::from(sv)],
                1 => vec![u64::from(pv)],
                _ => vec![u64::from(ov)],
            }
        }

        // Unreachable if target_field always 0/1/2
        _ => Vec::new(),
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
    /// Uses native lead-range + RDI / LF walks — **not** full-graph enumerate.
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
        let vals = collect_join_scan_native(
            self.heap(),
            as_u32(s),
            as_u32(p),
            as_u32(o),
            target_field,
        );
        if vals.is_empty() {
            return Box::new(EmptyTrieIter);
        }
        Box::new(BraidedJoinScan::new(vals))
    }

    /// Collect all distinct target-field values from [`Self::join_scan`].
    pub fn collect_join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Vec<u64> {
        let target_field = target_field.min(2);
        collect_join_scan_native(
            self.heap(),
            as_u32(s),
            as_u32(p),
            as_u32(o),
            target_field,
        )
    }

    /// Exact distinct count for the same filters as [`Self::join_scan`].
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
                let h = BraidedRingIndex::from_shared_triples(&t, 3);
                h.collect_join_scan(s, p, o, tf)
            };
            // Mapped image present: join_scan still uses heap native path
            // (heap is source of truth for navigation; mmap for D2).
            let mapped_vals = idx.collect_join_scan(s, p, o, tf);
            let oracle = oracle_join_scan(&t, s, p, o, tf);
            assert_eq!(
                mapped_vals, oracle,
                "mapped vs oracle s={s:?} p={p:?} o={o:?} tf={tf}"
            );
            assert_eq!(
                heap_vals, oracle,
                "heap vs oracle s={s:?} p={p:?} o={o:?} tf={tf}"
            );
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
