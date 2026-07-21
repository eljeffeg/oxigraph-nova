//! LOUDS prepared physical operators for recognized BGP shapes.
//!
//! Ring's product kernels use braided wavelet images + dense adjacency. LOUDS
//! has six-order tries and `join_scan` only — these operators hold a
//! [`GraphRingHandle`] (Arc-backed snapshot) and walk with nested join_scan so
//! execute does not re-acquire the store mutex per hop.
//!
//! Covers the same [`PhysicalShape`] variants as Ring:
//! - TwoHop / SpExpansion / KChain / Star: full nested bodies
//! - Wedge: nested a→b + object ∩ via join_scan (no braided D1 yet)

use crate::ring::GraphRingHandle;
use oxigraph_nova_core::{
    PhysicalShape, PreparedKChain, PreparedPhysicalOperator, PreparedSpExpansion, PreparedStar,
    PreparedTwoHop, PreparedWedge, TrieIterator,
};

/// Snapshot of one graph's LOUDS ring used by prepared shape ops.
#[derive(Clone)]
struct LoudsShapeCtx {
    ring: GraphRingHandle,
}

impl LoudsShapeCtx {
    #[inline]
    fn join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator> {
        self.ring.join_scan(s, p, o, target_field)
    }
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
        let mut rows = 0u64;
        let mut a_scan = self.ctx.join_scan(None, Some(self.p1), None, 0);
        while !a_scan.at_end() {
            let a = a_scan.key();
            let mut b_scan = self.ctx.join_scan(Some(a), Some(self.p1), None, 2);
            while !b_scan.at_end() {
                let b = b_scan.key();
                let mut c_scan = self.ctx.join_scan(Some(b), Some(self.p2), None, 2);
                while !c_scan.at_end() {
                    let c = c_scan.key();
                    emit(&[a, b, c])?;
                    rows += 1;
                    c_scan.advance();
                }
                b_scan.advance();
            }
            a_scan.advance();
        }
        Ok(rows)
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
        let mut rows = 0u64;
        // Outer: subjects of (P_filter, O_filter).
        let mut s_scan = self
            .ctx
            .join_scan(None, Some(self.p_filter), Some(self.o_filter), 0);
        while !s_scan.at_end() {
            let s = s_scan.key();
            let mut o_scan = self.ctx.join_scan(Some(s), Some(self.p_expand), None, 2);
            while !o_scan.at_end() {
                let o = o_scan.key();
                emit(&[s, o])?;
                rows += 1;
                o_scan.advance();
            }
            s_scan.advance();
        }
        Ok(rows)
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
        let mut rows = 0u64;
        let p = self.predicate;
        let mut a_scan = self.ctx.join_scan(None, Some(p), None, 0);
        while !a_scan.at_end() {
            let a = a_scan.key();
            let mut b_scan = self.ctx.join_scan(Some(a), Some(p), None, 2);
            while !b_scan.at_end() {
                let b = b_scan.key();
                // Objects of b under P (for ∩).
                let mut b_objs: Vec<u64> = Vec::new();
                {
                    let mut bo = self.ctx.join_scan(Some(b), Some(p), None, 2);
                    while !bo.at_end() {
                        b_objs.push(bo.key());
                        bo.advance();
                    }
                }
                b_objs.sort_unstable();
                // Objects of a under P, keep those also under b.
                let mut c_scan = self.ctx.join_scan(Some(a), Some(p), None, 2);
                while !c_scan.at_end() {
                    let c = c_scan.key();
                    if c != a && c != b && b_objs.binary_search(&c).is_ok() {
                        emit(&[a, b, c])?;
                        rows += 1;
                    }
                    c_scan.advance();
                }
                b_scan.advance();
            }
            a_scan.advance();
        }
        Ok(rows)
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
        let mut rows = 0u64;
        let mut a_scan = self.ctx.join_scan(None, Some(self.p1), None, 0);
        while !a_scan.at_end() {
            let a = a_scan.key();
            let mut b_scan = self.ctx.join_scan(Some(a), Some(self.p1), None, 2);
            while !b_scan.at_end() {
                let b = b_scan.key();
                let mut c_scan = self.ctx.join_scan(Some(b), Some(self.p2), None, 2);
                while !c_scan.at_end() {
                    let c = c_scan.key();
                    let mut d_scan = self.ctx.join_scan(Some(c), Some(self.p3), None, 2);
                    while !d_scan.at_end() {
                        let d = d_scan.key();
                        emit(&[a, b, c, d])?;
                        rows += 1;
                        d_scan.advance();
                    }
                    c_scan.advance();
                }
                b_scan.advance();
            }
            a_scan.advance();
        }
        Ok(rows)
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
        let mut rows = 0u64;
        let mut s_scan = self.ctx.join_scan(None, Some(self.p1), None, 0);
        while !s_scan.at_end() {
            let s = s_scan.key();
            let mut o1s: Vec<u64> = Vec::new();
            {
                let mut sc = self.ctx.join_scan(Some(s), Some(self.p1), None, 2);
                while !sc.at_end() {
                    o1s.push(sc.key());
                    sc.advance();
                }
            }
            if o1s.is_empty() {
                s_scan.advance();
                continue;
            }
            let mut o2s: Vec<u64> = Vec::new();
            {
                let mut sc = self.ctx.join_scan(Some(s), Some(self.p2), None, 2);
                while !sc.at_end() {
                    o2s.push(sc.key());
                    sc.advance();
                }
            }
            if o2s.is_empty() {
                s_scan.advance();
                continue;
            }
            let mut o3s: Vec<u64> = Vec::new();
            {
                let mut sc = self.ctx.join_scan(Some(s), Some(self.p3), None, 2);
                while !sc.at_end() {
                    o3s.push(sc.key());
                    sc.advance();
                }
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
