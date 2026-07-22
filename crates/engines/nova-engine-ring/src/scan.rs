//! ID-level LFTJ join/scan seam for Braided Ring.
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
//! LOUDS `join_scan` returns a lazy LOUDS trie cursor. Materializing **all**
//! distinct target IDs into a `Vec` on every open (and re-doing that work in
//! `lftj_real_count` for every VEO probe) hangs on 2-join / high-cardinality
//! patterns.
//!
//! This module streams with the primitives that already beat LOUDS in
//! microbenches:
//! - **mapped RDI** for LastCol when `NOVARNG1` is open
//! - heap [`CyclicRing::range_next_value`] (RNV) fallback — O(log σ) successor
//! - lead-range restriction + binary search on sorted middle runs (T_spo / T_osp / T_pos)
//!
//! Ring A tables / last columns:
//! - T_spo ordered (s,p,o), last = C_o, lead(S) partitions by subject
//! - T_osp ordered (o,s,p), last = C_p, lead(O) partitions by object
//! - T_pos ordered (p,o,s), last = C_s, lead(P) partitions by predicate

use crate::facade::BraidedRingIndex;
use crate::image::BraidedGraphImage;
use crate::mapped_qwt::{HotQwtColumn, MappedRangeDistinctIter};
use crate::product_path::{
    PredAdjacencyMode, SPARQL_PATH, effective_d1_tiny_merge_threshold,
    effective_pred_adjacency_mode, effective_wedge_left_once_threshold, ring_counters_log_enabled,
};
use crate::ring_nav::RingRef;
use crate::{Col, RowRange};
use oxigraph_nova_core::{
    EmptyTrieIter, PreparedLeftIntersect, PreparedPhysicalOperator, PreparedPredObjectIntersect,
    PreparedSpObjectScan, PreparedTwoHop, PreparedWedge, TrieIterator,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

// ── Dense navigation helpers ──────────────────────────────────────────────────

#[inline]
fn as_u32(id: Option<u64>) -> Option<u32> {
    id.map(|v| v as u32)
}

/// Middle p under T_spo row i: C_p[F_o(i)].
#[inline]
fn middle_p_spo(ring: RingRef<'_>, i: u32) -> u32 {
    ring.access(Col::P, ring.f(Col::O, i))
}

/// Middle s under T_osp row i: C_s[F_p(i)].
#[inline]
fn middle_s_osp(ring: RingRef<'_>, i: u32) -> u32 {
    ring.access(Col::S, ring.f(Col::P, i))
}

/// Middle o under T_pos row i: C_o[F_s(i)].
#[inline]
fn middle_o_pos(ring: RingRef<'_>, i: u32) -> u32 {
    ring.access(Col::O, ring.f(Col::S, i))
}

/// First index in `[lo, hi)` where `mid(i) >= target`, or `hi` if none.
///
/// Large lead ranges (e.g. P2 ≈ N·FAN_OUT on BSBM) make plain binary search
/// pay ~log₂(|range|) LF middle probes even when the hit sits near `lo`.
/// `feature_lookup` binds feature0, typically an early object under P2 —
/// galloping finds that frontier in O(log offset) probes, then binary search
/// finishes inside the narrowed window. Mid-range hits still cost ~2·log₂ n.
fn lower_bound_middle(
    ring: RingRef<'_>,
    mut lo: u32,
    hi: u32,
    target: u32,
    mid: fn(RingRef<'_>, u32) -> u32,
) -> u32 {
    if lo >= hi {
        return lo;
    }
    // Already at/past target at lo → lo is the bound.
    if mid(ring, lo) >= target {
        return lo;
    }
    // Gallop: find smallest `bound` with mid(bound) >= target (or hi).
    let mut step = 1u32;
    let mut bound = lo.saturating_add(1);
    while bound < hi && mid(ring, bound) < target {
        lo = bound.saturating_add(1);
        step = step.saturating_mul(2);
        bound = match lo.checked_add(step) {
            Some(v) => v.min(hi),
            None => hi,
        };
    }
    // Binary search in [lo, bound).
    let mut hi = bound.min(hi);
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
fn range_sp(ring: RingRef<'_>, s: u32, p: u32) -> RowRange {
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
fn range_os(ring: RingRef<'_>, o: u32, s: u32) -> RowRange {
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
fn range_po(ring: RingRef<'_>, p: u32, o: u32) -> RowRange {
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

// ── Pattern match (indexed; replaces full-graph enumerate) ────────────────────
//
// LOUDS `match_triples` seeks a bound prefix on one of six tries then walks
// only matching leaves. Ring must do the same with lead ranges:
//   - bound P (BSBM scan `?s P31 ?o`) → walk `range_p(p)` only (~50k, not n)
//   - bound S / O / SP / SO / PO → corresponding lead / middle range
//   - unbound → full T_spo walk (unavoidable for SELECT *)
//
// Triple recovery (same LF cycle as `CyclicRing::enumerate_spo`):
//   T_spo row i: o=C_o[i], p=C_p[F_o(i)], s=C_s[F_p(F_o(i))]
//   T_osp row k: p=C_p[k], s=C_s[F_p(k)], o via F_s → C_o
//   T_pos row j: s=C_s[j], o=C_o[F_s(j)], p via F_o → C_p  (or known lead)

/// Recover (s,p,o) from a T_spo row index.
#[inline]
fn triple_at_spo(ring: RingRef<'_>, i: u32) -> [u32; 3] {
    let o = ring.access(Col::O, i);
    let i_osp = ring.f(Col::O, i);
    let p = ring.access(Col::P, i_osp);
    let i_pos = ring.f(Col::P, i_osp);
    let s = ring.access(Col::S, i_pos);
    [s, p, o]
}

/// Recover (s,p,o) from a T_osp row index (lead = O).
#[inline]
fn triple_at_osp(ring: RingRef<'_>, k: u32) -> [u32; 3] {
    let p = ring.access(Col::P, k);
    let i_pos = ring.f(Col::P, k);
    let s = ring.access(Col::S, i_pos);
    let i_spo = ring.f(Col::S, i_pos);
    let o = ring.access(Col::O, i_spo);
    [s, p, o]
}

/// Recover (s,p,o) from a T_pos row index (lead = P).
#[inline]
fn triple_at_pos(ring: RingRef<'_>, j: u32) -> [u32; 3] {
    let s = ring.access(Col::S, j);
    let i_spo = ring.f(Col::S, j);
    let o = ring.access(Col::O, i_spo);
    let i_osp = ring.f(Col::O, i_spo);
    let p = ring.access(Col::P, i_osp);
    [s, p, o]
}

/// Push every triple in a contiguous row range, recovering via `at`.
#[inline]
fn push_range(
    ring: RingRef<'_>,
    r: RowRange,
    at: fn(RingRef<'_>, u32) -> [u32; 3],
    out: &mut Vec<[u32; 3]>,
) {
    if r.is_empty() {
        return;
    }
    out.reserve(r.len() as usize);
    for i in r.start..r.end {
        out.push(at(ring, i));
    }
}

/// Indexed dense pattern match — O(|matches|) row walks, not O(n) full enum.
pub fn match_triples_dense(
    ring: RingRef<'_>,
    s: Option<u32>,
    p: Option<u32>,
    o: Option<u32>,
) -> Vec<[u32; 3]> {
    let n = ring.n();
    if n == 0 {
        return Vec::new();
    }
    let universe = ring.universe();
    let ok = |v: Option<u32>| v.is_none_or(|x| x < universe);
    if !ok(s) || !ok(p) || !ok(o) {
        return Vec::new();
    }

    let mut out = Vec::new();
    match (s, p, o) {
        // Exact triple.
        (Some(sv), Some(pv), Some(ov)) => {
            let r = range_sp(ring, sv, pv);
            if !r.is_empty()
                && ring
                    .range_next_value(Col::O, r, ov)
                    .is_some_and(|v| v == ov)
            {
                out.push([sv, pv, ov]);
            }
        }

        // Bound S (+ optional filters via native ranges).
        (Some(sv), None, None) => {
            push_range(ring, ring.range_s(sv), triple_at_spo, &mut out);
        }
        (Some(sv), Some(pv), None) => {
            push_range(ring, range_sp(ring, sv, pv), triple_at_spo, &mut out);
        }
        (Some(sv), None, Some(ov)) => {
            // T_osp (o,s,p): contiguous OS range, recover via OSP.
            push_range(ring, range_os(ring, ov, sv), triple_at_osp, &mut out);
        }

        // Bound P only (BSBM scan / star predicate) — lead range on T_pos.
        (None, Some(pv), None) => {
            push_range(ring, ring.range_p(pv), triple_at_pos, &mut out);
        }
        (None, Some(pv), Some(ov)) => {
            push_range(ring, range_po(ring, pv, ov), triple_at_pos, &mut out);
        }

        // Bound O only.
        (None, None, Some(ov)) => {
            push_range(ring, ring.range_o(ov), triple_at_osp, &mut out);
        }

        // Fully unbound — full T_spo walk (same cost as enumerate, unavoidable).
        (None, None, None) => {
            push_range(ring, RowRange::full(n), triple_at_spo, &mut out);
        }
    }
    out
}

/// Exact (s,p,o) membership via SP range + O RNV — O(log σ), not O(n).
pub fn contains_dense(ring: RingRef<'_>, s: u32, p: u32, o: u32) -> bool {
    if s >= ring.universe() || p >= ring.universe() || o >= ring.universe() {
        return false;
    }
    let r = range_sp(ring, s, p);
    if r.is_empty() {
        return false;
    }
    ring.range_next_value(Col::O, r, o).is_some_and(|v| v == o)
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
    MiddleRuns { range: RowRange, mid: MiddleKind },
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

/// Build the dense scan kind for (s,p,o,target_field) on Ring A.
fn dense_scan_kind(
    ring: RingRef<'_>,
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
    let universe = ring.universe();
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
        // S+P → O: last col over contiguous SP range on T_spo.
        // BSBM scan / star often have |SP|=1 (one object per subject+pred) —
        // Singleton avoids RDI/RNV open overhead on every LFTJ depth-2 probe.
        (Some(sv), Some(pv), None, 2) => {
            let r = range_sp(ring, sv, pv);
            if r.is_empty() {
                DenseScanKind::Empty
            } else if r.len() == 1 {
                DenseScanKind::Singleton(ring.access(Col::O, r.start))
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
fn dense_seek(ring: RingRef<'_>, kind: DenseScanKind, target: u32) -> Option<u32> {
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
            let mid_fn: fn(RingRef<'_>, u32) -> u32 = match mid {
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
fn dense_advance(ring: RingRef<'_>, kind: DenseScanKind, current: u32) -> Option<u32> {
    if current == u32::MAX {
        return None;
    }
    dense_seek(ring, kind, current.saturating_add(1))
}

/// Count distinct middle values up to `budget` runs (O(min(distinct,budget)·log n)).
///
/// Returns `(count, exhausted)` where `exhausted == false` means the budget was
/// hit before the end of the range (caller should fall back to row-span heuristic).
///
/// Critical for VEO on BSBM **scan** `{ ?s P31 ?o }`: bound-P → target O is
/// `MiddleRuns` with ~50 distinct classes, while target S is LastCol with
/// ~50k subjects. Using `rows` for both made VEO bind S first → 50k SP opens.
fn count_middle_runs_budgeted(
    ring: RingRef<'_>,
    range: RowRange,
    mid: MiddleKind,
    budget: u64,
) -> (u64, bool) {
    if range.is_empty() {
        return (0, true);
    }
    if budget == 0 {
        return (0, false);
    }
    let mid_fn: fn(RingRef<'_>, u32) -> u32 = match mid {
        MiddleKind::PUnderS => middle_p_spo,
        MiddleKind::SUnderO => middle_s_osp,
        MiddleKind::OUnderP => middle_o_pos,
    };
    let mut n = 0u64;
    let mut i = range.start;
    while i < range.end {
        let v = mid_fn(ring, i);
        n += 1;
        if n >= budget {
            // More runs may remain — not exhausted.
            let next = if v == u32::MAX {
                range.end
            } else {
                lower_bound_middle(
                    ring,
                    i.saturating_add(1),
                    range.end,
                    v.saturating_add(1),
                    mid_fn,
                )
            };
            return (n, next >= range.end);
        }
        let next = if v == u32::MAX {
            range.end
        } else {
            lower_bound_middle(
                ring,
                i.saturating_add(1),
                range.end,
                v.saturating_add(1),
                mid_fn,
            )
        };
        i = next;
    }
    (n, true)
}

#[inline]
fn middle_rows_heuristic(ring: RingRef<'_>, range: RowRange, n_bound: u64) -> u64 {
    let rows = u64::from(range.len().max(1));
    let vocab = u64::from(ring.universe().max(1)) / (n_bound + 1);
    rows.min(vocab).max(1)
}

/// Cache key for MiddleRuns VEO estimates that do not depend on outer LFTJ
/// bindings (e.g. bound-P → target O). Adaptive VEO re-probes every depth with
/// the same (range, mid); without a cache, path_2hop walked up to
/// `VEO_MIDDLE_EXACT_RUN_BUDGET` runs × 50k outer subjects ( hang).
///
/// Keyed by ring identity (n, universe) + range + middle kind. Process-wide;
/// assumes one active compacted graph per process (RESULTS_MEM). Cap
/// keeps memory bounded if many patterns are probed.
#[derive(Clone, Copy, Hash, Eq, PartialEq)]
struct VeoMiddleCacheKey {
    n: u32,
    universe: u32,
    start: u32,
    end: u32,
    mid: u8,
    n_bound: u8,
}

fn middle_kind_tag(mid: MiddleKind) -> u8 {
    match mid {
        MiddleKind::PUnderS => 0,
        MiddleKind::SUnderO => 1,
        MiddleKind::OUnderP => 2,
    }
}

fn veo_middle_cache_get(key: VeoMiddleCacheKey) -> Option<u64> {
    VEO_MIDDLE_CACHE
        .lock()
        .ok()
        .and_then(|g| g.get(&key).copied())
}

fn veo_middle_cache_put(key: VeoMiddleCacheKey, val: u64) {
    if let Ok(mut g) = VEO_MIDDLE_CACHE.lock() {
        if g.len() >= VEO_MIDDLE_CACHE_CAP {
            // Drop half when full (cheap; order irrelevant for now).
            let drop_n = g.len() / 2;
            let keys: Vec<_> = g.keys().copied().take(drop_n).collect();
            for k in keys {
                g.remove(&k);
            }
        }
        g.insert(key, val);
    }
}

static VEO_MIDDLE_CACHE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<VeoMiddleCacheKey, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
const VEO_MIDDLE_CACHE_CAP: usize = 4096;

/// Distinct-count estimate for VEO (`lftj_real_count` / estimate).
///
/// - **MiddleRuns** (default): exact distinct-run count up to
///   [`VEO_MIDDLE_EXACT_RUN_BUDGET`]; fall back to row-span heuristic if budget
///   exceeded. Results are **cached** per (range, mid) so adaptive VEO does
///   not re-walk the same middle on every outer binding.
///   `NOVA_RING_VEO_OLD_HEURISTIC=1` forces the old row-span path (A/B).
/// - **LastCol**: min(row-span, vocab heuristic) — LOUDS-style, no full RDI.
///
/// Planning time is accumulated in `SPARQL_PATH.veo_plan_ns` (not query exec).
pub fn estimate_join_count(
    ring: RingRef<'_>,
    s: Option<u32>,
    p: Option<u32>,
    o: Option<u32>,
    target_field: usize,
) -> u64 {
    use crate::product_path::{SPARQL_PATH, VEO_MIDDLE_EXACT_RUN_BUDGET, ring_veo_old_heuristic};
    let t0 = std::time::Instant::now();
    SPARQL_PATH
        .veo_estimate_calls
        .fetch_add(1, Ordering::Relaxed);

    let n_bound = (s.is_some() as u64) + (p.is_some() as u64) + (o.is_some() as u64);
    let kind = dense_scan_kind(ring, s, p, o, target_field);
    let out = match kind {
        DenseScanKind::Empty => 0,
        DenseScanKind::Singleton(_) => 1,
        DenseScanKind::LastCol { range, .. } => {
            let rows = u64::from(range.len().max(1));
            let vocab = u64::from(ring.universe().max(1)) / (n_bound + 1);
            rows.min(vocab).max(1)
        }
        DenseScanKind::MiddleRuns { range, mid } => {
            let cache_key = VeoMiddleCacheKey {
                n: ring.n(),
                universe: ring.universe(),
                start: range.start,
                end: range.end,
                mid: middle_kind_tag(mid),
                n_bound: n_bound.min(255) as u8,
            };
            if let Some(cached) = veo_middle_cache_get(cache_key) {
                cached
            } else if ring_veo_old_heuristic() {
                SPARQL_PATH
                    .veo_middle_fallback
                    .fetch_add(1, Ordering::Relaxed);
                let v = middle_rows_heuristic(ring, range, n_bound);
                veo_middle_cache_put(cache_key, v);
                v
            } else {
                let (n, exhausted) =
                    count_middle_runs_budgeted(ring, range, mid, VEO_MIDDLE_EXACT_RUN_BUDGET);
                let v = if exhausted {
                    SPARQL_PATH.veo_middle_exact.fetch_add(1, Ordering::Relaxed);
                    n.max(1)
                } else {
                    // Budget hit: do not trust partial exact count for VEO.
                    SPARQL_PATH
                        .veo_middle_fallback
                        .fetch_add(1, Ordering::Relaxed);
                    middle_rows_heuristic(ring, range, n_bound)
                };
                veo_middle_cache_put(cache_key, v);
                v
            }
        }
    };

    let ns = t0.elapsed().as_nanos() as u64;
    SPARQL_PATH.veo_plan_ns.fetch_add(ns, Ordering::Relaxed);
    out
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
            let ring = img.index().ring_ref();
            dense_seek(ring, kind, 0)
        };
        Self { img, kind, current }
    }

    #[inline]
    fn ring(&self) -> RingRef<'_> {
        self.img.index().ring_ref()
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
        let d = self
            .current
            .expect("key() on exhausted BraidedStreamingScan");
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

// ── Medium LastCol scan (feature_lookup kernel) ───────────────────────
//
// feature_lookup is PO→S under (P2, feature0): ~N/N_FEATURES ≈ 500 subjects
// on the large BSBM corpus. Shape is ≈1 distinct S per row (sorted last col).
//
// LOUDS POS leaf walk is O(1) pos++ over pre-materialised labels. Ring cannot
// match that with a wavelet RDI tree walk (O(levels) per distinct). For this
// medium-range enumerate-all shape we:
//   1. Sequential-access the sorted last column once at open
//   2. Keep dense u32 ids only (to_external deferred to key())
//   3. Stream / partition_point seek over the small Vec
//
// This beats:
//   • full mapped RDI open + tree walk per symbol (≈1 distinct/row pays RDI
//     overhead with no empty-branch savings)
//   • lazy per-row access without materialise (worse locality + more calls)
//   • eager external materialise (remap paid for every value at open even if
//     the consumer only walks a prefix)
//
// Fallback when mmap RDI is unavailable and range is medium: same kernel.

/// Flat dense-key scan over a medium LastCol range (one small alloc at open).
struct BraidedMaterializedLastColScan {
    img: Arc<BraidedGraphImage>,
    /// Distinct dense symbols in ascending order.
    vals: Vec<u32>,
    pos: usize,
}

impl BraidedMaterializedLastColScan {
    fn open(img: Arc<BraidedGraphImage>, col: Col, range: RowRange) -> Self {
        if range.is_empty() {
            return Self {
                img,
                vals: Vec::new(),
                pos: 0,
            };
        }
        let ring = img.index().ring_ref();
        // Cap capacity to range len; consecutive-dedup may shrink.
        let mut vals = Vec::with_capacity(range.len() as usize);
        let mut prev: Option<u32> = None;
        for pos in range.start..range.end {
            let d = ring.access(col, pos);
            if prev == Some(d) {
                continue;
            }
            prev = Some(d);
            vals.push(d);
        }
        Self { img, vals, pos: 0 }
    }

    #[inline]
    fn external_key(&self, dense: u32) -> u64 {
        self.img
            .remap()
            .to_external(dense)
            .unwrap_or(u64::from(dense))
    }
}

impl TrieIterator for BraidedMaterializedLastColScan {
    #[inline]
    fn key(&self) -> u64 {
        self.external_key(self.vals[self.pos])
    }

    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        if self.external_key(self.vals[self.pos]) >= target {
            return;
        }
        // Dense ids assigned in ascending external order → partition on dense.
        let dense_target = match self.img.remap().to_dense(target) {
            Some(d) => d,
            None => match self.img.remap().dense_ceil(target) {
                Some(d) => d,
                None => {
                    self.pos = self.vals.len();
                    return;
                }
            },
        };
        self.pos = self.vals.partition_point(|&v| v < dense_target);
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

// ── Mapped LastCol RDI streaming scan (star kernel) ───────────────────
//
// Product star/scan kernel = mmap hot QWT + **stateful RDI**
//   (`MappedRangeDistinctIter` / `range_distinct_iter`).
// Product successor primitive = **RNV** (`range_next_value`) — O(log σ).
//
// LFTJ contract (nova-query): monotonic `seek` / `advance` on a scan opened
// once per pattern×depth. So:
//   • `advance` → RDI next while still in RDI mode (star / sequential walk).
//   • `seek`    → RNV to locate successor (never reopen RDI from range start).
//                 Small gap: forward-skip live RDI to that symbol.
//                 Large gap / after RNV-only: stay on RNV (path_2hop leapfrog).
//   • Heap RNV-only remains the no-mmap fallback (`BraidedStreamingScan`).
//
// Do not rebuild RDI at range.start and rescan on every seek — that is
// O(|distinct|) per seek and makes path_2hop ~40× slower than LOUDS.

/// How LastCol navigation is served after open / seek.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastColMode {
    /// Stateful mapped RDI (star kernel).
    Rdi,
    /// Mapped RNV only (leapfrog / large seek).
    Rnv,
}

/// Streaming distinct-value scan over **mapped** hot path for `DenseScanKind::LastCol`.
struct BraidedMappedLastColScan {
    img: Arc<BraidedGraphImage>,
    col: Col,
    range: RowRange,
    /// Stateful distinct iterator (RDI mode only; ignored in RNV mode).
    /// Holds HotQwtColumn by value (Copy mmap alias).
    rdi: MappedRangeDistinctIter<'static>,
    mode: LastColMode,
    current: Option<u32>,
}

/// If RNV lands more than this many dense ids ahead of `current`, switch to
/// RNV mode instead of burning RDI steps across the gap. Tuned for LFTJ
/// leapfrog (path_2hop) vs short star advances.
const RDI_FORWARD_GAP_MAX: u32 = 64;

impl BraidedMappedLastColScan {
    fn open(img: Arc<BraidedGraphImage>, col: Col, range: RowRange) -> Option<Self> {
        let mapped = img.index().mapped()?;
        if range.is_empty() {
            return None;
        }
        // Huffman C_p has no Qwt RDI stack — open in RNV mode (schema-sized σ_P).
        if col == Col::P && mapped.c_p_is_huff() {
            // Copy dummy hot before moving `img` into the struct.
            let hot = *mapped.col_hot(Col::O);
            let rdi = Self::new_rdi(&hot, RowRange::empty());
            let mut scan = Self {
                img,
                col,
                range,
                rdi,
                mode: LastColMode::Rnv,
                current: None,
            };
            scan.current = scan.mapped_rnv(0);
            return Some(scan);
        }
        let hot = *mapped.col_hot(col);
        let rdi = Self::new_rdi(&hot, range);
        let mut scan = Self {
            img,
            col,
            range,
            rdi,
            mode: LastColMode::Rdi,
            current: None,
        };
        // Open on RDI — matches e511 star_mmap first-symbol path.
        scan.current = scan.rdi.next_symbol().map(|(s, _)| s);
        Some(scan)
    }

    #[inline]
    fn new_rdi(hot: &HotQwtColumn, range: RowRange) -> MappedRangeDistinctIter<'static> {
        // HotQwtColumn is Copy and aliases immutable mmap; the RDI lifetime is
        // only PhantomData (no borrow of hot). Safe to rebind to 'static while
        // this scan (and the parent Arc<BraidedGraphImage> mmap) lives.
        let it = hot.range_distinct_iter(range.start as usize..range.end as usize);
        // SAFETY: MappedRangeDistinctIter stores HotQwtColumn by value; 'a is phantom.
        unsafe {
            std::mem::transmute::<MappedRangeDistinctIter<'_>, MappedRangeDistinctIter<'static>>(it)
        }
    }

    #[inline]
    fn external_key(&self, dense: u32) -> u64 {
        self.img
            .remap()
            .to_external(dense)
            .unwrap_or(u64::from(dense))
    }

    #[inline]
    fn mapped_rnv(&self, target: u32) -> Option<u32> {
        let mapped = self.img.index().mapped()?;
        mapped.range_next_value(self.col, self.range, target)
    }

    /// Forward-skip the **live** RDI until `current >= goal` (inclusive).
    fn forward_rdi_to(&mut self, goal: u32) {
        debug_assert_eq!(self.mode, LastColMode::Rdi);
        loop {
            match self.current {
                Some(c) if c >= goal => return,
                Some(_) => match self.rdi.next_symbol() {
                    Some((s, _)) => self.current = Some(s),
                    None => {
                        self.current = None;
                        return;
                    }
                },
                None => return,
            }
        }
    }

    /// rebind an existing LastCol cursor to a new SP range in-place.
    ///
    /// Prefers RDI bounds reset (no Arc re-open). Falls back to RNV mode for
    /// Huffman C_p (no Qwt RDI stack).
    #[inline]
    fn reset_to_range(&mut self, range: RowRange) -> bool {
        if range.is_empty() {
            self.current = None;
            self.range = range;
            return false;
        }
        self.range = range;
        // Huffman C_p: stay on RNV.
        if self.col == Col::P
            && let Some(m) = self.img.index().mapped()
            && m.c_p_is_huff()
        {
            self.mode = LastColMode::Rnv;
            self.current = self.mapped_rnv(0);
            return self.current.is_some();
        }
        self.mode = LastColMode::Rdi;
        // Rebuild RDI on the live hot column for this range.
        if let Some(mapped) = self.img.index().mapped() {
            let hot = *mapped.col_hot(self.col);
            self.rdi = Self::new_rdi(&hot, range);
            self.current = self.rdi.next_symbol().map(|(s, _)| s);
            return true; // empty distinct still counts as successful open
        }
        false
    }
}

impl TrieIterator for BraidedMappedLastColScan {
    #[inline]
    fn key(&self) -> u64 {
        let d = self
            .current
            .expect("key() on exhausted BraidedMappedLastColScan");
        self.external_key(d)
    }

    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        if let Some(cur) = self.current
            && self.external_key(cur) >= target
        {
            return;
        }
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

        // Always locate successor with mapped RNV (product successor primitive).
        let goal = match self.mapped_rnv(dense_target) {
            Some(v) => v,
            None => {
                self.current = None;
                return;
            }
        };

        match self.mode {
            LastColMode::Rdi => {
                let cur = self.current.unwrap_or(0);
                // Small monotonic gap: keep RDI cursor (star-like).
                // Large gap: switch to RNV mode (path leapfrog) — do **not**
                // restart RDI from range.start.
                if goal.saturating_sub(cur) <= RDI_FORWARD_GAP_MAX {
                    self.forward_rdi_to(goal);
                } else {
                    self.mode = LastColMode::Rnv;
                    self.current = Some(goal);
                }
            }
            LastColMode::Rnv => {
                self.current = Some(goal);
            }
        }
    }

    fn advance(&mut self) {
        let Some(cur) = self.current else {
            return;
        };
        match self.mode {
            LastColMode::Rdi => {
                // star kernel: stateful RDI next.
                self.current = self.rdi.next_symbol().map(|(s, _)| s);
            }
            LastColMode::Rnv => {
                if cur == u32::MAX {
                    self.current = None;
                } else {
                    self.current = self.mapped_rnv(cur.saturating_add(1));
                }
            }
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
        u64::from(!self.at_end())
    }
}

// ── D1 two-range object intersection scan ──────────────────────

/// Streaming D1 common-object scan under two subject ranges.
///
/// Used when LFTJ would leapfrog two `(bound_s, bound_p?, target_o)` scans
/// (triangle closing edge, 2-hop meet-in-middle). Requires mmap.
pub struct BraidedD1ObjectScan {
    img: Arc<BraidedGraphImage>,
    r0: RowRange,
    r1: RowRange,
    current: Option<u32>,
}

impl BraidedD1ObjectScan {
    fn open(img: Arc<BraidedGraphImage>, s0: u32, s1: u32) -> Option<Self> {
        if !img.has_mapped() {
            return None;
        }
        let idx = img.index();
        let r0 = idx.range_s(s0);
        let r1 = idx.range_s(s1);
        if r0.is_empty() || r1.is_empty() {
            return None;
        }
        let mut scan = Self {
            img,
            r0,
            r1,
            current: None,
        };
        scan.current = scan.next_from(0);
        Some(scan)
    }

    #[inline]
    fn next_from(&self, target: u32) -> Option<u32> {
        self.img
            .index()
            .intersection_next_value2(Col::O, self.r0, self.r1, target)
    }

    #[inline]
    fn external_key(&self, dense: u32) -> u64 {
        self.img
            .remap()
            .to_external(dense)
            .unwrap_or(u64::from(dense))
    }
}

impl TrieIterator for BraidedD1ObjectScan {
    #[inline]
    fn key(&self) -> u64 {
        let d = self
            .current
            .expect("key() on exhausted BraidedD1ObjectScan");
        self.external_key(d)
    }

    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        if let Some(cur) = self.current
            && self.external_key(cur) >= target
        {
            return;
        }
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
        self.current = self.next_from(dense_target);
    }

    fn advance(&mut self) {
        if let Some(cur) = self.current {
            if cur == u32::MAX {
                self.current = None;
            } else {
                self.current = self.next_from(cur.saturating_add(1));
            }
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
        u64::from(!self.at_end())
    }
}

// ── D2 three-range object intersection scan (triangle kernel) ─────

/// Streaming D2 common-object scan under three subject ranges.
///
/// Product triangle shape (RESULTS_MEM): distinct objects present
/// under subjects `s0`, `s1`, `s2` via `intersection_next_value3` on Col::O.
///
/// Not selected automatically by LFTJ leapfrog (three independent iterators);
/// exposed via [`BraidedGraphImage::d2_object_intersect_streaming`] for store-
/// local / future engine hooks. Unit tests gate semantics vs multi-scan oracle.
pub struct BraidedD2ObjectScan {
    img: Arc<BraidedGraphImage>,
    r0: RowRange,
    r1: RowRange,
    r2: RowRange,
    current: Option<u32>,
}

impl BraidedD2ObjectScan {
    fn open(img: Arc<BraidedGraphImage>, s0: u32, s1: u32, s2: u32) -> Option<Self> {
        if !img.has_mapped() {
            return None;
        }
        let idx = img.index();
        let r0 = idx.range_s(s0);
        let r1 = idx.range_s(s1);
        let r2 = idx.range_s(s2);
        if r0.is_empty() || r1.is_empty() || r2.is_empty() {
            return None;
        }
        let mut scan = Self {
            img,
            r0,
            r1,
            r2,
            current: None,
        };
        scan.current = scan.next_from(0);
        Some(scan)
    }

    #[inline]
    fn next_from(&self, target: u32) -> Option<u32> {
        self.img
            .index()
            .intersection_next_value3(Col::O, self.r0, self.r1, self.r2, target)
    }

    #[inline]
    fn external_key(&self, dense: u32) -> u64 {
        self.img
            .remap()
            .to_external(dense)
            .unwrap_or(u64::from(dense))
    }
}

impl TrieIterator for BraidedD2ObjectScan {
    #[inline]
    fn key(&self) -> u64 {
        let d = self
            .current
            .expect("key() on exhausted BraidedD2ObjectScan");
        self.external_key(d)
    }

    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        if let Some(cur) = self.current
            && self.external_key(cur) >= target
        {
            return;
        }
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
        self.current = self.next_from(dense_target);
    }

    fn advance(&mut self) {
        if let Some(cur) = self.current {
            if cur == u32::MAX {
                self.current = None;
            } else {
                self.current = self.next_from(cur.saturating_add(1));
            }
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
fn collect_dense(ring: RingRef<'_>, kind: DenseScanKind) -> Vec<u64> {
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
            self.ring_ref(),
            as_u32(s),
            as_u32(p),
            as_u32(o),
            target_field,
        );
        collect_dense(self.ring_ref(), kind)
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
            self.ring_ref(),
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
        let kind = dense_scan_kind(img.index().ring_ref(), sd, pd, od, target_field.min(2));
        let ctr = ring_counters_log_enabled();
        match kind {
            DenseScanKind::Empty => Box::new(EmptyTrieIter),
            // Small SP/PO/OS ranges (typical after binding S under bound P in
            // BSBM scan): mapped RDI open cost dominates a 1–few-symbol walk.
            // Heap RNV is O(log σ) per step and matches LOUDS open-per-row cost.
            // Threshold covers "one object per subject" (range len 1) through
            // modest fan-out; large star/scan lead ranges still take mapped RDI.
            DenseScanKind::LastCol { range, .. } if range.len() <= 16 => {
                if ctr {
                    SPARQL_PATH.path_heap_rnv.fetch_add(1, Ordering::Relaxed);
                }
                Box::new(BraidedStreamingScan::new(img, kind))
            }
            // Medium PO/SP/OS LastCol (feature_lookup ~500 subjects under one
            // feature, ≈1 distinct/row): sequential access + dense materialise
            // beats mapped RDI tree walk. LOUDS POS is O(1) leaf pos++; Ring
            // cannot match that with per-symbol RDI expands when every row is
            // a new symbol. Dense-only vec (to_external deferred to key()).
            DenseScanKind::LastCol { col, range } if range.len() <= LASTCOL_MATERIALIZE_MAX => {
                if ctr {
                    SPARQL_PATH.path_heap_rnv.fetch_add(1, Ordering::Relaxed);
                }
                Box::new(BraidedMaterializedLastColScan::open(img, col, range))
            }
            // Large LastCol (star / unbound lead ranges): mapped RDI.
            DenseScanKind::LastCol { col, range } if img.has_mapped() => {
                if ctr {
                    SPARQL_PATH.path_mapped_rdi.fetch_add(1, Ordering::Relaxed);
                }
                match BraidedMappedLastColScan::open(Arc::clone(&img), col, range) {
                    Some(scan) => Box::new(scan),
                    None => {
                        if ctr {
                            SPARQL_PATH.path_heap_rnv.fetch_add(1, Ordering::Relaxed);
                        }
                        Box::new(BraidedStreamingScan::new(img, kind))
                    }
                }
            }

            DenseScanKind::LastCol { .. } => {
                if ctr {
                    SPARQL_PATH.path_heap_rnv.fetch_add(1, Ordering::Relaxed);
                }
                Box::new(BraidedStreamingScan::new(img, kind))
            }

            DenseScanKind::MiddleRuns { .. } => {
                if ctr {
                    SPARQL_PATH.path_middle_runs.fetch_add(1, Ordering::Relaxed);
                }
                Box::new(BraidedStreamingScan::new(img, kind))
            }
            DenseScanKind::Singleton(_) => {
                if ctr {
                    SPARQL_PATH.path_singleton.fetch_add(1, Ordering::Relaxed);
                }
                Box::new(BraidedStreamingScan::new(img, kind))
            }
        }
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

    /// D2 common-object stream under three dense subjects (requires mmap).
    ///
    /// External IDs in / dense symbols out→external keys. Returns empty iterator
    /// if any subject is unmappable or ranges empty / no mmap.
    pub fn d2_object_intersect_streaming(
        img: Arc<Self>,
        s0: u64,
        s1: u64,
        s2: u64,
    ) -> Box<dyn TrieIterator> {
        use crate::product_path::ring_d2_enabled;
        let ctr = ring_counters_log_enabled();
        if ctr {
            SPARQL_PATH.d2_calls.fetch_add(1, Ordering::Relaxed);
        }
        if !ring_d2_enabled() || !img.has_mapped() {
            return Box::new(EmptyTrieIter);
        }
        let (Some(d0), Some(d1), Some(d2)) = (
            img.remap().to_dense(s0),
            img.remap().to_dense(s1),
            img.remap().to_dense(s2),
        ) else {
            return Box::new(EmptyTrieIter);
        };
        match BraidedD2ObjectScan::open(img, d0, d1, d2) {
            Some(scan) => {
                if ctr && !scan.at_end() {
                    SPARQL_PATH.d2_hits.fetch_add(1, Ordering::Relaxed);
                }
                Box::new(scan)
            }
            None => Box::new(EmptyTrieIter),
        }
    }

    /// W4b: multi-subject object intersection for LFTJ leapfrog collapse.
    ///
    /// `subjects` are **external** TermIds already bound as subjects of active
    /// patterns that all target the object field. Uses D1 for 2 subjects and
    /// D2 for ≥3 (product triangle kernel). Predicate bind is currently ignored
    /// at the range layer (subject lead ranges on Col::O) — same as
    /// harness which seeds `range_s` only; SP-restricted ranges can be layered
    /// later without changing the LFTJ API.
    ///
    /// Returns `None` when the kernel cannot run (no mmap / D2 off / unmappable)
    /// so the caller falls back to ordinary multi-scan leapfrog.
    pub fn multi_subject_object_intersect(
        img: Arc<Self>,
        subjects: &[u64],
    ) -> Option<Box<dyn TrieIterator>> {
        use crate::product_path::ring_d2_enabled;
        if !ring_d2_enabled() || !img.has_mapped() || subjects.len() < 2 {
            return None;
        }
        let ctr = ring_counters_log_enabled();
        if ctr {
            SPARQL_PATH.d2_calls.fetch_add(1, Ordering::Relaxed);
        }
        let mut dense = Vec::with_capacity(subjects.len());
        for &s in subjects {
            dense.push(img.remap().to_dense(s)?);
        }
        let scan: Box<dyn TrieIterator> = match dense.as_slice() {
            [d0, d1] => {
                let s = BraidedD1ObjectScan::open(Arc::clone(&img), *d0, *d1)?;
                Box::new(s)
            }
            [d0, d1, d2] => {
                let s = BraidedD2ObjectScan::open(Arc::clone(&img), *d0, *d1, *d2)?;
                Box::new(s)
            }
            // ≥4: fold pairwise via repeated D2 is out of scope; take first 3.
            xs if xs.len() > 3 => {
                let s = BraidedD2ObjectScan::open(Arc::clone(&img), xs[0], xs[1], xs[2])?;
                Box::new(s)
            }
            _ => return None,
        };
        if ctr && !scan.at_end() {
            SPARQL_PATH.d2_hits.fetch_add(1, Ordering::Relaxed);
        }
        Some(scan)
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
        estimate_join_count(self.index().ring_ref(), sd, pd, od, target_field)
    }

    /// Indexed pattern match in **external** TermId coordinates.
    ///
    /// Replaces full-graph `enumerate_spo_external` + filter for
    /// `quads_for_pattern`. Bound fields that fail dense remap yield empty.
    pub fn match_triples_external(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
    ) -> Vec<[u64; 3]> {
        let map_bound = |ext: Option<u64>| -> Option<Option<u32>> {
            match ext {
                None => Some(None),
                Some(e) => self.remap().to_dense(e).map(Some),
            }
        };
        let (Some(sd), Some(pd), Some(od)) = (map_bound(s), map_bound(p), map_bound(o)) else {
            return Vec::new();
        };
        match_triples_dense(self.index().ring_ref(), sd, pd, od)
            .into_iter()
            .filter_map(|t| self.remap().unmap_triple(t))
            .collect()
    }

    /// Exact SPO membership in external coordinates (O(log σ) RNV).
    pub fn contains_external(&self, s: u64, p: u64, o: u64) -> bool {
        let (Some(sd), Some(pd), Some(od)) = (
            self.remap().to_dense(s),
            self.remap().to_dense(p),
            self.remap().to_dense(o),
        ) else {
            return false;
        };
        contains_dense(self.index().ring_ref(), sd, pd, od)
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

// ── prepared resettable SP→O scanner + two-hop plan ───────────────────────
//
// Prepared scanners for the product path.
// RingRef ( mmap-only residency) + Huffman C_p product default.
//
// Hot path before prepare (per subject under fixed P):
//   graph snap + remap(s) + remap(P) + range_sp + open_lastcol + Box<dyn>
//
// PreparedSpObjectScan:
//   prepare once: img + dense P
//   reset_to_subject: remap(s) + range_sp + rebind RDI cursor only
//
// PreparedTwoHop:
//   hop1 + hop2 prepared scanners; execute nested a→b→c walk
// R1 middle-b range cache; R2 cheap hit rebind; pred adjacency

/// Max SP row-span that materialises objects via `access` instead of opening a
/// mapped LastCol RDI / RNV walk.
///
/// RESULTS_MEM `star_with_features` expands P2 with FAN_OUT_FEATURES=20 objects
/// per subject. Keeping T ≥ 20 avoids the expensive RNV path on that shape
/// (and still covers degree-1 P131 2join).
const SP_SMALL_RANGE_ACCESS: u32 = 32;

/// Max LastCol row-span served by dense materialise
/// ([`BraidedMaterializedLastColScan`]) instead of opening a mapped RDI stack.
///
/// `feature_lookup` is PO→S under (P2, feature0): ~N/N_FEATURES ≈ 500 subjects
/// on the large BSBM corpus. Opening a full mapped RDI for a one-shot sequential
/// walk dominates LOUDS POS trie open+leaf walk when ≈1 distinct/row. Within a
/// PO prefix on T_pos the last column C_s is sorted, so one sequential access
/// pass + consecutive dense dedup at open (to_external deferred to `key()`)
/// beats per-symbol RDI expands. Cap covers feature_lookup (~500) with headroom.
const LASTCOL_MATERIALIZE_MAX: u32 = 1024;

/// Body of a prepared SP→O cursor after `reset_to_subject` / `reset_from_range`.
///
/// `Mapped` holds a full RDI stack (~4KB+); boxing would allocate on every
/// subject reset that takes the large-range path.
#[allow(clippy::large_enum_variant)]
enum SpObjectBody {
    /// No objects under the current subject.
    Empty,
    /// Degree-1 SP: single dense object (external on key()).
    SingletonDense(u32),
    /// Tiny multi-valued SP: dense O ids via access. Two-hop hop2 adj can
    /// use key_dense() and skip external→dense remap per middle b.
    MaterializedDense { vals: Vec<u32>, pos: usize },
    /// Larger SP ranges: mapped LastCol RDI / RNV walk (keys external).
    Mapped(BraidedMappedLastColScan),
}

/// Resettable SP→O scanner for a fixed predicate.
///
/// Prepare once: img + dense P + optional shared adjacency (`Arc`).
///
/// **Adjacency policy:** never build a universe-sized adj table *inside*
/// bare [`prepare`] — SP-expansion prepares once per HTTP request and that
/// alone regressed 2join ~3.5→95 ms. Cross-request reuse is provided by
/// [`prepare_with_shared_adj`] + the store-level SP adj cache (warm hits get
/// O(1) range lookup; cold first request still uses `range_sp`).
///
/// `reset_to_subject`: remap(s) → adj or `range_sp` → small-range materialise
/// **or** rebind LastCol RDI.
pub struct PreparedSpObjectScanImpl {
    img: Arc<BraidedGraphImage>,
    pred_dense: u32,
    /// Shared dense subject → SP(RowRange). `None` ⇒ live `range_sp`.
    adj: Option<Arc<PredicateAdjacency>>,
    body: SpObjectBody,
    /// Retained mapped cursor across large-range resets (cheap rebind).
    mapped_hold: Option<BraidedMappedLastColScan>,
    last_range_len: u64,
}

impl PreparedSpObjectScanImpl {
    /// Cheap prepare (no adj build). Used by TwoHop hops and cold SP path.
    pub fn prepare(img: Arc<BraidedGraphImage>, predicate: u64) -> Option<Self> {
        Self::prepare_with_shared_adj(img, predicate, None)
    }

    /// Prepare with a pre-built (or `None`) adjacency table.
    ///
    /// Callers that own a process-wide SP adj cache pass `Some(Arc)` so warm
    /// HTTP 2join hits skip both adj rebuild and per-subject `range_sp`.
    pub fn prepare_with_shared_adj(
        img: Arc<BraidedGraphImage>,
        predicate: u64,
        adj: Option<Arc<PredicateAdjacency>>,
    ) -> Option<Self> {
        if !img.has_mapped() {
            return None;
        }
        let pred_dense = img.remap().to_dense(predicate)?;
        // Once-per-prepare (not per-row); free when counters off.
        if ring_counters_log_enabled() {
            SPARQL_PATH.k9_sp_prepare.fetch_add(1, Ordering::Relaxed);
        }
        Some(Self {
            img,
            pred_dense,
            adj,
            body: SpObjectBody::Empty,
            mapped_hold: None,
            last_range_len: 0,
        })
    }

    /// Build a adjacency table for `predicate` (dense). Expensive
    /// intended for the store-level SP adj cache cold path only.
    pub fn build_shared_adj(
        img: &BraidedGraphImage,
        predicate: u64,
    ) -> Option<Arc<PredicateAdjacency>> {
        let pred_dense = img.remap().to_dense(predicate)?;
        let mode = effective_pred_adjacency_mode();
        let universe = img.universe() as usize;
        PredicateAdjacency::build(img.index().ring_ref(), pred_dense, universe, mode).map(Arc::new)
    }

    /// Materialise distinct dense O ids from a small SP row span via access.
    /// T_spo is sorted by o within (s,p), so consecutive equal O values collapse.
    #[inline]
    fn materialize_small_range_dense(&self, range: RowRange) -> Vec<u32> {
        let ring = self.img.index().ring_ref();
        let mut vals = Vec::with_capacity(range.len().min(SP_SMALL_RANGE_ACCESS) as usize);
        let mut prev: Option<u32> = None;
        for pos in range.start..range.end {
            let d = ring.access(Col::O, pos);
            if prev == Some(d) {
                continue;
            }
            prev = Some(d);
            vals.push(d);
        }
        vals
    }

    #[inline]
    fn key_dense(&self) -> Option<u32> {
        match &self.body {
            SpObjectBody::SingletonDense(d) => Some(*d),
            SpObjectBody::MaterializedDense { vals, pos } => Some(vals[*pos]),
            _ => None,
        }
    }

    #[inline]
    fn external_of_dense(&self, d: u32) -> u64 {
        self.img.remap().to_external(d).unwrap_or(u64::from(d))
    }

    #[inline]
    fn bind_range(&mut self, range: RowRange, prefer_cheap_rebind: bool) -> bool {
        // Production path: no Instant / no atomics. Verbose counters only when
        // NOVA_RING_COUNTERS=1 (once-cached flag).
        let ctr = ring_counters_log_enabled();
        if ctr {
            SPARQL_PATH.k9_sp_reset.fetch_add(1, Ordering::Relaxed);
        }
        self.last_range_len = range.len() as u64;
        if range.is_empty() {
            if ctr {
                SPARQL_PATH
                    .k9_sp_empty_range
                    .fetch_add(1, Ordering::Relaxed);
            }
            self.body = SpObjectBody::Empty;
            return false;
        }

        // Degree-1 / low-fanout: one (or few) O access(es) beats RDI open.
        // 2join BSBM P131 is exactly degree 1 per subject — Singleton avoids Vec.
        if range.len() <= SP_SMALL_RANGE_ACCESS {
            let ring = self.img.index().ring_ref();
            let body = if range.len() == 1 {
                let d = ring.access(Col::O, range.start);
                SpObjectBody::SingletonDense(d)
            } else {
                let vals = self.materialize_small_range_dense(range);
                if vals.is_empty() {
                    self.body = SpObjectBody::Empty;
                    return false;
                }
                SpObjectBody::MaterializedDense { vals, pos: 0 }
            };
            // prefer_cheap_rebind only affects large-range RDI rebind path.
            let _ = prefer_cheap_rebind;
            self.body = body;
            return true;
        }

        // Large range: mapped LastCol RDI.
        let ok = if prefer_cheap_rebind {
            if let Some(cur) = self.mapped_hold.as_mut() {
                cur.reset_to_range(range)
            } else {
                match BraidedMappedLastColScan::open(Arc::clone(&self.img), Col::O, range) {
                    Some(scan) => {
                        self.mapped_hold = Some(scan);
                        true
                    }
                    None => false,
                }
            }
        } else {
            match BraidedMappedLastColScan::open(Arc::clone(&self.img), Col::O, range) {
                Some(scan) => {
                    self.mapped_hold = Some(scan);
                    true
                }
                None => false,
            }
        };
        if !ok {
            self.mapped_hold = None;
            self.body = SpObjectBody::Empty;
            return false;
        }
        if ctr {
            SPARQL_PATH.path_mapped_rdi.fetch_add(1, Ordering::Relaxed);
        }
        // Move held cursor into body for iteration; keep a clone path via take+put
        // on advance end is unnecessary — we stash back on next bind.
        // Use a swap: body owns the live cursor; mapped_hold is empty while live.
        let scan = self
            .mapped_hold
            .take()
            .expect("mapped cursor just opened/rebound");
        self.body = SpObjectBody::Mapped(scan);
        true
    }

    /// Park a live mapped cursor back into `mapped_hold` so the next large-range
    /// reset can cheaply rebind instead of re-opening.
    #[inline]
    fn park_mapped_body(&mut self) {
        if let SpObjectBody::Mapped(scan) = std::mem::replace(&mut self.body, SpObjectBody::Empty) {
            self.mapped_hold = Some(scan);
        } else {
            // Materialized / Empty: leave mapped_hold as-is (may already hold prior).
        }
    }

    #[inline]
    fn reset_from_range(&mut self, range: RowRange, cache_hit: bool) -> bool {
        self.park_mapped_body();
        self.bind_range(range, cache_hit)
    }
}

impl PreparedSpObjectScan for PreparedSpObjectScanImpl {
    fn reset_to_subject(&mut self, subject: u64) -> bool {
        let Some(s_dense) = self.img.remap().to_dense(subject) else {
            self.park_mapped_body();
            self.body = SpObjectBody::Empty;
            self.last_range_len = 0;
            return false;
        };

        // Shared adj (warm HTTP) → O(1); else live range_sp.
        let range = if let Some(adj) = self.adj.as_ref() {
            match adj.range_for_subject(s_dense) {
                Some(r) => r,
                None => {
                    self.park_mapped_body();
                    self.body = SpObjectBody::Empty;
                    self.last_range_len = 0;
                    return false;
                }
            }
        } else {
            range_sp(self.img.index().ring_ref(), s_dense, self.pred_dense)
        };

        // Prefer cheap rebind when a mapped cursor is parked (prior large range).
        let cache_hit = self.mapped_hold.is_some() || matches!(self.body, SpObjectBody::Mapped(_));
        self.park_mapped_body();
        self.bind_range(range, cache_hit)
    }

    #[inline]
    fn key(&self) -> u64 {
        match &self.body {
            SpObjectBody::SingletonDense(d) => self.external_of_dense(*d),
            SpObjectBody::MaterializedDense { vals, pos } => self.external_of_dense(vals[*pos]),
            SpObjectBody::Mapped(c) => c.key(),
            SpObjectBody::Empty => panic!("PreparedSpObjectScan::key at_end"),
        }
    }

    #[inline]
    fn advance(&mut self) {
        match &mut self.body {
            SpObjectBody::SingletonDense(_) => {
                self.body = SpObjectBody::Empty;
            }
            SpObjectBody::MaterializedDense { vals: _, pos } => {
                *pos += 1;
            }
            SpObjectBody::Mapped(c) => {
                c.advance();
            }
            SpObjectBody::Empty => {}
        }
    }

    #[inline]
    fn at_end(&self) -> bool {
        match &self.body {
            SpObjectBody::Empty => true,
            SpObjectBody::SingletonDense(_) => false,
            SpObjectBody::MaterializedDense { vals, pos } => *pos >= vals.len(),
            SpObjectBody::Mapped(c) => c.at_end(),
        }
    }

    #[inline]
    fn last_range_len(&self) -> u64 {
        self.last_range_len
    }
}

/// dense subject→SP(RowRange) table for a fixed predicate.
///
/// Shared across HTTP requests via the store-level SP adj cache (`Arc`).
///
/// Packed as parallel `starts`/`ends` (`u32` each) rather than
/// `Vec<Option<RowRange>>` — on 64-bit hosts `Option<RowRange>` is 12–16 B per
/// slot vs 8 B packed, so a universe-sized table (~50k–100k subjects on BSBM)
/// shrinks by ~40–50%. Empty range is encoded as `start >= end` (including the
/// zero slot `(0,0)`).
pub struct PredicateAdjacency {
    starts: Vec<u32>,
    ends: Vec<u32>,
    ranges_present: u64,
    bytes: u64,
    mode: PredAdjacencyMode,
}

impl PredicateAdjacency {
    #[inline]
    fn range_for_subject(&self, s_dense: u32) -> Option<RowRange> {
        let i = s_dense as usize;
        if i >= self.starts.len() {
            return None;
        }
        let start = self.starts[i];
        let end = self.ends[i];
        // Absent subjects stay at the zero-init (0,0) empty slot. Callers treat
        // empty as "no objects under this subject" — same as the old None arm.
        Some(RowRange { start, end })
    }

    #[inline]
    fn packed_bytes(len: usize) -> u64 {
        (len.saturating_mul(2 * std::mem::size_of::<u32>())) as u64
    }

    fn build_eager(ring: RingRef<'_>, pred: u32, universe: usize) -> Self {
        let n = universe.max(1);
        let mut starts = vec![0u32; n];
        let mut ends = vec![0u32; n];
        let mut present = 0u64;
        let rp = ring.range_p(pred);
        if !rp.is_empty() {
            let mut cur = 0u32;
            while let Some(s) = ring.range_next_value(Col::S, rp, cur) {
                let r = range_sp(ring, s, pred);
                let si = s as usize;
                if si < n {
                    starts[si] = r.start;
                    ends[si] = r.end;
                    if !r.is_empty() {
                        present += 1;
                    }
                }
                if s == u32::MAX {
                    break;
                }
                cur = s.saturating_add(1);
            }
        }
        let bytes = Self::packed_bytes(starts.capacity());
        Self {
            starts,
            ends,
            ranges_present: present,
            bytes,
            mode: PredAdjacencyMode::Eager,
        }
    }

    fn build_native(ring: RingRef<'_>, pred: u32, universe: usize) -> Self {
        let n = universe.max(1);
        let mut starts = vec![0u32; n];
        let mut ends = vec![0u32; n];
        let mut present = 0u64;
        let Some(a_s) = ring.col_a(Col::S) else {
            return Self {
                starts,
                ends,
                ranges_present: 0,
                bytes: 0,
                mode: PredAdjacencyMode::Native,
            };
        };
        let n_sym = a_s.len().saturating_sub(1).min(n);
        let p_next = pred.saturating_add(1);
        let p_is_max = pred == u32::MAX;
        for s in 0..n_sym {
            let start_s = a_s[s];
            let end_s = a_s[s + 1];
            if start_s >= end_s {
                continue;
            }
            let start = lower_bound_middle(ring, start_s, end_s, pred, middle_p_spo);
            let end = if p_is_max {
                end_s
            } else {
                lower_bound_middle(ring, start, end_s, p_next, middle_p_spo)
            };
            starts[s] = start;
            ends[s] = end;
            if start < end {
                present += 1;
            }
        }
        let bytes = Self::packed_bytes(starts.capacity());
        Self {
            starts,
            ends,
            ranges_present: present,
            bytes,
            mode: PredAdjacencyMode::Native,
        }
    }

    fn build(
        ring: RingRef<'_>,
        pred: u32,
        universe: usize,
        mode: PredAdjacencyMode,
    ) -> Option<Self> {
        let t0 = std::time::Instant::now();
        let adj = match mode {
            PredAdjacencyMode::Off => return None,
            PredAdjacencyMode::Eager => Self::build_eager(ring, pred, universe),
            PredAdjacencyMode::Native => Self::build_native(ring, pred, universe),
        };
        let ns = t0.elapsed().as_nanos() as u64;
        let mode_tag = match adj.mode {
            PredAdjacencyMode::Off => 0u64,
            PredAdjacencyMode::Eager => 1,
            PredAdjacencyMode::Native => 2,
        };
        SPARQL_PATH.k9_adj_prepare.fetch_add(1, Ordering::Relaxed);
        SPARQL_PATH
            .k9_adj_prepare_ns
            .fetch_add(ns, Ordering::Relaxed);
        SPARQL_PATH
            .k9_adj_ranges_present
            .store(adj.ranges_present, Ordering::Relaxed);
        SPARQL_PATH.k9_adj_bytes.store(adj.bytes, Ordering::Relaxed);
        SPARQL_PATH.k9_adj_mode.store(mode_tag, Ordering::Relaxed);
        Some(adj)
    }
}

/// Ring PreparedTwoHop body — product path_2hop kernel.
pub struct PreparedTwoHopImpl {
    img: Arc<BraidedGraphImage>,
    p1: u64,
    hop1: PreparedSpObjectScanImpl,
    hop2: PreparedSpObjectScanImpl,
    p2_dense: u32,
    p2_adj: Option<PredicateAdjacency>,
}

impl PreparedTwoHopImpl {
    pub fn prepare(img: Arc<BraidedGraphImage>, p1: u64, p2: u64) -> Option<Self> {
        // Build P1 adj once and share with hop1 so reset_to_subject is O(1)
        // (same table style as hop2). Critical on path_2hop: 50k outer subjects.
        let mode = effective_pred_adjacency_mode();
        let universe = img.universe() as usize;
        let p1_dense = img.remap().to_dense(p1)?;
        let p1_adj = PredicateAdjacency::build(img.index().ring_ref(), p1_dense, universe, mode)
            .map(Arc::new);
        let hop1 = PreparedSpObjectScanImpl::prepare_with_shared_adj(Arc::clone(&img), p1, p1_adj)?;
        let hop2 = PreparedSpObjectScanImpl::prepare(Arc::clone(&img), p2)?;
        let p2_dense = hop2.pred_dense;
        let p2_adj = PredicateAdjacency::build(img.index().ring_ref(), p2_dense, universe, mode);
        SPARQL_PATH
            .k9_two_hop_prepare
            .fetch_add(1, Ordering::Relaxed);
        Some(Self {
            img,
            p1,
            hop1,
            hop2,
            p2_dense,
            p2_adj,
        })
    }
}

impl PreparedTwoHop for PreparedTwoHopImpl {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        // Production hot path: no Instant / no per-row atomics.
        // Event counters only when NOVA_RING_COUNTERS=1.
        let ctr = ring_counters_log_enabled();
        if ctr {
            SPARQL_PATH
                .k9_two_hop_execute
                .fetch_add(1, Ordering::Relaxed);
        }

        let mut a_scan = BraidedGraphImage::join_scan_streaming(
            Arc::clone(&self.img),
            None,
            Some(self.p1),
            None,
            0,
        );
        if ctr {
            SPARQL_PATH.join_scan_open.fetch_add(1, Ordering::Relaxed);
        }

        let mut rows = 0u64;
        let use_adj = self.p2_adj.is_some();
        let universe = self.img.universe() as usize;
        let mut range_cache: Vec<Option<RowRange>> = if use_adj {
            Vec::new()
        } else {
            vec![None; universe.max(1)]
        };
        let hop2_pred = self.p2_dense;
        let img_ref = Arc::clone(&self.img);

        while !a_scan.at_end() {
            let a = a_scan.key();
            if !self.hop1.reset_to_subject(a) {
                a_scan.advance();
                continue;
            }
            while !self.hop1.at_end() {
                // Dense hop1 body skips external→dense remap on every middle b.
                let (b, b_dense) = if let Some(d) = self.hop1.key_dense() {
                    (self.hop1.external_of_dense(d), d)
                } else {
                    let b = self.hop1.key();
                    let Some(d) = img_ref.remap().to_dense(b) else {
                        self.hop1.advance();
                        continue;
                    };
                    (b, d)
                };
                let bi = b_dense as usize;

                let hop2_ok = if let Some(adj) = self.p2_adj.as_ref() {
                    match adj.range_for_subject(b_dense) {
                        Some(range) => self.hop2.reset_from_range(range, true),
                        None => false,
                    }
                } else if bi < range_cache.len() {
                    if let Some(cached) = range_cache[bi] {
                        self.hop2.reset_from_range(cached, true)
                    } else {
                        let range = range_sp(img_ref.index().ring_ref(), b_dense, hop2_pred);
                        range_cache[bi] = Some(range);
                        self.hop2.reset_from_range(range, false)
                    }
                } else {
                    self.hop2.reset_to_subject(b)
                };

                if hop2_ok {
                    while !self.hop2.at_end() {
                        let c = self.hop2.key();
                        emit(&[a, b, c])?;
                        rows += 1;
                        self.hop2.advance();
                    }
                }
                self.hop1.advance();
            }
            a_scan.advance();
        }

        if ctr {
            SPARQL_PATH
                .k9_two_hop_rows
                .fetch_add(rows, Ordering::Relaxed);
        }
        Ok(rows)
    }
}

// ── Prepared k-chain (k=3) ────────────────────────────────────────────────────
//
// Shape: `?a P1 ?b . ?b P2 ?c . ?c P3 ?d`
//
// Three resettable SP→O scanners (hop1 under P1, hop2 under P2, hop3 under P3)
// plus an outer subject scan under P1. Mirrors PreparedTwoHopImpl with one
// extra hop; hop2/hop3 use the same adj/range-cache pattern as hop2 in two-hop.

/// Prepared 3-hop chain body. Emits `[a, b, c, d]` external TermIds.
pub struct PreparedKChainImpl {
    img: Arc<BraidedGraphImage>,
    p1: u64,
    hop1: PreparedSpObjectScanImpl,
    hop2: PreparedSpObjectScanImpl,
    hop3: PreparedSpObjectScanImpl,
    p2_dense: u32,
    p3_dense: u32,
    p2_adj: Option<PredicateAdjacency>,
    p3_adj: Option<PredicateAdjacency>,
}

impl PreparedKChainImpl {
    pub fn prepare(img: Arc<BraidedGraphImage>, p1: u64, p2: u64, p3: u64) -> Option<Self> {
        let hop1 = PreparedSpObjectScanImpl::prepare(Arc::clone(&img), p1)?;
        let hop2 = PreparedSpObjectScanImpl::prepare(Arc::clone(&img), p2)?;
        let hop3 = PreparedSpObjectScanImpl::prepare(Arc::clone(&img), p3)?;
        let p2_dense = hop2.pred_dense;
        let p3_dense = hop3.pred_dense;
        let mode = effective_pred_adjacency_mode();
        let universe = img.universe() as usize;
        let p2_adj = PredicateAdjacency::build(img.index().ring_ref(), p2_dense, universe, mode);
        let p3_adj = PredicateAdjacency::build(img.index().ring_ref(), p3_dense, universe, mode);
        Some(Self {
            img,
            p1,
            hop1,
            hop2,
            hop3,
            p2_dense,
            p3_dense,
            p2_adj,
            p3_adj,
        })
    }

    /// Reset hop scanner to objects of `subject` under the hop's predicate,
    /// preferring adj / range-cache when available.
    #[inline]
    fn reset_hop(
        hop: &mut PreparedSpObjectScanImpl,
        adj: Option<&PredicateAdjacency>,
        range_cache: &mut [Option<RowRange>],
        img: &BraidedGraphImage,
        subject_ext: u64,
        pred_dense: u32,
    ) -> bool {
        let Some(s_dense) = img.remap().to_dense(subject_ext) else {
            return false;
        };
        let si = s_dense as usize;
        if let Some(adj) = adj {
            return match adj.range_for_subject(s_dense) {
                Some(range) => hop.reset_from_range(range, true),
                None => false,
            };
        }
        if si < range_cache.len() {
            if let Some(cached) = range_cache[si] {
                return hop.reset_from_range(cached, true);
            }
            let range = range_sp(img.index().ring_ref(), s_dense, pred_dense);
            range_cache[si] = Some(range);
            return hop.reset_from_range(range, false);
        }
        hop.reset_to_subject(subject_ext)
    }
}

impl PreparedPhysicalOperator for PreparedKChainImpl {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        let mut a_scan = BraidedGraphImage::join_scan_streaming(
            Arc::clone(&self.img),
            None,
            Some(self.p1),
            None,
            0,
        );
        let mut rows = 0u64;
        let universe = self.img.universe() as usize;
        let use_p2_adj = self.p2_adj.is_some();
        let use_p3_adj = self.p3_adj.is_some();
        let mut p2_cache: Vec<Option<RowRange>> = if use_p2_adj {
            Vec::new()
        } else {
            vec![None; universe.max(1)]
        };
        let mut p3_cache: Vec<Option<RowRange>> = if use_p3_adj {
            Vec::new()
        } else {
            vec![None; universe.max(1)]
        };
        let img_ref = Arc::clone(&self.img);

        while !a_scan.at_end() {
            let a = a_scan.key();
            if !self.hop1.reset_to_subject(a) {
                a_scan.advance();
                continue;
            }
            while !self.hop1.at_end() {
                let b = self.hop1.key();
                let hop2_ok = Self::reset_hop(
                    &mut self.hop2,
                    self.p2_adj.as_ref(),
                    &mut p2_cache,
                    &img_ref,
                    b,
                    self.p2_dense,
                );
                if hop2_ok {
                    while !self.hop2.at_end() {
                        let c = self.hop2.key();
                        let hop3_ok = Self::reset_hop(
                            &mut self.hop3,
                            self.p3_adj.as_ref(),
                            &mut p3_cache,
                            &img_ref,
                            c,
                            self.p3_dense,
                        );
                        if hop3_ok {
                            while !self.hop3.at_end() {
                                let d = self.hop3.key();
                                emit(&[a, b, c, d])?;
                                rows += 1;
                                self.hop3.advance();
                            }
                        }
                        self.hop2.advance();
                    }
                }
                self.hop1.advance();
            }
            a_scan.advance();
        }
        Ok(rows)
    }
}

// ── Prepared subject-star (k=3) ───────────────────────────────────────────────
//
// Shape: `?s P1 ?o1 . ?s P2 ?o2 . ?s P3 ?o3`
// Outer subjects under P1; Cartesian product of objects under each arm.
// Subjects missing any arm are skipped. Emit `[s, o1, o2, o3]`.

/// Prepared subject-star body (k=3).
///
/// Three SP→O hop scanners + optional adj for p2/p3 (same machinery as KChain).
pub struct PreparedStarImpl {
    img: Arc<BraidedGraphImage>,
    p1: u64,
    hop1: PreparedSpObjectScanImpl,
    hop2: PreparedSpObjectScanImpl,
    hop3: PreparedSpObjectScanImpl,
    p2_dense: u32,
    p3_dense: u32,
    p2_adj: Option<PredicateAdjacency>,
    p3_adj: Option<PredicateAdjacency>,
}

impl PreparedStarImpl {
    pub fn prepare(img: Arc<BraidedGraphImage>, p1: u64, p2: u64, p3: u64) -> Option<Self> {
        let hop1 = PreparedSpObjectScanImpl::prepare(Arc::clone(&img), p1)?;
        let hop2 = PreparedSpObjectScanImpl::prepare(Arc::clone(&img), p2)?;
        let hop3 = PreparedSpObjectScanImpl::prepare(Arc::clone(&img), p3)?;
        let p2_dense = hop2.pred_dense;
        let p3_dense = hop3.pred_dense;
        let mode = effective_pred_adjacency_mode();
        let universe = img.universe() as usize;
        let p2_adj = PredicateAdjacency::build(img.index().ring_ref(), p2_dense, universe, mode);
        let p3_adj = PredicateAdjacency::build(img.index().ring_ref(), p3_dense, universe, mode);
        Some(Self {
            img,
            p1,
            hop1,
            hop2,
            hop3,
            p2_dense,
            p3_dense,
            p2_adj,
            p3_adj,
        })
    }

    #[inline]
    fn reset_hop(
        hop: &mut PreparedSpObjectScanImpl,
        adj: Option<&PredicateAdjacency>,
        range_cache: &mut [Option<RowRange>],
        img: &BraidedGraphImage,
        subject_ext: u64,
        pred_dense: u32,
    ) -> bool {
        let Some(s_dense) = img.remap().to_dense(subject_ext) else {
            return false;
        };
        let si = s_dense as usize;
        if let Some(adj) = adj {
            return match adj.range_for_subject(s_dense) {
                Some(range) => hop.reset_from_range(range, true),
                None => false,
            };
        }
        if si < range_cache.len() {
            if let Some(cached) = range_cache[si] {
                return hop.reset_from_range(cached, true);
            }
            let range = range_sp(img.index().ring_ref(), s_dense, pred_dense);
            range_cache[si] = Some(range);
            return hop.reset_from_range(range, false);
        }
        hop.reset_to_subject(subject_ext)
    }
}

impl PreparedPhysicalOperator for PreparedStarImpl {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        let mut s_scan = BraidedGraphImage::join_scan_streaming(
            Arc::clone(&self.img),
            None,
            Some(self.p1),
            None,
            0,
        );
        let mut rows = 0u64;
        let universe = self.img.universe() as usize;
        let use_p2_adj = self.p2_adj.is_some();
        let use_p3_adj = self.p3_adj.is_some();
        let mut p2_cache: Vec<Option<RowRange>> = if use_p2_adj {
            Vec::new()
        } else {
            vec![None; universe.max(1)]
        };
        let mut p3_cache: Vec<Option<RowRange>> = if use_p3_adj {
            Vec::new()
        } else {
            vec![None; universe.max(1)]
        };
        let img_ref = Arc::clone(&self.img);

        // Collect o1/o2/o3 per subject then Cartesian emit (same semantics as
        // walker FALLBACK / LOUDS prepared star).
        while !s_scan.at_end() {
            let s = s_scan.key();
            // Arm 1 (p1): always via hop1 reset.
            if !self.hop1.reset_to_subject(s) {
                s_scan.advance();
                continue;
            }
            let mut o1s: Vec<u64> = Vec::new();
            while !self.hop1.at_end() {
                o1s.push(self.hop1.key());
                self.hop1.advance();
            }
            if o1s.is_empty() {
                s_scan.advance();
                continue;
            }

            let hop2_ok = Self::reset_hop(
                &mut self.hop2,
                self.p2_adj.as_ref(),
                &mut p2_cache,
                &img_ref,
                s,
                self.p2_dense,
            );
            if !hop2_ok {
                s_scan.advance();
                continue;
            }
            let mut o2s: Vec<u64> = Vec::new();
            while !self.hop2.at_end() {
                o2s.push(self.hop2.key());
                self.hop2.advance();
            }
            if o2s.is_empty() {
                s_scan.advance();
                continue;
            }

            let hop3_ok = Self::reset_hop(
                &mut self.hop3,
                self.p3_adj.as_ref(),
                &mut p3_cache,
                &img_ref,
                s,
                self.p3_dense,
            );
            if !hop3_ok {
                s_scan.advance();
                continue;
            }
            let mut o3s: Vec<u64> = Vec::new();
            while !self.hop3.at_end() {
                o3s.push(self.hop3.key());
                self.hop3.advance();
            }
            if o3s.is_empty() {
                s_scan.advance();
                continue;
            }

            for &o1 in &o1s {
                for &o2 in &o2s {
                    for &o3 in &o3s {
                        emit(&[s, o1, o2, o3])?;
                        rows += 1;
                    }
                }
            }
            s_scan.advance();
        }
        Ok(rows)
    }
}

// ── Prepared SP-expansion / 2join (dense-internal) ────────────────────────────
//
// Shape: `?s P_filter O_filter . ?s P_expand ?o`

// Gap vs LOUDS after Singleton + warm SpAdjCache (~1.24×): outer PO→S still
// streams external keys, then each subject does external→dense remap before
// adj lookup. This plan:
//   1. Materializes outer subjects of (P_filter, O_filter) once as dense ids
//      (cached across HTTP via physical-op cache).
//   2. Expands under P_expand with dense adj + O access (no per-s to_dense).
//   3. Emits external (s, o, 0) only at the emit boundary.

/// Dense-internal SP-expansion / 2join body.
///
/// Holds pre-materialized outer subjects + expand adjacency. Reused across
/// warm HTTP requests via the physical-op plan cache.
pub struct PreparedSpExpansionImpl {
    img: Arc<BraidedGraphImage>,
    /// Dense subjects of pattern A (P_filter, O_filter → S), external order.
    outer_subjects_dense: Vec<u32>,
    /// adj for P_expand (dense subject → SP RowRange).
    expand_adj: Option<Arc<PredicateAdjacency>>,
    expand_pred_dense: u32,
}

impl PreparedSpExpansionImpl {
    /// Prepare from external TermIds. Builds expand adj + materializes outer
    /// subjects once. Intended for physical-op cache cold path only.
    pub fn prepare(
        img: Arc<BraidedGraphImage>,
        p_filter: u64,
        o_filter: u64,
        p_expand: u64,
        expand_adj: Option<Arc<PredicateAdjacency>>,
    ) -> Option<Self> {
        if !img.has_mapped() {
            return None;
        }
        let remap = img.remap();
        let p_f = remap.to_dense(p_filter)?;
        let o_f = remap.to_dense(o_filter)?;
        let p_e = remap.to_dense(p_expand)?;

        // Outer: distinct dense S under (P_filter, O_filter) via T_pos LastCol.
        let ring = img.index().ring_ref();
        let po = range_po(ring, p_f, o_f);
        let mut outer = Vec::new();
        if !po.is_empty() {
            let mut cur = ring.range_next_value(Col::S, po, 0);
            while let Some(s) = cur {
                outer.push(s);
                if s == u32::MAX {
                    break;
                }
                cur = ring.range_next_value(Col::S, po, s.saturating_add(1));
            }
        }

        let adj = expand_adj.or_else(|| {
            let mode = effective_pred_adjacency_mode();
            let universe = img.universe() as usize;
            PredicateAdjacency::build(ring, p_e, universe, mode).map(Arc::new)
        });

        if ring_counters_log_enabled() {
            SPARQL_PATH.k9_sp_prepare.fetch_add(1, Ordering::Relaxed);
        }
        Some(Self {
            img,
            outer_subjects_dense: outer,
            expand_adj: adj,
            expand_pred_dense: p_e,
        })
    }

    #[inline]
    fn to_external(&self, dense: u32) -> u64 {
        self.img
            .remap()
            .to_external(dense)
            .unwrap_or(u64::from(dense))
    }

    /// SP row range for expand predicate under dense subject.
    #[inline]
    fn expand_range(&self, s_dense: u32) -> RowRange {
        if let Some(adj) = self.expand_adj.as_ref() {
            match adj.range_for_subject(s_dense) {
                Some(r) => r,
                None => RowRange::empty(),
            }
        } else {
            range_sp(self.img.index().ring_ref(), s_dense, self.expand_pred_dense)
        }
    }

    /// Emit objects under dense subject for expand predicate.
    ///
    /// Walks the SP range in-place (no per-subject `Vec`). T_spo is sorted by
    /// o within (s,p), so consecutive equal O values collapse. Degree-1 uses a
    /// single access (BSBM P131 / 2join).
    #[inline]
    fn emit_expand_objects(
        &self,
        s_dense: u32,
        s_ext: u64,
        emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>,
    ) -> Result<u64, ()> {
        let range = self.expand_range(s_dense);
        if range.is_empty() {
            return Ok(0);
        }

        let ring = self.img.index().ring_ref();
        if range.len() == 1 {
            let o_dense = ring.access(Col::O, range.start);
            let o_ext = self.to_external(o_dense);
            emit(&[s_ext, o_ext])?;
            return Ok(1);
        }

        // Small / medium SP: sequential O access + consecutive dedup.
        // star_with_features expands P2 with FAN_OUT_FEATURES=20 — stays here.
        if range.len() <= SP_SMALL_RANGE_ACCESS {
            let mut rows = 0u64;
            let mut prev: Option<u32> = None;
            for pos in range.start..range.end {
                let o_dense = ring.access(Col::O, pos);
                if prev == Some(o_dense) {
                    continue;
                }
                prev = Some(o_dense);
                let o_ext = self.to_external(o_dense);
                emit(&[s_ext, o_ext])?;
                rows += 1;
            }
            return Ok(rows);
        }

        // Large fan-out: distinct O via RNV (rare for 2join / star expand).
        let mut rows = 0u64;
        let mut cur = ring.range_next_value(Col::O, range, 0);
        while let Some(o_dense) = cur {
            let o_ext = self.to_external(o_dense);
            emit(&[s_ext, o_ext])?;
            rows += 1;
            if o_dense == u32::MAX {
                break;
            }
            cur = ring.range_next_value(Col::O, range, o_dense.saturating_add(1));
        }
        Ok(rows)
    }
}

impl PreparedPhysicalOperator for PreparedSpExpansionImpl {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        let mut rows = 0u64;
        let outer = std::mem::take(&mut self.outer_subjects_dense);

        for &s_dense in &outer {
            let s_ext = self.to_external(s_dense);
            rows += self.emit_expand_objects(s_dense, s_ext, emit)?;
        }

        // Restore outer list for next cached execute (physical-op cache reuse).
        self.outer_subjects_dense = outer;
        Ok(rows)
    }
}

// ── fixed-P wedge (prepared D1 body) ───────────────────────────────
//
// Shape: `?a P ?b . ?b P ?c . ?a P ?c` under one predicate.
//
// Plan:
// 1. Prepare once: densify P + optional adj (SP range O(1) per subject)
//   2. Enumerate outer a under P (join_scan subjects)
//   3. For each a: objects b under SP(a,P) via PreparedSpObjectScan
//   4. Close c via SP-restricted D1: ∩ Col::O over ranges SP(a,P) and SP(b,P)
//      (braided intersection_next_value2 — not unbound range_s multi-subject)
//
// Requires mmap (same as TwoHop / SpExpansion). No mmap → prepare returns None
// and the query walker keeps its nested multi-subject / join_scan fallback.

/// Opaque prepared fixed-predicate D1 context ( left-once).
///
/// Holds densified P + optional adj so each `bind_left` only remaps the outer
/// subject and looks up SP(a,P).
pub struct PreparedPredD1 {
    img: Arc<BraidedGraphImage>,
    pred_dense: u32,
    adj: Option<Arc<PredicateAdjacency>>,
}

impl PreparedPredD1 {
    pub fn prepare(img: Arc<BraidedGraphImage>, predicate: u64) -> Option<Self> {
        let adj = PreparedSpObjectScanImpl::build_shared_adj(&img, predicate);
        Self::prepare_with_adj(img, predicate, adj)
    }

    pub fn prepare_with_adj(
        img: Arc<BraidedGraphImage>,
        predicate: u64,
        adj: Option<Arc<PredicateAdjacency>>,
    ) -> Option<Self> {
        if !img.has_mapped() {
            return None;
        }
        let pred_dense = img.remap().to_dense(predicate)?;
        if ring_counters_log_enabled() {
            SPARQL_PATH.d1_pred_prepare.fetch_add(1, Ordering::Relaxed);
        }
        Some(Self {
            img,
            pred_dense,
            adj,
        })
    }

    #[inline]
    fn sp_range(&self, s_dense: u32) -> RowRange {
        if let Some(adj) = self.adj.as_ref() {
            match adj.range_for_subject(s_dense) {
                Some(r) => r,
                None => RowRange::empty(),
            }
        } else {
            range_sp(self.img.index().ring_ref(), s_dense, self.pred_dense)
        }
    }
}

/// Left-bound handle: SP(a,P) range fixed; each right subject does one D1 open.
///
/// When left SP is tiny (≤ wedge left-once T), `left_tiny` holds sorted distinct
/// dense O once per outer `a`. Each `intersect_right` only materialises right
/// and two-pointer merges — no re-walk of left.
struct PreparedLeftD1 {
    img: Arc<BraidedGraphImage>,
    pred_dense: u32,
    left_range: RowRange,
    left_tiny: Option<Vec<u32>>,
    adj: Option<Arc<PredicateAdjacency>>,
}

impl PreparedLeftD1 {
    #[inline]
    fn right_range(&self, s_dense: u32) -> RowRange {
        if let Some(adj) = self.adj.as_ref() {
            match adj.range_for_subject(s_dense) {
                Some(r) => r,
                None => RowRange::empty(),
            }
        } else {
            range_sp(self.img.index().ring_ref(), s_dense, self.pred_dense)
        }
    }
}

impl PreparedLeftIntersect for PreparedLeftD1 {
    fn intersect_right(&self, subject_b: u64) -> Option<Box<dyn TrieIterator>> {
        let b_dense = self.img.remap().to_dense(subject_b)?;
        let r1 = self.right_range(b_dense);
        if self.left_range.is_empty() || r1.is_empty() {
            return None;
        }

        if let Some(ref left_vals) = self.left_tiny {
            let t = effective_wedge_left_once_threshold().max(effective_d1_tiny_merge_threshold());
            if t > 0 && r1.len() <= t {
                if ring_counters_log_enabled() {
                    SPARQL_PATH.d1_open_calls.fetch_add(1, Ordering::Relaxed);
                }
                let right_vals = materialize_sp_o_dense(&self.img, r1);
                let commons = merge_dense_sorted_to_external(&self.img, left_vals, &right_vals);
                return Some(Box::new(TinyMergeScan {
                    vals: commons,
                    pos: 0,
                }) as Box<dyn TrieIterator>);
            }
        }

        Some(Box::new(BraidedSpD1ObjectScan::open(
            Arc::clone(&self.img),
            self.left_range,
            r1,
        )?))
    }
}

impl PreparedPredObjectIntersect for PreparedPredD1 {
    fn bind_left(&self, subject_a: u64) -> Option<Box<dyn PreparedLeftIntersect>> {
        let a_dense = self.img.remap().to_dense(subject_a)?;
        let left_range = self.sp_range(a_dense);
        if left_range.is_empty() {
            return None;
        }
        if ring_counters_log_enabled() {
            SPARQL_PATH
                .d1_left_range_reuse
                .fetch_add(1, Ordering::Relaxed);
        }
        let t = effective_wedge_left_once_threshold().max(effective_d1_tiny_merge_threshold());
        let left_tiny = if t > 0 && left_range.len() <= t {
            Some(materialize_sp_o_dense(&self.img, left_range))
        } else {
            None
        };
        Some(Box::new(PreparedLeftD1 {
            img: Arc::clone(&self.img),
            pred_dense: self.pred_dense,
            left_range,
            left_tiny,
            adj: self.adj.clone(),
        }))
    }

    fn intersect2(&self, subject_a: u64, subject_b: u64) -> Option<Box<dyn TrieIterator>> {
        let a_dense = self.img.remap().to_dense(subject_a)?;
        let b_dense = self.img.remap().to_dense(subject_b)?;
        let r0 = self.sp_range(a_dense);
        let r1 = self.sp_range(b_dense);
        if r0.is_empty() || r1.is_empty() {
            return None;
        }
        Some(Box::new(BraidedSpD1ObjectScan::open(
            Arc::clone(&self.img),
            r0,
            r1,
        )?))
    }
}

/// Streaming D1 object ∩ over two **SP-restricted** row ranges (fixed P).
///
/// Unlike [`BraidedD1ObjectScan`] (unbound `range_s`), this closes the wedge
/// chord under the same predicate as the outer edges.
///
/// ## Tiny-merge / left-once fast path
///
/// When both SP ranges have `len ≤ T` (product wedge default T=4 via
/// [`effective_wedge_left_once_threshold`], or global
/// [`effective_d1_tiny_merge_threshold`]), objects are materialised via O-column
/// `access` and two-pointer merged. This avoids a full braided
/// `intersection_next_value2` tree walk on the RESULTS_MEM related-graph shape
/// (every close is 3×3, |C| ∈ {0,1,2}).
struct BraidedSpD1ObjectScan {
    img: Arc<BraidedGraphImage>,
    r0: RowRange,
    r1: RowRange,
    /// Braided mode: next dense O (None = exhausted).
    current: Option<u32>,
    /// Tiny-merge mode: sorted external O intersection; `tiny_pos` is the cursor.
    tiny: Option<Vec<u64>>,
    tiny_pos: usize,
}

impl BraidedSpD1ObjectScan {
    fn open(img: Arc<BraidedGraphImage>, r0: RowRange, r1: RowRange) -> Option<Self> {
        if !img.has_mapped() || r0.is_empty() || r1.is_empty() {
            return None;
        }
        if ring_counters_log_enabled() {
            SPARQL_PATH.d1_open_calls.fetch_add(1, Ordering::Relaxed);
        }

        // Product wedge left-once default (4) OR global tiny-merge threshold.
        // Take the max so either gate can activate the short-c path.
        let t_wedge = effective_wedge_left_once_threshold();
        let t_global = effective_d1_tiny_merge_threshold();
        let t = t_wedge.max(t_global);
        let max_len = r0.len().max(r1.len());
        if t > 0 && max_len <= t {
            let commons = materialize_sp_d1_tiny_merge(&img, r0, r1);
            return Some(Self {
                img,
                r0,
                r1,
                current: None,
                tiny: Some(commons),
                tiny_pos: 0,
            });
        }

        let mut scan = Self {
            img,
            r0,
            r1,
            current: None,
            tiny: None,
            tiny_pos: 0,
        };
        scan.current = scan.next_from(0);
        Some(scan)
    }

    #[inline]
    fn next_from(&self, target: u32) -> Option<u32> {
        self.img
            .index()
            .intersection_next_value2(Col::O, self.r0, self.r1, target)
    }

    #[inline]
    fn external_key(&self, dense: u32) -> u64 {
        self.img
            .remap()
            .to_external(dense)
            .unwrap_or(u64::from(dense))
    }
}

/// Materialise distinct dense O ids from a tiny SP row span (sorted by o).
#[inline]
fn materialize_sp_o_dense(img: &BraidedGraphImage, range: RowRange) -> Vec<u32> {
    let ring = img.index().ring_ref();
    let mut vals = Vec::with_capacity(range.len() as usize);
    let mut prev: Option<u32> = None;
    for pos in range.start..range.end {
        let d = ring.access(Col::O, pos);
        if prev == Some(d) {
            continue;
        }
        prev = Some(d);
        vals.push(d);
    }
    vals
}

/// Two-pointer ∩ of two sorted dense id slices → external TermIds.
#[inline]
fn merge_dense_sorted_to_external(img: &BraidedGraphImage, a: &[u32], b: &[u32]) -> Vec<u64> {
    let remap = img.remap();
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                let d = a[i];
                out.push(remap.to_external(d).unwrap_or(u64::from(d)));
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// Two-pointer ∩ of two tiny SP object lists → external TermIds (sorted).
#[inline]
fn materialize_sp_d1_tiny_merge(img: &BraidedGraphImage, r0: RowRange, r1: RowRange) -> Vec<u64> {
    let a = materialize_sp_o_dense(img, r0);
    let b = materialize_sp_o_dense(img, r1);
    merge_dense_sorted_to_external(img, &a, &b)
}

/// Pre-buffered external intersection walk (left-once / tiny-merge result).
struct TinyMergeScan {
    vals: Vec<u64>,
    pos: usize,
}

impl TrieIterator for TinyMergeScan {
    #[inline]
    fn key(&self) -> u64 {
        self.vals[self.pos]
    }

    fn seek(&mut self, target: u64) {
        while self.pos < self.vals.len() && self.vals[self.pos] < target {
            self.pos += 1;
        }
    }

    fn advance(&mut self) {
        self.pos = self.pos.saturating_add(1).min(self.vals.len());
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

impl TrieIterator for BraidedSpD1ObjectScan {
    #[inline]
    fn key(&self) -> u64 {
        if let Some(ref vals) = self.tiny {
            return *vals
                .get(self.tiny_pos)
                .expect("key() on exhausted BraidedSpD1ObjectScan tiny");
        }
        let d = self
            .current
            .expect("key() on exhausted BraidedSpD1ObjectScan");
        self.external_key(d)
    }

    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        if let Some(ref vals) = self.tiny {
            while self.tiny_pos < vals.len() && vals[self.tiny_pos] < target {
                self.tiny_pos += 1;
            }
            return;
        }
        if let Some(cur) = self.current
            && self.external_key(cur) >= target
        {
            return;
        }
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
        self.current = self.next_from(dense_target);
    }

    fn advance(&mut self) {
        if let Some(ref vals) = self.tiny {
            self.tiny_pos = self.tiny_pos.saturating_add(1).min(vals.len());
            return;
        }
        if let Some(cur) = self.current {
            if cur == u32::MAX {
                self.current = None;
            } else {
                self.current = self.next_from(cur.saturating_add(1));
            }
        }
    }

    fn open(&self) -> Box<dyn TrieIterator> {
        Box::new(EmptyTrieIter)
    }

    #[inline]
    fn at_end(&self) -> bool {
        if let Some(ref vals) = self.tiny {
            return self.tiny_pos >= vals.len();
        }
        self.current.is_none()
    }

    fn remaining_count(&self) -> u64 {
        if let Some(ref vals) = self.tiny {
            return (vals.len().saturating_sub(self.tiny_pos)) as u64;
        }
        u64::from(!self.at_end())
    }
}

/// Ring prepared wedge body — outer a→b under P, close via SP-restricted D1.
pub struct PreparedWedgeImpl {
    img: Arc<BraidedGraphImage>,
    predicate: u64,
    /// Outer a→b scanner under P.
    hop: PreparedSpObjectScanImpl,
    /// Fixed-P D1 context (left-once SP ranges).
    d1: PreparedPredD1,
}

impl PreparedWedgeImpl {
    /// Prepare fixed-P triangle body. Requires mmap + densifiable predicate.
    ///
    /// Returns `None` when the kernel cannot run so walkers keep nested fallback
    /// (never an empty execute that would drop triangle rows).
    pub fn prepare(img: Arc<BraidedGraphImage>, predicate: u64) -> Option<Self> {
        if !img.has_mapped() {
            return None;
        }
        let adj = PreparedSpObjectScanImpl::build_shared_adj(&img, predicate);
        let hop = PreparedSpObjectScanImpl::prepare_with_shared_adj(
            Arc::clone(&img),
            predicate,
            adj.clone(),
        )?;
        let d1 = PreparedPredD1::prepare_with_adj(Arc::clone(&img), predicate, adj)?;
        Some(Self {
            img,
            predicate,
            hop,
            d1,
        })
    }
}

impl PreparedWedge for PreparedWedgeImpl {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        // Production hot path: no Instant / no per-row atomics.
        let mut rows = 0u64;

        // Outer subjects of P.
        let mut a_scan = BraidedGraphImage::join_scan_streaming(
            Arc::clone(&self.img),
            None,
            Some(self.predicate),
            None,
            0,
        );

        while !a_scan.at_end() {
            let a = a_scan.key();
            // Bind left SP(a,P) once for all b under this a.
            let Some(left) = self.d1.bind_left(a) else {
                a_scan.advance();
                continue;
            };
            if !self.hop.reset_to_subject(a) {
                a_scan.advance();
                continue;
            }
            while !self.hop.at_end() {
                let b = self.hop.key();
                if b != a
                    && let Some(mut c_scan) = left.intersect_right(b)
                {
                    while !c_scan.at_end() {
                        let c = c_scan.key();
                        if c != a && c != b {
                            emit(&[a, b, c])?;
                            rows += 1;
                        }
                        c_scan.advance();
                    }
                }
                self.hop.advance();
            }
            a_scan.advance();
        }

        Ok(rows)
    }
}

/// Diagnostic placeholders (benches may import these names).
#[derive(Debug, Clone, Default)]
pub struct WedgeOuterProfile {
    pub rows: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WedgeProfileOpts {
    pub materialize: bool,
    pub precompute_ranges: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::BraidedRingIndex;
    use crate::{Col, RowRange};

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
            .map(|x| {
                [
                    u64::from(x[0]) + 100,
                    u64::from(x[1]) + 10,
                    u64::from(x[2]) + 200,
                ]
            })
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

    /// feature_lookup shape: PO→S with |range| in (16, LASTCOL_MATERIALIZE_MAX].
    /// Dense materialise path must match external collect (and RNV oracle).
    #[test]
    fn sequential_medium_lastcol_po_s_matches_oracle() {
        // Build a PO range with many distinct S under one (P, O).
        // 40 subjects share (p=1, o=7) → |range_po| = 40 ∈ (16, 1024].
        let mut triples = Vec::new();
        for s in 0u32..40 {
            triples.push([s, 1, 7]);
            // noise under other objects so the lead-P range is larger.
            triples.push([s, 1, 8]);
        }
        // a few other predicates
        triples.push([0, 0, 0]);
        triples.push([1, 2, 3]);

        let ext: Vec<[u64; 3]> = triples
            .iter()
            .map(|x| {
                [
                    u64::from(x[0]) + 1000,
                    u64::from(x[1]) + 50,
                    u64::from(x[2]) + 2000,
                ]
            })
            .collect();
        let mut img = BraidedGraphImage::from_external_triples(&ext);
        let path = std::env::temp_dir().join(format!(
            "braided_seq_lastcol_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        img.materialize_mapped(&path).expect("mmap");
        let img = Arc::new(img);

        // feature_lookup: bound P=51, O=2007 → target S
        let p = 51u64;
        let o = 2007u64;
        let mut it =
            BraidedGraphImage::join_scan_streaming(Arc::clone(&img), None, Some(p), Some(o), 0);
        let mut got = Vec::new();
        while !it.at_end() {
            got.push(it.key());
            it.advance();
        }
        let want = img.join_scan_external(None, Some(p), Some(o), 0);
        assert_eq!(got, want, "lazy sequential LastCol vs external collect");
        assert_eq!(got.len(), 40, "expect 40 subjects under feature");

        // seek mid-stream
        let mut it2 =
            BraidedGraphImage::join_scan_streaming(Arc::clone(&img), None, Some(p), Some(o), 0);
        let mid = got[20];
        it2.seek(mid);
        assert!(!it2.at_end());
        assert_eq!(it2.key(), mid);
        it2.advance();
        assert_eq!(it2.key(), got[21]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mapped_streaming_lastcol_matches_heap_oracle() {
        let t = sample();
        let ext: Vec<[u64; 3]> = t
            .iter()
            .map(|x| {
                [
                    u64::from(x[0]) + 50,
                    u64::from(x[1]) + 5,
                    u64::from(x[2]) + 100,
                ]
            })
            .collect();
        let mut img = BraidedGraphImage::from_external_triples(&ext);
        let path = std::env::temp_dir().join(format!(
            "braided_mapped_stream_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        img.materialize_mapped(&path).expect("mmap");
        assert!(img.has_mapped());
        let img = Arc::new(img);

        let cases: &[(Option<u64>, Option<u64>, Option<u64>, usize)] = &[
            (None, None, None, 0),
            (None, None, None, 2),
            (Some(50), None, None, 2),
            (Some(50), Some(5), None, 2),
            (None, Some(5), None, 0),
        ];
        for &(s, p, o, tf) in cases {
            let mut it = BraidedGraphImage::join_scan_streaming(Arc::clone(&img), s, p, o, tf);
            let mut got = Vec::new();
            while !it.at_end() {
                got.push(it.key());
                it.advance();
            }
            let want = img.join_scan_external(s, p, o, tf);
            assert_eq!(
                got, want,
                "mapped stream vs external s={s:?} p={p:?} o={o:?} tf={tf}"
            );
        }
        let _ = std::fs::remove_file(&path);
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
    fn multi_subject_object_intersect_d1_matches_pair_oracle() {
        let t = sample();
        let ext: Vec<[u64; 3]> = t
            .iter()
            .map(|x| [u64::from(x[0]), u64::from(x[1]), u64::from(x[2])])
            .collect();
        let mut img = BraidedGraphImage::from_external_triples(&ext);
        let path = std::env::temp_dir().join(format!(
            "braided_d1_multi_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        img.materialize_mapped(&path).expect("mmap");
        let img = Arc::new(img);
        crate::product_path::SPARQL_PATH.reset();
        let mut it = BraidedGraphImage::multi_subject_object_intersect(Arc::clone(&img), &[0, 1])
            .expect("D1 multi-subject must open on mmap");
        let mut got = Vec::new();
        while !it.at_end() {
            got.push(it.key());
            it.advance();
        }
        let o0 = img.join_scan_external(Some(0), None, None, 2);
        let o1 = img.join_scan_external(Some(1), None, None, 2);
        let mut common = o0
            .into_iter()
            .filter(|v| o1.contains(v))
            .collect::<Vec<_>>();
        common.sort_unstable();
        common.dedup();
        assert_eq!(got, common, "D1 multi-subject must match pairwise object ∩");
        // d2_calls only bumps when NOVA_RING_COUNTERS=1.
        if crate::product_path::ring_counters_log_enabled() {
            let snap = crate::product_path::SPARQL_PATH.snapshot();
            assert!(snap.d2_calls >= 1, "W4b counter d2_calls must bump");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn d2_streaming_trie_matches_multi_scan_oracle() {
        let t = sample();
        let ext: Vec<[u64; 3]> = t
            .iter()
            .map(|x| [u64::from(x[0]), u64::from(x[1]), u64::from(x[2])])
            .collect();
        let mut img = BraidedGraphImage::from_external_triples(&ext);
        let path = std::env::temp_dir().join(format!(
            "braided_d2_stream_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        img.materialize_mapped(&path).expect("mmap");
        let img = Arc::new(img);

        // Dense subjects 0,1,2 map to external 0,1,2 on this sample.
        let mut it = BraidedGraphImage::d2_object_intersect_streaming(Arc::clone(&img), 0, 1, 2);
        let mut got = Vec::new();
        while !it.at_end() {
            got.push(it.key());
            it.advance();
        }
        let o0 = img.join_scan_external(Some(0), None, None, 2);
        let o1 = img.join_scan_external(Some(1), None, None, 2);
        let o2 = img.join_scan_external(Some(2), None, None, 2);
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
            crate::facade::oracle::sorted_common_symbols(&sets)
                .into_iter()
                .map(u64::from)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            got, common,
            "D2 TrieIterator must match multi-scan intersection"
        );
        let _ = std::fs::remove_file(&path);
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
            crate::facade::oracle::sorted_common_symbols(&sets)
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
        let ring = idx.ring_ref();
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

    fn oracle_match(
        triples: &[[u32; 3]],
        s: Option<u32>,
        p: Option<u32>,
        o: Option<u32>,
    ) -> Vec<[u32; 3]> {
        let mut out: Vec<[u32; 3]> = triples
            .iter()
            .copied()
            .filter(|t| {
                s.is_none_or(|sv| t[0] == sv)
                    && p.is_none_or(|pv| t[1] == pv)
                    && o.is_none_or(|ov| t[2] == ov)
            })
            .collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn match_triples_dense_matches_filter_oracle() {
        let t = sample();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let ring = idx.ring_ref();
        let cases: &[(Option<u32>, Option<u32>, Option<u32>)] = &[
            (None, None, None),
            (Some(0), None, None),
            (None, Some(0), None),
            (None, None, Some(1)),
            (Some(0), Some(0), None),
            (Some(1), None, Some(0)),
            (None, Some(1), Some(2)),
            (Some(0), Some(0), Some(1)),
            (Some(9), None, None),
            (None, Some(9), None),
        ];
        for &(s, p, o) in cases {
            let mut got = match_triples_dense(ring, s, p, o);
            got.sort_unstable();
            let want = oracle_match(&t, s, p, o);
            assert_eq!(got, want, "match s={s:?} p={p:?} o={o:?}");
            if let (Some(sv), Some(pv), Some(ov)) = (s, p, o) {
                assert_eq!(
                    contains_dense(ring, sv, pv, ov),
                    !want.is_empty(),
                    "contains s={sv} p={pv} o={ov}"
                );
            }
        }
    }

    #[test]
    fn match_triples_external_matches_enumerate_filter() {
        let t = sample();
        let ext: Vec<[u64; 3]> = t
            .iter()
            .map(|x| {
                [
                    u64::from(x[0]) + 10,
                    u64::from(x[1]) + 20,
                    u64::from(x[2]) + 30,
                ]
            })
            .collect();
        let img = BraidedGraphImage::from_external_triples(&ext);
        let cases: &[(Option<u64>, Option<u64>, Option<u64>)] = &[
            (None, None, None),
            (None, Some(20), None), // bound P — BSBM scan shape
            (Some(10), None, None),
            (Some(10), Some(20), None),
            (None, None, Some(31)),
        ];
        for &(s, p, o) in cases {
            let mut got = img.match_triples_external(s, p, o);
            got.sort_unstable();
            let mut want: Vec<[u64; 3]> = img
                .enumerate_spo_external()
                .into_iter()
                .filter(|t| {
                    s.is_none_or(|sv| t[0] == sv)
                        && p.is_none_or(|pv| t[1] == pv)
                        && o.is_none_or(|ov| t[2] == ov)
                })
                .collect();
            want.sort_unstable();
            assert_eq!(got, want, "external match s={s:?} p={p:?} o={o:?}");
        }
    }

    /// Prepared SP→O must match join_scan_external for small (degree ≤16) ranges
    /// via the materialize-access fast path (2join P131 shape).
    #[test]
    fn prepared_sp_object_scan_small_range_matches_join_scan() {
        let t = sample();
        let ext: Vec<[u64; 3]> = t
            .iter()
            .map(|x| {
                [
                    u64::from(x[0]) + 100,
                    u64::from(x[1]) + 10,
                    u64::from(x[2]) + 200,
                ]
            })
            .collect();
        let mut img = BraidedGraphImage::from_external_triples(&ext);
        let path = std::env::temp_dir().join(format!(
            "braided_prep_sp_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        img.materialize_mapped(&path).expect("mmap");
        let img = Arc::new(img);

        // Predicate external 10 (= dense p under sample).
        let mut prep = PreparedSpObjectScanImpl::prepare(Arc::clone(&img), 10)
            .expect("prepare SP scan under mmap");

        // Subjects that have p=10: external 100, 101, 102 (dense 0,1,2).
        for s_ext in [100u64, 101, 102] {
            let want = img.join_scan_external(Some(s_ext), Some(10), None, 2);
            if want.is_empty() {
                assert!(
                    !prep.reset_to_subject(s_ext),
                    "empty SP should fail reset s={s_ext}"
                );
                continue;
            }
            assert!(
                prep.reset_to_subject(s_ext),
                "reset must succeed for s={s_ext}"
            );
            let mut got = Vec::new();
            while !prep.at_end() {
                got.push(prep.key());
                prep.advance();
            }
            assert_eq!(got, want, "prepared SP vs join_scan s={s_ext}");
            // Degree-1 / small ranges should not open mapped RDI body.
            assert!(
                prep.last_range_len() <= u64::from(SP_SMALL_RANGE_ACCESS),
                "sample SP ranges are tiny"
            );
        }

        // Multi-reset reuse (2join outer loop shape).
        assert!(prep.reset_to_subject(100));
        let first = prep.key();
        assert!(prep.reset_to_subject(101));
        let second = prep.key();
        assert_ne!(first, 0);
        assert_ne!(second, 0);

        let _ = std::fs::remove_file(&path);
    }

    /// Fixed-P wedge (oriented triangles) must match a nested SP-filter oracle.
    ///
    /// Graph under external P=10: triangle 100→101→102→100 plus chord 100→102,
    /// and a non-closing edge 103→100. Emits (a,b,c) with a≠b≠c and edges
    /// aP b, bP c, aP c.
    #[test]
    fn prepared_wedge_fixed_p_triangle_matches_oracle() {
        // Dense-friendly external ids: nodes 100..103, predicate 10.
        let ext: Vec<[u64; 3]> = vec![
            [100, 10, 101], // a→b
            [101, 10, 102], // b→c
            [100, 10, 102], // a→c chord
            [102, 10, 100], // c→a (extra oriented close)
            [103, 10, 100], // dangling; no triangle through 103 as a with distinct b,c
            [101, 10, 100], // b→a
        ];
        let mut img = BraidedGraphImage::from_external_triples(&ext);
        let path = std::env::temp_dir().join(format!(
            "braided_wedge_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        img.materialize_mapped(&path).expect("mmap");
        let img = Arc::new(img);

        // No-mmap prepare must fail closed (walker fallback contract).
        let heap = Arc::new(BraidedGraphImage::from_external_triples(&ext));
        assert!(
            PreparedWedgeImpl::prepare(Arc::clone(&heap), 10).is_none(),
            "wedge prepare requires mmap"
        );

        let mut prep =
            PreparedWedgeImpl::prepare(Arc::clone(&img), 10).expect("prepare wedge under mmap");

        crate::product_path::SPARQL_PATH.reset();
        let mut got: Vec<(u64, u64, u64)> = Vec::new();
        let n = prep
            .execute(&mut |ids| {
                assert_eq!(ids.len(), 3);
                got.push((ids[0], ids[1], ids[2]));
                Ok(())
            })
            .expect("execute");
        assert_eq!(n as usize, got.len());

        // Oracle: all oriented (a,b,c) with distinct endpoints and three edges under P.
        let p = 10u64;
        let edge = |s: u64, o: u64| ext.iter().any(|t| t[0] == s && t[1] == p && t[2] == o);
        let nodes = {
            let mut v: Vec<u64> = ext.iter().flat_map(|t| [t[0], t[2]]).collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        let mut want: Vec<(u64, u64, u64)> = Vec::new();
        for &a in &nodes {
            for &b in &nodes {
                if a == b || !edge(a, b) {
                    continue;
                }
                for &c in &nodes {
                    if c == a || c == b {
                        continue;
                    }
                    if edge(b, c) && edge(a, c) {
                        want.push((a, b, c));
                    }
                }
            }
        }
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, want, "prepared wedge vs nested SP oracle");
        assert!(
            !got.is_empty(),
            "fixture must contain at least one oriented triangle"
        );

        // SP-restricted D1 counters only fire when NOVA_RING_COUNTERS=1.
        // Correctness is asserted above; counters are an optional path probe.
        if crate::product_path::ring_counters_log_enabled() {
            let snap = crate::product_path::SPARQL_PATH.snapshot();
            assert!(
                snap.d1_open_calls >= 1 || snap.d1_left_range_reuse >= 1,
                "wedge execute should exercise SP-restricted D1 (got open={} left_reuse={})",
                snap.d1_open_calls,
                snap.d1_left_range_reuse
            );
        }

        let _ = std::fs::remove_file(&path);
    }
}
