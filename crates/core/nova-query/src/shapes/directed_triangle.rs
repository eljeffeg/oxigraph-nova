//! Directed 3-cycle shape:
//! `?a P1 ?b . ?b P2 ?c . ?c P3 ?a`
//!
//! Three directed edges forming a cycle over distinct join vars `{a,b,c}`.
//! Predicates may be equal or distinct. Distinct from the fixed-P chordal
//! [`super::WedgePlan`] (`a→b, b→c, a→c`).

use super::{ShapeId, ShapePlan, ShapeRecognizer};
use crate::lftj::{PatternSpec, field_as_const, field_as_join};

/// Bound plan for the directed 3-cycle.
///
/// Orientation: edges `(a,b)` under P1, `(b,c)` under P2, `(c,a)` under P3.
/// The walker enumerates `(a,b)` then `(b,c)`, and closes via existence of
/// `(c, P3, a)`.
#[derive(Debug, Clone)]
pub struct DirectedTrianglePlan {
    pub p1: u64,
    pub p2: u64,
    pub p3: u64,
    pub a: usize,
    pub b: usize,
    pub c: usize,
}

impl DirectedTrianglePlan {
    /// Constants-only view for [`LftjSource::lftj_prepare_shape`](oxigraph_nova_core::LftjSource::lftj_prepare_shape).
    #[inline]
    pub fn to_physical(&self) -> oxigraph_nova_core::PhysicalShape {
        oxigraph_nova_core::PhysicalShape::DirectedTriangle {
            p1: self.p1,
            p2: self.p2,
            p3: self.p3,
        }
    }
}

/// Catalog entry for the directed 3-cycle.
pub(super) struct DirectedTriangleRecognizer;

impl ShapeRecognizer for DirectedTriangleRecognizer {
    fn id(&self) -> ShapeId {
        ShapeId::DirectedTriangle
    }

    fn recognize(&self, specs: &[PatternSpec], n_vars: usize) -> Option<ShapePlan> {
        try_recognize_directed_triangle(specs, n_vars).map(ShapePlan::DirectedTriangle)
    }
}

/// Try to recognize `?a P1 ?b . ?b P2 ?c . ?c P3 ?a` (any pattern order / labeling).
///
/// Requires exactly 3 patterns, exactly 3 join vars, every pattern
/// `JoinVar Const(P) JoinVar`, no self-loops, three distinct directed edges,
/// and a labeling such that edges `(a,b)`, `(b,c)`, `(c,a)` are all present.
///
/// Does **not** match the chordal wedge `a→b, b→c, a→c` (no back-edge to a
/// from c) — that remains [`super::WedgePlan`].
fn try_recognize_directed_triangle(
    specs: &[PatternSpec],
    n_vars: usize,
) -> Option<DirectedTrianglePlan> {
    if specs.len() != 3 || n_vars != 3 {
        return None;
    }

    let mut edges: Vec<(usize, usize, u64)> = Vec::with_capacity(3);
    for sp in specs {
        let s = field_as_join(&sp.s)?;
        let p = field_as_const(&sp.p)?;
        let o = field_as_join(&sp.o)?;
        if s == o || s >= 3 || o >= 3 {
            return None;
        }
        edges.push((s, o, p));
    }

    // Distinct directed edges (duplicate pattern is not a cycle).
    if (edges[0].0 == edges[1].0 && edges[0].1 == edges[1].1)
        || (edges[0].0 == edges[2].0 && edges[0].1 == edges[2].1)
        || (edges[1].0 == edges[2].0 && edges[1].1 == edges[2].1)
    {
        return None;
    }

    let find = |x: usize, y: usize| -> Option<u64> {
        edges
            .iter()
            .find(|&&(s, o, _)| s == x && o == y)
            .map(|&(_, _, p)| p)
    };

    // Find orientation a→b→c→a over a permutation of {0,1,2}.
    const PERMS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for [a, b, c] in PERMS {
        if let (Some(p1), Some(p2), Some(p3)) = (find(a, b), find(b, c), find(c, a)) {
            return Some(DirectedTrianglePlan {
                p1,
                p2,
                p3,
                a,
                b,
                c,
            });
        }
    }
    None
}
