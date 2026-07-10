//! Multi-rule semi-naive fixpoint driver over a [`RuleSet`].
//!
//! ## The technique
//!
//! For recursive rules (e.g. `R(x,z) :- R(x,y), R(y,z)`), naive evaluation
//! would recompute the *entire* self-join (`Total ⋈ Total`) every round —
//! wasteful, since most of those pairs were already found in a previous
//! round. Semi-naive evaluation exploits the observation that any *newly*
//! derivable fact in round `k+1` must involve at least one fact that was
//! itself newly derived in round `k` (the "delta") — a derivation built
//! entirely from facts already known before round `k` would have already
//! been found by round `k`. So each round only needs, per rule body atom
//! position `i`:
//!
//! ```text
//! new_facts |= body[0] ⋈ ... ⋈ Delta(body[i]) ⋈ ... ⋈ body[n-1]     (for each i)
//! ```
//!
//! evaluated against `Total` for every *other* atom — i.e. "at least one
//! atom scans Delta, the rest scan Total" — rather than every atom scanning
//! `Total` (which would rediscover only-old facts every round). This is the
//! standard technique (see e.g. Abiteboul/Hull/Vianu, "Foundations of
//! Databases", §13.3), generalized here from a single hardcoded 2-atom rule
//! to arbitrary N-atom bodies across an arbitrary number of rules sharing
//! one global `Total`/`Delta`.
//!
//! ## Predicate-indexed dispatch
//!
//! Not every rule can produce something new on every round: a rule whose
//! body atoms only ever reference predicates absent from the current
//! `delta` cannot fire (semi-naive evaluation requires *some* atom to touch
//! the delta). [`RuleSet::active_rules`] filters the rule list down to just
//! the ones worth running each round — see that method's doc comment.
//!
//! ## Termination
//!
//! The universe of constants is finite (bounded by the store's dictionary),
//! `Total` is monotonically growing and bounded, so the loop always
//! terminates once a round produces no genuinely new facts across every
//! active rule.

use crate::join::{Atom, AtomField, AtomSource, CombinedSource, SliceSource, leapfrog_join};
use crate::rule::{Rule, RuleAtom, RuleSet};
use std::collections::BTreeSet;

/// Run every rule in `rules` to a shared closure over `base_triples` (rows
/// `[s, p, o]`), returning the full set of triples in the closure (base
/// facts included).
///
/// `base_triples` need not be sorted or deduplicated; this function does
/// that internally before iterating. All rule bodies join purely over the
/// in-memory `Total`/`Delta` relations built from `base_triples` — there is
/// no direct store access here (see [`closure_over_store`] for the variant
/// that scans a real store's compacted LOUDS tries for the `Total` side
/// instead of copying it into memory).
pub fn closure(rules: &RuleSet, base_triples: &[[u64; 3]]) -> Vec<[u64; 3]> {
    run_fixpoint(rules, base_triples, &crate::join::NullSource)
}

/// Like [`closure`], but every rule body atom's "Total" scan is answered by
/// `store` first (via [`CombinedSource`]) — so as `Total` grows across
/// rounds, the *original* base/EDB facts are never re-copied into memory:
/// only newly-**derived** facts ever get added to the in-memory
/// `Total`/`Delta` sets. `store` should be a
/// [`crate::store_source::StoreAtomSource`] wrapping a compacted
/// [`oxigraph_nova_core::LftjSource`].
///
/// `seed_triples` **must** contain every base fact the rules need to
/// bootstrap the very first round's `Delta` — the semi-naive loop only
/// discovers a new fact when it can join against something already in
/// `Delta`, so a fact that exists only in `store` and never appears in
/// `seed_triples` will never be selected as a `Delta` atom's binding and
/// therefore can never itself trigger a derivation (it can still
/// *participate* as a `Total`-side atom once something else triggers a
/// join, via `store`'s `CombinedSource` half). In practice, callers should
/// obtain `seed_triples` via one targeted read (e.g.
/// `QuadStore::quads_for_pattern` filtered to just the predicates the
/// `RuleSet` actually references) rather than a full store scan — a small,
/// rule-relevant seed is enough, since every derivable fact's *first*
/// derivation always involves at least one seeded fact, transitively. The
/// returned closure includes
/// `seed_triples` themselves (already in `Total` from round 0) plus every
/// newly-derived fact.
pub fn closure_over_store(
    rules: &RuleSet,
    store: &dyn AtomSource,
    seed_triples: &[[u64; 3]],
) -> Vec<[u64; 3]> {
    run_fixpoint(rules, seed_triples, store)
}

/// Shared driver: `store` answers the stable/base side of every rule body
/// atom (via [`CombinedSource`], layered underneath the in-memory `Total`);
/// `base_triples` seeds `Total`/`Delta` before the first round.
fn run_fixpoint(
    rules: &RuleSet,
    base_triples: &[[u64; 3]],
    store: &dyn AtomSource,
) -> Vec<[u64; 3]> {
    let mut total: BTreeSet<[u64; 3]> = base_triples.iter().copied().collect();
    let mut delta: Vec<[u64; 3]> = total.iter().copied().collect();

    while !delta.is_empty() {
        let active = rules.active_rules(&delta);
        if active.is_empty() {
            break;
        }

        let total_rows: Vec<[u64; 3]> = total.iter().copied().collect();
        let total_src = SliceSource::new(&total_rows);
        let delta_src = SliceSource::new(&delta);
        // "Total" atoms scan store ∪ in-memory-total-so-far; the store side
        // covers every base fact without copying it into `total_rows`.
        let combined_total = CombinedSource {
            primary: store,
            secondary: &total_src,
        };

        let mut new_facts: BTreeSet<[u64; 3]> = BTreeSet::new();
        for &rule_idx in &active {
            let rule = &rules.rules()[rule_idx];
            for derived in eval_rule_round(rule, &combined_total, &delta_src) {
                if !total.contains(&derived) {
                    new_facts.insert(derived);
                }
            }
        }

        delta = new_facts.into_iter().collect();
        total.extend(delta.iter().copied());
    }

    total.into_iter().collect()
}

/// Evaluate one [`Rule`] for one semi-naive round: for every body-atom
/// position `i`, run a join where atom `i` scans `delta_src` and every other
/// atom scans `combined_total`, unioning the head-projected results across
/// all `i` (deduplicating identical derivations from different `i` choices
/// is the caller's job via a `BTreeSet`/`HashSet`).
fn eval_rule_round(
    rule: &Rule,
    combined_total: &CombinedSource,
    delta_src: &SliceSource,
) -> Vec<[u64; 3]> {
    let mut out = Vec::new();
    for delta_pos in 0..rule.body.len() {
        let atoms: Vec<Atom> = rule
            .body
            .iter()
            .enumerate()
            .map(|(i, ra)| Atom {
                s: ra.s,
                p: ra.p,
                o: ra.o,
                source: if i == delta_pos {
                    delta_src as &dyn AtomSource
                } else {
                    combined_total as &dyn AtomSource
                },
            })
            .collect();

        for binding in leapfrog_join(&atoms, rule.num_vars) {
            out.push(project_head(&rule.head, &binding));
        }
    }
    out
}

/// Resolve a rule's head atom against a completed variable binding,
/// producing a concrete `[s, p, o]` derived triple.
fn project_head(head: &RuleAtom, binding: &[u64]) -> [u64; 3] {
    let resolve = |f: &AtomField| -> u64 {
        match f {
            AtomField::Const(id) => *id,
            AtomField::Var(v) => binding[*v],
        }
    };
    [resolve(&head.s), resolve(&head.p), resolve(&head.o)]
}

/// Convenience wrapper: run a single transitivity [`Rule`] to closure (the
/// original spike's entry point, kept for the existing integration test and
/// as the simplest possible usage example).
pub fn transitive_closure(rule: Rule, base_triples: &[[u64; 3]]) -> Vec<[u64; 3]> {
    let rules = RuleSet::new(vec![rule]);
    closure(&rules, base_triples)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SC: u64 = 100; // stand-in interned predicate id (e.g. rdfs:subClassOf)
    const TY: u64 = 200; // stand-in interned predicate id (e.g. rdf:type)

    /// A ⊑ B ⊑ C ⊑ D chain must close to every transitive pair: A⊑B, A⊑C,
    /// A⊑D, B⊑C, B⊑D, C⊑D (6 pairs total for a 4-node chain).
    #[test]
    fn chain_closure_is_exact() {
        let (a, b, c, d) = (1u64, 2u64, 3u64, 4u64);
        let base = vec![[a, SC, b], [b, SC, c], [c, SC, d]];
        let rule = Rule::transitive(SC);
        let mut closure = transitive_closure(rule, &base);
        closure.sort();

        let mut expected = vec![
            [a, SC, b],
            [a, SC, c],
            [a, SC, d],
            [b, SC, c],
            [b, SC, d],
            [c, SC, d],
        ];
        expected.sort();
        assert_eq!(closure, expected);
    }

    /// A single edge has no transitive consequences — closure == base.
    #[test]
    fn single_edge_closure_is_itself() {
        let base = vec![[1, SC, 2]];
        let closure = transitive_closure(Rule::transitive(SC), &base);
        assert_eq!(closure, vec![[1, SC, 2]]);
    }

    /// A 3-cycle (A⊑B⊑C⊑A) closes to the complete relation on {A,B,C}
    /// including self-loops.
    #[test]
    fn cycle_closure_terminates_and_is_complete() {
        let (a, b, c) = (1u64, 2u64, 3u64);
        let base = vec![[a, SC, b], [b, SC, c], [c, SC, a]];
        let mut closure = transitive_closure(Rule::transitive(SC), &base);
        closure.sort();

        let mut expected: Vec<[u64; 3]> = Vec::new();
        for &x in &[a, b, c] {
            for &y in &[a, b, c] {
                expected.push([x, SC, y]);
            }
        }
        expected.sort();
        assert_eq!(closure, expected);
    }

    /// Two disjoint chains must not cross-pollinate.
    #[test]
    fn disjoint_chains_stay_disjoint() {
        let (a, b, c) = (1u64, 2u64, 3u64);
        let (x, y, z) = (10u64, 20u64, 30u64);
        let base = vec![[a, SC, b], [b, SC, c], [x, SC, y], [y, SC, z]];
        let mut closure = transitive_closure(Rule::transitive(SC), &base);
        closure.sort();

        let mut expected = vec![
            [a, SC, b],
            [a, SC, c],
            [b, SC, c],
            [x, SC, y],
            [x, SC, z],
            [y, SC, z],
        ];
        expected.sort();
        assert_eq!(closure, expected);
    }

    /// Empty input closes to empty output.
    #[test]
    fn empty_base_closure_is_empty() {
        let closure = transitive_closure(Rule::transitive(SC), &[]);
        assert!(closure.is_empty());
    }

    /// Two interacting rules: `subClassOf` transitivity plus type
    /// propagation (`?x rdf:type ?c ∧ ?c rdfs:subClassOf ?d → ?x rdf:type
    /// ?d`). A instance of the most-specific class in a subclass chain must
    /// end up typed at every ancestor class, including ancestors only
    /// reachable via the *derived* (not asserted) subClassOf edges.
    #[test]
    fn multi_rule_type_propagation_through_transitive_subclass() {
        let (animal, mammal, dog) = (1u64, 2u64, 3u64);
        let fido = 10u64;

        let base = vec![
            [mammal, SC, animal], // Mammal ⊑ Animal
            [dog, SC, mammal],    // Dog ⊑ Mammal (so Dog ⊑ Animal is *derived*)
            [fido, TY, dog],      // fido : Dog
        ];

        let rules = RuleSet::new(vec![Rule::transitive(SC), Rule::type_propagation(TY, SC)]);
        let mut closure = closure(&rules, &base);
        closure.sort();

        let mut expected = vec![
            [mammal, SC, animal],
            [dog, SC, mammal],
            [dog, SC, animal], // derived subclass transitivity
            [fido, TY, dog],
            [fido, TY, mammal], // derived: one hop of type propagation
            [fido, TY, animal], // derived: type propagation through the *derived* subclass edge
        ];
        expected.sort();
        assert_eq!(
            closure, expected,
            "fido must be inferred rdf:type Animal via the derived Dog⊑Animal edge"
        );
    }
}
