//! BGP shape catalog — planner-side, engine-agnostic pattern recognition.
//!
//! Shape → engine strategy split:
//!
//! 1. **Shape** (this module) — pure structural predicate over [`PatternSpec`]
//! 2. **Physical strategy** — `eval_*_walk` in `lftj.rs`
//! 3. **Engine capability** — [`oxigraph_nova_core::LftjSource::lftj_prepare_shape`]
//!    (one entry point, match on [`oxigraph_nova_core::PhysicalShape`])
//!
//! Recognizers are tried in specificity order (most specific first). A miss
//! falls through to generic VEO LFTJ. Adding a shape means adding a recognizer
//! to the catalog — not editing an if-chain in the evaluator.
//!
//! Nesting (`ShapePlan` trees) is intentionally deferred until a real nested
//! case (e.g. `star_with_features`) needs it.
//!
//! ## Live catalog
//!
//! - [`ShapeId::SpExpansion`] / [`ShapeId::TwoHop`] / [`ShapeId::Wedge`] /
//!   [`ShapeId::DirectedTriangle`] / [`ShapeId::KChain`] (k = 3) /
//!   [`ShapeId::Star`] (k = 3)

//! ## Reserved shape ids (no recognizer / walk yet)
//!
//! See [`ShapeId`] variants marked *reserved*. When implementing one:
//! 1. Add `*Plan` + `ShapeRecognizer` in its own module
//! 2. Register in [`CATALOG`] (specificity order)
//! 3. Add `ShapePlan` variant + `eval_*_walk` dispatch
//! 4. Extend [`oxigraph_nova_core::PhysicalShape`] + engine `lftj_prepare_shape`
//!
//! Nesting note: Star-with-features / chain-of-stars should become
//! `ShapePlan` trees (`Box<ShapePlan>` children), not flattened mega-plans.

mod directed_triangle;
mod k_chain;
mod sp_expansion;
mod star;
mod two_hop;
mod wedge;

use crate::lftj::PatternSpec;
pub use directed_triangle::DirectedTrianglePlan;
pub use k_chain::KChainPlan;
pub use sp_expansion::SpExpansionPlan;
pub use star::StarPlan;
pub use two_hop::TwoHopPlan;
pub use wedge::WedgePlan;

/// Stable identifier for a recognized BGP shape.
///
/// Used for observability (`shape_selected[ShapeId]`) and engine capability
/// advertisement. Reserved variants exist so counters / capability tables can
/// name known motifs before recognizers are wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShapeId {
    // ── Live (catalog-selected today) ────────────────────────────────────
    /// `?s P1 O1 . ?s P2 ?o` — subject-star SP-expansion (2 patterns, 2 vars).
    SpExpansion,
    /// `?a P1 ?b . ?b P2 ?c` — two-hop chain (2 patterns, 3 vars).
    TwoHop,
    /// `?a P ?b . ?b P ?c . ?a P ?c` — fixed-P undirected triangle (3 patterns, 3 vars).
    Wedge,
    /// `?a P1 ?b . ?b P2 ?c . ?c P3 ?a` — directed 3-cycle (3 patterns, 3 vars).
    /// Predicates may be equal or distinct. Distinct from fixed-P [`Self::Wedge`].
    DirectedTriangle,
    /// `?a P1 ?b . ?b P2 ?c . ?c P3 ?d` — 3-hop chain (3 patterns, 4 vars).
    /// Recognized for k = 3 only; longer chains would reuse this id once the
    /// plan grows variable arity.
    KChain,
    /// `?s P1 ?o1 . ?s P2 ?o2 . ?s P3 ?o3` — subject-star k-way fan-out
    /// (3 patterns, 4 vars). Recognized for k = 3 only; longer stars would
    /// reuse this id once the plan grows variable arity.
    /// Generalizes [`Self::SpExpansion`].
    Star,

    // ── Reserved (not in catalog; no ShapePlan / walk yet) ───────────────
    /// *Reserved.* Two paths a→…→d sharing endpoints:
    /// `?a→?b→?d` and `?a→?c→?d` (diamond / multi-path meet).
    Diamond,
    /// *Reserved.* Shared-object reverse fan:
    /// `?a P1 ?o . ?b P2 ?o . …` (inverse of [`Self::Star`]).
    ObjectStar,
}

impl ShapeId {
    /// `true` when a recognizer may currently return this id from the catalog.
    #[inline]
    pub fn is_live(self) -> bool {
        matches!(
            self,
            ShapeId::SpExpansion
                | ShapeId::TwoHop
                | ShapeId::Wedge
                | ShapeId::DirectedTriangle
                | ShapeId::KChain
                | ShapeId::Star
        )
    }

    /// All known shape ids (live + reserved), for capability / counter tables.
    pub const ALL: &'static [ShapeId] = &[
        ShapeId::SpExpansion,
        ShapeId::TwoHop,
        ShapeId::Wedge,
        ShapeId::KChain,
        ShapeId::Star,
        ShapeId::DirectedTriangle,
        ShapeId::Diamond,
        ShapeId::ObjectStar,
    ];
}

/// Abstract, bound description of a recognized shape.
///
/// Flat for now. When nesting is needed, variants may gain
/// `Box<ShapePlan>` children without changing the catalog/dispatch seam.
#[derive(Debug, Clone)]
pub enum ShapePlan {
    SpExpansion(SpExpansionPlan),
    TwoHop(TwoHopPlan),
    Wedge(WedgePlan),
    DirectedTriangle(DirectedTrianglePlan),
    KChain(KChainPlan),
    Star(StarPlan),
}

impl ShapePlan {
    #[inline]
    pub fn id(&self) -> ShapeId {
        match self {
            ShapePlan::SpExpansion(_) => ShapeId::SpExpansion,
            ShapePlan::TwoHop(_) => ShapeId::TwoHop,
            ShapePlan::Wedge(_) => ShapeId::Wedge,
            ShapePlan::DirectedTriangle(_) => ShapeId::DirectedTriangle,
            ShapePlan::KChain(_) => ShapeId::KChain,
            ShapePlan::Star(_) => ShapeId::Star,
        }
    }

    /// Drop join-var indices; keep only the constants the store needs to prepare.
    #[inline]
    pub fn to_physical(&self) -> oxigraph_nova_core::PhysicalShape {
        match self {
            ShapePlan::SpExpansion(p) => p.to_physical(),
            ShapePlan::TwoHop(p) => p.to_physical(),
            ShapePlan::Wedge(p) => p.to_physical(),
            ShapePlan::DirectedTriangle(p) => p.to_physical(),
            ShapePlan::KChain(p) => p.to_physical(),
            ShapePlan::Star(p) => p.to_physical(),
        }
    }
}

/// Pure structural predicate over classified BGP patterns.
///
/// Recognizers must not touch the store — only `[PatternSpec]` + var count.
/// `pub(crate)` because `PatternSpec` is crate-private (shared with LFTJ).
pub(crate) trait ShapeRecognizer: Send + Sync {
    /// Stable id for this recognizer (observability / capability table).
    #[allow(dead_code)] // used once shape_selected[ShapeId] counters land
    fn id(&self) -> ShapeId;
    fn recognize(&self, specs: &[PatternSpec], n_vars: usize) -> Option<ShapePlan>;
}

/// Shape catalog in specificity order (most specific → least).
///
/// Today most entries have disjoint (n_patterns, n_vars) guards, so order is
/// mostly moot — but the contract is "first match wins". Wedge (3p/3v) and
/// KChain/Star (both 3p/4v) share pattern count; KChain and Star differ by
/// topology (each rejects the other's motif), so either order is safe.
/// Two-hop is 2p/3v; SP-expansion is 2p/2v.
fn catalog() -> &'static [&'static dyn ShapeRecognizer] {
    // Static trait-object slices can't be built with `const` on stable without
    // a helper type; once_cell-style lazy is overkill for a few entries. A plain
    // function returning a fixed array reference is enough.
    &CATALOG
}

/// Concrete catalog entries (order = specificity).
///
/// Wedge before DirectedTriangle: both are 3p/3v; Wedge is the chordal fixed-P
/// motif, DirectedTriangle is the pure cycle (rejects chordal / accepts mixed P).
static CATALOG: [&'static dyn ShapeRecognizer; 6] = [
    &wedge::WedgeRecognizer,                        // 3p/3v chordal fixed-P
    &directed_triangle::DirectedTriangleRecognizer, // 3p/3v directed cycle
    &k_chain::KChainRecognizer,                     // 3p/4v chain topology
    &star::StarRecognizer,                          // 3p/4v star topology (rejects chains)
    &sp_expansion::SpExpansionRecognizer,
    &two_hop::TwoHopRecognizer,
];

/// Try each recognizer in catalog order; return the first match.
///
/// `pub(crate)` — takes crate-private [`PatternSpec`].
#[inline]
pub(crate) fn recognize_shape(specs: &[PatternSpec], n_vars: usize) -> Option<ShapePlan> {
    for rec in catalog() {
        if let Some(plan) = rec.recognize(specs, n_vars) {
            return Some(plan);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lftj::{FieldSpec, PatternSpec};

    fn jv(i: usize) -> FieldSpec {
        FieldSpec::JoinVar(i)
    }
    fn c(v: u64) -> FieldSpec {
        FieldSpec::Const(v)
    }
    fn spec(s: FieldSpec, p: FieldSpec, o: FieldSpec) -> PatternSpec {
        PatternSpec { s, p, o }
    }

    // ── SP-expansion ────────────────────────────────────────────────────────

    #[test]
    fn sp_expansion_basic() {
        // ?s P1 O1 . ?s P2 ?o
        let specs = [spec(jv(0), c(10), c(20)), spec(jv(0), c(11), jv(1))];
        let plan = recognize_shape(&specs, 2).expect("should recognize");
        assert_eq!(plan.id(), ShapeId::SpExpansion);
        match plan {
            ShapePlan::SpExpansion(p) => {
                assert_eq!(p.p_filter, 10);
                assert_eq!(p.o_filter, 20);
                assert_eq!(p.p_expand, 11);
                assert_eq!(p.s_idx, 0);
                assert_eq!(p.o_idx, 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn sp_expansion_swapped_pattern_order() {
        // expand first, filter second
        let specs = [spec(jv(0), c(11), jv(1)), spec(jv(0), c(10), c(20))];
        let plan = recognize_shape(&specs, 2).expect("should recognize");
        assert_eq!(plan.id(), ShapeId::SpExpansion);
    }

    #[test]
    fn sp_expansion_rejects_wrong_var_count() {
        let specs = [spec(jv(0), c(10), c(20)), spec(jv(0), c(11), jv(1))];
        assert!(recognize_shape(&specs, 3).is_none());
    }

    #[test]
    fn sp_expansion_rejects_different_subjects() {
        // ?s P1 O1 . ?t P2 ?o  — different subjects, 2 vars
        let specs = [
            spec(jv(0), c(10), c(20)),
            spec(jv(1), c(11), jv(0)), // s=1, o=0
        ];
        // n_vars=2 but subjects differ → no SP-expansion; not a two-hop either
        // (two-hop needs join-var objects on both + 3 vars).
        assert!(recognize_shape(&specs, 2).is_none());
    }

    // ── Two-hop ─────────────────────────────────────────────────────────────

    #[test]
    fn two_hop_basic() {
        // ?a P1 ?b . ?b P2 ?c
        let specs = [spec(jv(0), c(10), jv(1)), spec(jv(1), c(11), jv(2))];
        let plan = recognize_shape(&specs, 3).expect("should recognize");
        assert_eq!(plan.id(), ShapeId::TwoHop);
        match plan {
            ShapePlan::TwoHop(p) => {
                assert_eq!(p.p1, 10);
                assert_eq!(p.p2, 11);
                assert_eq!(p.a, 0);
                assert_eq!(p.b, 1);
                assert_eq!(p.c, 2);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn two_hop_reversed_edge_order() {
        // patterns listed c←b←a
        let specs = [spec(jv(1), c(11), jv(2)), spec(jv(0), c(10), jv(1))];
        let plan = recognize_shape(&specs, 3).expect("should recognize");
        match plan {
            ShapePlan::TwoHop(p) => {
                assert_eq!((p.a, p.b, p.c), (0, 1, 2));
                assert_eq!((p.p1, p.p2), (10, 11));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn two_hop_rejects_wedge() {
        // ?a P1 ?b . ?a P2 ?c  (wedge, not chain) — 2 patterns, not triangle
        let specs = [spec(jv(0), c(10), jv(1)), spec(jv(0), c(11), jv(2))];
        assert!(recognize_shape(&specs, 3).is_none());
    }

    #[test]
    fn two_hop_rejects_wrong_var_count() {
        let specs = [spec(jv(0), c(10), jv(1)), spec(jv(1), c(11), jv(2))];
        assert!(recognize_shape(&specs, 2).is_none());
    }

    // ── Wedge / fixed-P triangle ────────────────────────────────────────────

    #[test]
    fn wedge_basic() {
        // ?a P ?b . ?b P ?c . ?a P ?c
        let specs = [
            spec(jv(0), c(10), jv(1)),
            spec(jv(1), c(10), jv(2)),
            spec(jv(0), c(10), jv(2)),
        ];
        let plan = recognize_shape(&specs, 3).expect("should recognize");
        assert_eq!(plan.id(), ShapeId::Wedge);
        match plan {
            ShapePlan::Wedge(p) => {
                assert_eq!(p.predicate, 10);
                assert_eq!((p.a, p.b, p.c), (0, 1, 2));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn wedge_permuted_patterns_and_labels() {
        // edges listed as c→a, a→b, c→b with vars renamed: still a triangle.
        // Directed: 2→0, 0→1, 2→1 → orientation a=2,b=0,c=1.
        let specs = [
            spec(jv(2), c(7), jv(0)),
            spec(jv(0), c(7), jv(1)),
            spec(jv(2), c(7), jv(1)),
        ];
        let plan = recognize_shape(&specs, 3).expect("should recognize");
        match plan {
            ShapePlan::Wedge(p) => {
                assert_eq!(p.predicate, 7);
                assert_eq!((p.a, p.b, p.c), (2, 0, 1));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn wedge_rejects_mixed_predicates() {
        let specs = [
            spec(jv(0), c(10), jv(1)),
            spec(jv(1), c(11), jv(2)),
            spec(jv(0), c(10), jv(2)),
        ];
        assert!(recognize_shape(&specs, 3).is_none());
    }

    #[test]
    fn wedge_rejects_cycle_without_chord() {
        // Pure directed cycle (no a→c chord) is DirectedTriangle, not Wedge.
        let specs = [
            spec(jv(0), c(10), jv(1)),
            spec(jv(1), c(10), jv(2)),
            spec(jv(2), c(10), jv(0)),
        ];
        assert_eq!(
            recognize_shape(&specs, 3).map(|p| p.id()),
            Some(ShapeId::DirectedTriangle)
        );
    }

    #[test]
    fn wedge_rejects_wrong_pattern_count() {
        // Two edges is a chain, not a triangle — catalog hits TwoHop instead.
        let chain = [spec(jv(0), c(10), jv(1)), spec(jv(1), c(10), jv(2))];
        assert_eq!(
            recognize_shape(&chain, 3).map(|p| p.id()),
            Some(ShapeId::TwoHop)
        );
        // Four patterns under fixed P: not wedge (and not two-hop / SP-exp).
        let four = [
            spec(jv(0), c(10), jv(1)),
            spec(jv(1), c(10), jv(2)),
            spec(jv(0), c(10), jv(2)),
            spec(jv(2), c(10), jv(0)),
        ];
        assert!(recognize_shape(&four, 3).is_none());
    }

    #[test]
    fn catalog_prefers_first_match_disjoint_guards() {
        // Sanity: SP-expansion, two-hop, wedge have disjoint (len, n_vars);
        // KChain and Star share 3p/4v but differ by topology.
        let sp = [spec(jv(0), c(10), c(20)), spec(jv(0), c(11), jv(1))];
        assert_eq!(
            recognize_shape(&sp, 2).map(|p| p.id()),
            Some(ShapeId::SpExpansion)
        );
        let th = [spec(jv(0), c(10), jv(1)), spec(jv(1), c(11), jv(2))];
        assert_eq!(
            recognize_shape(&th, 3).map(|p| p.id()),
            Some(ShapeId::TwoHop)
        );
        let w = [
            spec(jv(0), c(10), jv(1)),
            spec(jv(1), c(10), jv(2)),
            spec(jv(0), c(10), jv(2)),
        ];
        assert_eq!(recognize_shape(&w, 3).map(|p| p.id()), Some(ShapeId::Wedge));
        let kc = [
            spec(jv(0), c(1), jv(1)),
            spec(jv(1), c(2), jv(2)),
            spec(jv(2), c(3), jv(3)),
        ];
        assert_eq!(
            recognize_shape(&kc, 4).map(|p| p.id()),
            Some(ShapeId::KChain)
        );
        let st = [
            spec(jv(0), c(1), jv(1)),
            spec(jv(0), c(2), jv(2)),
            spec(jv(0), c(3), jv(3)),
        ];
        assert_eq!(recognize_shape(&st, 4).map(|p| p.id()), Some(ShapeId::Star));
    }

    #[test]
    fn unrecognized_falls_through() {
        // Single pattern — neither shape.
        let specs = [spec(jv(0), c(10), jv(1))];
        assert!(recognize_shape(&specs, 2).is_none());
    }

    // ── K-chain (k = 3) ─────────────────────────────────────────────────────

    #[test]
    fn k_chain_basic() {
        // ?a P1 ?b . ?b P2 ?c . ?c P3 ?d
        let specs = [
            spec(jv(0), c(10), jv(1)),
            spec(jv(1), c(11), jv(2)),
            spec(jv(2), c(12), jv(3)),
        ];
        let plan = recognize_shape(&specs, 4).expect("should recognize");
        assert_eq!(plan.id(), ShapeId::KChain);
        match plan {
            ShapePlan::KChain(p) => {
                assert_eq!((p.p1, p.p2, p.p3), (10, 11, 12));
                assert_eq!((p.a, p.b, p.c, p.d), (0, 1, 2, 3));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn k_chain_permuted_pattern_order() {
        // edges listed hop3, hop1, hop2
        let specs = [
            spec(jv(2), c(12), jv(3)),
            spec(jv(0), c(10), jv(1)),
            spec(jv(1), c(11), jv(2)),
        ];
        let plan = recognize_shape(&specs, 4).expect("should recognize");
        match plan {
            ShapePlan::KChain(p) => {
                assert_eq!((p.a, p.b, p.c, p.d), (0, 1, 2, 3));
                assert_eq!((p.p1, p.p2, p.p3), (10, 11, 12));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn k_chain_rejects_star() {
        // Subject star is not a chain — catalog selects Star instead.
        let specs = [
            spec(jv(0), c(1), jv(1)),
            spec(jv(0), c(2), jv(2)),
            spec(jv(0), c(3), jv(3)),
        ];
        assert_eq!(
            recognize_shape(&specs, 4).map(|p| p.id()),
            Some(ShapeId::Star)
        );
    }

    // ── Star (k = 3) ────────────────────────────────────────────────────────

    #[test]
    fn star_basic() {
        // ?s P1 ?o1 . ?s P2 ?o2 . ?s P3 ?o3
        let specs = [
            spec(jv(0), c(10), jv(1)),
            spec(jv(0), c(11), jv(2)),
            spec(jv(0), c(12), jv(3)),
        ];
        let plan = recognize_shape(&specs, 4).expect("should recognize");
        assert_eq!(plan.id(), ShapeId::Star);
        match plan {
            ShapePlan::Star(p) => {
                assert_eq!((p.p1, p.p2, p.p3), (10, 11, 12));
                assert_eq!((p.s, p.o1, p.o2, p.o3), (0, 1, 2, 3));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn star_permuted_pattern_order() {
        // arms listed o3, o1, o2 — first-appearance labeling
        let specs = [
            spec(jv(0), c(12), jv(3)),
            spec(jv(0), c(10), jv(1)),
            spec(jv(0), c(11), jv(2)),
        ];
        let plan = recognize_shape(&specs, 4).expect("should recognize");
        match plan {
            ShapePlan::Star(p) => {
                assert_eq!(p.s, 0);
                assert_eq!((p.o1, p.o2, p.o3), (3, 1, 2));
                assert_eq!((p.p1, p.p2, p.p3), (12, 10, 11));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn star_rejects_chain() {
        // 3-hop chain is not a star — catalog selects KChain.
        let specs = [
            spec(jv(0), c(1), jv(1)),
            spec(jv(1), c(2), jv(2)),
            spec(jv(2), c(3), jv(3)),
        ];
        assert_eq!(
            recognize_shape(&specs, 4).map(|p| p.id()),
            Some(ShapeId::KChain)
        );
    }

    #[test]
    fn star_rejects_wrong_var_count() {
        let specs = [
            spec(jv(0), c(1), jv(1)),
            spec(jv(0), c(2), jv(2)),
            spec(jv(0), c(3), jv(3)),
        ];
        assert!(recognize_shape(&specs, 3).is_none());
    }

    #[test]
    fn k_chain_rejects_wrong_var_count() {
        let specs = [
            spec(jv(0), c(1), jv(1)),
            spec(jv(1), c(2), jv(2)),
            spec(jv(2), c(3), jv(3)),
        ];
        // n_vars=3 would be wedge territory (and fails chain endpoint check).
        assert!(recognize_shape(&specs, 3).is_none());
    }

    // ── Directed triangle (3-cycle) ─────────────────────────────────────────

    #[test]
    fn directed_triangle_basic_mixed_p() {
        // ?a P1 ?b . ?b P2 ?c . ?c P3 ?a
        let specs = [
            spec(jv(0), c(1), jv(1)),
            spec(jv(1), c(2), jv(2)),
            spec(jv(2), c(3), jv(0)),
        ];
        let plan = recognize_shape(&specs, 3).expect("should recognize");
        assert_eq!(plan.id(), ShapeId::DirectedTriangle);
        match plan {
            ShapePlan::DirectedTriangle(p) => {
                assert_eq!((p.p1, p.p2, p.p3), (1, 2, 3));
                assert_eq!((p.a, p.b, p.c), (0, 1, 2));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn directed_triangle_same_p_cycle() {
        // Fixed-P cycle without chord — still DirectedTriangle (not Wedge).
        let specs = [
            spec(jv(0), c(10), jv(1)),
            spec(jv(1), c(10), jv(2)),
            spec(jv(2), c(10), jv(0)),
        ];
        let plan = recognize_shape(&specs, 3).expect("should recognize");
        assert_eq!(plan.id(), ShapeId::DirectedTriangle);
    }

    #[test]
    fn directed_triangle_permuted_pattern_order() {
        // edges listed c→a, a→b, b→c
        let specs = [
            spec(jv(2), c(30), jv(0)),
            spec(jv(0), c(10), jv(1)),
            spec(jv(1), c(20), jv(2)),
        ];
        let plan = recognize_shape(&specs, 3).expect("should recognize");
        match plan {
            ShapePlan::DirectedTriangle(p) => {
                assert_eq!((p.a, p.b, p.c), (0, 1, 2));
                assert_eq!((p.p1, p.p2, p.p3), (10, 20, 30));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn directed_triangle_does_not_steal_wedge() {
        // Chordal a→b, b→c, a→c stays Wedge.
        let specs = [
            spec(jv(0), c(10), jv(1)),
            spec(jv(1), c(10), jv(2)),
            spec(jv(0), c(10), jv(2)),
        ];
        assert_eq!(
            recognize_shape(&specs, 3).map(|p| p.id()),
            Some(ShapeId::Wedge)
        );
    }

    // ── Reserved shape ids ──────────────────────────────────────────────────

    #[test]
    fn reserved_shape_ids_are_not_live() {
        assert!(ShapeId::SpExpansion.is_live());
        assert!(ShapeId::TwoHop.is_live());
        assert!(ShapeId::Wedge.is_live());
        assert!(ShapeId::DirectedTriangle.is_live());
        assert!(ShapeId::KChain.is_live());
        assert!(ShapeId::Star.is_live());
        assert!(!ShapeId::Diamond.is_live());
        assert!(!ShapeId::ObjectStar.is_live());
        // ALL lists every known id exactly once.
        assert_eq!(ShapeId::ALL.len(), 8);
        for id in ShapeId::ALL {
            let _ = id.is_live(); // smoke: every variant is matchable
        }
    }

    #[test]
    fn reserved_motifs_fall_through_until_recognizers_land() {
        // Diamond a→b→d, a→c→d — future ShapeId::Diamond.
        let diamond = [
            spec(jv(0), c(1), jv(1)),
            spec(jv(1), c(2), jv(3)),
            spec(jv(0), c(3), jv(2)),
            spec(jv(2), c(4), jv(3)),
        ];
        assert!(recognize_shape(&diamond, 4).is_none());

        // Object-star shared o — future ShapeId::ObjectStar.
        let ostar = [spec(jv(0), c(1), jv(2)), spec(jv(1), c(2), jv(2))];
        assert!(recognize_shape(&ostar, 3).is_none());
    }
}
