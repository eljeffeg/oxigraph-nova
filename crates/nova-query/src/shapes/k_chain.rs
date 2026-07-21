//! k-chain shape (k = 3): `?a P1 ?b . ?b P2 ?c . ?c P3 ?d`
//!
//! Generalizes [`super::two_hop::TwoHopPlan`] (path length 2) to path length 3
//! over four distinct join vars. Longer chains (k > 3) stay on the roadmap
//! until the recognizer grows a variable-arity plan.
//!
//! Prepared physical bodies are optional: the walker always has a fixed-order
//! nested `join_scan` fallback. Engines may return `None` from
//! Engines prepare via `lftj_prepare_shape` for [`PhysicalShape::KChain`] and
//! emit four TermIds through the multi-var slice emit API.

use super::{ShapeId, ShapePlan, ShapeRecognizer};
use crate::lftj::{field_as_const, field_as_join, PatternSpec};

/// Bound plan for the 3-hop chain shape (k = 3).
///
/// Orientation: edges `(a,b)` under `p1`, `(b,c)` under `p2`, `(c,d)` under `p3`.
#[derive(Debug, Clone)]
pub struct KChainPlan {
    pub p1: u64,
    pub p2: u64,
    pub p3: u64,
    pub a: usize,
    pub b: usize,
    pub c: usize,
    pub d: usize,
}

impl KChainPlan {
    /// Constants-only view for [`LftjSource::lftj_prepare_shape`](oxigraph_nova_core::LftjSource::lftj_prepare_shape).
    #[inline]
    pub fn to_physical(&self) -> oxigraph_nova_core::PhysicalShape {
        oxigraph_nova_core::PhysicalShape::KChain {
            p1: self.p1,
            p2: self.p2,
            p3: self.p3,
        }
    }
}

/// Catalog entry for the 3-hop chain.
pub(super) struct KChainRecognizer;

impl ShapeRecognizer for KChainRecognizer {
    fn id(&self) -> ShapeId {
        ShapeId::KChain
    }

    fn recognize(&self, specs: &[PatternSpec], n_vars: usize) -> Option<ShapePlan> {
        try_recognize_k_chain(specs, n_vars).map(ShapePlan::KChain)
    }
}

/// Try to recognize `?a P1 ?b . ?b P2 ?c . ?c P3 ?d` (any pattern order).
///
/// Requires exactly 3 patterns, exactly 4 join vars, every pattern
/// `JoinVar Const(P) JoinVar`, no self-loops, and a labeling of the four vars
/// such that directed edges `(a,b)`, `(b,c)`, `(c,d)` are all present with
/// pairwise-distinct endpoints.
fn try_recognize_k_chain(specs: &[PatternSpec], n_vars: usize) -> Option<KChainPlan> {
    if specs.len() != 3 || n_vars != 4 {
        return None;
    }

    let mut edges: Vec<(usize, usize, u64)> = Vec::with_capacity(3);
    for sp in specs {
        let s = field_as_join(&sp.s)?;
        let p = field_as_const(&sp.p)?;
        let o = field_as_join(&sp.o)?;
        if s == o || s >= 4 || o >= 4 {
            return None;
        }
        edges.push((s, o, p));
    }

    // Distinct directed edges (duplicate hop is not a 3-chain).
    if (edges[0].0 == edges[1].0 && edges[0].1 == edges[1].1)
        || (edges[0].0 == edges[2].0 && edges[0].1 == edges[2].1)
        || (edges[1].0 == edges[2].0 && edges[1].1 == edges[2].1)
    {
        return None;
    }

    // Try every ordering of the three edges as hop1 → hop2 → hop3.
    const PERMS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for [i, j, k] in PERMS {
        let (a, b, p1) = edges[i];
        let (b2, c, p2) = edges[j];
        let (c2, d, p3) = edges[k];
        if b != b2 || c != c2 {
            continue;
        }
        // Four distinct endpoints.
        if a == b || a == c || a == d || b == c || b == d || c == d {
            continue;
        }
        // All four join-var indices must appear.
        let mut seen = [false; 4];
        seen[a] = true;
        seen[b] = true;
        seen[c] = true;
        seen[d] = true;
        if !seen.iter().all(|&x| x) {
            continue;
        }
        return Some(KChainPlan {
            p1,
            p2,
            p3,
            a,
            b,
            c,
            d,
        });
    }
    None
}
