//! Semi-naive fixpoint driver for a single transitivity [`Rule`].
//!
//! ## The technique
//!
//! For a recursive rule `R(x,z) :- R(x,y), R(y,z)`, naive evaluation would
//! recompute the *entire* self-join (`Total ⋈ Total`) every round — wasteful,
//! since most of those pairs were already found in a previous round.
//! Semi-naive evaluation exploits the observation that any *newly*
//! derivable pair in round `k+1` must involve at least one edge that was
//! itself newly derived in round `k` (the "delta") — a pair built entirely
//! from edges already known before round `k` would have already been found
//! by round `k`. So each round only needs:
//!
//! ```text
//! new_pairs = (Total ⋈ Delta) ∪ (Delta ⋈ Total)
//! ```
//!
//! rather than `Total ⋈ Total`. This is the standard technique (see e.g.
//! Abiteboul/Hull/Vianu, "Foundations of Databases", §13.3), applied here
//! with Nova's [`crate::join::leapfrog_join`] answering each of the two
//! joins — and, critically, each side of each join can be backed by a
//! *different* [`crate::AtomSource`] (`Total` is the accumulated closure so
//! far; `Delta` is just the previous round's new rows) — see
//! `sorted_vec_trie`'s module docs for why this heterogeneity is the whole
//! point: it lets LFTJ-shaped joins run without ever touching Nova's LOUDS
//! index or its `lftj_has_delta()` gate mid-fixpoint.
//!
//! ## Termination
//!
//! Predicate is fixed (one [`Rule`] = one predicate), the universe of
//! constants is finite (bounded by the store's dictionary), so `Total` is
//! monotonically growing and bounded — the loop always terminates once a
//! round produces no genuinely new pairs.

use crate::join::{Atom, AtomField, SliceSource, leapfrog_join};
use crate::rule::Rule;
use std::collections::BTreeSet;

/// Run `rule` to closure over `base_triples` (rows `[x, predicate, y]`),
/// returning the full set of triples in the transitive closure (base facts
/// included).
///
/// `base_triples` need not be sorted or deduplicated; this function does
/// that internally before iterating.
pub fn transitive_closure(rule: Rule, base_triples: &[[u64; 3]]) -> Vec<[u64; 3]> {
    let mut total: BTreeSet<[u64; 3]> = base_triples.iter().copied().collect();
    let mut delta: Vec<[u64; 3]> = total.iter().copied().collect();

    while !delta.is_empty() {
        let total_rows: Vec<[u64; 3]> = total.iter().copied().collect();

        let total_delta = join_round(rule.predicate, &total_rows, &delta);
        let delta_total = join_round(rule.predicate, &delta, &total_rows);

        let mut new_pairs: BTreeSet<[u64; 3]> = BTreeSet::new();
        for row in total_delta.into_iter().chain(delta_total) {
            if !total.contains(&row) {
                new_pairs.insert(row);
            }
        }

        delta = new_pairs.into_iter().collect();
        total.extend(delta.iter().copied());
    }

    total.into_iter().collect()
}

/// One `left ⋈ right` join round for `R(x,y) ∧ R(y,z) → R(x,z)`, projecting
/// results back down to head triples `[x, predicate, z]`.
fn join_round(predicate: u64, left: &[[u64; 3]], right: &[[u64; 3]]) -> Vec<[u64; 3]> {
    if left.is_empty() || right.is_empty() {
        return Vec::new();
    }
    let left_src = SliceSource::new(left);
    let right_src = SliceSource::new(right);
    let atoms = vec![
        Atom {
            s: AtomField::Var(0),
            p: AtomField::Const(predicate),
            o: AtomField::Var(1),
            source: &left_src,
        },
        Atom {
            s: AtomField::Var(1),
            p: AtomField::Const(predicate),
            o: AtomField::Var(2),
            source: &right_src,
        },
    ];
    leapfrog_join(&atoms, 3)
        .into_iter()
        .map(|b| [b[0], predicate, b[2]])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SC: u64 = 100; // stand-in interned predicate id (e.g. rdfs:subClassOf)

    /// A ⊑ B ⊑ C ⊑ D chain must close to every transitive pair: A⊑B, A⊑C,
    /// A⊑D, B⊑C, B⊑D, C⊑D (6 pairs total for a 4-node chain), plus the 3
    /// base facts already counted among those 6 — i.e. exactly 6 distinct
    /// triples, no more, no less.
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
    /// including self-loops — exercises the loop actually terminating on
    /// cyclic input (a case naive recomputation could infinite-loop on if
    /// implemented incorrectly, though this monotone-closure formulation
    /// cannot).
    #[test]
    fn cycle_closure_terminates_and_is_complete() {
        let (a, b, c) = (1u64, 2u64, 3u64);
        let base = vec![[a, SC, b], [b, SC, c], [c, SC, a]];
        let mut closure = transitive_closure(Rule::transitive(SC), &base);
        closure.sort();

        // Complete relation on {a,b,c} (every ordered pair, including
        // self-loops, since a 3-cycle transitively reaches everything
        // including itself).
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

        let mut expected = vec![[a, SC, b], [a, SC, c], [b, SC, c], [x, SC, y], [x, SC, z], [
            y, SC, z,
        ]];
        expected.sort();
        assert_eq!(closure, expected);
    }

    /// Empty input closes to empty output.
    #[test]
    fn empty_base_closure_is_empty() {
        let closure = transitive_closure(Rule::transitive(SC), &[]);
        assert!(closure.is_empty());
    }
}
