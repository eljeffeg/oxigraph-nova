//! Two-hop chain shape: `?a P1 ?b . ?b P2 ?c`
//!
//! K9 path_2hop — directed chain of two edges over three distinct join vars.
//! Wedges (`?a P1 ?b . ?a P2 ?c`) are rejected.

use super::{ShapeId, ShapePlan, ShapeRecognizer};
use crate::lftj::{PatternSpec, field_as_const, field_as_join};

/// Bound plan for the two-hop chain shape.
#[derive(Debug, Clone)]
pub struct TwoHopPlan {
    pub p1: u64,
    pub p2: u64,
    pub a: usize,
    pub b: usize,
    pub c: usize,
}

impl TwoHopPlan {
    /// Constants-only view for [`LftjSource::lftj_prepare_shape`](oxigraph_nova_core::LftjSource::lftj_prepare_shape).
    #[inline]
    pub fn to_physical(&self) -> oxigraph_nova_core::PhysicalShape {
        oxigraph_nova_core::PhysicalShape::TwoHop {
            p1: self.p1,
            p2: self.p2,
        }
    }
}

/// Catalog entry for two-hop.
pub(super) struct TwoHopRecognizer;

impl ShapeRecognizer for TwoHopRecognizer {
    fn id(&self) -> ShapeId {
        ShapeId::TwoHop
    }

    fn recognize(&self, specs: &[PatternSpec], n_vars: usize) -> Option<ShapePlan> {
        try_recognize_two_hop(specs, n_vars).map(ShapePlan::TwoHop)
    }
}

/// Try to recognize the two-hop chain BGP shape on classified specs.
///
/// Requires exactly 2 patterns, exactly 3 join vars, both patterns of the form
/// `JoinVar(s) Const(P) JoinVar(o)`, directed edges forming a chain a→b→c
/// (not a two-edge wedge or self-loop). P1 and P2 may be the same or different.
fn try_recognize_two_hop(specs: &[PatternSpec], n_vars: usize) -> Option<TwoHopPlan> {
    if specs.len() != 2 || n_vars != 3 {
        return None;
    }
    let mut edges: Vec<(usize, usize, u64)> = Vec::with_capacity(2);
    for sp in specs {
        let s = field_as_join(&sp.s)?;
        let p = field_as_const(&sp.p)?;
        let o = field_as_join(&sp.o)?;
        if s == o || s >= 3 || o >= 3 {
            return None;
        }
        edges.push((s, o, p));
    }
    // Distinct directed edges.
    if edges[0].0 == edges[1].0 && edges[0].1 == edges[1].1 {
        return None;
    }

    // Find chain a→b→c: edge1 head = edge2 tail, three distinct vars.
    for i in 0..2 {
        let (a, b, p1) = edges[i];
        let (b2, c, p2) = edges[1 - i];
        if b != b2 {
            continue;
        }
        if a == b || b == c || a == c {
            continue;
        }
        // All three join-var indices must appear.
        let mut seen = [false; 3];
        seen[a] = true;
        seen[b] = true;
        seen[c] = true;
        if !seen.iter().all(|&x| x) {
            continue;
        }
        return Some(TwoHopPlan { p1, p2, a, b, c });
    }
    None
}
