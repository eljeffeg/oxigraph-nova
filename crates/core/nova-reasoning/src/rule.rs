//! Rules-as-data for the semi-naive fixpoint driver.
//!
//! A [`Rule`] is a conjunction of body [`RuleAtom`]s plus a single head atom,
//! all sharing a common variable space (`0..num_vars`). Arbitrary N-atom rule
//! bodies let the full OWL 2 RL / RDFS rule table (`cax-sco`, `scm-sco`,
//! `scm-spo`, `prp-spo1`, `prp-dom`, `prp-rng`, `prp-trp`, `prp-symp`,
//! `prp-inv1`/`prp-inv2`, ...) be expressed as plain data rather than
//! bespoke Rust functions.
//!
//! [`RuleSet`] groups many [`Rule`]s together and provides predicate-indexed
//! dispatch (`active_rules`): given a round's newly-derived `delta` facts,
//! only the rules whose body could *possibly* match one of those facts (by
//! constant predicate) need to run — a rule none of whose new facts touch
//! cannot produce anything new this round, since semi-naive evaluation only
//! needs `(Total ⋈ Delta) ∪ (Delta ⋈ Total)` per rule.

use crate::join::AtomField;
use std::collections::{HashMap, HashSet};

/// One atom in a rule body (or the head): its three fields, each either a
/// constant `TermId` or a rule-local variable.
///
/// Reuses [`AtomField`] (the same field-shape `join::Atom` uses) so a
/// `RuleAtom` can be trivially lifted into a concrete `Atom` once bound to a
/// real [`crate::AtomSource`] at evaluation time.
#[derive(Clone, Copy, Debug)]
pub struct RuleAtom {
    pub s: AtomField,
    pub p: AtomField,
    pub o: AtomField,
}

impl RuleAtom {
    pub fn new(s: AtomField, p: AtomField, o: AtomField) -> Self {
        Self { s, p, o }
    }
}

/// A datalog-style rule: `head :- body[0], body[1], ..., body[n-1]`.
///
/// `num_vars` bounds the variable indices used anywhere in `body`/`head`
/// (variables are `0..num_vars`); it is needed up front so the join engine
/// can allocate a fixed-size bindings vector.
#[derive(Clone, Debug)]
pub struct Rule {
    pub name: &'static str,
    pub body: Vec<RuleAtom>,
    pub head: RuleAtom,
    pub num_vars: usize,
}

impl Rule {
    pub fn new(name: &'static str, body: Vec<RuleAtom>, head: RuleAtom, num_vars: usize) -> Self {
        Self {
            name,
            body,
            head,
            num_vars,
        }
    }

    /// A transitivity rule over a single predicate:
    /// `R(x,y) ∧ R(y,z) → R(x,z)`.
    ///
    /// This is exactly OWL 2 RL's `prp-trp` (`owl:TransitiveProperty`) shape
    /// and also covers `rdfs:subClassOf`/`rdfs:subPropertyOf` transitivity
    /// (`scm-sco`/`scm-spo` in the OWL 2 RL rule table).
    pub fn transitive(predicate: u64) -> Self {
        Self::new(
            "transitive",
            vec![
                RuleAtom::new(
                    AtomField::Var(0),
                    AtomField::Const(predicate),
                    AtomField::Var(1),
                ),
                RuleAtom::new(
                    AtomField::Var(1),
                    AtomField::Const(predicate),
                    AtomField::Var(2),
                ),
            ],
            RuleAtom::new(
                AtomField::Var(0),
                AtomField::Const(predicate),
                AtomField::Var(2),
            ),
            3,
        )
    }

    /// `rdfs:subClassOf` type propagation (OWL 2 RL's `cax-sco`):
    /// `?x rdf:type ?c ∧ ?c rdfs:subClassOf ?d → ?x rdf:type ?d`.
    pub fn type_propagation(rdf_type: u64, sub_class_of: u64) -> Self {
        Self::new(
            "type_propagation",
            vec![
                RuleAtom::new(
                    AtomField::Var(0),
                    AtomField::Const(rdf_type),
                    AtomField::Var(1),
                ),
                RuleAtom::new(
                    AtomField::Var(1),
                    AtomField::Const(sub_class_of),
                    AtomField::Var(2),
                ),
            ],
            RuleAtom::new(
                AtomField::Var(0),
                AtomField::Const(rdf_type),
                AtomField::Var(2),
            ),
            3,
        )
    }

    /// Property hierarchy propagation (OWL 2 RL's `prp-spo1`):
    /// `?p1 rdfs:subPropertyOf ?p2 ∧ ?x ?p1 ?y → ?x ?p2 ?y`.
    ///
    /// Unlike [`Rule::transitive`]/[`Rule::type_propagation`], this rule's
    /// second body atom has a *variable* predicate position (`?p1`) —
    /// intentionally left as `AtomField::Var`, which makes
    /// [`Rule::has_variable_predicate`] mark this rule a [`RuleSet`]
    /// wildcard (always active every round, since it cannot be
    /// predicate-indexed).
    ///
    /// This does **not** require seeding the entire store with every base
    /// triple, even though its second atom is a fully generic `?x ?p1 ?y`
    /// scan: every rule body atom's "Total" side is always answered
    /// directly against the real store first (via `CombinedSource`, see
    /// `crate::fixpoint::run_fixpoint`), so a base fact never needs to
    /// appear in the semi-naive seed just to *participate* in a join — it
    /// only needs to be seeded if it must itself act as the round's
    /// `Delta` (the fact that triggers a new derivation). Here, the
    /// `subPropertyOf` edge atom is always sufficient to trigger the
    /// round (as `Delta` at body position 0), and the generic `?x ?p1 ?y`
    /// atom is then resolved as a `Total`-side store scan for that
    /// specific (now-bound) `p1` — exactly as cheap as any other
    /// predicate-bound scan.
    pub fn subproperty_propagation(sub_property_of: u64) -> Self {
        Self::new(
            "subproperty_propagation",
            vec![
                RuleAtom::new(
                    AtomField::Var(0), // ?p1
                    AtomField::Const(sub_property_of),
                    AtomField::Var(1), // ?p2
                ),
                RuleAtom::new(
                    AtomField::Var(2), // ?x
                    AtomField::Var(0), // ?p1 (variable predicate position)
                    AtomField::Var(3), // ?y
                ),
            ],
            RuleAtom::new(
                AtomField::Var(2), // ?x
                AtomField::Var(1), // ?p2
                AtomField::Var(3), // ?y
            ),
            4,
        )
    }

    /// Property domain propagation (OWL 2 RL's `prp-dom`):
    /// `?p rdfs:domain ?c ∧ ?x ?p ?y → ?x rdf:type ?c`.
    ///
    /// Same variable-predicate-body-atom shape as
    /// [`Rule::subproperty_propagation`] (see its doc comment for why this
    /// does not require seeding the entire store).
    pub fn domain(rdfs_domain: u64, rdf_type: u64) -> Self {
        Self::new(
            "domain",
            vec![
                RuleAtom::new(
                    AtomField::Var(0), // ?p
                    AtomField::Const(rdfs_domain),
                    AtomField::Var(1), // ?c
                ),
                RuleAtom::new(
                    AtomField::Var(2), // ?x
                    AtomField::Var(0), // ?p (variable predicate position)
                    AtomField::Var(3), // ?y
                ),
            ],
            RuleAtom::new(
                AtomField::Var(2), // ?x
                AtomField::Const(rdf_type),
                AtomField::Var(1), // ?c
            ),
            4,
        )
    }

    /// Property range propagation (OWL 2 RL's `prp-rng`):
    /// `?p rdfs:range ?c ∧ ?x ?p ?y → ?y rdf:type ?c`.
    ///
    /// Same variable-predicate-body-atom shape as
    /// [`Rule::subproperty_propagation`] (see its doc comment for why this
    /// does not require seeding the entire store).
    pub fn range(rdfs_range: u64, rdf_type: u64) -> Self {
        Self::new(
            "range",
            vec![
                RuleAtom::new(
                    AtomField::Var(0), // ?p
                    AtomField::Const(rdfs_range),
                    AtomField::Var(1), // ?c
                ),
                RuleAtom::new(
                    AtomField::Var(2), // ?x
                    AtomField::Var(0), // ?p (variable predicate position)
                    AtomField::Var(3), // ?y
                ),
            ],
            RuleAtom::new(
                AtomField::Var(3), // ?y
                AtomField::Const(rdf_type),
                AtomField::Var(1), // ?c
            ),
            4,
        )
    }

    /// Generic OWL 2 RL `prp-trp`: **any** predicate declared
    /// `owl:TransitiveProperty` is transitive — `?p rdf:type
    /// owl:TransitiveProperty ∧ ?x ?p ?y ∧ ?y ?p ?z → ?x ?p ?z`.
    ///
    /// Unlike an earlier design that minted one specialized
    /// [`Rule::transitive`] instance per declared transitive property, this
    /// is a single generic rule whose predicate position is itself a
    /// variable (`?p`, bound identically across all three body atoms and
    /// the head) — mirroring how QLever's own `ReasonerRules.h` expresses
    /// `prp-trp` as one `CONSTRUCT` with a `WILDCARD` output predicate,
    /// rather than one query per transitive property. The engine driving
    /// this rule still needs to *seed* each declared property's edges
    /// separately (a data-dependent set the rule body can't discover on its
    /// own — see `nova-reasoning::engine`'s seeding loop), but only ever
    /// constructs this one `Rule` value regardless of how many properties
    /// are declared transitive.
    pub fn transitive_property(rdf_type: u64, owl_transitive_property: u64) -> Self {
        Self::new(
            "transitive_property",
            vec![
                RuleAtom::new(
                    AtomField::Var(0), // ?p
                    AtomField::Const(rdf_type),
                    AtomField::Const(owl_transitive_property),
                ),
                RuleAtom::new(
                    AtomField::Var(1), // ?x
                    AtomField::Var(0), // ?p (variable predicate position)
                    AtomField::Var(2), // ?y
                ),
                RuleAtom::new(
                    AtomField::Var(2), // ?y
                    AtomField::Var(0), // ?p (variable predicate position)
                    AtomField::Var(3), // ?z
                ),
            ],
            RuleAtom::new(
                AtomField::Var(1), // ?x
                AtomField::Var(0), // ?p
                AtomField::Var(3), // ?z
            ),
            4,
        )
    }

    /// Generic OWL 2 RL `prp-symp`: **any** predicate declared
    /// `owl:SymmetricProperty` is symmetric — `?p rdf:type
    /// owl:SymmetricProperty ∧ ?x ?p ?y → ?y ?p ?x`. Same
    /// one-generic-rule-not-one-per-property shape as
    /// [`Rule::transitive_property`] — see its doc comment.
    pub fn symmetric_property(rdf_type: u64, owl_symmetric_property: u64) -> Self {
        Self::new(
            "symmetric_property",
            vec![
                RuleAtom::new(
                    AtomField::Var(0), // ?p
                    AtomField::Const(rdf_type),
                    AtomField::Const(owl_symmetric_property),
                ),
                RuleAtom::new(
                    AtomField::Var(1), // ?x
                    AtomField::Var(0), // ?p (variable predicate position)
                    AtomField::Var(2), // ?y
                ),
            ],
            RuleAtom::new(
                AtomField::Var(2), // ?y
                AtomField::Var(0), // ?p
                AtomField::Var(1), // ?x
            ),
            3,
        )
    }

    /// OWL 2 RL `cax-eqc` (forward half): `?c1 owl:equivalentClass ?c2 →
    /// ?c1 rdfs:subClassOf ?c2`. Pair with
    /// [`Rule::equivalent_class_backward`] for the mutual-subClassOf
    /// expansion (`cax-eqc` produces both directions).
    pub fn equivalent_class_forward(owl_equivalent_class: u64, rdfs_sub_class_of: u64) -> Self {
        Self::new(
            "equivalent_class_forward",
            vec![RuleAtom::new(
                AtomField::Var(0),
                AtomField::Const(owl_equivalent_class),
                AtomField::Var(1),
            )],
            RuleAtom::new(
                AtomField::Var(0),
                AtomField::Const(rdfs_sub_class_of),
                AtomField::Var(1),
            ),
            2,
        )
    }

    /// OWL 2 RL `cax-eqc` (backward half): `?c1 owl:equivalentClass ?c2 →
    /// ?c2 rdfs:subClassOf ?c1`. See [`Rule::equivalent_class_forward`].
    pub fn equivalent_class_backward(owl_equivalent_class: u64, rdfs_sub_class_of: u64) -> Self {
        Self::new(
            "equivalent_class_backward",
            vec![RuleAtom::new(
                AtomField::Var(0),
                AtomField::Const(owl_equivalent_class),
                AtomField::Var(1),
            )],
            RuleAtom::new(
                AtomField::Var(1),
                AtomField::Const(rdfs_sub_class_of),
                AtomField::Var(0),
            ),
            2,
        )
    }

    /// OWL 2 RL `prp-eqp` (forward half): `?p1 owl:equivalentProperty ?p2 →
    /// ?p1 rdfs:subPropertyOf ?p2`. Pair with
    /// [`Rule::equivalent_property_backward`].
    pub fn equivalent_property_forward(
        owl_equivalent_property: u64,
        rdfs_sub_property_of: u64,
    ) -> Self {
        Self::new(
            "equivalent_property_forward",
            vec![RuleAtom::new(
                AtomField::Var(0),
                AtomField::Const(owl_equivalent_property),
                AtomField::Var(1),
            )],
            RuleAtom::new(
                AtomField::Var(0),
                AtomField::Const(rdfs_sub_property_of),
                AtomField::Var(1),
            ),
            2,
        )
    }

    /// OWL 2 RL `prp-eqp` (backward half): `?p1 owl:equivalentProperty ?p2 →
    /// ?p2 rdfs:subPropertyOf ?p1`. See
    /// [`Rule::equivalent_property_forward`].
    pub fn equivalent_property_backward(
        owl_equivalent_property: u64,
        rdfs_sub_property_of: u64,
    ) -> Self {
        Self::new(
            "equivalent_property_backward",
            vec![RuleAtom::new(
                AtomField::Var(0),
                AtomField::Const(owl_equivalent_property),
                AtomField::Var(1),
            )],
            RuleAtom::new(
                AtomField::Var(1),
                AtomField::Const(rdfs_sub_property_of),
                AtomField::Var(0),
            ),
            2,
        )
    }

    /// OWL 2 RL `prp-inv1`: `?p1 owl:inverseOf ?p2 ∧ ?x ?p1 ?y → ?y ?p2
    /// ?x`. The second body atom's predicate position is a variable (`?p1`,
    /// bound by the first atom) — a `RuleSet` wildcard, same shape as
    /// [`Rule::subproperty_propagation`]. Pair with
    /// [`Rule::inverse_backward`] (`prp-inv2`) for the other direction.
    pub fn inverse_forward(owl_inverse_of: u64) -> Self {
        Self::new(
            "inverse_forward",
            vec![
                RuleAtom::new(
                    AtomField::Var(0), // ?p1
                    AtomField::Const(owl_inverse_of),
                    AtomField::Var(1), // ?p2
                ),
                RuleAtom::new(
                    AtomField::Var(2), // ?x
                    AtomField::Var(0), // ?p1 (variable predicate position)
                    AtomField::Var(3), // ?y
                ),
            ],
            RuleAtom::new(
                AtomField::Var(3), // ?y
                AtomField::Var(1), // ?p2
                AtomField::Var(2), // ?x
            ),
            4,
        )
    }

    /// OWL 2 RL `prp-inv2`: `?p1 owl:inverseOf ?p2 ∧ ?x ?p2 ?y → ?y ?p1
    /// ?x`. See [`Rule::inverse_forward`] (`prp-inv1`) for the other
    /// direction.
    pub fn inverse_backward(owl_inverse_of: u64) -> Self {
        Self::new(
            "inverse_backward",
            vec![
                RuleAtom::new(
                    AtomField::Var(0), // ?p1
                    AtomField::Const(owl_inverse_of),
                    AtomField::Var(1), // ?p2
                ),
                RuleAtom::new(
                    AtomField::Var(2), // ?x
                    AtomField::Var(1), // ?p2 (variable predicate position)
                    AtomField::Var(3), // ?y
                ),
            ],
            RuleAtom::new(
                AtomField::Var(3), // ?y
                AtomField::Var(0), // ?p1
                AtomField::Var(2), // ?x
            ),
            4,
        )
    }

    /// Every constant predicate referenced by a body atom (deduplicated is
    /// the caller's job — see [`RuleSet::new`]).
    fn body_predicates(&self) -> impl Iterator<Item = u64> + '_ {
        self.body.iter().filter_map(|a| match a.p {
            AtomField::Const(id) => Some(id),
            AtomField::Var(_) => None,
        })
    }

    /// `true` if any body atom's predicate position is itself a variable
    /// (e.g. a generic `?s ?p ?o` rule) — such a rule cannot be indexed by a
    /// fixed predicate and must be treated as always-active.
    fn has_variable_predicate(&self) -> bool {
        self.body.iter().any(|a| matches!(a.p, AtomField::Var(_)))
    }
}

/// A collection of [`Rule`]s plus a predicate → rule-indices index, so a
/// fixpoint round can cheaply skip rules that provably cannot fire.
pub struct RuleSet {
    rules: Vec<Rule>,
    by_predicate: HashMap<u64, Vec<usize>>,
    wildcard_rules: Vec<usize>,
}

impl RuleSet {
    pub fn new(rules: Vec<Rule>) -> Self {
        let mut by_predicate: HashMap<u64, Vec<usize>> = HashMap::new();
        let mut wildcard_rules = Vec::new();

        for (idx, rule) in rules.iter().enumerate() {
            if rule.has_variable_predicate() {
                wildcard_rules.push(idx);
            }
            let mut seen: HashSet<u64> = HashSet::new();
            for pred in rule.body_predicates() {
                if seen.insert(pred) {
                    by_predicate.entry(pred).or_default().push(idx);
                }
            }
        }

        Self {
            rules,
            by_predicate,
            wildcard_rules,
        }
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Indices of every rule whose body *could* match at least one fact in
    /// `delta` (by constant predicate), plus every rule with a
    /// variable-predicate body atom (always considered active, since it
    /// cannot be predicate-indexed).
    ///
    /// A rule not returned here is guaranteed to produce no new facts this
    /// round — semi-naive evaluation only needs atoms that touch the
    /// current delta.
    pub fn active_rules(&self, delta: &[[u64; 3]]) -> Vec<usize> {
        let mut active: HashSet<usize> = self.wildcard_rules.iter().copied().collect();
        for row in delta {
            if let Some(idxs) = self.by_predicate.get(&row[1]) {
                active.extend(idxs.iter().copied());
            }
        }
        let mut out: Vec<usize> = active.into_iter().collect();
        out.sort_unstable();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_rules_finds_matching_predicate_only() {
        let sc = 100u64;
        let ty = 200u64;
        let rules = RuleSet::new(vec![Rule::transitive(sc), Rule::type_propagation(ty, sc)]);

        // A delta containing only a `sc`-predicate fact should activate both
        // rules (transitive's body uses `sc`; type_propagation's body also
        // references `sc` in its second atom).
        let active = rules.active_rules(&[[1, sc, 2]]);
        assert_eq!(active, vec![0, 1]);

        // A delta containing only a `ty`-predicate fact should activate only
        // type_propagation (index 1).
        let active = rules.active_rules(&[[1, ty, 2]]);
        assert_eq!(active, vec![1]);
    }

    #[test]
    fn active_rules_empty_delta_yields_no_rules() {
        let rules = RuleSet::new(vec![Rule::transitive(100)]);
        assert!(rules.active_rules(&[]).is_empty());
    }

    #[test]
    fn domain_rule_produces_type_triple() {
        let dom = 100u64;
        let ty = 200u64;
        let has_parent = 1u64;
        let person = 2u64;
        let alice = 10u64;
        let bob = 11u64;
        let base = vec![[has_parent, dom, person], [alice, has_parent, bob]];
        let rules = RuleSet::new(vec![Rule::domain(dom, ty)]);
        let closure = crate::fixpoint::closure(&rules, &base);
        assert!(closure.contains(&[alice, ty, person]));
    }

    /// Mimics `engine.rs`'s `closure_over_store` usage: only the
    /// `rdfs:domain` fact is in `seed`, while the `alice hasParent bob` fact
    /// is available *only* via `store` (the `AtomSource` side), never in
    /// `seed` — proving domain propagation doesn't need `hasParent` facts
    /// seeded, only scanned.
    #[test]
    fn domain_rule_over_store_like_engine() {
        let dom = 100u64;
        let ty = 200u64;
        let has_parent = 1u64;
        let person = 2u64;
        let alice = 10u64;
        let bob = 11u64;

        let store_rows = vec![[has_parent, dom, person], [alice, has_parent, bob]];
        let store = crate::join::SliceSource::new(&store_rows);
        let seed = vec![[has_parent, dom, person]];

        let rules = RuleSet::new(vec![Rule::domain(dom, ty)]);
        let closure = crate::fixpoint::closure_over_store(&rules, &store, &seed);
        assert!(closure.contains(&[alice, ty, person]));
    }

    /// Same as `domain_rule_over_store_like_engine`, but with BOTH `domain`
    /// and `range` rules registered simultaneously (two wildcard rules at
    /// once) — confirms two simultaneously-active variable-predicate rules
    /// don't interact badly.
    #[test]
    fn domain_and_range_rules_together_over_store() {
        let dom = 100u64;
        let rng = 101u64;
        let ty = 200u64;
        let has_parent = 1u64;
        let person = 2u64;
        let alice = 10u64;
        let bob = 11u64;

        let store_rows = vec![
            [has_parent, dom, person],
            [has_parent, rng, person],
            [alice, has_parent, bob],
        ];
        let store = crate::join::SliceSource::new(&store_rows);
        let seed = vec![[has_parent, dom, person], [has_parent, rng, person]];

        let rules = RuleSet::new(vec![Rule::domain(dom, ty), Rule::range(rng, ty)]);
        let closure = crate::fixpoint::closure_over_store(&rules, &store, &seed);
        assert!(closure.contains(&[alice, ty, person]), "domain-derived");
        assert!(closure.contains(&[bob, ty, person]), "range-derived");
    }
}
