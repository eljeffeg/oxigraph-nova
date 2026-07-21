//! End-to-end spike: two interacting rules — `rdfs:subClassOf` transitivity
//! plus `rdf:type` propagation through the subclass hierarchy — evaluated
//! directly over a real, compacted `LoudsStore`'s LOUDS index via
//! [`StoreAtomSource`], not copied `Vec` fixtures.
//!
//! This is the increment beyond `transitivity_spike.rs` that actually
//! exercises the production-shaped entry point: `fixpoint::closure_over_store`
//! + `StoreAtomSource` + `CombinedSource`.
//!
//! Every "Total" scan a rule body atom performs is answered by the store's
//! real LOUDS tries (via `LftjSource::lftj_join_scan`) unioned on the fly
//! with whatever has been derived so far this run — the base relation is
//! never materialized into a `Vec` up front.
//!
//! ## What this proves
//!
//! 1. `RuleSet` correctly dispatches two interacting rules (`transitive` +
//!    `type_propagation`) sharing one semi-naive fixpoint loop.
//! 2. `StoreAtomSource` correctly answers rule-body scans against a real
//!    compacted `LoudsStore` (three-level S/P/O LOUDS descent via
//!    `lftj_join_scan`), not a copied fixture.
//! 3. A type fact only reachable through a **derived** (not asserted)
//!    subClassOf edge (`Dog ⊑ Animal`, itself only derivable from `Dog ⊑
//!    Mammal ⊑ Animal`) is correctly inferred — proving the two rules
//!    genuinely interact through the shared `Total`/`Delta`, not just run
//!    independently.

use oxigraph_nova_core::{GraphName, LftjSource, NamedNode, Quad, QuadStore, Term};
use oxigraph_nova_engine_ring::LoudsStore;
use oxigraph_nova_reasoning::rule::{Rule, RuleSet};
use oxigraph_nova_reasoning::{StoreAtomSource, fixpoint};

const NS: &str = "http://example.org/";

fn iri(name: &str) -> NamedNode {
    NamedNode::new(format!("{NS}{name}")).unwrap()
}

fn sub_class_of() -> NamedNode {
    NamedNode::new("http://www.w3.org/2000/01/rdf-schema#subClassOf").unwrap()
}

fn rdf_type() -> NamedNode {
    NamedNode::new("http://www.w3.org/1999/02/22-rdf-syntax-ns#type").unwrap()
}

fn assert_subclass(store: &LoudsStore, sub: &str, sup: &str) {
    store
        .insert(&Quad::new(
            iri(sub),
            sub_class_of(),
            Term::NamedNode(iri(sup)),
            GraphName::DefaultGraph,
        ))
        .unwrap();
}

fn assert_type(store: &LoudsStore, instance: &str, class: &str) {
    store
        .insert(&Quad::new(
            iri(instance),
            rdf_type(),
            Term::NamedNode(iri(class)),
            GraphName::DefaultGraph,
        ))
        .unwrap();
}

#[test]
fn type_propagation_through_transitive_subclass_over_ringstore() {
    let store = LoudsStore::new();

    // Animal <- Mammal <- Dog (Dog ⊑ Animal is derivable only via the
    // transitive closure of the two asserted edges, never asserted
    // directly), plus fido : Dog.
    assert_subclass(&store, "Mammal", "Animal");
    assert_subclass(&store, "Dog", "Mammal");
    assert_type(&store, "fido", "Dog");

    // A disjoint branch that must not cross-pollinate.
    assert_subclass(&store, "Plant", "Organism");
    assert_type(&store, "oak", "Plant");

    store.compact().unwrap();

    let sc_id = store
        .lftj_intern_term(&Term::NamedNode(sub_class_of()))
        .expect("subClassOf must be interned after compact");
    let ty_id = store
        .lftj_intern_term(&Term::NamedNode(rdf_type()))
        .expect("rdf:type must be interned after compact");

    let default_graph_id = store.lftj_graph_id(&GraphName::DefaultGraph).unwrap();
    let store_src = StoreAtomSource::new(&store, default_graph_id);

    let rules = RuleSet::new(vec![
        Rule::transitive(sc_id),
        Rule::type_propagation(ty_id, sc_id),
    ]);

    // Seed the fixpoint's Delta with every base fact for the two predicates
    // the rules reference — a single targeted read via the ordinary
    // QuadStore API (not a full store scan). The semi-naive loop needs
    // these seeded explicitly because Delta (not just Total/StoreAtomSource)
    // is what actually triggers a derivation each round — see
    // `closure_over_store`'s doc comment.
    let seed_triples: Vec<[u64; 3]> = store
        .quads_for_pattern(None, None, None, Some(&GraphName::DefaultGraph))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .filter_map(|sq| {
            let s = store.lftj_intern_term(sq.subject.as_ref())?;
            let p = store.lftj_intern_term(&Term::NamedNode(sq.predicate.clone()))?;
            let o = store.lftj_intern_term(sq.object.as_ref())?;
            Some([s, p, o])
        })
        .collect();

    let mut closure = fixpoint::closure_over_store(&rules, &store_src, &seed_triples);
    closure.sort();

    let id_of = |name: &str| -> u64 {
        store
            .lftj_intern_term(&Term::NamedNode(iri(name)))
            .unwrap_or_else(|| panic!("{name} must be interned after compact"))
    };

    let (animal, mammal, dog, plant, organism) = (
        id_of("Animal"),
        id_of("Mammal"),
        id_of("Dog"),
        id_of("Plant"),
        id_of("Organism"),
    );
    let (fido, oak) = (id_of("fido"), id_of("oak"));

    let mut expected: Vec<[u64; 3]> = vec![
        [mammal, sc_id, animal],
        [dog, sc_id, mammal],
        [dog, sc_id, animal], // derived transitivity
        [plant, sc_id, organism],
        [fido, ty_id, dog],
        [fido, ty_id, mammal], // derived: one hop of type propagation
        [fido, ty_id, animal], // derived: through the *derived* subclass edge
        [oak, ty_id, plant],
        [oak, ty_id, organism], // derived, disjoint branch
    ];
    expected.sort();

    assert_eq!(
        closure, expected,
        "closure computed via StoreAtomSource over a real compacted LoudsStore must match \
         the hand-computed expected closure exactly"
    );

    // Spot-check the "interesting" long-range inference explicitly: fido
    // must be typed Animal even though that's neither an asserted type nor
    // an asserted subclass edge — it requires both rules firing across two
    // rounds of the shared fixpoint.
    assert!(
        closure.contains(&[fido, ty_id, animal]),
        "fido must be inferred rdf:type Animal via Dog ⊑ Mammal ⊑ Animal"
    );

    // And the disjoint Plant/Organism branch must not have leaked any
    // Animal-side facts (no cross-pollination between unrelated rule runs).
    assert!(!closure.contains(&[oak, ty_id, animal]));
    assert!(!closure.contains(&[fido, ty_id, organism]));
}
