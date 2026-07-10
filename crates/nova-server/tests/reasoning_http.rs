//! OWL 2 RL reasoning HTTP-surface integration tests.
//!
//! Exercises `Server::with_reasoning` end-to-end through the real axum
//! router (via `tower::ServiceExt::oneshot`, matching the pattern used by
//! `nova-server/src/lib.rs`'s own unit tests): a fact only derivable by the
//! reasoner (never asserted) must be visible to `/sparql`, `GET
//! /reasoning/diagnostics` must report it, and `GET /` (SPARQL Service
//! Description) must advertise the OWL-RL entailment regime — all only
//! when `Server::with_reasoning` was actually configured.
//!
//! Uses a real `RingStore` (not `MemoryStore`) since
//! `oxigraph_nova_reasoning::LftjFixpointEngine` only produces useful
//! results over an LFTJ-capable dataset (see `engine.rs`'s module doc
//! comment, "Rule coverage") — a plain in-memory store has no LFTJ support
//! and would yield an empty (but not incorrect) closure, which would make
//! these tests vacuous.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use oxigraph_nova_core::{GraphName, QuadStore};
use oxigraph_nova_reasoning::{LftjFixpointEngine, ReasoningEngine};
use oxigraph_nova_server::Server;
use oxigraph_nova_storage_ring::RingStore;
use oxrdf::{NamedNode, Quad, Term};
use std::sync::Arc;
use tower::ServiceExt;

fn iri(s: &str) -> NamedNode {
    NamedNode::new_unchecked(s)
}

fn rdf_type() -> NamedNode {
    iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
}

fn rdfs_sub_class_of() -> NamedNode {
    iri("http://www.w3.org/2000/01/rdf-schema#subClassOf")
}

/// A store with `fido rdf:type Dog`, `Dog subClassOf Mammal`, `Mammal
/// subClassOf Animal` — `fido rdf:type Animal` is derivable but never
/// asserted, requiring two rounds of the reasoner's fixpoint (subclass
/// transitivity, then type propagation) to surface.
fn make_reasoning_store() -> Arc<RingStore> {
    let store = RingStore::new();
    let dg = GraphName::DefaultGraph;

    store
        .insert(&Quad::new(
            iri("http://ex/fido"),
            rdf_type(),
            Term::NamedNode(iri("http://ex/Dog")),
            dg.clone(),
        ))
        .unwrap();
    store
        .insert(&Quad::new(
            iri("http://ex/Dog"),
            rdfs_sub_class_of(),
            Term::NamedNode(iri("http://ex/Mammal")),
            dg.clone(),
        ))
        .unwrap();
    store
        .insert(&Quad::new(
            iri("http://ex/Mammal"),
            rdfs_sub_class_of(),
            Term::NamedNode(iri("http://ex/Animal")),
            dg,
        ))
        .unwrap();

    // LFTJ only operates over the compacted LOUDS index — the reasoning
    // engine requires a compacted store (see `StoreAtomSource`'s doc
    // comment's "Precondition").
    store.compact().unwrap();
    Arc::new(store)
}

fn router_with_reasoning() -> Router {
    let engine: Arc<dyn ReasoningEngine> = Arc::new(LftjFixpointEngine::new());
    Server::new(make_reasoning_store())
        .with_reasoning(engine)
        .into_router()
}

fn router_without_reasoning() -> Router {
    Server::new(make_reasoning_store()).into_router()
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let text = body_text(resp).await;
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON: {e}\nbody: {text}"))
}

fn ask_request(sparql: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/sparql")
        .header(header::CONTENT_TYPE, "application/sparql-query")
        .body(Body::from(sparql.to_string()))
        .unwrap()
}

/// The core promise of `--reasoning`: a fact reachable only via the
/// reasoner's fixpoint (never an asserted base triple) must be visible to
/// an ordinary `/sparql` query.
#[tokio::test]
async fn ask_returns_true_for_fact_only_derivable_via_reasoning() {
    let router = router_with_reasoning();
    let resp = router
        .oneshot(ask_request(
            "ASK { <http://ex/fido> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> \
             <http://ex/Animal> }",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(
        json["boolean"], true,
        "fido rdf:type Animal must be visible via the reasoning overlay \
         (derived through Dog ⊑ Mammal ⊑ Animal + type propagation), got: {json}"
    );
}

/// Without `--reasoning`, the same query must NOT see the inferred fact —
/// proving the two routers genuinely differ, not that the base data itself
/// happens to satisfy the query.
#[tokio::test]
async fn ask_returns_false_for_same_fact_without_reasoning_enabled() {
    let router = router_without_reasoning();
    let resp = router
        .oneshot(ask_request(
            "ASK { <http://ex/fido> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> \
             <http://ex/Animal> }",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(
        json["boolean"], false,
        "fido rdf:type Animal is never asserted, and reasoning is disabled — \
         must be false, got: {json}"
    );
}

/// Asserted (base) facts must still be visible with reasoning enabled — the
/// overlay must not somehow hide or replace the underlying store's data.
#[tokio::test]
async fn ask_returns_true_for_asserted_fact_with_reasoning_enabled() {
    let router = router_with_reasoning();
    let resp = router
        .oneshot(ask_request(
            "ASK { <http://ex/fido> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> \
             <http://ex/Dog> }",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(
        json["boolean"], true,
        "asserted base fact must remain visible"
    );
}

/// `GET /reasoning/diagnostics` must report the overlay's inferred count
/// (> 0 for this fixture — subclass transitivity alone derives at least
/// `Dog subClassOf Animal`) when reasoning is enabled.
#[tokio::test]
async fn diagnostics_endpoint_reports_nonzero_inferred_len_when_enabled() {
    let router = router_with_reasoning();
    let req = Request::builder()
        .method("GET")
        .uri("/reasoning/diagnostics")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["enabled"], true);
    let inferred_len = json["inferred_len"]
        .as_u64()
        .expect("inferred_len must be a number");
    assert!(
        inferred_len > 0,
        "expected at least one inferred fact (e.g. Dog subClassOf Animal, \
         fido rdf:type Mammal, fido rdf:type Animal), got inferred_len={inferred_len}"
    );
    assert!(
        json["diagnostics"].is_array(),
        "diagnostics field must be a JSON array, got: {json}"
    );
}

/// `GET /reasoning/diagnostics` must be `404 Not Found` when
/// `Server::with_reasoning` was never configured — there is no overlay to
/// report on.
#[tokio::test]
async fn diagnostics_endpoint_404s_when_reasoning_disabled() {
    let router = router_without_reasoning();
    let req = Request::builder()
        .method("GET")
        .uri("/reasoning/diagnostics")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// SPARQL Service Description (`GET /`) must advertise
/// `http://www.w3.org/ns/entailment/OWL-RL` as `sd:defaultEntailmentRegime`
/// when reasoning is enabled.
#[tokio::test]
async fn service_description_advertises_owl_rl_when_reasoning_enabled() {
    let router = router_with_reasoning();
    let req = Request::builder()
        .method("GET")
        .uri("/")
        .header(header::ACCEPT, "application/n-triples")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(
        body.contains("http://www.w3.org/ns/entailment/OWL-RL"),
        "service description must advertise the OWL-RL entailment regime \
         when reasoning is enabled, got:\n{body}"
    );
    assert!(
        !body.contains("http://www.w3.org/ns/entailment/Simple"),
        "must not simultaneously advertise the Simple entailment regime, \
         got:\n{body}"
    );
}

/// Without `--reasoning`, the service description must advertise the plain
/// `Simple` entailment regime instead.
#[tokio::test]
async fn service_description_advertises_simple_when_reasoning_disabled() {
    let router = router_without_reasoning();
    let req = Request::builder()
        .method("GET")
        .uri("/")
        .header(header::ACCEPT, "application/n-triples")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(
        body.contains("http://www.w3.org/ns/entailment/Simple"),
        "service description must advertise the Simple entailment regime \
         when reasoning is disabled, got:\n{body}"
    );
    assert!(
        !body.contains("http://www.w3.org/ns/entailment/OWL-RL"),
        "must not advertise OWL-RL when reasoning was never enabled, got:\n{body}"
    );
}

/// A SELECT query (not just ASK) must also see inferred facts through the
/// reasoning overlay — proving the overlay is wired into the general query
/// path, not just a special-cased ASK shortcut.
#[tokio::test]
async fn select_query_returns_inferred_type_row() {
    let router = router_with_reasoning();
    let sparql = "SELECT ?class WHERE { \
                  <http://ex/fido> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ?class }";
    let req = Request::builder()
        .method("POST")
        .uri("/sparql")
        .header(header::CONTENT_TYPE, "application/sparql-query")
        .body(Body::from(sparql))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let bindings = json["results"]["bindings"]
        .as_array()
        .expect("expected SELECT results bindings array");
    let classes: Vec<String> = bindings
        .iter()
        .map(|b| b["class"]["value"].as_str().unwrap().to_string())
        .collect();
    for expected in ["http://ex/Dog", "http://ex/Mammal", "http://ex/Animal"] {
        assert!(
            classes.iter().any(|c| c == expected),
            "expected {expected} among fido's rdf:type results {classes:?} \
             (Mammal/Animal are only reachable via the reasoning overlay)"
        );
    }
}
