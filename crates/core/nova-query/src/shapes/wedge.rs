//! Fixed-P undirected triangle ( wedge):
//! `?a P ?b . ?b P ?c . ?a P ?c`
//!
//! Three directed edges under one predicate that cover the three pairs among
//! distinct join vars `{a,b,c}` in the oriented form a→b, b→c, a→c (pattern
//! order and var labels may vary).

use super::{ShapeId, ShapePlan, ShapeRecognizer};
use crate::lftj::{PatternSpec, field_as_const, field_as_join};

/// Bound plan for the fixed-P triangle wedge.
///
/// Orientation: edges `(a,b)`, `(b,c)`, `(a,c)` under [`predicate`](Self::predicate).
/// The walker enumerates `(a,b)` then closes `c` via multi-subject object ∩.
#[derive(Debug, Clone)]
pub struct WedgePlan {
    pub predicate: u64,
    pub a: usize,
    pub b: usize,
    pub c: usize,
}

impl WedgePlan {
    /// Constants-only view for [`LftjSource::lftj_prepare_shape`](oxigraph_nova_core::LftjSource::lftj_prepare_shape).
    #[inline]
    pub fn to_physical(&self) -> oxigraph_nova_core::PhysicalShape {
        oxigraph_nova_core::PhysicalShape::Wedge {
            predicate: self.predicate,
        }
    }
}

/// Catalog entry for the fixed-P triangle wedge.
pub(super) struct WedgeRecognizer;

impl ShapeRecognizer for WedgeRecognizer {
    fn id(&self) -> ShapeId {
        ShapeId::Wedge
    }

    fn recognize(&self, specs: &[PatternSpec], n_vars: usize) -> Option<ShapePlan> {
        try_recognize_wedge(specs, n_vars).map(ShapePlan::Wedge)
    }
}

/// Try to recognize `?a P ?b . ?b P ?c . ?a P ?c` (any pattern order / labeling).
///
/// Requires exactly 3 patterns, exactly 3 join vars, every pattern
/// `JoinVar Const(P) JoinVar` with the **same** P, no self-loops, and a
/// labeling of the three vars such that directed edges `(a,b)`, `(b,c)`,
/// `(a,c)` are all present.
fn try_recognize_wedge(specs: &[PatternSpec], n_vars: usize) -> Option<WedgePlan> {
    if specs.len() != 3 || n_vars != 3 {
        return None;
    }

    let mut edges: Vec<(usize, usize)> = Vec::with_capacity(3);
    let mut pred: Option<u64> = None;
    for sp in specs {
        let s = field_as_join(&sp.s)?;
        let p = field_as_const(&sp.p)?;
        let o = field_as_join(&sp.o)?;
        if s == o || s >= 3 || o >= 3 {
            return None;
        }
        match pred {
            None => pred = Some(p),
            Some(prev) if prev != p => return None,
            Some(_) => {}
        }
        edges.push((s, o));
    }
    let predicate = pred?;

    // Distinct directed edges (duplicate pattern is not a triangle).
    if edges[0] == edges[1] || edges[0] == edges[2] || edges[1] == edges[2] {
        return None;
    }

    let has = |x: usize, y: usize| edges.iter().any(|&(s, o)| s == x && o == y);

    // Find orientation a→b, b→c, a→c over a permutation of {0,1,2}.
    const PERMS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for [a, b, c] in PERMS {
        if has(a, b) && has(b, c) && has(a, c) {
            return Some(WedgePlan { predicate, a, b, c });
        }
    }
    None
}
