//! Rule shape for the semi-naive fixpoint driver.
//!
//! This is deliberately minimal for the initial spike: a single
//! transitivity-shaped rule (`R(x,y) ∧ R(y,z) → R(x,z)`) over one constant
//! predicate, which is exactly OWL 2 RL's `prp-trp`
//! (`owl:TransitiveProperty`) rule shape and also covers `rdfs:subClassOf`/
//! `rdfs:subPropertyOf` transitivity (`scm-sco`/`scm-spo` in the OWL 2 RL
//! rule table). See `CLAUDE.md`'s Phase 3 design section for how this
//! generalizes to the full rules-as-data `RuleSet` (arbitrary N-atom
//! bodies, multiple head predicates) planned for the production reasoner —
//! that generalization is future work; this spike exists to de-risk the
//! `SortedVecTrie` + heterogeneous-join + semi-naive-loop combination on the
//! simplest possible recursive rule before investing in the general case.

/// A transitivity rule over a single predicate: `R(x,y) ∧ R(y,z) → R(x,z)`.
///
/// `predicate` is the already-interned `TermId` (as a raw `u64`) shared by
/// every atom in the body and the head — transitivity rules never change
/// predicate between body and head.
#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub predicate: u64,
}

impl Rule {
    pub fn transitive(predicate: u64) -> Self {
        Self { predicate }
    }
}
