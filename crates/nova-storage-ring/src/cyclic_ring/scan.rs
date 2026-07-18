//! ID-level LFTJ join/scan seam for Braided Ring (Phase 4b / streaming).
//!
//! Provides a [`TrieIterator`]-compatible `join_scan` over shared-alphabet
//! `u32` triples **without** dictionary, delta, or `QuadStore`.
//!
//! ## Contract (matches production LFTJ usage)
//!
//! `nova-query`'s LFTJ evaluator calls `lftj_join_scan` once per pattern at
//! each depth and then only uses `key` / `seek` / `advance` / `at_end` —
//! never `open()`. Depth descent is a fresh scan with more binds.
//!
//! ## Why this must stream (not materialize)
//!
//! LOUDS `join_scan` returns a lazy LOUDS trie cursor. The previous Ring path
//! collected **all** distinct target IDs into a `Vec` on every open, and
//! `lftj_real_count` re-did the same work for every VEO probe. On 2-join /
//! high-cardinality patterns that is still a hang — just a later one.
//!
//! This module streams with the primitives that already beat LOUDS in
//! microbenches:
//! - [`CyclicRing::range_next_value`] (RNV) — O(log σ) successor, independent of |range|
//! - lead-range restriction + binary search on sorted middle runs (T_spo / T_osp / T_pos)
//! - optional RDI only for small eager collect helpers / tests
//!
//! Ring A tables / last columns:
//! - T_spo ordered (s,p,o), last = C_o, lead(S) partitions by subject
//! - T_osp ordered (o,s,p), last = C_p, lead(O) partitions by object
//! - T_pos ordered (p,o,s), last = C_s, lead(P) partitions by predicate

use super::facade::BraidedRingIndex;
use super::image::BraidedGraphImage;
use super::{Col, CyclicRing, RowRange};
use oxigraph_nova_core::{EmptyTrieIter, TrieIterator};
use std::sync::Arc;

// ── Dense navigation helpers ──────────────────────────────────────────────────

#[inline]
fn as_u32(id: Option<u64>) -> Option<u32> {
    id.map(|v| v as u32)
}

/// Middle p under T_spo row i: C_p[F_o(i)].
#[inline]
fn middle_p_spo(ring: &CyclicRing, i: u32) -> u32 {
    ring.access(Col::P, ring.f(Col::O, i))
}

/// Middle s under T_osp row i: C_s[F_p(i)].
#[inline]
fn middle_s_osp(ring: &CyclicRing, i: u32) -> u32 {
    ring.access(Col::S, ring.f(Col::P, i))
}

/// Middle o under T_pos row i: C_o[F_s(i)].
#[inline]
fn middle_o_pos(ring: &CyclicRing, i: u32) -> u32 {
    ring.access(Col::O, ring.f(Col::S, i))
}

/// First index in `[lo, hi)` where `mid(i) >= target`, or `hi` if none.
fn lower_bound_middle(
    ring: &CyclicRing,
    lo: u32,
    hi: u32,
    target: u32,
    mid: fn(&CyclicRing, u32) -> u32,
) -> u32 {
    let mut lo = lo;
    let mut hi = hi;
    while lo < hi {
        let m = lo + (hi - lo) / 2;
        if mid(ring, m) < target {
            lo = m + 1;
        } else {
            hi = m;
        }
    }
    lo
}

/// Contiguous (s,p) row range on T_spo (sorted by s,p,o).
fn range_sp(ring: &CyclicRing, s: u32, p: u32) -> RowRange {
    let r = ring.range_s(s);
    if r.is_empty() {
        return r;
    }
    let start = lower_bound_middle(ring, r.start, r.end, p, middle_p_spo);
    let end = if p == u32::MAX {
        r.end
    } else {
        lower_bound_middle(ring, start, r.end, p.saturating_add(1), middle_p_spo)
    };
    RowRange { start, end }
}

/// Contiguous (o,s) row range on T_osp (sorted by o,s,p).
fn range_os(ring: &CyclicRing, o: u32, s: u32) -> RowRange {
    let r = ring.range_o(o);
    if r.is_empty() {
        return r;
    }
    let start = lower_bound_middle(ring, r.start, r.end, s, middle_s_osp);
    let end = if s == u32::MAX {
        r.end
    } else {
        lower_bound_middle(ring, start, r.end, s.saturating_add(1), middle_s_osp)
    };
    RowRange { start, end }
}

/// Contiguous (p,o) row range on T_pos (sorted by p,o,s).
fn range_po(ring: &CyclicRing, p: u32, o: u32) -> RowRange {
    let r = ring.range_p(p);
    if r.is_empty() {
        return r;
    }
    let start = lower_bound_middle(ring, r.start, r.end, o, middle_o_pos);
    let end = if o == u32::MAX {
        r.end
    } else {
        lower_bound_middle(ring, start, r.end, o.saturating_add(1), middle_o_pos)
    };
    RowRange { start, end }
}

// ── Streaming scan kinds (dense alphabet) ─────────────────────────────────────

/// How to stream distinct dense target IDs under a bound pattern.
#[derive(Clone, Copy, Debug)]
enum DenseScanKind {
    /// Empty result.
    Empty,
    /// At most one value (existence / self-target).
    Singleton(u32),
    /// RNV over a last-column wavelet range.
    LastCol { col: Col, range: RowRange },
    /// Distinct middle under a lead range (run-aware binary search).
    MiddleRuns {
        range: RowRange,
        mid: MiddleKind,
    },
}

#[derive(Clone, Copy, Debug)]
enum MiddleKind {
    /// T_spo lead(S): middle = p
    PUnderS,
    /// T_osp lead(O): middle = s
    SUnderO,
    /// T_pos lead(P): middle = o
    OUnderP,
}

impl MiddleKind {
    #[inline]
    fn eval(self, ring: &CyclicRing, i: u32) -> u32 {
        match self {
            MiddleKind::PUnderS => middle_p_spo(ring, i),
            MiddleKind::SUnderO => middle_s_osp(ring, i),
            MiddleKind::OUnderP => middle_o_pos(ring, i),
        }
    }
}

/// Build the dense scan kind for (s,p,o,target_field) on Ring A.
fn dense_scan_kind(
    ring: &CyclicRing,
    s: Option<u32>,
    p: Option<u32>,
    o: Option<u32>,
    target_field: usize,
) -> DenseScanKind {
    let n = ring.n();
    if n == 0 {
        return DenseScanKind::Empty;
    }
    let full = RowRange::full(n);
    let universe = ring.universe;
    let ok = |v: Option<u32>| v.is_none_or(|x| x < universe);
    if !ok(s) || !ok(p) || !ok(o) {
        return DenseScanKind::Empty;
    }
    let tf = target_field.min(2);

    match (s, p, o, tf) {
        // ── Fully unbound: distinct of one role via full last-column RNV ──
        (None, None, None, 0) => DenseScanKind::LastCol {
            col: Col::S,
            range: full,
        },
        (None, None, None, 1) => DenseScanKind::LastCol {
            col: Col::P,
            range: full,
        },
        (None, None, None, 2) => DenseScanKind::LastCol {
            col: Col::O,
            range: full,
        },

        // ── Single bind ───────────────────────────────────────────────────
        (Some(sv), None, None, 2) => {
            let r = ring.range_s(sv);
            if r.is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::LastCol {
                    col: Col::O,
                    range: r,
                }
            }
        }
        (Some(sv), None, None, 1) => {
            let r = ring.range_s(sv);
            if r.is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::MiddleRuns {
                    range: r,
                    mid: MiddleKind::PUnderS,
                }
            }
        }
        (Some(sv), None, None, 0) => {
            if ring.range_s(sv).is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::Singleton(sv)
            }
        }

        (None, Some(pv), None, 0) => {
            let r = ring.range_p(pv);
            if r.is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::LastCol {
                    col: Col::S,
                    range: r,
                }
            }
        }
        (None, Some(pv), None, 2) => {
            let r = ring.range_p(pv);
            if r.is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::MiddleRuns {
                    range: r,
                    mid: MiddleKind::OUnderP,
                }
            }
        }
        (None, Some(pv), None, 1) => {
            if ring.range_p(pv).is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::Singleton(pv)
            }
        }

        (None, None, Some(ov), 1) => {
            let r = ring.range_o(ov);
            if r.is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::LastCol {
                    col: Col::P,
                    range: r,
                }
            }
        }
        (None, None, Some(ov), 0) => {
            let r = ring.range_o(ov);
            if r.is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::MiddleRuns {
                    range: r,
                    mid: MiddleKind::SUnderO,
                }
            }
        }
        (None, None, Some(ov), 2) => {
            if ring.range_o(ov).is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::Singleton(ov)
            }
        }

        // ── Two binds ─────────────────────────────────────────────────────
        // S+P → O: last col over contiguous SP range on T_spo
        (Some(sv), Some(pv), None, 2) => {
            let r = range_sp(ring, sv, pv);
            if r.is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::LastCol {
                    col: Col::O,
                    range: r,
                }
            }
        }
        (Some(sv), Some(pv), None, 0) => {
            if range_sp(ring, sv, pv).is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::Singleton(sv)
            }
        }
        (Some(sv), Some(pv), None, 1) => {
            if range_sp(ring, sv, pv).is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::Singleton(pv)
            }
        }

        // S+O → P: use T_osp (o,s,p) contiguous OS range, last = C_p
        (Some(sv), None, Some(ov), 1) => {
            let r = range_os(ring, ov, sv);
            if r.is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::LastCol {
                    col: Col::P,
                    range: r,
                }
            }
        }
        (Some(sv), None, Some(ov), 0) => {
            if range_os(ring, ov, sv).is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::Singleton(sv)
            }
        }
        (Some(sv), None, Some(ov), 2) => {
            if range_os(ring, ov, sv).is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::Singleton(ov)
            }
        }

        // P+O → S: T_pos (p,o,s) contiguous PO range, last = C_s
        (None, Some(pv), Some(ov), 0) => {
            let r = range_po(ring, pv, ov);
            if r.is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::LastCol {
                    col: Col::S,
                    range: r,
                }
            }
        }
        (None, Some(pv), Some(ov), 1) => {
            if range_po(ring, pv, ov).is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::Singleton(pv)
            }
        }
        (None, Some(pv), Some(ov), 2) => {
            if range_po(ring, pv, ov).is_empty() {
                DenseScanKind::Empty
            } else {
                DenseScanKind::Singleton(ov)
            }
        }

        // ── Three binds: existence ────────────────────────────────────────
        (Some(sv), Some(pv), Some(ov), tf) => {
            // Prefer SP range then check o via RNV / rank, O(log σ).
            let r = range_sp(ring, sv, pv);
            if r.is_empty() {
                return DenseScanKind::Empty;
            }
            let hit = ring
                .range_next_value(Col::O, r, ov)
                .is_some_and(|v| v == ov);
            if !hit {
                DenseScanKind::Empty
            } else {
                match tf {
                    0 => DenseScanKind::Singleton(sv),
                    1 => DenseScanKind::Singleton(pv),
                    _ => DenseScanKind::Singleton(ov),
                }
            }
        }

        _ => DenseScanKind::Empty,
    }
}

/// First dense value ≥ `target` for this scan kind (None ⇒ exhausted).
fn dense_seek(ring: &CyclicRing, kind: DenseScanKind, target: u32) -> Option<u32> {
    match kind {
        DenseScanKind::Empty => None,
        DenseScanKind::Singleton(v) => {
            if v >= target {
                Some(v)
            } else {
                None
            }
        }
        DenseScanKind::LastCol { col, range } => {
            if range.is_empty() {
                None
            } else {
                ring.range_next_value(col, range, target)
            }
        }
        DenseScanKind::MiddleRuns { range, mid } => {
            if range.is_empty() {
                return None;
            }
            let mid_fn: fn(&CyclicRing, u32) -> u32 = match mid {
                MiddleKind::PUnderS => middle_p_spo,
                MiddleKind::SUnderO => middle_s_osp,
                MiddleKind::OUnderP => middle_o_pos,
            };
            let i = lower_bound_middle(ring, range.start, range.end, target, mid_fn);
            if i >= range.end {
                None
            } else {
                Some(mid_fn(ring, i))
            }
        }

    }
}

/// Successor after `current` (exclusive).
fn dense_advance(ring: &CyclicRing, kind: DenseScanKind, current: u32) -> Option<u32> {
    if current == u32::MAX {
        return None;
    }
    dense_seek(ring, kind, current.saturating_add(1))
}

/// Cheap distinct-count estimate (LOUDS-style heuristic, not exact).
///
/// Used by VEO when we deliberately avoid full materialization in
/// `lftj_real_count`. Prefer over exact RDI full walks on large graphs.
pub fn estimate_join_count(
    ring: &CyclicRing,
    s: Option<u32>,
    p: Option<u32>,
    o: Option<u32>,
    target_field: usize,
) -> u64 {
    let n_bound = (s.is_some() as u64) + (p.is_some() as u64) + (o.is_some() as u64);
    let kind = dense_scan_kind(ring, s, p, o, target_field);
    match kind {
        DenseScanKind::Empty => 0,
        DenseScanKind::Singleton(_) => 1,
        DenseScanKind::LastCol { range, .. } => {
            // Upper bound: every row a distinct symbol; scale by binds.
            let rows = u64::from(range.len().max(1));
            // Prefer smaller of row-span and vocab heuristic.
            let vocab = u64::from(ring.universe.max(1)) / (n_bound + 1);
            rows.min(vocab).max(1)
        }
        DenseScanKind::MiddleRuns { range, .. } => {
            let rows = u64::from(range.len().max(1));
            let vocab = u64::from(ring.universe.max(1)) / (n_bound + 1);
            rows.min(vocab).max(1)
        }
    }
}

// ── Streaming TrieIterator (dense) ────────────────────────────────────────────

/// Lazy distinct-value scan over heap Ring A (dense shared alphabet).
///
/// Holds a shared [`BraidedGraphImage`] so the wavelet lives for the scan
/// lifetime without keeping the store mutex.
pub struct BraidedStreamingScan {
    img: Arc<BraidedGraphImage>,
    kind: DenseScanKind,
    /// Current dense symbol; `None` ⇒ at_end.
    current: Option<u32>,
}

impl BraidedStreamingScan {
    fn new(img: Arc<BraidedGraphImage>, kind: DenseScanKind) -> Self {
        let current = {
            let ring = img.index().heap();
            dense_seek(ring, kind, 0)
        };
        Self {
            img,
            kind,
            current,
        }
    }

    #[inline]
    fn ring(&self) -> &CyclicRing {
        self.img.index().heap()
    }

    /// Map dense → external for the iterator contract.
    #[inline]
    fn external_key(&self, dense: u32) -> u64 {
        self.img
            .remap()
            .to_external(dense)
            .unwrap_or(u64::from(dense))
    }
}

impl TrieIterator for BraidedStreamingScan {
    #[inline]
    fn key(&self) -> u64 {
        let d = self.current.expect("key() on exhausted BraidedStreamingScan");
        self.external_key(d)
    }

    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        // No-op if already past (compare in external order).
        if let Some(cur) = self.current {
            let cur_ext = self.external_key(cur);
            if cur_ext >= target {
                return;
            }
        }
        // External → dense: exact hit, or ceil to next denser external.
        // Dense ids are assigned in ascending external order, so dense order
        // matches external order for leapfrog.
        let dense_target = match self.img.remap().to_dense(target) {
            Some(d) => d,
            None => match self.img.remap().dense_ceil(target) {
                Some(d) => d,
                None => {
                    self.current = None;
                    return;
                }
            },
        };
        self.current = dense_seek(self.ring(), self.kind, dense_target);
    }


    fn advance(&mut self) {
        if let Some(cur) = self.current {
            self.current = dense_advance(self.ring(), self.kind, cur);
        }
    }

    fn open(&self) -> Box<dyn TrieIterator> {
        Box::new(EmptyTrieIter)
    }

    #[inline]
    fn at_end(&self) -> bool {
        self.current.is_none()
    }

    fn remaining_count(&self) -> u64 {
        // Exact remaining would re-walk; LOUDS remaining is also approximate.
        // Report 1 if live, 0 if done — leapfrog only needs a relative signal.
        u64::from(!self.at_end())
    }
}

// ── Eager Vec scan (tests / collect helpers only) ─────────────────────────────

/// Flat materialised scan — **not** used on the LFTJ hot path.
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

    #[inline]
    fn at_end(&self) -> bool {
        self.pos >= self.vals.len()
    }

    fn remaining_count(&self) -> u64 {
        self.vals.len().saturating_sub(self.pos) as u64
    }
}

/// Collect all distinct dense values by streaming RNV/runs (tests / oracles).
fn collect_dense(ring: &CyclicRing, kind: DenseScanKind) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cur = dense_seek(ring, kind, 0);
    while let Some(v) = cur {
        out.push(u64::from(v));
        cur = dense_advance(ring, kind, v);
    }
    out
}

// ── Public API on BraidedRingIndex (dense IDs) ────────────────────────────────

impl BraidedRingIndex {
    /// ID-level streaming `join_scan` (dense shared-alphabet IDs).
    ///
    /// Prefer [`BraidedGraphImage::join_scan_streaming`] for store-level LFTJ
    /// (external TermIds + Arc lifetime). This entry point materialises a
    /// temporary Arc-less image view only for the dense API used by unit tests.
    pub fn join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator> {
        let vals = self.collect_join_scan(s, p, o, target_field);
        if vals.is_empty() {
            Box::new(EmptyTrieIter)
        } else {
            Box::new(BraidedJoinScan::new(vals))
        }
    }

    /// Collect all distinct target-field values (eager; tests / diagnostics).
    pub fn collect_join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Vec<u64> {
        let target_field = target_field.min(2);
        let kind = dense_scan_kind(
            self.heap(),
            as_u32(s),
            as_u32(p),
            as_u32(o),
            target_field,
        );
        collect_dense(self.heap(), kind)
    }

    /// Exact distinct count via streaming walk (still O(|distinct|) RNV steps).
    pub fn real_count(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> u64 {
        self.collect_join_scan(s, p, o, target_field).len() as u64
    }

    /// Cheap VEO estimate (no full distinct walk).
    pub fn estimate_count(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> u64 {
        estimate_join_count(
            self.heap(),
            as_u32(s),
            as_u32(p),
            as_u32(o),
            target_field,
        )
    }
}

// ── External-ID streaming on BraidedGraphImage ────────────────────────────────

impl BraidedGraphImage {
    /// Streaming LFTJ scan in **external** TermId coordinates.
    ///
    /// Bound fields external→dense; yielded keys dense→external. Unmappable
    /// binds yield an empty scan. Holds `img` so navigation is off the store lock.
    pub fn join_scan_streaming(
        img: Arc<Self>,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator> {
        let map_bound = |ext: Option<u64>| -> Option<Option<u32>> {
            match ext {
                None => Some(None),
                Some(e) => img.remap().to_dense(e).map(Some),
            }
        };
        let (Some(sd), Some(pd), Some(od)) = (map_bound(s), map_bound(p), map_bound(o)) else {
            return Box::new(EmptyTrieIter);
        };
        let kind = dense_scan_kind(img.index().heap(), sd, pd, od, target_field.min(2));
        if matches!(kind, DenseScanKind::Empty) {
            return Box::new(EmptyTrieIter);
        }
        Box::new(BraidedStreamingScan::new(img, kind))
    }

    /// Eager external collect (tests / pattern helpers).
    pub fn join_scan_external(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Vec<u64> {
        let map_bound = |ext: Option<u64>| -> Option<Option<u64>> {
            match ext {
                None => Some(None),
                Some(e) => self.remap().to_dense(e).map(|d| Some(u64::from(d))),
            }
        };
        let (Some(sd), Some(pd), Some(od)) = (map_bound(s), map_bound(p), map_bound(o)) else {
            return Vec::new();
        };
        self.index()
            .collect_join_scan(sd, pd, od, target_field)
            .into_iter()
            .filter_map(|d| self.remap().to_external(d as u32))
            .collect()
    }

    /// Cheap estimate in external coordinates.
    pub fn estimate_count_external(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> u64 {
        let map_bound = |ext: Option<u64>| -> Option<Option<u32>> {
            match ext {
                None => Some(None),
                Some(e) => self.remap().to_dense(e).map(Some),
            }
        };
        let (Some(sd), Some(pd), Some(od)) = (map_bound(s), map_bound(p), map_bound(o)) else {
            return 0;
        };
        estimate_join_count(self.index().heap(), sd, pd, od, target_field)
    }
}

// ── Oracle helpers ────────────────────────────────────────────────────────────

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
    fn join_scan_bound_s_target_p_middle_runs() {
        let t = sample();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let got = idx.collect_join_scan(Some(0), None, None, 1);
        let want = oracle_join_scan(&t, Some(0), None, None, 1);
        assert_eq!(got, want);
    }

    #[test]
    fn join_scan_seek_and_remaining() {
        let t = sample();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let mut it = idx.join_scan(None, None, None, 0);
        assert!(!it.at_end());
        it.seek(1);
        assert_eq!(it.key(), 1);
        it.seek(1); // no-op
        assert_eq!(it.key(), 1);
        it.advance();
        assert_eq!(it.key(), 2);
        it.advance();
        assert!(it.at_end());
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
    fn streaming_external_matches_oracle() {
        let t = sample();
        // Sparse external IDs via image remap.
        let ext: Vec<[u64; 3]> = t
            .iter()
            .map(|x| [u64::from(x[0]) + 100, u64::from(x[1]) + 10, u64::from(x[2]) + 200])
            .collect();
        let img = Arc::new(BraidedGraphImage::from_external_triples(&ext));
        let mut it = BraidedGraphImage::join_scan_streaming(Arc::clone(&img), None, None, None, 0);
        let mut got = Vec::new();
        while !it.at_end() {
            got.push(it.key());
            it.advance();
        }
        let want = img.join_scan_external(None, None, None, 0);
        assert_eq!(got, want);
        assert!(!got.is_empty());
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
        let t = sample();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);

        let a = idx.collect_join_scan(None, Some(0), Some(1), 0);
        let b = idx.collect_join_scan(None, Some(1), Some(0), 0);
        let want_a = oracle_join_scan(&t, None, Some(0), Some(1), 0);
        let want_b = oracle_join_scan(&t, None, Some(1), Some(0), 0);
        assert_eq!(a, want_a);
        assert_eq!(b, want_b);

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
        assert_eq!(common, vec![2]);
    }

    #[test]
    fn d2_three_subject_object_intersection_still_reaches_product_path() {
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

        let d2 = idx
            .collect_intersection3(Col::O, r0, r1, r2)
            .expect("mapped D2");

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
        let full_check = idx.intersection_next_value3(Col::O, r0, r1, r2, 0);
        let dual = idx.intersection_next_value3_dual_rnv(Col::O, r0, r1, r2, 0);
        assert_eq!(full_check, dual);
        if let Some(first) = d2.first().copied() {
            assert_eq!(full_check, Some(first));
        }
        let _ = RowRange::full(idx.n());
    }

    #[test]
    fn range_sp_restricts_to_matching_p() {
        let t = sample();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let ring = idx.heap();
        let r = range_sp(ring, 0, 0);
        // s=0,p=0 → objects 1,2
        let mut objs = Vec::new();
        let mut cur = ring.range_next_value(Col::O, r, 0);
        while let Some(v) = cur {
            objs.push(v);
            cur = ring.range_next_value(Col::O, r, v.saturating_add(1));
        }
        assert_eq!(objs, vec![1, 2]);
    }
}
