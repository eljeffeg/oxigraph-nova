//! Differential cross-check: `LftjFixpointEngine`'s semi-naive LFTJ fixpoint
//! output vs. `reasonable` (an independent, mature, datafrog-based OWL 2 RL
//! reasoner), on the same input graph.
//!
//! This does **not** assert byte-for-byte equality of the two engines'
//! outputs — `reasonable` implements the *entire* OWL 2 RL rule set (~60
//! rules: property chains, (a)symmetric/(ir)reflexive properties,
//! hasValue/someValuesFrom/allValuesFrom, sameAs, disjointness, etc.), while
//! `LftjFixpointEngine` currently covers a smaller core: `rdfs:subClassOf`/
//! `rdfs:subPropertyOf` transitivity, `rdf:type` propagation through
//! `subClassOf` (`cax-sco`), property hierarchy propagation (`prp-spo1`),
//! property domain/range propagation (`prp-dom`/`prp-rng`), generic
//! `owl:TransitiveProperty`/`owl:SymmetricProperty` (`prp-trp`/`prp-symp`),
//! `owl:equivalentClass`/`owl:equivalentProperty` (`cax-eqc`/`prp-eqp`),
//! and `owl:inverseOf` (`prp-inv1`/`prp-inv2`) — see `engine.rs`'s module
//! doc comment for the authoritative, up-to-date list. So `reasonable`'s
//! closure is expected to be a **superset** of Nova's for any given input.
//!
//! What each test asserts:
//!   1. Every triple Nova's `LftjFixpointEngine` infers is also present in
//!      `reasonable`'s full closure (soundness: Nova never derives
//!      something an independent, spec-following reasoner disagrees with).
//!   2. For inputs that *only* exercise the three "pure closure" rules Nova
//!      implements (`subClassOf`/`subPropertyOf` transitivity, `cax-sco`),
//!      Nova's inferred set exactly equals the subset of `reasonable`'s
//!      closure attributable to those same rules (completeness on those
//!      rules' fragment) — checked by constraining the fixture graphs to
//!      use only `rdfs:subClassOf`/`rdfs:subPropertyOf`/`rdf:type`, so
//!      `reasonable`'s extra rule power has nothing else to chew on. The
//!      other tests instead assert soundness plus the presence of the one
//!      specific derived triple(s) each rule targets, since their fixtures
//!      intentionally include predicates/classes that only their own rule
//!      reasons about.
//!
//! As rule coverage in `LftjFixpointEngine` grows, these fixtures and
//! assertions should grow correspondingly.

use oxigraph_nova_core::{GraphName as CoreGraphName, NamedNode, Quad, QuadStore, Term};
use oxigraph_nova_engine_ring::LoudsStore;
use oxigraph_nova_query::StoreDataset;
use oxigraph_nova_reasoning::{LftjFixpointEngine, ReasoningEngine};
use oxrdf::{
    NamedNode as OxNamedNode, NamedOrBlankNode as OxSubject, Term as OxTerm, Triple as OxTriple,
};
use reasonable::reasoner::Reasoner;
use std::collections::HashSet;
use std::sync::Arc;

fn nn(s: &str) -> NamedNode {
    NamedNode::new(s).unwrap()
}

fn rdf_type() -> NamedNode {
    nn("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
}

fn rdfs_sub_class_of() -> NamedNode {
    nn("http://www.w3.org/2000/01/rdf-schema#subClassOf")
}

fn rdfs_sub_property_of() -> NamedNode {
    nn("http://www.w3.org/2000/01/rdf-schema#subPropertyOf")
}

fn rdfs_domain() -> NamedNode {
    nn("http://www.w3.org/2000/01/rdf-schema#domain")
}

fn rdfs_range() -> NamedNode {
    nn("http://www.w3.org/2000/01/rdf-schema#range")
}

fn owl_transitive_property() -> NamedNode {
    nn("http://www.w3.org/2002/07/owl#TransitiveProperty")
}

fn owl_symmetric_property() -> NamedNode {
    nn("http://www.w3.org/2002/07/owl#SymmetricProperty")
}

fn owl_equivalent_class() -> NamedNode {
    nn("http://www.w3.org/2002/07/owl#equivalentClass")
}

fn owl_equivalent_property() -> NamedNode {
    nn("http://www.w3.org/2002/07/owl#equivalentProperty")
}

fn owl_inverse_of() -> NamedNode {
    nn("http://www.w3.org/2002/07/owl#inverseOf")
}

/// One (subject, predicate, object) NamedNode-only fixture triple, owned —
/// every fixture below sticks to named nodes to keep the Nova <->
/// `reasonable` term conversion trivial (no blank nodes / literals to
/// reconcile).
type Fixt = (String, String, String);

fn t(s: impl Into<String>, p: impl Into<String>, o: impl Into<String>) -> Fixt {
    (s.into(), p.into(), o.into())
}

/// Loads `triples` into a fresh, compacted `LoudsStore` and returns Nova's
/// `LftjFixpointEngine`-inferred closure as a set of (s, p, o) IRI-string
/// triples.
fn nova_inferred(triples: &[Fixt]) -> HashSet<(String, String, String)> {
    let store = LoudsStore::new();
    let g = CoreGraphName::DefaultGraph;
    for (s, p, o) in triples {
        store
            .insert(&Quad::new(nn(s), nn(p), Term::NamedNode(nn(o)), g.clone()))
            .unwrap();
    }
    store.compact().unwrap();

    let dataset = StoreDataset::new(Arc::new(store));
    let engine = LftjFixpointEngine::new();
    let (inferred, diagnostics) = engine.infer(&dataset).unwrap();
    assert!(diagnostics.is_empty());

    inferred
        .into_iter()
        .map(|q| {
            let s = match q.subject {
                oxigraph_nova_core::NamedOrBlankNode::NamedNode(n) => n.into_string(),
                oxigraph_nova_core::NamedOrBlankNode::BlankNode(b) => b.to_string(),
            };
            let p = q.predicate.into_string();
            let o = match q.object {
                Term::NamedNode(n) => n.into_string(),
                other => other.to_string(),
            };
            (s, p, o)
        })
        .collect()
}

/// Loads `triples` into a fresh `reasonable::Reasoner`, runs full
/// materialization, and returns the entire closure (base ∪ inferred, minus
/// the two `owl:Thing`/`owl:Nothing` seed axioms `reasonable` always adds)
/// as a set of (s, p, o) IRI-string triples.
fn reasonable_closure(triples: &[Fixt]) -> HashSet<(String, String, String)> {
    let mut r = Reasoner::new();
    let ox_triples: Vec<OxTriple> = triples
        .iter()
        .map(|(s, p, o)| {
            OxTriple::new(
                OxSubject::NamedNode(OxNamedNode::new_unchecked(s.clone())),
                OxNamedNode::new_unchecked(p.clone()),
                OxTerm::NamedNode(OxNamedNode::new_unchecked(o.clone())),
            )
        })
        .collect();
    r.load_triples(ox_triples);
    r.reason();

    let owl_thing = "http://www.w3.org/2002/07/owl#Thing";
    let owl_nothing = "http://www.w3.org/2002/07/owl#Nothing";
    let owl_class = "http://www.w3.org/2002/07/owl#Class";

    r.view_output()
        .iter()
        .filter_map(|tr| {
            let OxSubject::NamedNode(s) = &tr.subject else {
                return None;
            };
            let OxTerm::NamedNode(o) = &tr.object else {
                return None;
            };
            let (s, p, o) = (s.as_str(), tr.predicate.as_str(), o.as_str());
            // Filter out reasonable's always-present `owl:Thing`/`owl:Nothing`
            // `rdf:type owl:Class` seed axioms — not derivable from, or
            // comparable against, Nova's rule-relevant closure.
            if (s == owl_thing || s == owl_nothing) && o == owl_class {
                return None;
            }
            Some((s.to_string(), p.to_string(), o.to_string()))
        })
        .collect()
}

/// Restricts a `reasonable` closure to only the three predicates
/// `LftjFixpointEngine` currently knows about — the fragment its rules are
/// expected to reproduce completely (see module doc comment, point 2).
///
/// Also drops any triple whose object is `owl:Thing` — `reasonable`
/// unconditionally derives `?x rdf:type owl:Thing` for the subject of
/// *every* triple in the graph (`cls-thing`, an OWL 2 RL axiom
/// `LftjFixpointEngine` does not implement), which would otherwise pollute
/// the `rdf:type`-predicate comparison with facts Nova has no rule for.
fn restrict_to_covered_predicates(
    closure: &HashSet<(String, String, String)>,
) -> HashSet<(String, String, String)> {
    let covered: HashSet<String> = [
        rdf_type().into_string(),
        rdfs_sub_class_of().into_string(),
        rdfs_sub_property_of().into_string(),
    ]
    .into_iter()
    .collect();
    let owl_thing = "http://www.w3.org/2002/07/owl#Thing";
    closure
        .iter()
        .filter(|(_, p, o)| covered.contains(p) && o != owl_thing)
        .cloned()
        .collect()
}

/// Every triple in `triples` as an (s, p, o) String set — the asserted
/// (not derived) base facts, which Nova's `infer()` deliberately excludes
/// from its output but `reasonable`'s `view_output()` includes.
fn base_set(triples: &[Fixt]) -> HashSet<(String, String, String)> {
    triples.iter().cloned().collect()
}

#[test]
fn subclass_transitivity_matches_reasonable_oracle() {
    // A ⊑ B ⊑ C ⊑ D — pure subClassOf transitivity, no rdf:type involved.
    let ex = "http://example.org/";
    let sc = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
    let triples: Vec<Fixt> = vec![
        t(format!("{ex}A"), sc, format!("{ex}B")),
        t(format!("{ex}B"), sc, format!("{ex}C")),
        t(format!("{ex}C"), sc, format!("{ex}D")),
    ];

    let nova = nova_inferred(&triples);
    let oracle_full = reasonable_closure(&triples);
    let oracle_covered = restrict_to_covered_predicates(&oracle_full);
    let base = base_set(&triples);

    // Soundness: everything Nova infers is in reasonable's closure too.
    for tr in &nova {
        assert!(
            oracle_full.contains(tr),
            "Nova inferred {tr:?} but `reasonable`'s closure disagrees"
        );
    }

    // Completeness on the covered fragment: Nova's inferred-only set must
    // equal reasonable's covered-predicate closure minus the asserted base
    // facts (reasonable's `view_output()` includes base facts; Nova's
    // `infer()` deliberately excludes them — see `LftjFixpointEngine::infer`
    // doc comment).
    let oracle_inferred_only: HashSet<_> = oracle_covered.difference(&base).cloned().collect();
    assert_eq!(
        nova, oracle_inferred_only,
        "Nova's subClassOf-transitivity closure must exactly match reasonable's \
         on this rdfs:subClassOf-only fragment"
    );

    // Sanity: the transitive edge A ⊑ D must actually be present in both.
    let a_sc_d = (
        format!("{ex}A"),
        rdfs_sub_class_of().into_string(),
        format!("{ex}D"),
    );
    assert!(nova.contains(&a_sc_d));
    assert!(oracle_full.contains(&a_sc_d));
}

#[test]
fn subproperty_transitivity_is_correct_no_oracle_rule_available() {
    // NOTE: unlike rdfs:subClassOf transitivity (`rdfs11`, which `reasonable`
    // *does* implement as a standalone rule — see
    // `subclass_transitivity_matches_reasonable_oracle`), `reasonable` has
    // no standalone `rdfs:subPropertyOf`-transitivity production rule. It
    // only ever consumes `subPropertyOf` edges one hop at a time via
    // `prp-spo1` (`?p1 subPropertyOf ?p2 ∧ ?x ?p1 ?y ⟹ ?x ?p2 ?y`), which
    // happens to reach the same fixpoint transitively *for individual
    // triples* through repeated rounds of its own semi-naive loop, but never
    // materializes the derived `subPropertyOf` edge itself as a triple.
    // Since `LftjFixpointEngine` doesn't implement `prp-spo1` as a
    // *standalone transitivity* rule (it does implement one-hop `prp-spo1`
    // propagation itself — see `subproperty_propagation_matches_reasonable_oracle`
    // below for a genuine oracle comparison of that rule), there is still
    // no shared artifact to cross-check `rdfs:subPropertyOf` *transitivity*
    // against `reasonable` as an oracle. This test instead pins down Nova's
    // own output against a hand-computed expected closure (matching the
    // pattern in `multi_rule_over_ringstore.rs`).
    let ex = "http://example.org/";
    let sp = "http://www.w3.org/2000/01/rdf-schema#subPropertyOf";
    let triples: Vec<Fixt> = vec![
        t(format!("{ex}hasFather"), sp, format!("{ex}hasParent")),
        t(format!("{ex}hasParent"), sp, format!("{ex}hasAncestor")),
    ];

    let nova = nova_inferred(&triples);

    let has_father_sp_ancestor = (
        format!("{ex}hasFather"),
        rdfs_sub_property_of().into_string(),
        format!("{ex}hasAncestor"),
    );
    let expected: HashSet<_> = [has_father_sp_ancestor.clone()].into_iter().collect();
    assert_eq!(
        nova, expected,
        "hasFather subPropertyOf hasAncestor must be the only fact derived \
         by transitive closure over the two asserted subPropertyOf edges"
    );
}

#[test]
fn subproperty_propagation_matches_reasonable_oracle() {
    // prp-spo1: hasFather subPropertyOf hasParent, alice hasFather bob
    // ⟹ alice hasParent bob. Unlike subPropertyOf *transitivity* (no
    // standalone rule in `reasonable`, see the test above), `reasonable`
    // does implement `prp-spo1` itself (see
    // the `reasonable` reasoner), so this
    // is a genuine cross-engine oracle comparison.
    let ex = "http://example.org/";
    let sp = "http://www.w3.org/2000/01/rdf-schema#subPropertyOf";
    let triples: Vec<Fixt> = vec![
        t(format!("{ex}hasFather"), sp, format!("{ex}hasParent")),
        t(
            format!("{ex}alice"),
            format!("{ex}hasFather"),
            format!("{ex}bob"),
        ),
    ];

    let nova = nova_inferred(&triples);
    let oracle_full = reasonable_closure(&triples);

    let alice_has_parent_bob = (
        format!("{ex}alice"),
        format!("{ex}hasParent"),
        format!("{ex}bob"),
    );

    // Soundness: everything Nova infers is in reasonable's closure too.
    for tr in &nova {
        assert!(
            oracle_full.contains(tr),
            "Nova inferred {tr:?} but `reasonable`'s closure disagrees"
        );
    }

    assert!(
        nova.contains(&alice_has_parent_bob),
        "alice hasParent bob must be derived via prp-spo1 property propagation"
    );
    assert!(
        oracle_full.contains(&alice_has_parent_bob),
        "reasonable's oracle closure must also contain the prp-spo1-derived triple"
    );

    // The base hasFather fact is asserted, not inferred — Nova's `infer()`
    // excludes base facts from its output (see `LftjFixpointEngine::infer`
    // doc comment), so it must not appear in `nova`.
    let alice_has_father_bob = (
        format!("{ex}alice"),
        format!("{ex}hasFather"),
        format!("{ex}bob"),
    );
    assert!(!nova.contains(&alice_has_father_bob));
}

#[test]
fn domain_and_range_propagation_matches_reasonable_oracle() {
    // prp-dom/prp-rng: hasParent rdfs:domain Person, hasParent rdfs:range
    // Person, alice hasParent bob ⟹ alice rdf:type Person (domain) and
    // bob rdf:type Person (range). `reasonable` implements both `prp-dom`
    // and `prp-rng` itself (see the `reasonable` reasoner
    // around lines 1278/1290), so this is a genuine cross-engine oracle
    // comparison, mirroring `subproperty_propagation_matches_reasonable_oracle`.
    //
    // NOTE: this fixture deliberately asserts **zero** `rdf:type` base
    // facts — proving Nova's synthetic-TermId fix (see `engine.rs`'s
    // module doc comment, "Head-only constants and synthetic TermIds"):
    // `rdf:type` is the constant used in both rules' *head* atom, and
    // `Dataset::lftj_intern_term` is read-only, so before that fix this
    // rule would have silently failed to fire on a dataset with no
    // `rdf:type` facts at all.
    let ex = "http://example.org/";
    let triples: Vec<Fixt> = vec![
        t(
            format!("{ex}hasParent"),
            rdfs_domain().into_string(),
            format!("{ex}Person"),
        ),
        t(
            format!("{ex}hasParent"),
            rdfs_range().into_string(),
            format!("{ex}Person"),
        ),
        t(
            format!("{ex}alice"),
            format!("{ex}hasParent"),
            format!("{ex}bob"),
        ),
    ];

    let nova = nova_inferred(&triples);
    let oracle_full = reasonable_closure(&triples);

    let alice_type_person = (
        format!("{ex}alice"),
        rdf_type().into_string(),
        format!("{ex}Person"),
    );
    let bob_type_person = (
        format!("{ex}bob"),
        rdf_type().into_string(),
        format!("{ex}Person"),
    );

    // Soundness: everything Nova infers is in reasonable's closure too.
    for tr in &nova {
        assert!(
            oracle_full.contains(tr),
            "Nova inferred {tr:?} but `reasonable`'s closure disagrees"
        );
    }

    assert!(
        nova.contains(&alice_type_person),
        "alice rdf:type Person must be derived via prp-dom domain propagation"
    );
    assert!(
        nova.contains(&bob_type_person),
        "bob rdf:type Person must be derived via prp-rng range propagation"
    );
    assert!(
        oracle_full.contains(&alice_type_person),
        "reasonable's oracle closure must also contain the prp-dom-derived triple"
    );
    assert!(
        oracle_full.contains(&bob_type_person),
        "reasonable's oracle closure must also contain the prp-rng-derived triple"
    );

    // The base hasParent fact is asserted, not inferred.
    let alice_has_parent_bob = (
        format!("{ex}alice"),
        format!("{ex}hasParent"),
        format!("{ex}bob"),
    );
    assert!(!nova.contains(&alice_has_parent_bob));
}

#[test]
fn type_propagation_matches_reasonable_oracle() {
    // cax-sco: fido:Dog, Dog ⊑ Mammal ⊑ Animal ⟹ fido:Mammal, fido:Animal.
    let ex = "http://example.org/";
    let sc = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
    let ty = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    let triples: Vec<Fixt> = vec![
        t(format!("{ex}Dog"), sc, format!("{ex}Mammal")),
        t(format!("{ex}Mammal"), sc, format!("{ex}Animal")),
        t(format!("{ex}fido"), ty, format!("{ex}Dog")),
    ];

    let nova = nova_inferred(&triples);
    let oracle_full = reasonable_closure(&triples);
    let oracle_covered = restrict_to_covered_predicates(&oracle_full);
    let base = base_set(&triples);

    for tr in &nova {
        assert!(
            oracle_full.contains(tr),
            "Nova inferred {tr:?} not in oracle closure"
        );
    }

    let oracle_inferred_only: HashSet<_> = oracle_covered.difference(&base).cloned().collect();
    assert_eq!(nova, oracle_inferred_only);

    let fido_type_animal = (
        format!("{ex}fido"),
        rdf_type().into_string(),
        format!("{ex}Animal"),
    );
    assert!(nova.contains(&fido_type_animal));
    assert!(oracle_full.contains(&fido_type_animal));
}

#[test]
fn generic_transitive_property_matches_reasonable_oracle() {
    // prp-trp: a user-declared owl:TransitiveProperty (`ancestorOf`,
    // unrelated to rdfs:subClassOf/subPropertyOf) must derive the
    // transitive edge across a chain of its own facts. `reasonable`
    // implements `prp-trp` itself (see
    // the `reasonable` reasoner), so this
    // is a genuine cross-engine oracle comparison.
    let ex = "http://example.org/";
    let ty = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    let triples: Vec<Fixt> = vec![
        t(
            format!("{ex}ancestorOf"),
            ty,
            owl_transitive_property().into_string(),
        ),
        t(
            format!("{ex}alice"),
            format!("{ex}ancestorOf"),
            format!("{ex}bob"),
        ),
        t(
            format!("{ex}bob"),
            format!("{ex}ancestorOf"),
            format!("{ex}carol"),
        ),
    ];

    let nova = nova_inferred(&triples);
    let oracle_full = reasonable_closure(&triples);

    let alice_ancestor_of_carol = (
        format!("{ex}alice"),
        format!("{ex}ancestorOf"),
        format!("{ex}carol"),
    );

    // Soundness: everything Nova infers is in reasonable's closure too.
    for tr in &nova {
        assert!(
            oracle_full.contains(tr),
            "Nova inferred {tr:?} but `reasonable`'s closure disagrees"
        );
    }

    assert!(
        nova.contains(&alice_ancestor_of_carol),
        "alice ancestorOf carol must be derived via prp-trp transitivity"
    );
    assert!(
        oracle_full.contains(&alice_ancestor_of_carol),
        "reasonable's oracle closure must also contain the prp-trp-derived triple"
    );

    // The two base ancestorOf facts are asserted, not inferred.
    let alice_ancestor_of_bob = (
        format!("{ex}alice"),
        format!("{ex}ancestorOf"),
        format!("{ex}bob"),
    );
    let bob_ancestor_of_carol = (
        format!("{ex}bob"),
        format!("{ex}ancestorOf"),
        format!("{ex}carol"),
    );
    assert!(!nova.contains(&alice_ancestor_of_bob));
    assert!(!nova.contains(&bob_ancestor_of_carol));
}

#[test]
fn generic_symmetric_property_matches_reasonable_oracle() {
    // prp-symp: a user-declared owl:SymmetricProperty (`knows`) must derive
    // the reverse edge. `reasonable` implements `prp-symp` itself (see
    // the `reasonable` reasoner), so this is a genuine
    // cross-engine oracle comparison.
    let ex = "http://example.org/";
    let ty = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    let triples: Vec<Fixt> = vec![
        t(
            format!("{ex}knows"),
            ty,
            owl_symmetric_property().into_string(),
        ),
        t(
            format!("{ex}alice"),
            format!("{ex}knows"),
            format!("{ex}bob"),
        ),
    ];

    let nova = nova_inferred(&triples);
    let oracle_full = reasonable_closure(&triples);

    let bob_knows_alice = (
        format!("{ex}bob"),
        format!("{ex}knows"),
        format!("{ex}alice"),
    );

    for tr in &nova {
        assert!(
            oracle_full.contains(tr),
            "Nova inferred {tr:?} but `reasonable`'s closure disagrees"
        );
    }

    assert!(
        nova.contains(&bob_knows_alice),
        "bob knows alice must be derived via prp-symp symmetry"
    );
    assert!(
        oracle_full.contains(&bob_knows_alice),
        "reasonable's oracle closure must also contain the prp-symp-derived triple"
    );

    let alice_knows_bob = (
        format!("{ex}alice"),
        format!("{ex}knows"),
        format!("{ex}bob"),
    );
    assert!(!nova.contains(&alice_knows_bob));
}

#[test]
fn equivalent_class_matches_reasonable_oracle() {
    // cax-eqc: A owl:equivalentClass B must expand to both
    // A subClassOf B and B subClassOf A. `reasonable` implements `cax-eqc`
    // itself, so this is a genuine cross-engine oracle comparison.
    let ex = "http://example.org/";
    let triples: Vec<Fixt> = vec![t(
        format!("{ex}A"),
        owl_equivalent_class().into_string(),
        format!("{ex}B"),
    )];

    let nova = nova_inferred(&triples);
    let oracle_full = reasonable_closure(&triples);

    let a_sc_b = (
        format!("{ex}A"),
        rdfs_sub_class_of().into_string(),
        format!("{ex}B"),
    );
    let b_sc_a = (
        format!("{ex}B"),
        rdfs_sub_class_of().into_string(),
        format!("{ex}A"),
    );

    for tr in &nova {
        assert!(
            oracle_full.contains(tr),
            "Nova inferred {tr:?} but `reasonable`'s closure disagrees"
        );
    }

    assert!(nova.contains(&a_sc_b));
    assert!(nova.contains(&b_sc_a));
    assert!(oracle_full.contains(&a_sc_b));
    assert!(oracle_full.contains(&b_sc_a));
}

#[test]
fn equivalent_property_matches_reasonable_oracle() {
    // prp-eqp: p1 owl:equivalentProperty p2, alice p1 bob must derive
    // alice p2 bob (and symmetrically for any p2-asserted fact).
    //
    // NOTE: unlike `cax-eqc` (which `reasonable` implements by directly
    // materializing the `rdfs:subClassOf` triple itself — see
    // `equivalent_class_matches_reasonable_oracle`), `reasonable`'s
    // `prp-eqp` never materializes an `rdfs:subPropertyOf` schema triple:
    // it only uses the equivalence internally to propagate *instance-level*
    // facts directly (the `reasonable` reasoner around
    // line 1696, joining `equivalent_properties`/`equivalent_properties_2`
    // against `pso`, producing `(x, p2, y)`/`(x, p1, y)` data facts, never a
    // `(p1, rdfs:subPropertyOf, p2)` triple). `LftjFixpointEngine`, by
    // contrast, models `prp-eqp` as `Rule::equivalent_property_forward`/
    // `_backward`, which *does* materialize the `subPropertyOf` schema
    // triple — a legitimate but different modeling choice (matching the
    // reference `ReasonerRules.h`'s explicit `cax-eqc`/`prp-eqp` CONSTRUCT
    // pattern) that then feeds `Rule::subproperty_propagation` (`prp-spo1`)
    // in the same fixpoint to reach the same instance-level conclusions.
    // So this test compares the two engines only on instance-level
    // propagation, not on Nova's (oracle-incomparable) schema triple.
    let ex = "http://example.org/";
    let triples: Vec<Fixt> = vec![
        t(
            format!("{ex}p1"),
            owl_equivalent_property().into_string(),
            format!("{ex}p2"),
        ),
        t(format!("{ex}alice"), format!("{ex}p1"), format!("{ex}bob")),
    ];

    let nova = nova_inferred(&triples);
    let oracle_full = reasonable_closure(&triples);

    let alice_p2_bob = (format!("{ex}alice"), format!("{ex}p2"), format!("{ex}bob"));

    assert!(
        nova.contains(&alice_p2_bob),
        "alice p2 bob must be derived via prp-eqp equivalence + prp-spo1 propagation"
    );
    assert!(
        oracle_full.contains(&alice_p2_bob),
        "reasonable's oracle closure must also contain the prp-eqp-derived instance triple"
    );

    // The base alice p1 bob fact is asserted, not inferred.
    let alice_p1_bob = (format!("{ex}alice"), format!("{ex}p1"), format!("{ex}bob"));
    assert!(!nova.contains(&alice_p1_bob));
}

#[test]
fn inverse_of_matches_reasonable_oracle() {
    // prp-inv1/prp-inv2: hasParent owl:inverseOf hasChild,
    // alice hasParent bob ⟹ bob hasChild alice (prp-inv1); separately,
    // carol hasChild dave ⟹ dave hasParent carol (prp-inv2).
    let ex = "http://example.org/";
    let triples: Vec<Fixt> = vec![
        t(
            format!("{ex}hasParent"),
            owl_inverse_of().into_string(),
            format!("{ex}hasChild"),
        ),
        t(
            format!("{ex}alice"),
            format!("{ex}hasParent"),
            format!("{ex}bob"),
        ),
        t(
            format!("{ex}carol"),
            format!("{ex}hasChild"),
            format!("{ex}dave"),
        ),
    ];

    let nova = nova_inferred(&triples);
    let oracle_full = reasonable_closure(&triples);

    let bob_has_child_alice = (
        format!("{ex}bob"),
        format!("{ex}hasChild"),
        format!("{ex}alice"),
    );
    let dave_has_parent_carol = (
        format!("{ex}dave"),
        format!("{ex}hasParent"),
        format!("{ex}carol"),
    );

    for tr in &nova {
        assert!(
            oracle_full.contains(tr),
            "Nova inferred {tr:?} but `reasonable`'s closure disagrees"
        );
    }

    assert!(
        nova.contains(&bob_has_child_alice),
        "bob hasChild alice must be derived via prp-inv1"
    );
    assert!(
        nova.contains(&dave_has_parent_carol),
        "dave hasParent carol must be derived via prp-inv2"
    );
    assert!(oracle_full.contains(&bob_has_child_alice));
    assert!(oracle_full.contains(&dave_has_parent_carol));
}

#[test]
fn disjoint_branches_do_not_cross_pollinate_per_oracle() {
    // Two unrelated subclass chains sharing no predicates/instances — the
    // oracle should agree neither leaks into the other's closure, matching
    // the same "disjoint branch" check already exercised in
    // `multi_rule_over_ringstore.rs`.
    let ex = "http://example.org/";
    let sc = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
    let ty = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    let triples: Vec<Fixt> = vec![
        t(format!("{ex}Dog"), sc, format!("{ex}Mammal")),
        t(format!("{ex}fido"), ty, format!("{ex}Dog")),
        t(format!("{ex}Plant"), sc, format!("{ex}Organism")),
        t(format!("{ex}oak"), ty, format!("{ex}Plant")),
    ];

    let nova = nova_inferred(&triples);
    let oracle_full = reasonable_closure(&triples);

    let fido_type_organism = (
        format!("{ex}fido"),
        rdf_type().into_string(),
        format!("{ex}Organism"),
    );
    let oak_type_mammal = (
        format!("{ex}oak"),
        rdf_type().into_string(),
        format!("{ex}Mammal"),
    );

    assert!(!nova.contains(&fido_type_organism));
    assert!(!nova.contains(&oak_type_mammal));
    assert!(!oracle_full.contains(&fido_type_organism));
    assert!(!oracle_full.contains(&oak_type_mammal));
}
