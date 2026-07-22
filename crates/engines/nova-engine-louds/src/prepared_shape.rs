//! LOUDS prepared physical operators for recognized BGP shapes.
//!
//! Ring's product kernels use braided wavelet images + dense adjacency. LOUDS
//! has six-order tries — these operators hold a [`GraphRingHandle`] (Arc-backed
//! snapshot) and walk with **concrete** (non-boxed) `CltjTrieIter` descent so
//! execute does not re-acquire the store mutex per hop and pays no per-hop
//! `Box<dyn TrieIterator>` / vtable cost.
//!
//! Covers the same [`PhysicalShape`] variants as Ring:
//! - TwoHop / SpExpansion / KChain / Star: full nested bodies
//! - Wedge: nested a→b + sorted merge-intersect of object lists (no braided D1)
//! - DirectedTriangle: a→b→c cycle closed by seek on (c,P3,a)

use crate::cltj::VocabRepr;
use crate::louds::{BorrowedLouds, LoudsNav};
use crate::ring::{GraphRing, GraphRingHandle};
use oxigraph_nova_core::{
    PhysicalShape, PreparedDirectedTriangle, PreparedKChain, PreparedPhysicalOperator,
    PreparedSpExpansion, PreparedStar, PreparedTwoHop, PreparedWedge,
};

/// Snapshot of one graph's LOUDS ring used by prepared shape ops.
#[derive(Clone)]
struct LoudsShapeCtx {
    ring: GraphRingHandle,
}

// ── Concrete monomorphic walk helpers ─────────────────────────────────────────
//
// Each body is generic over the LOUDS substrate so Owned and Mapped handles
// share one implementation. No `Box`/`dyn` in the inner loops.

/// Sorted merge-intersect of two already-sorted unique key streams.
///
/// Emits keys present in both `left` and `right` into `out` (cleared first).
/// Both inputs must be strictly increasing.
#[inline]
fn merge_intersect_sorted(left: &[u64], right: &[u64], out: &mut Vec<u64>) {
    out.clear();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < left.len() && j < right.len() {
        let a = left[i];
        let b = right[j];
        match a.cmp(&b) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a);
                i += 1;
                j += 1;
            }
        }
    }
}

#[inline]
fn two_hop_body<Louds, V>(
    ring: &GraphRing<Louds, V>,
    p1: u64,
    p2: u64,
    emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>,
) -> Result<u64, ()>
where
    Louds: LoudsNav + Send + Sync + 'static,
    V: AsRef<[u64]> + Send + Sync + 'static,
{
    let mut rows = 0u64;
    // Outer: subjects of P1 (PSO depth-0 under bound P).
    let mut a_scan = match ring.join_scan_concrete(None, Some(p1), None, 0) {
        Some(it) => it,
        None => return Ok(0),
    };
    while !a_scan.at_end_c() {
        let a = a_scan.key_c();
        // Objects of (a, P1).
        if let Some(mut b_scan) = ring.join_scan_concrete(Some(a), Some(p1), None, 2) {
            while !b_scan.at_end_c() {
                let b = b_scan.key_c();
                // Objects of (b, P2).
                if let Some(mut c_scan) = ring.join_scan_concrete(Some(b), Some(p2), None, 2) {
                    while !c_scan.at_end_c() {
                        let c = c_scan.key_c();
                        emit(&[a, b, c])?;
                        rows += 1;
                        c_scan.advance_c();
                    }
                }
                b_scan.advance_c();
            }
        }
        a_scan.advance_c();
    }
    Ok(rows)
}

#[inline]
fn sp_expansion_body<Louds, V>(
    ring: &GraphRing<Louds, V>,
    p_filter: u64,
    o_filter: u64,
    p_expand: u64,
    emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>,
) -> Result<u64, ()>
where
    Louds: LoudsNav + Send + Sync + 'static,
    V: AsRef<[u64]> + Send + Sync + 'static,
{
    let mut rows = 0u64;
    let mut s_scan = match ring.join_scan_concrete(None, Some(p_filter), Some(o_filter), 0) {
        Some(it) => it,
        None => return Ok(0),
    };
    while !s_scan.at_end_c() {
        let s = s_scan.key_c();
        if let Some(mut o_scan) = ring.join_scan_concrete(Some(s), Some(p_expand), None, 2) {
            while !o_scan.at_end_c() {
                let o = o_scan.key_c();
                emit(&[s, o])?;
                rows += 1;
                o_scan.advance_c();
            }
        }
        s_scan.advance_c();
    }
    Ok(rows)
}

#[inline]
fn wedge_body<Louds, V>(
    ring: &GraphRing<Louds, V>,
    predicate: u64,
    emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>,
) -> Result<u64, ()>
where
    Louds: LoudsNav + Send + Sync + 'static,
    V: AsRef<[u64]> + Send + Sync + 'static,
{
    let mut rows = 0u64;
    let p = predicate;
    // Reused buffers: a_objs hoisted per-a; b_objs + common rebuilt per-b.
    let mut a_objs: Vec<u64> = Vec::new();
    let mut b_objs: Vec<u64> = Vec::new();
    let mut common: Vec<u64> = Vec::new();

    let mut a_scan = match ring.join_scan_concrete(None, Some(p), None, 0) {
        Some(it) => it,
        None => return Ok(0),
    };
    while !a_scan.at_end_c() {
        let a = a_scan.key_c();

        // Materialize a's object list once — invariant across all b under a.
        // Trie keys are already sorted, so no extra sort needed.
        a_objs.clear();
        if let Some(mut ao) = ring.join_scan_concrete(Some(a), Some(p), None, 2) {
            ao.collect_keys_into(&mut a_objs);
        }
        if a_objs.is_empty() {
            a_scan.advance_c();
            continue;
        }

        if let Some(mut b_scan) = ring.join_scan_concrete(Some(a), Some(p), None, 2) {
            while !b_scan.at_end_c() {
                let b = b_scan.key_c();
                b_objs.clear();
                if let Some(mut bo) = ring.join_scan_concrete(Some(b), Some(p), None, 2) {
                    bo.collect_keys_into(&mut b_objs);
                }
                // Sorted merge-intersect a_objs ∩ b_objs (both trie-sorted).
                merge_intersect_sorted(&a_objs, &b_objs, &mut common);
                for &c in &common {
                    if c != a && c != b {
                        emit(&[a, b, c])?;
                        rows += 1;
                    }
                }
                b_scan.advance_c();
            }
        }
        a_scan.advance_c();
    }
    Ok(rows)
}

#[inline]
fn k_chain_body<Louds, V>(
    ring: &GraphRing<Louds, V>,
    p1: u64,
    p2: u64,
    p3: u64,
    emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>,
) -> Result<u64, ()>
where
    Louds: LoudsNav + Send + Sync + 'static,
    V: AsRef<[u64]> + Send + Sync + 'static,
{
    let mut rows = 0u64;
    let mut a_scan = match ring.join_scan_concrete(None, Some(p1), None, 0) {
        Some(it) => it,
        None => return Ok(0),
    };
    while !a_scan.at_end_c() {
        let a = a_scan.key_c();
        if let Some(mut b_scan) = ring.join_scan_concrete(Some(a), Some(p1), None, 2) {
            while !b_scan.at_end_c() {
                let b = b_scan.key_c();
                if let Some(mut c_scan) = ring.join_scan_concrete(Some(b), Some(p2), None, 2) {
                    while !c_scan.at_end_c() {
                        let c = c_scan.key_c();
                        if let Some(mut d_scan) =
                            ring.join_scan_concrete(Some(c), Some(p3), None, 2)
                        {
                            while !d_scan.at_end_c() {
                                let d = d_scan.key_c();
                                emit(&[a, b, c, d])?;
                                rows += 1;
                                d_scan.advance_c();
                            }
                        }
                        c_scan.advance_c();
                    }
                }
                b_scan.advance_c();
            }
        }
        a_scan.advance_c();
    }
    Ok(rows)
}

#[inline]
fn star_body<Louds, V>(
    ring: &GraphRing<Louds, V>,
    p1: u64,
    p2: u64,
    p3: u64,
    emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>,
) -> Result<u64, ()>
where
    Louds: LoudsNav + Send + Sync + 'static,
    V: AsRef<[u64]> + Send + Sync + 'static,
{
    let mut rows = 0u64;
    let mut o1s: Vec<u64> = Vec::new();
    let mut o2s: Vec<u64> = Vec::new();
    let mut o3s: Vec<u64> = Vec::new();

    let mut s_scan = match ring.join_scan_concrete(None, Some(p1), None, 0) {
        Some(it) => it,
        None => return Ok(0),
    };
    while !s_scan.at_end_c() {
        let s = s_scan.key_c();
        o1s.clear();
        if let Some(mut sc) = ring.join_scan_concrete(Some(s), Some(p1), None, 2) {
            sc.collect_keys_into(&mut o1s);
        }
        if o1s.is_empty() {
            s_scan.advance_c();
            continue;
        }
        o2s.clear();
        if let Some(mut sc) = ring.join_scan_concrete(Some(s), Some(p2), None, 2) {
            sc.collect_keys_into(&mut o2s);
        }
        if o2s.is_empty() {
            s_scan.advance_c();
            continue;
        }
        o3s.clear();
        if let Some(mut sc) = ring.join_scan_concrete(Some(s), Some(p3), None, 2) {
            sc.collect_keys_into(&mut o3s);
        }
        if o3s.is_empty() {
            s_scan.advance_c();
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
        s_scan.advance_c();
    }
    Ok(rows)
}

#[inline]
fn directed_triangle_body<Louds, V>(
    ring: &GraphRing<Louds, V>,
    p1: u64,
    p2: u64,
    p3: u64,
    emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>,
) -> Result<u64, ()>
where
    Louds: LoudsNav + Send + Sync + 'static,
    V: AsRef<[u64]> + Send + Sync + 'static,
{
    let mut rows = 0u64;
    // a under P1 → b under (a,P1) → c under (b,P2) → probe (c,P3,a) via seek.
    let mut a_scan = match ring.join_scan_concrete(None, Some(p1), None, 0) {
        Some(it) => it,
        None => return Ok(0),
    };
    while !a_scan.at_end_c() {
        let a = a_scan.key_c();
        if let Some(mut b_scan) = ring.join_scan_concrete(Some(a), Some(p1), None, 2) {
            while !b_scan.at_end_c() {
                let b = b_scan.key_c();
                if b != a
                    && let Some(mut c_scan) = ring.join_scan_concrete(Some(b), Some(p2), None, 2)
                {
                    while !c_scan.at_end_c() {
                        let c = c_scan.key_c();
                        if c != a && c != b {
                            // Existence probe: objects of (c, P3) contain a?
                            if let Some(mut close) =
                                ring.join_scan_concrete(Some(c), Some(p3), None, 2)
                            {
                                close.seek_c(a);
                                if !close.at_end_c() && close.key_c() == a {
                                    emit(&[a, b, c])?;
                                    rows += 1;
                                }
                            }
                        }
                        c_scan.advance_c();
                    }
                }
                b_scan.advance_c();
            }
        }
        a_scan.advance_c();
    }
    Ok(rows)
}

/// Dispatch a monomorphic body across Owned / Mapped ring handles.
macro_rules! dispatch_ring {
    ($ring:expr, $body:expr) => {
        match $ring {
            GraphRingHandle::Owned(r) => $body(r.as_ref()),
            GraphRingHandle::Mapped(m) => $body(m.ring()),
        }
    };
}

// ── Two-hop ───────────────────────────────────────────────────────────────────

pub struct LoudsPreparedTwoHop {
    ctx: LoudsShapeCtx,
    p1: u64,
    p2: u64,
}

impl LoudsPreparedTwoHop {
    pub fn prepare(ring: GraphRingHandle, p1: u64, p2: u64) -> Self {
        Self {
            ctx: LoudsShapeCtx { ring },
            p1,
            p2,
        }
    }
}

impl PreparedTwoHop for LoudsPreparedTwoHop {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        let p1 = self.p1;
        let p2 = self.p2;
        dispatch_ring!(&self.ctx.ring, |r| two_hop_body(r, p1, p2, emit))
    }
}

// ── SP-expansion / 2join ──────────────────────────────────────────────────────

pub struct LoudsPreparedSpExpansion {
    ctx: LoudsShapeCtx,
    p_filter: u64,
    o_filter: u64,
    p_expand: u64,
}

impl LoudsPreparedSpExpansion {
    pub fn prepare(ring: GraphRingHandle, p_filter: u64, o_filter: u64, p_expand: u64) -> Self {
        Self {
            ctx: LoudsShapeCtx { ring },
            p_filter,
            o_filter,
            p_expand,
        }
    }
}

impl PreparedSpExpansion for LoudsPreparedSpExpansion {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        let p_filter = self.p_filter;
        let o_filter = self.o_filter;
        let p_expand = self.p_expand;
        dispatch_ring!(&self.ctx.ring, |r| {
            sp_expansion_body(r, p_filter, o_filter, p_expand, emit)
        })
    }
}

// ── Wedge / fixed-P triangle ──────────────────────────────────────────────────

pub struct LoudsPreparedWedge {
    ctx: LoudsShapeCtx,
    predicate: u64,
}

impl LoudsPreparedWedge {
    pub fn prepare(ring: GraphRingHandle, predicate: u64) -> Self {
        Self {
            ctx: LoudsShapeCtx { ring },
            predicate,
        }
    }
}

impl PreparedWedge for LoudsPreparedWedge {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        let predicate = self.predicate;
        dispatch_ring!(&self.ctx.ring, |r| wedge_body(r, predicate, emit))
    }
}

// ── Directed triangle (3-cycle) ───────────────────────────────────────────────

pub struct LoudsPreparedDirectedTriangle {
    ctx: LoudsShapeCtx,
    p1: u64,
    p2: u64,
    p3: u64,
}

impl LoudsPreparedDirectedTriangle {
    pub fn prepare(ring: GraphRingHandle, p1: u64, p2: u64, p3: u64) -> Self {
        Self {
            ctx: LoudsShapeCtx { ring },
            p1,
            p2,
            p3,
        }
    }
}

impl PreparedDirectedTriangle for LoudsPreparedDirectedTriangle {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        let p1 = self.p1;
        let p2 = self.p2;
        let p3 = self.p3;
        dispatch_ring!(&self.ctx.ring, |r| directed_triangle_body(
            r, p1, p2, p3, emit
        ))
    }
}

// ── K-chain (k=3) ─────────────────────────────────────────────────────────────

/// Nested join_scan body for `?a P1 ?b . ?b P2 ?c . ?c P3 ?d`.
pub struct LoudsPreparedKChain {
    ctx: LoudsShapeCtx,
    p1: u64,
    p2: u64,
    p3: u64,
}

impl LoudsPreparedKChain {
    pub fn prepare(ring: GraphRingHandle, p1: u64, p2: u64, p3: u64) -> Self {
        Self {
            ctx: LoudsShapeCtx { ring },
            p1,
            p2,
            p3,
        }
    }
}

impl PreparedKChain for LoudsPreparedKChain {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        let p1 = self.p1;
        let p2 = self.p2;
        let p3 = self.p3;
        dispatch_ring!(&self.ctx.ring, |r| k_chain_body(r, p1, p2, p3, emit))
    }
}

// ── Subject-star (k=3) ────────────────────────────────────────────────────────

/// Nested join_scan body for `?s P1 ?o1 . ?s P2 ?o2 . ?s P3 ?o3`.
///
/// Outer subjects under P1; Cartesian product of objects under each arm.
/// Subjects missing any arm are skipped.
pub struct LoudsPreparedStar {
    ctx: LoudsShapeCtx,
    p1: u64,
    p2: u64,
    p3: u64,
}

impl LoudsPreparedStar {
    pub fn prepare(ring: GraphRingHandle, p1: u64, p2: u64, p3: u64) -> Self {
        Self {
            ctx: LoudsShapeCtx { ring },
            p1,
            p2,
            p3,
        }
    }
}

impl PreparedStar for LoudsPreparedStar {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        let p1 = self.p1;
        let p2 = self.p2;
        let p3 = self.p3;
        dispatch_ring!(&self.ctx.ring, |r| star_body(r, p1, p2, p3, emit))
    }
}

/// Build a LOUDS prepared operator for `shape`, or `None` if the graph handle
/// is missing (caller keeps nested-scan fallback).
pub fn prepare_shape(
    ring: GraphRingHandle,
    shape: PhysicalShape,
) -> Option<Box<dyn PreparedPhysicalOperator>> {
    Some(match shape {
        PhysicalShape::TwoHop { p1, p2 } => Box::new(LoudsPreparedTwoHop::prepare(ring, p1, p2))
            as Box<dyn PreparedPhysicalOperator>,
        PhysicalShape::Wedge { predicate } => {
            Box::new(LoudsPreparedWedge::prepare(ring, predicate))
                as Box<dyn PreparedPhysicalOperator>
        }
        PhysicalShape::DirectedTriangle { p1, p2, p3 } => {
            Box::new(LoudsPreparedDirectedTriangle::prepare(ring, p1, p2, p3))
                as Box<dyn PreparedPhysicalOperator>
        }
        PhysicalShape::SpExpansion {
            p_filter,
            o_filter,
            p_expand,
        } => Box::new(LoudsPreparedSpExpansion::prepare(
            ring, p_filter, o_filter, p_expand,
        )) as Box<dyn PreparedPhysicalOperator>,
        PhysicalShape::KChain { p1, p2, p3 } => {
            Box::new(LoudsPreparedKChain::prepare(ring, p1, p2, p3))
                as Box<dyn PreparedPhysicalOperator>
        }
        PhysicalShape::Star { p1, p2, p3 } => {
            Box::new(LoudsPreparedStar::prepare(ring, p1, p2, p3))
                as Box<dyn PreparedPhysicalOperator>
        }
    })
}

// Silence unused-import warnings when mmap is off (BorrowedLouds/VocabRepr
// only appear in the Mapped arm of dispatch_ring, which is still type-checked).
#[allow(dead_code)]
fn _type_anchors() {
    let _: Option<&GraphRing<BorrowedLouds, VocabRepr>> = None;
}
