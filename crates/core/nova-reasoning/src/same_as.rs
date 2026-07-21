//! [`SameAsTracker`] — a frozen (build-once, immutable) union-find over RDF
//! [`Term`]s, used to support `owl:sameAs` via **query-time canonicalization**
//! rather than materializing the quadratic `eq-rep-*` replacement-rule
//! closure.
//!
//! ## Why query-time canonicalization instead of a materialized closure
//!
//! `owl:sameAs`'s symmetry/transitivity/replacement closure (`eq-sym`,
//! `eq-trans`, `eq-rep-s`, `eq-rep-p`, `eq-rep-o` in the OWL 2 RL rule table)
//! is not materialized into new triples — doing so would, in the worst case,
//! multiply every triple mentioning a term by the size of its `sameAs`
//! equivalence class, for every position that term appears in. Instead, a
//! frozen union-find is built over the dataset's `owl:sameAs` pairs once,
//! and used to:
//!   1. **Canonicalize** a query's bound subject/object terms — i.e. widen a
//!      lookup for one specific term into a lookup across every member of
//!      its equivalence class (any of them might be the one actually used in
//!      a stored triple).
//!   2. **Expand** results back out — an unbound subject/object variable
//!      that matched some class member `m` is understood to also match every
//!      other member of `m`'s class (since they denote the same resource),
//!      so each such position is expanded into one result row per class
//!      member.
//!
//! This is exactly `eq-sym`+`eq-trans`+`eq-rep-s`+`eq-rep-o` implemented as a
//! query-time rewrite instead of a materialized closure — see
//! [`crate::reasoning_dataset::ReasoningDataset`] for where canonicalization
//! and expansion are actually applied to `find_quads`.
//!
//! `eq-rep-p` (predicate-position substitution) is intentionally **not**
//! implemented: `owl:sameAs` is defined over individuals, and substituting
//! sameAs-linked terms into the predicate position is a degenerate corner
//! case with no practical use in the OWL 2 RL profile, since `owl:sameAs`
//! only meaningfully denotes equivalence between individuals in subject/
//! object position.
//!
//! ## Why `Term`-keyed rather than `TermId`-keyed
//!
//! Unlike [`crate::engine::LftjFixpointEngine`]'s main fixpoint (which
//! operates over `u64` `TermId`s for LFTJ join-scan compatibility),
//! [`SameAsTracker`] is queried directly from
//! [`crate::reasoning_dataset::ReasoningDataset::find_quads`], which only
//! ever sees [`Term`]s (the `Dataset` trait's query-facing vocabulary) — and
//! `Term` derives `Hash`/`Eq` (though not `Ord`), which is sufficient for a
//! `HashMap`-backed union-find. Working at the `Term` level also means this
//! tracker requires nothing beyond ordinary [`Dataset::find_quads`] to
//! build — no LFTJ capability is required, unlike the main closure engine.

use oxigraph_nova_core::Term;
use std::collections::HashMap;
use std::sync::Arc;

/// A frozen union-find over RDF terms connected by `owl:sameAs` edges.
///
/// Built once via [`SameAsTracker::build`] and never mutated afterward. An
/// empty tracker (no `owl:sameAs` triples in the dataset) is the common case
/// and adds zero overhead: [`SameAsTracker::canonicalize`],
/// [`SameAsTracker::class_members`] degenerate to "return the term itself"
/// whenever the term is not part of any recorded equivalence class.
///
/// Equivalence-class members are held as `Arc<Term>` so query-time expand
/// paths can `Arc::clone` rather than deep-copy Term string content.
#[derive(Debug, Clone, Default)]
pub struct SameAsTracker {
    /// Maps every term that is part of a non-trivial equivalence class to
    /// that class's representative (root) term. Terms with no recorded
    /// `owl:sameAs` edges are simply absent from this map.
    canonical: HashMap<Term, Arc<Term>>,
    /// Maps every representative (root) term to the full list of terms in
    /// its equivalence class (including the root itself).
    members: HashMap<Term, Vec<Arc<Term>>>,
}

impl SameAsTracker {
    /// An empty tracker — every term canonicalizes to itself and has a
    /// singleton equivalence class. This is what every
    /// [`crate::engine::ReasoningEngine::same_as_tracker`] default
    /// implementation returns.
    pub fn empty() -> Self {
        Self::default()
    }

    /// `true` if this tracker has no recorded `owl:sameAs` pairs at all —
    /// callers use this to skip the canonicalization/expansion machinery
    /// entirely when there is nothing to do.
    pub fn is_empty(&self) -> bool {
        self.canonical.is_empty()
    }

    /// Build a frozen union-find from a list of `(x, y)` pairs, each
    /// representing one `x owl:sameAs y` triple. Order and duplicate pairs
    /// do not matter — symmetry and transitivity fall out automatically from
    /// the union-find structure.
    pub fn build(pairs: Vec<(Term, Term)>) -> Self {
        if pairs.is_empty() {
            return Self::empty();
        }

        // Standard union-find over `Term` keys via a HashMap-backed parent
        // pointer structure. `find` performs path compression as it goes
        // (mutating `parent` even though the overall tracker is frozen once
        // constructed — this is purely a one-time build-phase optimization).
        let mut parent: HashMap<Term, Term> = HashMap::new();

        fn find(parent: &mut HashMap<Term, Term>, t: &Term) -> Term {
            let p = parent.get(t).cloned().unwrap_or_else(|| t.clone());
            if p == *t {
                t.clone()
            } else {
                let root = find(parent, &p);
                parent.insert(t.clone(), root.clone());
                root
            }
        }

        for (a, b) in &pairs {
            parent.entry(a.clone()).or_insert_with(|| a.clone());
            parent.entry(b.clone()).or_insert_with(|| b.clone());
        }
        for (a, b) in &pairs {
            let ra = find(&mut parent, a);
            let rb = find(&mut parent, b);
            if ra != rb {
                parent.insert(ra, rb);
            }
        }

        // Finalize: fully compress every entry so `canonical` is a direct
        // term → root map (no further path-following needed at query time),
        // then build the inverse `root → members` map for expansion.
        // Roots are Arc-shared so each class's member list and the canonical
        // map can share the same root allocation.
        let mut root_arcs: HashMap<Term, Arc<Term>> = HashMap::new();
        let keys: Vec<Term> = parent.keys().cloned().collect();
        let mut canonical: HashMap<Term, Arc<Term>> = HashMap::new();
        for k in &keys {
            let root = find(&mut parent, k);
            let root_arc = root_arcs
                .entry(root.clone())
                .or_insert_with(|| Arc::new(root))
                .clone();
            canonical.insert(k.clone(), root_arc);
        }

        let mut members: HashMap<Term, Vec<Arc<Term>>> = HashMap::new();
        for (term, root) in &canonical {
            members
                .entry(root.as_ref().clone())
                .or_default()
                .push(Arc::new(term.clone()));
        }

        Self { canonical, members }
    }

    /// Canonicalize `term` to its equivalence class's representative.
    /// Returns `term` itself, cloned, if it is not part of any recorded
    /// `owl:sameAs` class.
    pub fn canonicalize(&self, term: &Term) -> Term {
        self.canonical
            .get(term)
            .map(|r| r.as_ref().clone())
            .unwrap_or_else(|| term.clone())
    }

    /// Every term in `term`'s equivalence class, including `term` itself
    /// (or just `[Arc::new(term.clone())]` if it is not part of any recorded
    /// class). Members are returned as shared `Arc<Term>` so expand paths
    /// can bump the refcount rather than deep-copy Term string content.
    /// This is the "expand back out" half of query-time canonicalization —
    /// see the module doc comment.
    pub fn class_members(&self, term: &Term) -> Vec<Arc<Term>> {
        match self.canonical.get(term) {
            Some(root) => self
                .members
                .get(root.as_ref())
                .map(|v| v.iter().map(Arc::clone).collect())
                .unwrap_or_else(|| vec![Arc::clone(root)]),
            None => vec![Arc::new(term.clone())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::NamedNode;

    fn iri(s: &str) -> Term {
        Term::NamedNode(NamedNode::new_unchecked(s))
    }

    fn member_terms(members: Vec<Arc<Term>>) -> Vec<Term> {
        members.into_iter().map(|t| (*t).clone()).collect()
    }

    #[test]
    fn empty_tracker_canonicalizes_to_self() {
        let t = SameAsTracker::empty();
        assert!(t.is_empty());
        let x = iri("http://ex/x");
        assert_eq!(t.canonicalize(&x), x);
        assert_eq!(member_terms(t.class_members(&x)), vec![x]);
    }

    #[test]
    fn direct_pair_is_symmetric() {
        let a = iri("http://ex/a");
        let b = iri("http://ex/b");
        let t = SameAsTracker::build(vec![(a.clone(), b.clone())]);
        assert!(!t.is_empty());
        assert_eq!(t.canonicalize(&a), t.canonicalize(&b));

        let mut members = member_terms(t.class_members(&a));
        members.sort_by_key(|t| t.to_string());
        let mut expected = vec![a, b];
        expected.sort_by_key(|t| t.to_string());
        assert_eq!(members, expected);
    }

    #[test]
    fn transitive_chain_collapses_to_one_class() {
        let a = iri("http://ex/a");
        let b = iri("http://ex/b");
        let c = iri("http://ex/c");
        let t = SameAsTracker::build(vec![(a.clone(), b.clone()), (b.clone(), c.clone())]);

        assert_eq!(t.canonicalize(&a), t.canonicalize(&c));
        let mut members = member_terms(t.class_members(&a));
        members.sort_by_key(|t| t.to_string());
        let mut expected = vec![a, b, c];
        expected.sort_by_key(|t| t.to_string());
        assert_eq!(members, expected);
    }

    #[test]
    fn disjoint_classes_stay_disjoint() {
        let a = iri("http://ex/a");
        let b = iri("http://ex/b");
        let x = iri("http://ex/x");
        let y = iri("http://ex/y");
        let t = SameAsTracker::build(vec![(a.clone(), b.clone()), (x.clone(), y.clone())]);

        assert_eq!(t.canonicalize(&a), t.canonicalize(&b));
        assert_eq!(t.canonicalize(&x), t.canonicalize(&y));
        assert_ne!(t.canonicalize(&a), t.canonicalize(&x));
    }

    #[test]
    fn unrelated_term_is_unaffected() {
        let a = iri("http://ex/a");
        let b = iri("http://ex/b");
        let unrelated = iri("http://ex/unrelated");
        let t = SameAsTracker::build(vec![(a, b)]);
        assert_eq!(t.canonicalize(&unrelated), unrelated);
        assert_eq!(member_terms(t.class_members(&unrelated)), vec![unrelated]);
    }

    #[test]
    fn duplicate_and_reversed_pairs_are_harmless() {
        let a = iri("http://ex/a");
        let b = iri("http://ex/b");
        let t = SameAsTracker::build(vec![
            (a.clone(), b.clone()),
            (b.clone(), a.clone()),
            (a.clone(), b.clone()),
        ]);
        assert_eq!(t.canonicalize(&a), t.canonicalize(&b));
        assert_eq!(t.class_members(&a).len(), 2);
    }
}
