//! Subject-star shape (k = 3): `?s P1 ?o1 . ?s P2 ?o2 . ?s P3 ?o3`
//!
//! Generalizes [`super::sp_expansion::SpExpansionPlan`] (one filter + one open
//! arm) to three open object arms under distinct predicates on a shared
//! subject. Longer stars (k > 3) are not recognized yet; the plan would need
//! variable arity.
//!
//! Prepared physical bodies are optional: the walker always has a fixed-order
//! nested `join_scan` fallback. Engines prepare via `lftj_prepare_shape` for
//! [`PhysicalShape::Star`] and emit four TermIds (`[s, o1, o2, o3]`) through
//! the multi-var slice emit API.

use super::{ShapeId, ShapePlan, ShapeRecognizer};
use crate::lftj::{PatternSpec, field_as_const, field_as_join};

/// Bound plan for the 3-arm subject-star shape (k = 3).
///
/// Orientation: edges `(s,o1)` under `p1`, `(s,o2)` under `p2`, `(s,o3)` under
/// `p3`. Predicate order follows first-appearance in the BGP after recognition
/// (any pattern order is accepted; arms are labeled by distinct object vars).
#[derive(Debug, Clone)]
pub struct StarPlan {
    pub p1: u64,
    pub p2: u64,
    pub p3: u64,
    /// Shared subject join-var index.
    pub s: usize,
    pub o1: usize,
    pub o2: usize,
    pub o3: usize,
}

impl StarPlan {
    /// Constants-only view for [`LftjSource::lftj_prepare_shape`](oxigraph_nova_core::LftjSource::lftj_prepare_shape).
    #[inline]
    pub fn to_physical(&self) -> oxigraph_nova_core::PhysicalShape {
        oxigraph_nova_core::PhysicalShape::Star {
            p1: self.p1,
            p2: self.p2,
            p3: self.p3,
        }
    }
}

/// Catalog entry for the 3-arm subject star.
pub(super) struct StarRecognizer;

impl ShapeRecognizer for StarRecognizer {
    fn id(&self) -> ShapeId {
        ShapeId::Star
    }

    fn recognize(&self, specs: &[PatternSpec], n_vars: usize) -> Option<ShapePlan> {
        try_recognize_star(specs, n_vars).map(ShapePlan::Star)
    }
}

/// Try to recognize `?s P1 ?o1 . ?s P2 ?o2 . ?s P3 ?o3` (any pattern order).
///
/// Requires exactly 3 patterns, exactly 4 join vars, every pattern
/// `JoinVar(s) Const(P) JoinVar(oi)` with a single shared subject, three
/// distinct object vars, and three distinct predicates. Self-loops rejected.
fn try_recognize_star(specs: &[PatternSpec], n_vars: usize) -> Option<StarPlan> {
    if specs.len() != 3 || n_vars != 4 {
        return None;
    }

    let mut arms: Vec<(usize, u64, usize)> = Vec::with_capacity(3); // (s, p, o)
    for sp in specs {
        let s = field_as_join(&sp.s)?;
        let p = field_as_const(&sp.p)?;
        let o = field_as_join(&sp.o)?;
        if s == o || s >= 4 || o >= 4 {
            return None;
        }
        arms.push((s, p, o));
    }

    // Shared subject.
    let s = arms[0].0;
    if arms[1].0 != s || arms[2].0 != s {
        return None;
    }

    let o1 = arms[0].2;
    let o2 = arms[1].2;
    let o3 = arms[2].2;
    let p1 = arms[0].1;
    let p2 = arms[1].1;
    let p3 = arms[2].1;

    // Three distinct object vars (and none equal to s — already checked).
    if o1 == o2 || o1 == o3 || o2 == o3 {
        return None;
    }
    // Distinct predicates (duplicate P arms are not a clean 3-star).
    if p1 == p2 || p1 == p3 || p2 == p3 {
        return None;
    }

    // All four join-var indices must appear (s + three objects).
    let mut seen = [false; 4];
    seen[s] = true;
    seen[o1] = true;
    seen[o2] = true;
    seen[o3] = true;
    if !seen.iter().all(|&x| x) {
        return None;
    }

    Some(StarPlan {
        p1,
        p2,
        p3,
        s,
        o1,
        o2,
        o3,
    })
}
