//! SP-expansion shape: `?s P_filter O_const . ?s P_expand ?o`
//!
//! Subject-star 2join (RESULTS_MEM 2join shape). Not a 3-var two-hop chain.

use super::{ShapeId, ShapePlan, ShapeRecognizer};
use crate::lftj::{field_as_const, field_as_join, PatternSpec};

/// Bound plan for the SP-expansion shape.
#[derive(Debug, Clone)]
pub struct SpExpansionPlan {
    /// Bound predicate on the filter pattern (P31).
    pub p_filter: u64,
    /// Bound object on the filter pattern (class0).
    pub o_filter: u64,
    /// Bound predicate on the expansion pattern (P131).
    pub p_expand: u64,
    /// Join-var index of the shared subject (?s).
    pub s_idx: usize,
    /// Join-var index of the expansion object (?region).
    pub o_idx: usize,
}

impl SpExpansionPlan {
    /// Constants-only view for [`LftjSource::lftj_prepare_shape`](oxigraph_nova_core::LftjSource::lftj_prepare_shape).
    #[inline]
    pub fn to_physical(&self) -> oxigraph_nova_core::PhysicalShape {
        oxigraph_nova_core::PhysicalShape::SpExpansion {
            p_filter: self.p_filter,
            o_filter: self.o_filter,
            p_expand: self.p_expand,
        }
    }
}

/// Catalog entry for SP-expansion.
pub(super) struct SpExpansionRecognizer;

impl ShapeRecognizer for SpExpansionRecognizer {
    fn id(&self) -> ShapeId {
        ShapeId::SpExpansion
    }

    fn recognize(&self, specs: &[PatternSpec], n_vars: usize) -> Option<ShapePlan> {
        try_recognize_sp_expansion(specs, n_vars).map(ShapePlan::SpExpansion)
    }
}

/// Recognize `?s P1 O1 . ?s P2 ?o` (or pattern order swapped).
///
/// Requires exactly 2 patterns and 2 join vars.
fn try_recognize_sp_expansion(specs: &[PatternSpec], n_vars: usize) -> Option<SpExpansionPlan> {
    if specs.len() != 2 || n_vars != 2 {
        return None;
    }
    // Classify each pattern:
    //   filter:  JoinVar(s) Const(P) Const(O)
    //   expand:  JoinVar(s) Const(P) JoinVar(o)   with s != o
    let mut filter: Option<(usize, u64, u64)> = None; // (s_idx, p, o)
    let mut expand: Option<(usize, u64, usize)> = None; // (s_idx, p, o_idx)

    for sp in specs {
        let s = field_as_join(&sp.s)?;
        let p = field_as_const(&sp.p)?;
        match (field_as_join(&sp.o), field_as_const(&sp.o)) {
            (Some(o_jv), _) if o_jv != s => {
                // expand form
                if expand.is_some() {
                    return None;
                }
                expand = Some((s, p, o_jv));
            }
            (None, Some(o_c)) => {
                if filter.is_some() {
                    return None;
                }
                filter = Some((s, p, o_c));
            }
            _ => return None,
        }
    }
    let (fs, p_filter, o_filter) = filter?;
    let (es, p_expand, o_idx) = expand?;
    if fs != es {
        return None; // must share subject var
    }
    // Both join-var indices must appear (s and o).
    if fs >= 2 || o_idx >= 2 || fs == o_idx {
        return None;
    }
    Some(SpExpansionPlan {
        p_filter,
        o_filter,
        p_expand,
        s_idx: fs,
        o_idx,
    })
}
