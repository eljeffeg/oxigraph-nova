//! SPARQL 1.1 HTTP endpoint — wired to the evaluator.
//!
//! Implements the SPARQL 1.1 Protocol (W3C Rec):
//!
//!   GET  `/sparql?query=<encoded>`                              — query via URL param
//!   POST `/sparql`  Content-Type: application/sparql-query      — query in body
//!   POST `/sparql`  Content-Type: application/x-www-form-urlencoded; `query=` field
//!   POST `/sparql/update`                                       — SPARQL Update (stub)
//!
//! Result serialisation:
//!   SELECT / ASK  → `application/sparql-results+json`  (via `sparesults`)
//!   CONSTRUCT     → `application/n-triples`             (via oxrdf Display)
//!
//! # Usage
//! ```no_run
//! use oxigraph_nova_server::Server;
//! use oxigraph_nova_storage_memory::MemoryStore;
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() {
//!     let store = Arc::new(MemoryStore::new());
//!     Server::new(store).run("0.0.0.0:3030").await.unwrap();
//! }
//! ```

use axum::Router;
use axum::extract::{Query as AxumQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use oxigraph_nova_core::QuadStore;
use oxigraph_nova_query::{Evaluator, QueryResult, Solution, StoreDataset};
use oxrdf::Variable;
use serde::Deserialize;
use sparesults::{QueryResultsFormat, QueryResultsSerializer};
use spargebra::algebra::GraphPattern;
use spargebra::{Query, SparqlParser};
use std::sync::Arc;

// ── Application state ─────────────────────────────────────────────────────────

/// Shared server state.  Holds only an `Arc` to the backing store so axum can
/// cheaply clone it for every request without requiring `S: Clone`.
pub struct AppState<S: QuadStore + 'static> {
    pub store: Arc<S>,
}

/// Manual `Clone` impl — `Arc<S>` is always `Clone` regardless of whether `S`
/// is `Clone`, so we do not impose that bound on the user's store type.
impl<S: QuadStore + 'static> Clone for AppState<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
        }
    }
}

/// Query parameters for GET /sparql.
#[derive(Deserialize)]
pub struct SparqlQueryParams {
    pub query: Option<String>,
}

// ── Server ────────────────────────────────────────────────────────────────────

/// Oxigraph Nova SPARQL server.
pub struct Server<S: QuadStore + 'static> {
    store: Arc<S>,
}

impl<S: QuadStore + Send + Sync + 'static> Server<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self { store }
    }

    /// Build the axum `Router`.
    ///
    /// Returns a router that can be used directly in integration tests via
    /// `tower::ServiceExt::oneshot`, or passed to `axum::serve`.
    pub fn into_router(self) -> Router {
        let state = AppState { store: self.store };
        Router::new()
            .route("/sparql", get(sparql_get::<S>).post(sparql_post::<S>))
            .route("/sparql/update", post(sparql_update::<S>))
            .with_state(state)
    }

    /// Bind to `addr` and serve until the process is killed.
    pub async fn run(self, addr: &str) -> anyhow::Result<()> {
        let app = self.into_router();
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("Oxigraph Nova SPARQL endpoint listening on {}", addr);
        axum::serve(listener, app).await?;
        Ok(())
    }
}

// ── Request handlers ──────────────────────────────────────────────────────────

/// `GET /sparql?query=<percent-encoded SPARQL>`
async fn sparql_get<S: QuadStore + 'static>(
    State(state): State<AppState<S>>,
    AxumQuery(params): AxumQuery<SparqlQueryParams>,
) -> Response {
    match params.query {
        None => (StatusCode::BAD_REQUEST, "Missing ?query= parameter").into_response(),
        Some(q) => execute_sparql_query(&state.store, &q),
    }
}

/// `POST /sparql`
///
/// Accepted content-types (SPARQL 1.1 Protocol § 2.1):
///   - `application/sparql-query`          — body is the SPARQL text
///   - `application/x-www-form-urlencoded` — body contains `query=<encoded>`
///   - `text/plain` / no content-type      — lenient: body treated as SPARQL text
async fn sparql_post<S: QuadStore + 'static>(
    State(state): State<AppState<S>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "Empty request body").into_response();
    }

    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let query_str: String = if ct.starts_with("application/sparql-query") {
        body
    } else if ct.starts_with("application/x-www-form-urlencoded") {
        match form_param(&body, "query") {
            Some(q) => q,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    "Missing 'query' field in form body",
                )
                    .into_response();
            }
        }
    } else if ct.is_empty() || ct.starts_with("text/plain") {
        // Lenient: accept untyped or plain-text bodies as direct SPARQL
        body
    } else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Use Content-Type: application/sparql-query or application/x-www-form-urlencoded",
        )
            .into_response();
    };

    execute_sparql_query(&state.store, &query_str)
}

/// `POST /sparql/update` — SPARQL Update (stub for future implementation).
async fn sparql_update<S: QuadStore + 'static>(
    State(_state): State<AppState<S>>,
    body: String,
) -> Response {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "Empty request body").into_response();
    }
    (
        StatusCode::NOT_IMPLEMENTED,
        "SPARQL Update not yet implemented",
    )
        .into_response()
}

// ── Core execution pipeline ───────────────────────────────────────────────────

/// Parse → adapt store → evaluate → serialise.
///
/// This is sync because the evaluator is sync; the async handlers just call it.
fn execute_sparql_query<S: QuadStore + 'static>(store: &Arc<S>, sparql: &str) -> Response {
    // 1. Parse
    let query = match SparqlParser::new().parse_query(sparql) {
        Ok(q) => q,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("SPARQL parse error: {e}")).into_response();
        }
    };

    // 2. Build the Dataset adapter (bridges any QuadStore → Dataset trait)
    let dataset = StoreDataset::new(Arc::clone(store));

    // 3. Evaluate
    let evaluator = Evaluator::new(&dataset);
    match evaluator.evaluate(&query) {
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Evaluation error: {e}"),
        )
            .into_response(),
        Ok(QueryResult::Boolean(b)) => serialize_boolean(b),
        Ok(QueryResult::Solutions(solutions)) => {
            let vars = query_select_vars(&query);
            serialize_solutions(&vars, &solutions)
        }
        Ok(QueryResult::Triples(triples)) => serialize_triples(&triples),
    }
}

// ── Result serialization ──────────────────────────────────────────────────────

/// Serialize an ASK boolean result as `application/sparql-results+json`.
fn serialize_boolean(value: bool) -> Response {
    let mut buf = Vec::new();
    match QueryResultsSerializer::from_format(QueryResultsFormat::Json)
        .serialize_boolean_to_writer(&mut buf, value)
    {
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Ok(_) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "application/sparql-results+json; charset=utf-8",
            )],
            buf,
        )
            .into_response(),
    }
}

/// Serialize a SELECT result set as `application/sparql-results+json`.
///
/// `variables` is the ordered projection list from the query's `Project` node;
/// it defines the JSON `"head": {"vars": [...]}` header and determines which
/// bindings are emitted for each solution (unbound variables are omitted, per
/// the W3C SPARQL 1.1 results format).
fn serialize_solutions(variables: &[Variable], solutions: &[Solution]) -> Response {
    let mut buf = Vec::<u8>::new();
    let result: std::io::Result<()> = (|| {
        let mut writer = QueryResultsSerializer::from_format(QueryResultsFormat::Json)
            .serialize_solutions_to_writer(&mut buf, variables.to_vec())?;
        for sol in solutions {
            // Emit only bound variables; unbound are absent (SPARQL JSON § 3.2.1).
            writer.serialize(
                variables
                    .iter()
                    .filter_map(|v| sol.get(v).map(|t| (v.as_ref(), t.as_ref()))),
            )?;
        }
        writer.finish()?;
        Ok(())
    })();

    match result {
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Ok(()) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "application/sparql-results+json; charset=utf-8",
            )],
            buf,
        )
            .into_response(),
    }
}

/// Serialize a CONSTRUCT result as `application/n-triples`.
///
/// oxrdf's `Display` implementations produce syntactically correct N-Triples:
///   IRI        → `<http://…>`
///   blank node → `_:label`
///   literal    → `"value"` / `"value"@lang` / `"value"^^<dt>`
fn serialize_triples(triples: &[oxrdf::Triple]) -> Response {
    let mut buf = String::new();
    for t in triples {
        // N-Triples format: subject predicate object .
        buf.push_str(&format!("{} {} {} .\n", t.subject, t.predicate, t.object));
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/n-triples; charset=utf-8")],
        buf,
    )
        .into_response()
}

// ── SPARQL algebra helpers ────────────────────────────────────────────────────

/// Extract the ordered SELECT variable list from the outermost `Project` node.
///
/// spargebra resolves `SELECT *` to an explicit `Project` with all WHERE-clause
/// variables during parsing, so this is always populated for valid SELECT queries.
fn query_select_vars(query: &Query) -> Vec<Variable> {
    if let Query::Select { pattern, .. } = query {
        extract_project_vars(pattern)
    } else {
        vec![]
    }
}

fn extract_project_vars(pattern: &GraphPattern) -> Vec<Variable> {
    match pattern {
        GraphPattern::Project { variables, .. } => variables.clone(),
        GraphPattern::Distinct { inner } => extract_project_vars(inner),
        GraphPattern::Reduced { inner } => extract_project_vars(inner),
        GraphPattern::OrderBy { inner, .. } => extract_project_vars(inner),
        GraphPattern::Slice { inner, .. } => extract_project_vars(inner),
        _ => vec![],
    }
}

// ── Form URL-decoding ─────────────────────────────────────────────────────────

/// Extract a named parameter from an `application/x-www-form-urlencoded` body.
fn form_param(body: &str, key: &str) -> Option<String> {
    for kv in body.split('&') {
        if let Some((k, v)) = kv.split_once('=')
            && percent_decode(k).trim() == key
        {
            return Some(percent_decode(v));
        }
    }
    None
}

/// Decode a percent-encoded, `+`-for-space form value.
///
/// Handles arbitrary UTF-8: bytes are decoded individually and then reassembled
/// from raw bytes via `String::from_utf8_lossy`, so multi-byte sequences
/// (e.g. percent-encoded non-ASCII SPARQL comments) round-trip correctly.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut result: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                result.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
                    && let Ok(val) = u8::from_str_radix(hex, 16)
                {
                    result.push(val);
                    i += 3;
                    continue;
                }
                // Malformed %XX — pass through literally.
                result.push(b'%');
                i += 1;
            }
            b => {
                result.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&result).into_owned()
}

// ── Integration tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use oxigraph_nova_core::{GraphName, Quad};
    use oxigraph_nova_storage_memory::MemoryStore;
    use oxrdf::{Literal, NamedNode, Term};
    use tower::ServiceExt; // for Router::oneshot

    // ── Test fixtures ─────────────────────────────────────────────────────────

    fn make_store() -> Arc<MemoryStore> {
        let store = Arc::new(MemoryStore::new());
        let alice = NamedNode::new_unchecked("http://ex/alice");
        let bob = NamedNode::new_unchecked("http://ex/bob");
        let name = NamedNode::new_unchecked("http://ex/name");
        let age = NamedNode::new_unchecked("http://ex/age");
        let xsd_int = NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer");
        let dg = GraphName::DefaultGraph;

        store
            .insert(&Quad::new(
                alice.clone(),
                name.clone(),
                Term::Literal(Literal::new_simple_literal("Alice")),
                dg.clone(),
            ))
            .unwrap();
        store
            .insert(&Quad::new(
                bob.clone(),
                name.clone(),
                Term::Literal(Literal::new_simple_literal("Bob")),
                dg.clone(),
            ))
            .unwrap();
        store
            .insert(&Quad::new(
                alice.clone(),
                age.clone(),
                Term::Literal(Literal::new_typed_literal("30", xsd_int)),
                dg.clone(),
            ))
            .unwrap();
        store
    }

    fn make_router() -> Router {
        Server::new(make_store()).into_router()
    }

    /// Collect the response body into a `serde_json::Value`.
    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("failed to collect body");
        serde_json::from_slice(&bytes).expect("response is not valid JSON")
    }

    /// Collect the response body as a `String`.
    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("failed to collect body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    // ── GET /sparql?query= ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_get_select_returns_two_names() {
        // SELECT ?s WHERE { ?s <http://ex/name> ?n }  — expects 2 rows (alice, bob)
        // URL-encoded form of the query (spaces → +, special chars → %XX):
        let q = "SELECT+%3Fs+WHERE+%7B+%3Fs+%3Chttp%3A%2F%2Fex%2Fname%3E+%3Fn+%7D";

        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("/sparql?query={q}"))
            .body(Body::empty())
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK, "expected 200 OK");
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("sparql-results+json"),
            "wrong content-type: {ct}"
        );

        let json = body_json(resp).await;
        let vars = json["head"]["vars"].as_array().unwrap();
        assert_eq!(vars.len(), 1, "expected one projected variable");
        assert_eq!(vars[0].as_str().unwrap(), "s");

        let bindings = json["results"]["bindings"].as_array().unwrap();
        assert_eq!(
            bindings.len(),
            2,
            "expected 2 bindings, got {}",
            bindings.len()
        );
    }

    #[tokio::test]
    async fn test_get_missing_query_returns_400() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/sparql")
            .body(Body::empty())
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ── POST application/sparql-query ─────────────────────────────────────────

    #[tokio::test]
    async fn test_post_sparql_query_ask_true() {
        let sparql = r#"ASK { <http://ex/alice> <http://ex/name> "Alice" }"#;

        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(resp).await;
        assert_eq!(json["boolean"], true, "expected ASK → true");
    }

    #[tokio::test]
    async fn test_post_sparql_query_ask_false() {
        let sparql = r#"ASK { <http://ex/alice> <http://ex/name> "Nobody" }"#;

        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(resp).await;
        assert_eq!(json["boolean"], false, "expected ASK → false");
    }

    #[tokio::test]
    async fn test_post_sparql_query_select_with_filter() {
        // SELECT ?s WHERE { ?s <http://ex/name> ?n . FILTER(?n = "Alice") }
        let sparql = r#"SELECT ?s WHERE { ?s <http://ex/name> ?n . FILTER(?n = "Alice") }"#;

        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(resp).await;
        let bindings = json["results"]["bindings"].as_array().unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings[0]["s"]["value"].as_str().unwrap(),
            "http://ex/alice"
        );
    }

    // ── POST application/x-www-form-urlencoded ────────────────────────────────

    #[tokio::test]
    async fn test_post_form_urlencoded_select_all() {
        // query=SELECT ?s ?p ?o WHERE { ?s ?p ?o } — expects all 3 quads
        // URL-encoded: SELECT+%3Fs+%3Fp+%3Fo+WHERE+%7B+%3Fs+%3Fp+%3Fo+%7D
        let form = "query=SELECT+%3Fs+%3Fp+%3Fo+WHERE+%7B+%3Fs+%3Fp+%3Fo+%7D";

        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(resp).await;
        let bindings = json["results"]["bindings"].as_array().unwrap();
        // 3 quads inserted: alice-name, bob-name, alice-age
        assert_eq!(
            bindings.len(),
            3,
            "expected 3 bindings, got {}",
            bindings.len()
        );
    }

    #[tokio::test]
    async fn test_post_form_missing_query_field_returns_400() {
        let form = "update=DELETE+WHERE+%7B+%3Fs+%3Fp+%3Fo+%7D"; // no "query" key

        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ── Error cases ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_parse_error_returns_400() {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from("THIS IS NOT SPARQL"))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_text(resp).await;
        assert!(
            body.contains("parse error"),
            "expected parse error message, got: {body}"
        );
    }

    #[tokio::test]
    async fn test_unsupported_content_type_returns_415() {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    // ── percent_decode unit tests ─────────────────────────────────────────────

    #[test]
    fn test_percent_decode_plus_as_space() {
        assert_eq!(percent_decode("hello+world"), "hello world");
    }

    #[test]
    fn test_percent_decode_hex_sequences() {
        assert_eq!(percent_decode("SELECT+%3Fs"), "SELECT ?s");
        assert_eq!(percent_decode("%7B%7D"), "{}");
        assert_eq!(percent_decode("%3Chttp%3A%2F%2Fex%2Fs%3E"), "<http://ex/s>");
    }

    #[test]
    fn test_percent_decode_passthrough() {
        assert_eq!(percent_decode("plain"), "plain");
    }
}
