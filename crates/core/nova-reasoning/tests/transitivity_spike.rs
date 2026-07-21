//! End-to-end integration: `rdfs:subClassOf` transitivity over a real
//! `LoudsStore`.
//!
//! Exercises `SortedVecTrie` + heterogeneous-source `leapfrog_join` +
//! semi-naive `transitive_closure` against Nova's storage backend (not just
//! synthetic `[u64; 3]` fixtures — see unit tests in
//! `fixpoint.rs`/`join.rs`/`sorted_vec_trie.rs` for those). Independent of
//! the production overlay path `oxigraph_nova_reasoning::ReasoningDataset`.
//!
//! ## Coverage
//!
//! 1. Base facts loaded into a `LoudsStore` and compacted can be read back
//!    out as `TermId`s via `LftjSource::lftj_intern_term` (the same
//!    interning path the LFTJ evaluator uses).
//! 2. `fixpoint::transitive_closure` computes the exact expected transitive
//!    closure over those `TermId`s.
//! 3. The derived closure can be decoded back to `Term`s
//!    (`LftjSource::lftj_decode_term`) and round-tripped into the store as
//!    new quads in a dedicated inference graph, then read back out via the
//!    ordinary `QuadStore::quads_for_pattern` path and matches exactly.
//!
//! ## Named-graph writeback
//!
//! There is no reserved `GraphId` for OWL 2 RL inference output —
//! `ReasoningDataset` keeps derived facts as an in-memory overlay and never
//! writes them back. This test instead writes its derived closure into an
//! ordinary dedicated named graph (`urn:nova:inference`), assigned whatever
//! `GraphId` `Dictionary::intern_graph` hands out — the insert/compact
//! mechanics don't depend on which id that is.

use oxigraph_nova_core::{GraphName, LftjSource, NamedNode, Quad, QuadStore, Term};
use oxigraph_nova_engine_ring::LoudsStore;
use oxigraph_nova_reasoning::fixpoint::transitive_closure;
use oxigraph_nova_reasoning::rule::Rule;

const NS: &str = "http://example.org/";

fn class(name: &str) -> NamedNode {
    NamedNode::new(format!("{NS}{name}")).unwrap()
}

fn sub_class_of() -> NamedNode {
    NamedNode::new("http://www.w3.org/2000/01/rdf-schema#subClassOf").unwrap()
}

fn inference_graph() -> GraphName {
    GraphName::NamedNode(NamedNode::new("urn:nova:inference").unwrap())
}

/// Insert `sub ⊑ sup` (`sub rdfs:subClassOf sup`) into the store's default
/// graph.
fn assert_subclass(store: &LoudsStore, sub: &str, sup: &str) {
    let quad = Quad::new(
        class(sub),
        sub_class_of(),
        Term::NamedNode(class(sup)),
        GraphName::DefaultGraph,
    );
    store.insert(&quad).unwrap();
}

#[test]
fn subclassof_transitivity_end_to_end_over_ringstore() {
    let store = LoudsStore::new();

    // A ⊑ B ⊑ C ⊑ D chain, plus a disjoint E ⊑ F edge that must not
    // cross-pollinate into the A..D closure.
    assert_subclass(&store, "A", "B");
    assert_subclass(&store, "B", "C");
    assert_subclass(&store, "C", "D");
    assert_subclass(&store, "E", "F");

    // Compact so terms are in the stable, queryable dictionary (mirrors the
    // real reasoner's "run over a settled Ring, not a live delta" model —
    // see `sorted_vec_trie`'s module docs for why a non-empty delta is
    // avoided during the join itself).
    store.compact().unwrap();

    // ── Intern known constants to their TermIds ─────────────────────────
    let sc_id = store
        .lftj_intern_term(&Term::NamedNode(sub_class_of()))
        .expect("predicate must be interned after compact");

    let id_of = |name: &str| -> u64 {
        store
            .lftj_intern_term(&Term::NamedNode(class(name)))
            .unwrap_or_else(|| panic!("class {name} must be interned after compact"))
    };
    let (a, b, c, d, e, f) = (
        id_of("A"),
        id_of("B"),
        id_of("C"),
        id_of("D"),
        id_of("E"),
        id_of("F"),
    );

    let base_triples: Vec<[u64; 3]> =
        vec![[a, sc_id, b], [b, sc_id, c], [c, sc_id, d], [e, sc_id, f]];

    // ── Run the semi-naive fixpoint ──────────────────────────────────────
    let rule = Rule::transitive(sc_id);
    let mut closure = transitive_closure(rule, &base_triples);
    closure.sort();

    let mut expected: Vec<[u64; 3]> = vec![
        [a, sc_id, b],
        [a, sc_id, c],
        [a, sc_id, d],
        [b, sc_id, c],
        [b, sc_id, d],
        [c, sc_id, d],
        [e, sc_id, f],
    ];
    expected.sort();
    assert_eq!(
        closure, expected,
        "transitive closure over LoudsStore-interned ids must match hand-computed closure exactly"
    );

    // ── Decode the *newly derived* triples (closure minus base) and write
    //    them into a dedicated inference graph ──────────────────────────
    let base_set: std::collections::BTreeSet<[u64; 3]> = base_triples.iter().copied().collect();
    let derived: Vec<[u64; 3]> = closure
        .iter()
        .copied()
        .filter(|t| !base_set.contains(t))
        .collect();
    // For a 3-chain + 1 disjoint edge, exactly 3 new pairs are derivable:
    // A⊑C, A⊑D, B⊑D.
    assert_eq!(derived.len(), 3);

    let graph = inference_graph();
    for [s, p, o] in &derived {
        let s_term = store.lftj_decode_term(*s).expect("decode subject");
        let p_term = store.lftj_decode_term(*p).expect("decode predicate");
        let o_term = store.lftj_decode_term(*o).expect("decode object");
        let Term::NamedNode(s_nn) = s_term else {
            panic!("subject must decode to a NamedNode")
        };
        let Term::NamedNode(p_nn) = p_term else {
            panic!("predicate must decode to a NamedNode")
        };
        store
            .insert(&Quad::new(s_nn, p_nn, o_term, graph.clone()))
            .unwrap();
    }
    store.compact().unwrap();

    // ── Verify round-trip via the ordinary QuadStore read path ──────────
    let read_back: Vec<_> = store
        .quads_for_pattern(None, Some(&sub_class_of()), None, Some(&graph))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        read_back.len(),
        3,
        "inference graph must contain exactly the 3 newly-derived triples"
    );

    // Spot-check the "interesting" long-range inference: A ⊑ D.
    let has_a_d = read_back.iter().any(|sq| {
        sq.subject.as_ref() == &Term::NamedNode(class("A"))
            && sq.object.as_ref() == &Term::NamedNode(class("D"))
    });
    assert!(has_a_d, "A subClassOf D must be present in the closure");

    // And confirm the disjoint E/F edge produced no spurious inferences.
    let mentions_e_or_f = read_back.iter().any(|sq| {
        let e_iri = Term::NamedNode(class("E"));
        let f_iri = Term::NamedNode(class("F"));
        sq.subject.as_ref() == &e_iri
            || sq.subject.as_ref() == &f_iri
            || sq.object.as_ref() == &e_iri
            || sq.object.as_ref() == &f_iri
    });
    assert!(
        !mentions_e_or_f,
        "disjoint E⊑F edge must not produce any derived triple"
    );
}
