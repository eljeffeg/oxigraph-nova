//! Integration tests: parse → lower → evaluate, against a real
//! `LoudsStore`/`StoreDataset`/`Evaluator` — confirms lowered Cypher queries
//! actually run end-to-end and return correct results, not just that they
//! produce a structurally-plausible `spargebra::Query`.

use oxigraph_nova_core::{GraphName, NamedNode, Quad, QuadStore, Term};
use oxigraph_nova_cypher::{LABEL_NS, PROP_NS, REL_NS, parse_and_lower, parse_and_lower_update};
use oxigraph_nova_query::update::execute_update;
use oxigraph_nova_query::{Evaluator, StoreDataset};
use oxigraph_nova_storage_ring::LoudsStore;
use std::sync::Arc;

fn nn(s: &str) -> NamedNode {
    NamedNode::new_unchecked(s)
}

fn label(name: &str) -> NamedNode {
    nn(&format!("{LABEL_NS}{name}"))
}

fn prop(name: &str) -> NamedNode {
    nn(&format!("{PROP_NS}{name}"))
}

fn rel(name: &str) -> NamedNode {
    nn(&format!("{REL_NS}{name}"))
}

fn build_store(quads: Vec<Quad>) -> Arc<LoudsStore> {
    let store = LoudsStore::new();
    for q in quads {
        store.insert(&q).unwrap();
    }
    store.compact().unwrap();
    Arc::new(store)
}

fn run(store: &Arc<LoudsStore>, cypher: &str) -> (Vec<String>, Vec<Vec<Option<Term>>>) {
    let query = parse_and_lower(cypher).unwrap_or_else(|e| panic!("lowering failed: {e}"));
    let dataset = StoreDataset::new(Arc::clone(store));
    let evaluator = Evaluator::new(&dataset);

    let result = evaluator
        .evaluate(&query)
        .unwrap_or_else(|e| panic!("evaluation failed: {e}"));
    let (vars, solutions) = result.into_solutions_vec().unwrap();
    let var_names: Vec<String> = vars.iter().map(|v| v.as_str().to_string()).collect();
    let rows: Vec<Vec<Option<Term>>> = solutions
        .iter()
        .map(|sol| vars.iter().map(|v| sol.get(v).cloned()).collect())
        .collect();
    (var_names, rows)
}

#[test]
fn match_label_and_return_node() {
    let alice = nn("http://example.org/alice");
    let store = build_store(vec![Quad::new(
        alice.clone(),
        oxrdf::vocab::rdf::TYPE.into_owned(),
        label("Person"),
        GraphName::DefaultGraph,
    )]);

    let (vars, rows) = run(&store, "MATCH (n:Person) RETURN n");
    assert_eq!(vars, vec!["n".to_string()]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Some(Term::NamedNode(alice)));
}

#[test]
fn where_filters_on_property_and_return_aliases_it() {
    let alice = nn("http://example.org/alice");
    let bob = nn("http://example.org/bob");
    let store = build_store(vec![
        Quad::new(
            alice.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            alice.clone(),
            prop("age"),
            Term::Literal(oxrdf::Literal::from(42_i64)),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            alice.clone(),
            prop("name"),
            Term::Literal(oxrdf::Literal::new_simple_literal("Alice")),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            bob.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            bob.clone(),
            prop("age"),
            Term::Literal(oxrdf::Literal::from(10_i64)),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            bob.clone(),
            prop("name"),
            Term::Literal(oxrdf::Literal::new_simple_literal("Bob")),
            GraphName::DefaultGraph,
        ),
    ]);

    let (vars, rows) = run(
        &store,
        "MATCH (n:Person) WHERE n.age > 30 RETURN n.name AS name",
    );
    assert_eq!(vars, vec!["name".to_string()]);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        Some(Term::Literal(oxrdf::Literal::new_simple_literal("Alice")))
    );
}

#[test]
fn relationship_pattern_matches_typed_edge() {
    let alice = nn("http://example.org/alice");
    let bob = nn("http://example.org/bob");
    let store = build_store(vec![
        Quad::new(
            alice.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            bob.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            alice.clone(),
            rel("KNOWS"),
            bob.clone(),
            GraphName::DefaultGraph,
        ),
    ]);

    let (vars, rows) = run(&store, "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b");
    assert_eq!(vars, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Some(Term::NamedNode(alice)));
    assert_eq!(rows[0][1], Some(Term::NamedNode(bob)));
}

#[test]
fn distinct_order_by_skip_limit_end_to_end() {
    let mut quads = Vec::new();
    let mut names = Vec::new();
    for (i, name) in ["Carol", "Alice", "Bob"].iter().enumerate() {
        let n = nn(&format!("http://example.org/p{i}"));
        quads.push(Quad::new(
            n.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ));
        quads.push(Quad::new(
            n.clone(),
            prop("name"),
            Term::Literal(oxrdf::Literal::new_simple_literal(*name)),
            GraphName::DefaultGraph,
        ));
        names.push(n);
    }
    let store = build_store(quads);

    let (vars, rows) = run(
        &store,
        "MATCH (n:Person) RETURN DISTINCT n.name AS name ORDER BY n.name ASC LIMIT 2",
    );
    assert_eq!(vars, vec!["name".to_string()]);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0][0],
        Some(Term::Literal(oxrdf::Literal::new_simple_literal("Alice")))
    );
    assert_eq!(
        rows[1][0],
        Some(Term::Literal(oxrdf::Literal::new_simple_literal("Bob")))
    );
}

#[test]
fn variable_length_relationship_one_or_more() {
    let a = nn("http://example.org/a");
    let b = nn("http://example.org/b");
    let c = nn("http://example.org/c");
    let store = build_store(vec![
        Quad::new(a.clone(), rel("KNOWS"), b.clone(), GraphName::DefaultGraph),
        Quad::new(b.clone(), rel("KNOWS"), c.clone(), GraphName::DefaultGraph),
    ]);

    let (vars, rows) = run(&store, "MATCH (x)-[:KNOWS*]->(y) RETURN x, y");
    assert_eq!(vars, vec!["x".to_string(), "y".to_string()]);
    // a->b, b->c, a->c (transitively) = 3 rows
    assert_eq!(rows.len(), 3);
}

// ── Write statements ────────────────────────────────────────────

fn run_update(store: &Arc<LoudsStore>, cypher: &str) {
    let update = parse_and_lower_update(cypher).unwrap_or_else(|e| panic!("lowering failed: {e}"));
    execute_update(store, &update).unwrap_or_else(|e| panic!("execution failed: {e}"));
}

#[test]
fn create_bare_node_with_label_and_property() {
    let store = build_store(vec![]);

    run_update(&store, "CREATE (n:Person {name: \"Alice\"})");

    let (vars, rows) = run(&store, "MATCH (n:Person) RETURN n.name AS name");
    assert_eq!(vars, vec!["name".to_string()]);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        Some(Term::Literal(oxrdf::Literal::new_simple_literal("Alice")))
    );
}

#[test]
fn create_relationship_anchored_to_matched_nodes() {
    let alice = nn("http://example.org/alice");
    let store = build_store(vec![Quad::new(
        alice.clone(),
        oxrdf::vocab::rdf::TYPE.into_owned(),
        label("Person"),
        GraphName::DefaultGraph,
    )]);

    run_update(
        &store,
        "MATCH (a:Person) CREATE (a)-[:KNOWS]->(b:Person {name: \"Bob\"})",
    );

    // The matched node `a` gets a new KNOWS edge to a freshly-created node.
    let (vars, rows) = run(
        &store,
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b.name AS bname",
    );
    assert_eq!(vars, vec!["a".to_string(), "bname".to_string()]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Some(Term::NamedNode(alice)));
    assert_eq!(
        rows[0][1],
        Some(Term::Literal(oxrdf::Literal::new_simple_literal("Bob")))
    );
}

#[test]
fn set_property_creates_then_overwrites() {
    let alice = nn("http://example.org/alice");
    let store = build_store(vec![Quad::new(
        alice.clone(),
        oxrdf::vocab::rdf::TYPE.into_owned(),
        label("Person"),
        GraphName::DefaultGraph,
    )]);

    run_update(&store, "MATCH (n:Person) SET n.age = 30");
    let (_, rows) = run(&store, "MATCH (n:Person) RETURN n.age AS age");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        Some(Term::Literal(oxrdf::Literal::from(30_i64)))
    );

    run_update(&store, "MATCH (n:Person) SET n.age = 31");
    let (_, rows) = run(&store, "MATCH (n:Person) RETURN n.age AS age");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        Some(Term::Literal(oxrdf::Literal::from(31_i64)))
    );
}

#[test]
fn set_label_adds_additional_type() {
    let alice = nn("http://example.org/alice");
    let store = build_store(vec![Quad::new(
        alice.clone(),
        oxrdf::vocab::rdf::TYPE.into_owned(),
        label("Person"),
        GraphName::DefaultGraph,
    )]);

    run_update(&store, "MATCH (n:Person) SET n:Employee");

    let (_, rows) = run(&store, "MATCH (n:Employee) RETURN n");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Some(Term::NamedNode(alice.clone())));

    // Original label is preserved.
    let (_, rows) = run(&store, "MATCH (n:Person) RETURN n");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Some(Term::NamedNode(alice)));
}

#[test]
fn remove_property_deletes_it() {
    let alice = nn("http://example.org/alice");
    let store = build_store(vec![
        Quad::new(
            alice.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            alice.clone(),
            prop("age"),
            Term::Literal(oxrdf::Literal::from(42_i64)),
            GraphName::DefaultGraph,
        ),
    ]);

    run_update(&store, "MATCH (n:Person) REMOVE n.age");

    let (_, rows) = run(&store, "MATCH (n:Person) RETURN n.age AS age");
    assert_eq!(rows.len(), 0);
}

#[test]
fn remove_label_deletes_it() {
    let alice = nn("http://example.org/alice");
    let store = build_store(vec![
        Quad::new(
            alice.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            alice.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Employee"),
            GraphName::DefaultGraph,
        ),
    ]);

    run_update(&store, "MATCH (n:Person) REMOVE n:Employee");

    let (_, rows) = run(&store, "MATCH (n:Employee) RETURN n");
    assert_eq!(rows.len(), 0);
    let (_, rows) = run(&store, "MATCH (n:Person) RETURN n");
    assert_eq!(rows.len(), 1);
}

#[test]
fn delete_node_removes_all_its_triples() {
    let alice = nn("http://example.org/alice");
    let store = build_store(vec![
        Quad::new(
            alice.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            alice.clone(),
            prop("name"),
            Term::Literal(oxrdf::Literal::new_simple_literal("Alice")),
            GraphName::DefaultGraph,
        ),
    ]);

    run_update(&store, "MATCH (n:Person) DELETE n");

    let (_, rows) = run(&store, "MATCH (n:Person) RETURN n");
    assert_eq!(rows.len(), 0);
}

#[test]
fn detach_delete_removes_node_and_incident_relationships() {
    let alice = nn("http://example.org/alice");
    let bob = nn("http://example.org/bob");
    let store = build_store(vec![
        Quad::new(
            alice.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            bob.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            alice.clone(),
            rel("KNOWS"),
            bob.clone(),
            GraphName::DefaultGraph,
        ),
    ]);

    run_update(
        &store,
        "MATCH (n:Person {name: \"placeholder\"}) DETACH DELETE n",
    );
    // Nothing matched (no node has that property) — sanity check the store
    // is untouched.
    let (_, rows) = run(&store, "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b");
    assert_eq!(rows.len(), 1);

    run_update(
        &store,
        "MATCH (a:Person)-[:KNOWS]->(b:Person) DETACH DELETE a",
    );

    // `alice` and its KNOWS edge are gone; `bob` remains.
    let (_, rows) = run(&store, "MATCH (n:Person) RETURN n");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Some(Term::NamedNode(bob)));
    let (_, rows) = run(&store, "MATCH (a)-[:KNOWS]->(b) RETURN a, b");
    assert_eq!(rows.len(), 0);
}

#[test]
fn delete_relationship_removes_only_the_edge() {
    let alice = nn("http://example.org/alice");
    let bob = nn("http://example.org/bob");
    let store = build_store(vec![
        Quad::new(
            alice.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            bob.clone(),
            oxrdf::vocab::rdf::TYPE.into_owned(),
            label("Person"),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            alice.clone(),
            rel("KNOWS"),
            bob.clone(),
            GraphName::DefaultGraph,
        ),
    ]);

    run_update(&store, "MATCH (a:Person)-[r:KNOWS]->(b:Person) DELETE r");

    let (_, rows) = run(&store, "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b");
    assert_eq!(rows.len(), 0);
    // The nodes themselves are untouched.
    let (_, rows) = run(&store, "MATCH (n:Person) RETURN n");
    assert_eq!(rows.len(), 2);
}

// ── Relationship properties (RDF 1.2 annotation syntax) ──────────────────
//
// MATCH-side relationship properties lower to quoted-triple annotation BGPs.
// CREATE-side relationship properties are rejected because oxrdf::Quad cannot
// express a quoted-triple subject (see lower.rs module docs). These tests
// cover the parse/lower surface only.

#[test]
fn match_relationship_property_lowers_successfully() {
    // Confirms the Cypher surface accepts and lowers relationship-property
    // patterns without error (the structural shape is covered by the unit
    // test in lower.rs).
    let query = parse_and_lower("MATCH (a:Person)-[:KNOWS {since: 2020}]->(b:Person) RETURN a, b")
        .expect("MATCH with relationship properties should lower");
    assert!(matches!(query, spargebra::Query::Select { .. }));
}

#[test]
fn create_relationship_property_is_rejected() {
    let err = parse_and_lower_update("CREATE (a)-[:KNOWS {since: 2020}]->(b)")
        .expect_err("CREATE with relationship properties must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("oxrdf") || msg.contains("relationship properties"),
        "error should mention the limitation: {msg}"
    );
}
