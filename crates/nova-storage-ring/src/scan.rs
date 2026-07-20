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
//! - **mapped RDI** (E5.11 B1/B2) for LastCol when `NOVARNG1` is open (W2)
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
use crate::product_path::SPARQL_PATH;
use crate::{Col, CyclicRing, RowRange};
use oxigraph_nova_core::{EmptyTrieIter, TrieIterator};
use std::sync::atomic::Ordering;
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
fn triple_at_spo(ring: &CyclicRing, i: u32) -> [u32; 3] {
    let o = ring.access(Col::O, i);
    let i_osp = ring.f(Col::O, i);
    let p = ring.access(Col::P, i_osp);
    let i_pos = ring.f(Col::P, i_osp);
    let s = ring.access(Col::S, i_pos);
    [s, p, o]
}

/// Recover (s,p,o) from a T_osp row index (lead = O).
#[inline]
fn triple_at_osp(ring: &CyclicRing, k: u32) -> [u32; 3] {
    let p = ring.access(Col::P, k);
    let i_pos = ring.f(Col::P, k);
    let s = ring.access(Col::S, i_pos);
    let i_spo = ring.f(Col::S, i_pos);
    let o = ring.access(Col::O, i_spo);
    [s, p, o]
}

/// Recover (s,p,o) from a T_pos row index (lead = P).
#[inline]
fn triple_at_pos(ring: &CyclicRing, j: u32) -> [u32; 3] {
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
    ring: &CyclicRing,
    r: RowRange,
    at: fn(&CyclicRing, u32) -> [u32; 3],
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
    ring: &CyclicRing,
    s: Option<u32>,
    p: Option<u32>,
    o: Option<u32>,
) -> Vec<[u32; 3]> {
    let n = ring.n();
    if n == 0 {
        return Vec::new();
    }
    let universe = ring.universe;
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
pub fn contains_dense(ring: &CyclicRing, s: u32, p: u32, o: u32) -> bool {
    if s >= ring.universe || p >= ring.universe || o >= ring.universe {
        return false;
    }
    let r = range_sp(ring, s, p);
    if r.is_empty() {
        return false;
    }
    ring.range_next_value(Col::O, r, o)
        .is_some_and(|v| v == o)
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

/// Count distinct middle values up to `budget` runs (O(min(distinct,budget)·log n)).
///
/// Returns `(count, exhausted)` where `exhausted == false` means the budget was
/// hit before the end of the range (caller should fall back to row-span heuristic).
///
/// Critical for VEO on BSBM **scan** `{ ?s P31 ?o }`: bound-P → target O is
/// `MiddleRuns` with ~50 distinct classes, while target S is LastCol with
/// ~50k subjects. Using `rows` for both made VEO bind S first → 50k SP opens.
fn count_middle_runs_budgeted(
    ring: &CyclicRing,
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
    let mid_fn: fn(&CyclicRing, u32) -> u32 = match mid {
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
fn middle_rows_heuristic(ring: &CyclicRing, range: RowRange, n_bound: u64) -> u64 {
    let rows = u64::from(range.len().max(1));
    let vocab = u64::from(ring.universe.max(1)) / (n_bound + 1);
    rows.min(vocab).max(1)
}

/// Cache key for MiddleRuns VEO estimates that do not depend on outer LFTJ
/// bindings (e.g. bound-P → target O). Adaptive VEO re-probes every depth with
/// the same (range, mid); without a cache, path_2hop walked up to
/// `VEO_MIDDLE_EXACT_RUN_BUDGET` runs × 50k outer subjects (Phase K0 hang).
///
/// Keyed by ring identity (n, universe) + range + middle kind. Process-wide;
/// pilot assumes one active compacted graph per process (RESULTS_MEM). Cap
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
            // Drop half when full (cheap; order irrelevant for pilot).
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
///   not re-walk the same middle on every outer binding (Phase K0).
///   `NOVA_RING_VEO_OLD_HEURISTIC=1` forces the old row-span path (A/B).
/// - **LastCol**: min(row-span, vocab heuristic) — LOUDS-style, no full RDI.
///
/// Planning time is accumulated in `SPARQL_PATH.veo_plan_ns` (not query exec).
pub fn estimate_join_count(
    ring: &CyclicRing,
    s: Option<u32>,
    p: Option<u32>,
    o: Option<u32>,
    target_field: usize,
) -> u64 {
    use crate::product_path::{
        SPARQL_PATH, VEO_MIDDLE_EXACT_RUN_BUDGET, ring_veo_old_heuristic,
    };
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
            let vocab = u64::from(ring.universe.max(1)) / (n_bound + 1);
            rows.min(vocab).max(1)
        }
        DenseScanKind::MiddleRuns { range, mid } => {
            let cache_key = VeoMiddleCacheKey {
                n: ring.n(),
                universe: ring.universe,
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
                    SPARQL_PATH
                        .veo_middle_exact
                        .fetch_add(1, Ordering::Relaxed);
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

// ── Mapped LastCol RDI streaming scan (W2 / E5.11 star kernel) ────────────────
//
// Design (research/BRAIDED_RING.md + notes/e5.11-sparql-product-wire.md W2 +
// benches e511_ring_perf_profile star_mmap):
//
//   Product star/scan kernel = mmap hot QWT + **stateful RDI**
//     (`MappedRangeDistinctIter` / `range_distinct_iter`) — same as G2.
//   Product successor primitive = **RNV** (`range_next_value`) — O(log σ).
//
// LFTJ contract (nova-query): monotonic `seek` / `advance` on a scan opened
// once per pattern×depth. So:
//   • `advance` → RDI next while still in RDI mode (star / sequential walk).
//   • `seek`    → RNV to locate successor (never reopen RDI from range start).
//                 Small gap: forward-skip live RDI to that symbol.
//                 Large gap / after RNV-only: stay on RNV (path_2hop leapfrog).
//   • Heap RNV-only remains the no-mmap fallback (`BraidedStreamingScan`).
//
// Forbidden regression: `resync_from` that rebuilds RDI at range.start and
// rescans every seek — O(|distinct|) per seek → path_2hop ~40× LOUDS.

/// How LastCol navigation is served after open / seek.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastColMode {
    /// Stateful mapped RDI (E5.11 star kernel).
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
}

impl TrieIterator for BraidedMappedLastColScan {
    #[inline]
    fn key(&self) -> u64 {
        let d = self.current.expect("key() on exhausted BraidedMappedLastColScan");
        self.external_key(d)
    }

    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        if let Some(cur) = self.current {
            if self.external_key(cur) >= target {
                return;
            }
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
                // E5.11 star kernel: stateful RDI next.
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

// ── D1 two-range object intersection scan (W4b / E5.11) ──────────────────────

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
        let d = self.current.expect("key() on exhausted BraidedD1ObjectScan");
        self.external_key(d)
    }

    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        if let Some(cur) = self.current {
            if self.external_key(cur) >= target {
                return;
            }
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

// ── D2 three-range object intersection scan (W4 / E5.11 triangle kernel) ─────

/// Streaming D2 common-object scan under three subject ranges.
///
/// Product triangle shape (RESULTS_MEM / E5.11 G3): distinct objects present
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
    fn open(
        img: Arc<BraidedGraphImage>,
        s0: u32,
        s1: u32,
        s2: u32,
    ) -> Option<Self> {
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
        let d = self.current.expect("key() on exhausted BraidedD2ObjectScan");
        self.external_key(d)
    }

    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        if let Some(cur) = self.current {
            if self.external_key(cur) >= target {
                return;
            }
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
        match kind {
            DenseScanKind::Empty => Box::new(EmptyTrieIter),
            // Small SP/PO/OS ranges (typical after binding S under bound P in
            // BSBM scan): mapped RDI open cost dominates a 1–few-symbol walk.
            // Heap RNV is O(log σ) per step and matches LOUDS open-per-row cost.
            // Threshold covers "one object per subject" (range len 1) through
            // modest fan-out; large star/scan lead ranges still take mapped RDI.
            DenseScanKind::LastCol { range, .. } if range.len() <= 16 => {
                SPARQL_PATH.path_heap_rnv.fetch_add(1, Ordering::Relaxed);
                Box::new(BraidedStreamingScan::new(img, kind))
            }
            DenseScanKind::LastCol { col, range } if img.has_mapped() => {
                // W2: mapped hot RDI (E5.11 star / large lead-range kernel).
                SPARQL_PATH
                    .path_mapped_rdi
                    .fetch_add(1, Ordering::Relaxed);
                match BraidedMappedLastColScan::open(Arc::clone(&img), col, range) {
                    Some(scan) => Box::new(scan),
                    None => {
                        SPARQL_PATH.path_heap_rnv.fetch_add(1, Ordering::Relaxed);
                        Box::new(BraidedStreamingScan::new(img, kind))
                    }
                }
            }
            DenseScanKind::LastCol { .. } => {
                SPARQL_PATH.path_heap_rnv.fetch_add(1, Ordering::Relaxed);
                Box::new(BraidedStreamingScan::new(img, kind))
            }

            DenseScanKind::MiddleRuns { .. } => {
                SPARQL_PATH
                    .path_middle_runs
                    .fetch_add(1, Ordering::Relaxed);
                Box::new(BraidedStreamingScan::new(img, kind))
            }
            DenseScanKind::Singleton(_) => {
                SPARQL_PATH
                    .path_singleton
                    .fetch_add(1, Ordering::Relaxed);
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
        SPARQL_PATH.d2_calls.fetch_add(1, Ordering::Relaxed);
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
                if !scan.at_end() {
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
    /// at the range layer (subject lead ranges on Col::O) — same as E5.11 G3
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
        SPARQL_PATH.d2_calls.fetch_add(1, Ordering::Relaxed);
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
        if !scan.at_end() {
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
        estimate_join_count(self.index().heap(), sd, pd, od, target_field)
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
        match_triples_dense(self.index().heap(), sd, pd, od)
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
        contains_dense(self.index().heap(), sd, pd, od)
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
    fn mapped_streaming_lastcol_matches_heap_oracle() {
        let t = sample();
        let ext: Vec<[u64; 3]> = t
            .iter()
            .map(|x| [u64::from(x[0]) + 50, u64::from(x[1]) + 5, u64::from(x[2]) + 100])
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
            assert_eq!(got, want, "mapped stream vs external s={s:?} p={p:?} o={o:?} tf={tf}");
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
        let mut it = BraidedGraphImage::multi_subject_object_intersect(
            Arc::clone(&img),
            &[0, 1],
        )
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
        let snap = crate::product_path::SPARQL_PATH.snapshot();
        assert!(snap.d2_calls >= 1, "W4b counter d2_calls must bump");
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
        assert_eq!(got, common, "D2 TrieIterator must match multi-scan intersection");
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

    fn oracle_match(triples: &[[u32; 3]], s: Option<u32>, p: Option<u32>, o: Option<u32>) -> Vec<[u32; 3]> {
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
        let ring = idx.heap();
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
            .map(|x| [u64::from(x[0]) + 10, u64::from(x[1]) + 20, u64::from(x[2]) + 30])
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
}
