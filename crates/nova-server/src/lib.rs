//! SPARQL 1.1 HTTP endpoint — wired to the evaluator.
//!
//! Implements the SPARQL 1.1 Protocol (W3C Rec):
//!
//!   GET  `/sparql?query=<encoded>`                              — query via URL param
//!   POST `/sparql`  Content-Type: application/sparql-query      — query in body
//!   POST `/sparql`  Content-Type: application/x-www-form-urlencoded; `query=` field
//!   POST `/update`                                               — SPARQL 1.1 Update
//!
//! `/query` is routed to the same handler as `/sparql` (both GET and POST) —
//! an alias matching upstream Oxigraph's endpoint naming (`oxigraph serve`
//! exposes `POST /query` and `POST /update`), so a client written against
//! Oxigraph's endpoint conventions works against Nova unmodified. Unlike
//! Oxigraph's `/sparql`, which accepts *either* a query or an update, Nova's
//! `/sparql`/`/query` are query-only; use `/update` for updates.
//!
//! Also implements the SPARQL 1.1 Graph Store HTTP Protocol (W3C Rec):
//!
//!   GET    `/store?graph=<iri>` / `?default`                    — read a graph
//!   PUT    `/store?graph=<iri>` / `?default`                    — replace a graph
//!   POST   `/store?graph=<iri>` / `?default`                    — merge into a graph
//!   DELETE `/store?graph=<iri>` / `?default`                    — clear a graph
//!
//! Extra/unrecognised query parameters (e.g. BSBM's `?no_transaction`) are
//! silently ignored rather than rejected, since `axum::extract::Query` does
//! not enable `#[serde(deny_unknown_fields)]`.
//!
//! Result serialisation:
//!   SELECT / ASK        → content-negotiated via `Accept`: `application/sparql-results+xml`
//!                          (`sparesults` XML), `text/csv` (CSV), `text/tab-separated-values`
//!                          (TSV), or `application/sparql-results+json` (default, and used
//!                          whenever `Accept` doesn't specifically ask for one of the others).
//!   CONSTRUCT/DESCRIBE   → content-negotiated via `Accept`: `application/ld+json`
//!                          (via `oxjsonld::JsonLdSerializer`), `text/turtle` (via
//!                          `oxttl::TurtleSerializer`), `application/rdf+xml` (via
//!                          `oxrdfxml::RdfXmlSerializer`), `application/n-quads` (via
//!                          `oxttl::NQuadsSerializer`), `application/trig` (via
//!                          `oxttl::TriGSerializer`), or `application/n-triples`
//!                          (default, and used whenever `Accept` doesn't ask for one of
//!                          the others specifically). N-Quads/TriG output places every
//!                          triple in the default graph, since CONSTRUCT/DESCRIBE results
//!                          are graph-agnostic in this server's `QueryResult::Triples`
//!                          representation.
//!
//! Bulk-load / Graph Store PUT/POST bodies are parsed by Content-Type:
//!   `text/turtle` (default / unrecognised) → `oxttl::TurtleParser`
//!   `application/n-triples`                → `oxttl::NTriplesParser`
//!   `application/n-quads`                  → `oxttl::NQuadsParser`
//!   `application/trig`                     → `oxttl::TriGParser`
//!   `application/rdf+xml`                  → `oxrdfxml::RdfXmlParser`
//!   `application/ld+json`                  → `oxjsonld::JsonLdParser`
//!
//! As with JSON-LD, N-Quads/TriG bulk-load bodies may carry their own named-graph
//! information, but since `parse_body_triples` always returns plain `Triple`s for
//! insertion into the *one* target graph named by the Graph Store Protocol request,
//! any non-default-graph component on a parsed quad is dropped in favor of that
//! target graph.
//!
//! # Usage
//!
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
//!
//! # Transactional isolation semantics
//!
//! Upstream Oxigraph documents a "repeatable read" isolation guarantee for
//! every operation: a query, or an Update, observes one fixed snapshot of
//! the store for its entire duration, and only fully-committed writes are
//! ever visible. **This server does not currently provide that guarantee.**
//! Each individual `QuadStore` call (one `quads_for_pattern`, one `insert`,
//! one `remove`, ...) is atomic on its own, but a request that issues
//! *several* such calls — a multi-triple-pattern `SELECT`, or a
//! `DELETE/INSERT ... WHERE` Update applying its WHERE-clause solutions
//! row-by-row (see `oxigraph_nova_query::update`'s module doc comment) — has
//! no snapshot spanning those calls. A concurrent write landing between two
//! of them is visible to the later one, and a concurrent reader can observe
//! a multi-row Update partially applied.
//!
//! This is an accepted, documented limitation of the current
//! single-`Mutex<RingStoreInner>` design (see
//! `oxigraph_nova_storage_ring`'s `store.rs` module doc comment, "Isolation
//! semantics", for the storage-level explanation), demonstrated directly by
//! `crates/nova-server/tests/isolation.rs`'s two integration tests.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Query as AxumQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use oxigraph_nova_core::QuadStore;
use oxigraph_nova_query::{
    Evaluator, QueryResult, Solutions, StoreDataset, clear_graph, execute_update,
};
use oxjsonld::{JsonLdParser, JsonLdSerializer};
use oxrdf::{
    GraphName, GraphNameRef, NamedNode, NamedOrBlankNode, Quad, QuadRef, Term, Triple, Variable,
};
use oxrdfxml::{RdfXmlParser, RdfXmlSerializer};
use oxttl::{
    NQuadsParser, NQuadsSerializer, NTriplesParser, NTriplesSerializer, TriGParser, TriGSerializer,
    TurtleParser, TurtleSerializer,
};
use serde::Deserialize;
use sparesults::{QueryResultsFormat, QueryResultsSerializer};
use spargebra::algebra::GraphPattern;
use spargebra::{Query, SparqlParser};
use std::io::Write as _;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;

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

/// Query parameters for the SPARQL 1.1 Graph Store HTTP Protocol endpoints
/// (`/store`). Exactly one of `graph` / `default` should be given to name the
/// target graph; any other query parameters (e.g. BSBM's `?no_transaction`,
/// `?lenient`) are silently ignored since `axum::extract::Query` does not
/// enable `#[serde(deny_unknown_fields)]`.
#[derive(Deserialize)]
pub struct GraphStoreParams {
    pub graph: Option<String>,
    pub default: Option<String>,
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
            // `/query` is an alias matching upstream Oxigraph's endpoint
            // naming (`oxigraph serve` exposes `POST /query`), so clients
            // written against Oxigraph's conventions work against Nova
            // unmodified.
            .route("/query", get(sparql_get::<S>).post(sparql_post::<S>))
            .route("/update", post(sparql_update::<S>))
            .route(
                "/store",
                get(store_get::<S>)
                    .put(store_put::<S>)
                    .post(store_post::<S>)
                    .delete(store_delete::<S>),
            )
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
    headers: HeaderMap,
) -> Response {
    match params.query {
        None => (StatusCode::BAD_REQUEST, "Missing ?query= parameter").into_response(),
        Some(q) => execute_sparql_query(&state.store, &q, accept_header(&headers)),
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

    execute_sparql_query(&state.store, &query_str, accept_header(&headers))
}

/// `POST /update` — SPARQL 1.1 Update.
///
/// Accepted content-types (SPARQL 1.1 Protocol § 2.2):
///   - `application/sparql-update`         — body is the update text
///   - `application/x-www-form-urlencoded` — body contains `update=<encoded>`
///   - `text/plain` / no content-type      — lenient: body treated as update text
///
/// See `oxigraph_nova_query::update` for the operations implemented
/// (`INSERT DATA`/`DELETE DATA`/`DELETE WHERE`/`INSERT`+`DELETE ... WHERE`/
/// `LOAD`/`CLEAR`/`CREATE`/`DROP`) and documented limitations (Update
/// atomicity across a multi-statement request; `LOAD` requires a fetchable
/// HTTP client which is not wired up).
async fn sparql_update<S: QuadStore + 'static>(
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

    let update_str: String = if ct.starts_with("application/sparql-update") {
        body
    } else if ct.starts_with("application/x-www-form-urlencoded") {
        match form_param(&body, "update") {
            Some(u) => u,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    "Missing 'update' field in form body",
                )
                    .into_response();
            }
        }
    } else if ct.is_empty() || ct.starts_with("text/plain") {
        body
    } else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Use Content-Type: application/sparql-update or application/x-www-form-urlencoded",
        )
            .into_response();
    };

    execute_sparql_update(&state.store, &update_str)
}

/// Parse → execute a SPARQL Update request against the store.
fn execute_sparql_update<S: QuadStore + 'static>(store: &Arc<S>, sparql: &str) -> Response {
    let update = match SparqlParser::new().parse_update(sparql) {
        Ok(u) => u,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("SPARQL parse error: {e}")).into_response();
        }
    };

    match execute_update(store, &update) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Update execution error: {e}"),
        )
            .into_response(),
    }
}

// ── SPARQL 1.1 Graph Store HTTP Protocol ─────────────────────────────────────

/// Resolve `?graph=<iri>` / `?default` query params into a target [`GraphName`].
///
/// Per the W3C Graph Store HTTP Protocol, exactly one direct-reference query
/// parameter must identify the target graph. `?default` wins if both are
/// somehow present. Indirect referencing (naming a graph via the request URL
/// path itself, e.g. `/store/mygraph`) is out of scope — only `/store` with a
/// query parameter is implemented.
fn resolve_target_graph(params: &GraphStoreParams) -> Result<GraphName, Box<Response>> {
    if params.default.is_some() {
        Ok(GraphName::DefaultGraph)
    } else if let Some(iri) = &params.graph {
        match NamedNode::new(iri) {
            Ok(n) => Ok(GraphName::NamedNode(n)),
            Err(e) => Err(Box::new(
                (StatusCode::BAD_REQUEST, format!("Invalid graph IRI: {e}")).into_response(),
            )),
        }
    } else {
        Err(Box::new(
            (
                StatusCode::BAD_REQUEST,
                "Missing '?graph=<iri>' or '?default' query parameter",
            )
                .into_response(),
        ))
    }
}

/// Returns `true` if `g` is either explicitly registered or has at least one
/// quad. The default graph always "exists". Mirrors
/// `oxigraph_nova_query::update`'s private `named_graph_exists` helper —
/// duplicated here (rather than exposed) since it's a small, self-contained
/// check and keeps `nova-query`'s update-specific internals private.
fn graph_exists<S: QuadStore>(store: &Arc<S>, g: &GraphName) -> Result<bool, Box<Response>> {
    if matches!(g, GraphName::DefaultGraph) {
        return Ok(true);
    }
    let known = store.known_named_graphs().map_err(|e| {
        Box::new((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
    })?;
    for kg in known {
        let kg = kg.map_err(|e| {
            Box::new((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
        })?;
        if &kg == g {
            return Ok(true);
        }
    }
    let mut matches = store
        .quads_for_pattern(None, None, None, Some(g))
        .map_err(|e| {
            Box::new((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
        })?;
    Ok(matches.next().is_some())
}

/// Parse an RDF body into `Triple`s, dispatching on `Content-Type`:
///   - `application/n-triples`  → `oxttl::NTriplesParser`
///   - `application/rdf+xml`    → `oxrdfxml::RdfXmlParser`
///   - `application/ld+json`    → `oxjsonld::JsonLdParser`
///   - `text/turtle` / anything else (default) → `oxttl::TurtleParser`
///
/// JSON-LD parses to `Quad`s (it can express named graphs); since this
/// function only returns `Triple`s (per the Graph Store Protocol / bulk-load
/// callers, which always target one specific graph), any non-default-graph
/// component on a parsed quad is simply dropped and only its triple is kept —
/// mirroring how a JSON-LD document's own `@graph` nesting still ultimately
/// gets inserted into the one target graph named by the request.
fn parse_body_triples(content_type: &str, body: &[u8]) -> Result<Vec<Triple>, Box<Response>> {
    let err = |e: String| {
        Box::new((StatusCode::BAD_REQUEST, format!("RDF parse error: {e}")).into_response())
    };

    if content_type.starts_with("application/n-triples") {
        NTriplesParser::new()
            .for_reader(body)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| err(e.to_string()))
    } else if content_type.starts_with("application/n-quads") {
        NQuadsParser::new()
            .for_reader(body)
            .map(|r| r.map(|q| Triple::new(q.subject, q.predicate, q.object)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| err(e.to_string()))
    } else if content_type.starts_with("application/trig") {
        TriGParser::new()
            .for_reader(body)
            .map(|r| r.map(|q| Triple::new(q.subject, q.predicate, q.object)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| err(e.to_string()))
    } else if content_type.starts_with("application/rdf+xml") {
        RdfXmlParser::new()
            .for_reader(body)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| err(e.to_string()))
    } else if content_type.starts_with("application/ld+json") {
        JsonLdParser::new()
            .for_reader(body)
            .map(|r| r.map(|q| Triple::new(q.subject, q.predicate, q.object)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| err(e.to_string()))
    } else {
        // Default: Turtle (also used for text/turtle and unrecognised types).
        TurtleParser::new()
            .for_reader(body)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| err(e.to_string()))
    }
}

/// Insert parsed `Triple`s into `graph` as `Quad`s.
fn insert_triples_into_graph<S: QuadStore>(
    store: &Arc<S>,
    graph: &GraphName,
    triples: Vec<Triple>,
) -> Result<(), Box<Response>> {
    // Ensure the target named graph is registered (so it shows up in
    // `known_named_graphs()`/exists-checks even if the body was empty).
    if !matches!(graph, GraphName::DefaultGraph) {
        store.register_named_graph(graph).map_err(|e| {
            Box::new((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
        })?;
    }
    for t in triples {
        let quad = Quad::new(t.subject, t.predicate, t.object, graph.clone());
        store.insert(&quad).map_err(|e| {
            Box::new((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
        })?;
    }
    Ok(())
}

/// `GET /store?graph=<iri>` / `?default` — read a graph's triples.
///
/// Always returns 200 OK (even for an empty/unregistered graph — mirrors the
/// original oxigraph server's leniency for GET, reserving 404 for DELETE on a
/// missing named graph only).
async fn store_get<S: QuadStore + 'static>(
    State(state): State<AppState<S>>,
    AxumQuery(params): AxumQuery<GraphStoreParams>,
    headers: HeaderMap,
) -> Response {
    let graph = match resolve_target_graph(&params) {
        Ok(g) => g,
        Err(resp) => return *resp,
    };
    let stored = match state
        .store
        .quads_for_pattern(None, None, None, Some(&graph))
    {
        Ok(iter) => match iter.collect::<Result<Vec<_>, _>>() {
            Ok(v) => v,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let triples: Vec<Triple> = stored
        .into_iter()
        .filter_map(|sq| {
            let subject = match sq.subject.as_ref() {
                Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n.clone()),
                Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b.clone()),
                _ => return None,
            };
            let object = Arc::unwrap_or_clone(sq.object);
            Some(Triple::new(subject, sq.predicate, object))
        })
        .collect();

    serialize_triples(&triples, accept_header(&headers))
}

/// `PUT /store?graph=<iri>` / `?default` — replace a graph's contents.
///
/// Clears the target graph first (replace semantics), then loads the body.
/// Returns `201 CREATED` if the named graph did not previously exist, else
/// `204 NO_CONTENT` (the default graph always "exists", so PUT on it is
/// always `204`).
async fn store_put<S: QuadStore + 'static>(
    State(state): State<AppState<S>>,
    AxumQuery(params): AxumQuery<GraphStoreParams>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let graph = match resolve_target_graph(&params) {
        Ok(g) => g,
        Err(resp) => return *resp,
    };
    let existed = match graph_exists(&state.store, &graph) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let triples = match parse_body_triples(ct, &body) {
        Ok(t) => t,
        Err(resp) => return *resp,
    };
    if let Err(e) = clear_graph(&state.store, &graph) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    if let Err(resp) = insert_triples_into_graph(&state.store, &graph, triples) {
        return *resp;
    }
    if existed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::CREATED.into_response()
    }
}

/// `POST /store?graph=<iri>` / `?default` — merge triples into a graph.
///
/// Like `PUT` but does not clear the target graph first. Returns `201
/// CREATED` if the named graph did not previously exist, else `204
/// NO_CONTENT`.
async fn store_post<S: QuadStore + 'static>(
    State(state): State<AppState<S>>,
    AxumQuery(params): AxumQuery<GraphStoreParams>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let graph = match resolve_target_graph(&params) {
        Ok(g) => g,
        Err(resp) => return *resp,
    };
    let existed = match graph_exists(&state.store, &graph) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let triples = match parse_body_triples(ct, &body) {
        Ok(t) => t,
        Err(resp) => return *resp,
    };
    if let Err(resp) = insert_triples_into_graph(&state.store, &graph, triples) {
        return *resp;
    }
    if existed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::CREATED.into_response()
    }
}

/// `DELETE /store?graph=<iri>` / `?default` — clear a graph.
///
/// The default graph always "exists" so `DELETE` on it always succeeds
/// (`204`). `DELETE` on a nonexistent named graph returns `404 NOT_FOUND`.
async fn store_delete<S: QuadStore + 'static>(
    State(state): State<AppState<S>>,
    AxumQuery(params): AxumQuery<GraphStoreParams>,
) -> Response {
    let graph = match resolve_target_graph(&params) {
        Ok(g) => g,
        Err(resp) => return *resp,
    };
    if !matches!(graph, GraphName::DefaultGraph) {
        match graph_exists(&state.store, &graph) {
            Ok(true) => {}
            Ok(false) => {
                return (StatusCode::NOT_FOUND, "The graph does not exist").into_response();
            }
            Err(resp) => return *resp,
        }
    }

    match clear_graph(&state.store, &graph) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Core execution pipeline ───────────────────────────────────────────────────

/// Parse → adapt store → evaluate → serialise.
///
/// This is sync because the evaluator is sync; the async handlers just call it.
fn execute_sparql_query<S: QuadStore + 'static>(
    store: &Arc<S>,
    sparql: &str,
    accept: &str,
) -> Response {
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
        Ok(QueryResult::Boolean(b)) => serialize_boolean(b, accept),
        Ok(QueryResult::Solutions(solutions)) => {
            let vars = query_select_vars(&query);
            serialize_solutions(&vars, solutions, accept)
        }

        Ok(QueryResult::Triples(triples)) => serialize_triples(&triples, accept),
    }
}

// ── Result serialization ──────────────────────────────────────────────────────

/// Resolve the `Accept` header to a [`QueryResultsFormat`] for SELECT/ASK
/// serialization, content-negotiated across the four formats `sparesults`
/// supports:
///   - `Accept` contains `application/sparql-results+xml` → XML
///   - `Accept` contains `text/csv`                       → CSV
///   - `Accept` contains `text/tab-separated-values`      → TSV
///   - anything else (default)                            → JSON
///
/// The SPARQL 1.1 Protocol requires servers to support at least JSON and XML
/// for SELECT/ASK; this covers all four `sparesults`-supported formats.
fn results_format_for_accept(accept: &str) -> QueryResultsFormat {
    if accept.contains("application/sparql-results+xml") {
        QueryResultsFormat::Xml
    } else if accept.contains("text/csv") {
        QueryResultsFormat::Csv
    } else if accept.contains("text/tab-separated-values") {
        QueryResultsFormat::Tsv
    } else {
        QueryResultsFormat::Json
    }
}

/// Serialize an ASK boolean result, content-negotiated via `Accept` (see
/// `results_format_for_accept`).
fn serialize_boolean(value: bool, accept: &str) -> Response {
    let format = results_format_for_accept(accept);
    let mut buf = Vec::new();
    match QueryResultsSerializer::from_format(format).serialize_boolean_to_writer(&mut buf, value) {
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Ok(_) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, results_content_type(format))],
            buf,
        )
            .into_response(),
    }
}

/// Chunk size threshold (bytes) at which a streamed response flushes its
/// buffered output to the client. ~1 MiB, matching QLever's chunked
/// streaming design: large enough to amortize channel-send overhead, small
/// enough to bound peak memory for the serializer's output buffer.
const STREAM_CHUNK_SIZE: usize = 1 << 20;

/// A [`std::io::Write`] implementation that batches bytes and forwards them
/// as `~1 MiB` [`Bytes`] chunks over a bounded `tokio` channel, so the
/// (synchronous) `sparesults` serializer can run on a background thread
/// while the response body is streamed to the client as it's produced,
/// rather than being fully buffered in memory first.
struct ChannelWriter {
    tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    buf: Vec<u8>,
}

impl ChannelWriter {
    fn new(tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>) -> Self {
        Self {
            tx,
            buf: Vec::with_capacity(STREAM_CHUNK_SIZE),
        }
    }

    /// Send the current buffer contents (if any) as one chunk.
    fn flush_chunk(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::take(&mut self.buf));
        self.tx.blocking_send(Ok(chunk)).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "response receiver dropped")
        })
    }
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= STREAM_CHUNK_SIZE {
            self.flush_chunk()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_chunk()
    }
}

/// Serialize a SELECT result set, content-negotiated via `Accept` (see
/// `results_format_for_accept`).
///
/// `variables` is the ordered projection list from the query's `Project` node;
/// it defines the results header (JSON `"head": {"vars": [...]}` / XML
/// `<head><variable name=.../></head>` / CSV+TSV header row) and determines
/// which bindings are emitted for each solution (unbound variables are
/// omitted, per the W3C SPARQL 1.1 results format).
///
/// The response body is streamed: the (synchronous) `sparesults` serializer
/// runs on a background thread and forwards `~1 MiB` chunks through a
/// bounded channel as they're produced, so the full serialized output is
/// never buffered in memory all at once (matches QLever's chunked-transfer
/// design and avoids doubling peak memory for large result sets).
///
/// `Solution`'s `BNODE()` cache is `Arc<Mutex<_>>` (not `Rc<RefCell<_>>`),
/// so the whole `Solutions` vector is `Send` and can be moved into the
/// background thread as-is — each row is serialized straight from its own
/// borrowed `(&Variable, &Term)` pairs, with zero per-row allocation or
/// term cloning (mirrors Oxigraph's own `QuerySolution`-by-reference
/// handoff to `sparesults`, which never materializes an intermediate
/// owned copy of the result set before serializing).
fn serialize_solutions(variables: &[Variable], solutions: Solutions, accept: &str) -> Response {
    let format = results_format_for_accept(accept);
    let content_type = results_content_type(format);
    let variables = variables.to_vec();

    // Bounded channel: a handful of ~1 MiB chunks in flight is enough to
    // decouple producer/consumer speed without unbounded buffering.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);

    std::thread::spawn(move || {
        let writer = ChannelWriter::new(tx.clone());
        let result: std::io::Result<()> = (|| {
            let mut ser = QueryResultsSerializer::from_format(format)
                .serialize_solutions_to_writer(writer, variables.clone())?;
            for sol in &solutions {
                // Emit only bound variables; unbound are absent (SPARQL JSON § 3.2.1).
                ser.serialize(
                    variables
                        .iter()
                        .filter_map(|v| sol.get(v).map(|t| (v.as_ref(), t.as_ref()))),
                )?;
            }
            let mut writer = ser.finish()?;
            writer.flush()?;
            Ok(())
        })();

        if let Err(e) = result {
            tracing::error!("error while streaming SPARQL solutions: {e}");
            let _ = tx.blocking_send(Err(e));
        }
    });

    let body = Body::from_stream(ReceiverStream::new(rx));
    (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// Content-Type header value for a given [`QueryResultsFormat`].
fn results_content_type(format: QueryResultsFormat) -> &'static str {
    match format {
        QueryResultsFormat::Json => "application/sparql-results+json; charset=utf-8",
        QueryResultsFormat::Xml => "application/sparql-results+xml; charset=utf-8",
        QueryResultsFormat::Csv => "text/csv; charset=utf-8",
        QueryResultsFormat::Tsv => "text/tab-separated-values; charset=utf-8",
        _ => "application/sparql-results+json; charset=utf-8",
    }
}

/// Serialize a CONSTRUCT/DESCRIBE result, content-negotiated via `Accept`:
///   - `Accept` contains `application/ld+json` → JSON-LD (`oxjsonld::JsonLdSerializer`)
///   - `Accept` contains `text/turtle`         → Turtle (`oxttl::TurtleSerializer`)
///   - `Accept` contains `application/rdf+xml` → RDF/XML (`oxrdfxml::RdfXmlSerializer`)
///   - `Accept` contains `application/n-quads` → N-Quads (`oxttl::NQuadsSerializer`)
///   - `Accept` contains `application/trig`    → TriG (`oxttl::TriGSerializer`)
///   - anything else (default)                 → `application/n-triples`
///
/// N-Quads/TriG output places every triple in the default graph, since
/// CONSTRUCT/DESCRIBE results are graph-agnostic in this server's
/// `QueryResult::Triples` representation.
///
/// Also used directly by the Graph Store Protocol's `GET /store` handler so
/// both code paths share one serializer.
fn serialize_triples(triples: &[Triple], accept: &str) -> Response {
    if accept.contains("application/ld+json") {
        serialize_triples_jsonld(triples)
    } else if accept.contains("text/turtle") {
        serialize_triples_turtle(triples)
    } else if accept.contains("application/rdf+xml") {
        serialize_triples_rdfxml(triples)
    } else if accept.contains("application/n-quads") {
        serialize_triples_nquads(triples)
    } else if accept.contains("application/trig") {
        serialize_triples_trig(triples)
    } else {
        serialize_triples_ntriples(triples)
    }
}

fn serialize_triples_jsonld(triples: &[Triple]) -> Response {
    let mut writer = JsonLdSerializer::new().for_writer(Vec::new());
    for t in triples {
        let quad = QuadRef::new(
            &t.subject,
            &t.predicate,
            &t.object,
            GraphNameRef::DefaultGraph,
        );
        if let Err(e) = writer.serialize_quad(quad) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    match writer.finish() {
        Ok(buf) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/ld+json; charset=utf-8")],
            buf,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn serialize_triples_ntriples(triples: &[Triple]) -> Response {
    let mut writer = NTriplesSerializer::new().for_writer(Vec::new());
    for t in triples {
        if let Err(e) = writer.serialize_triple(t) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    let buf = writer.finish();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/n-triples; charset=utf-8")],
        buf,
    )
        .into_response()
}

fn serialize_triples_turtle(triples: &[Triple]) -> Response {
    let mut writer = TurtleSerializer::new().for_writer(Vec::new());
    for t in triples {
        if let Err(e) = writer.serialize_triple(t) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    match writer.finish() {
        Ok(buf) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/turtle; charset=utf-8")],
            buf,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn serialize_triples_rdfxml(triples: &[Triple]) -> Response {
    let mut writer = RdfXmlSerializer::new().for_writer(Vec::new());
    for t in triples {
        if let Err(e) = writer.serialize_triple(t) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    match writer.finish() {
        Ok(buf) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/rdf+xml; charset=utf-8")],
            buf,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn serialize_triples_nquads(triples: &[Triple]) -> Response {
    let mut writer = NQuadsSerializer::new().for_writer(Vec::new());
    for t in triples {
        let quad = QuadRef::new(
            &t.subject,
            &t.predicate,
            &t.object,
            GraphNameRef::DefaultGraph,
        );
        if let Err(e) = writer.serialize_quad(quad) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    let buf = writer.finish();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/n-quads; charset=utf-8")],
        buf,
    )
        .into_response()
}

fn serialize_triples_trig(triples: &[Triple]) -> Response {
    let mut writer = TriGSerializer::new().for_writer(Vec::new());
    for t in triples {
        let quad = QuadRef::new(
            &t.subject,
            &t.predicate,
            &t.object,
            GraphNameRef::DefaultGraph,
        );
        if let Err(e) = writer.serialize_quad(quad) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    match writer.finish() {
        Ok(buf) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/trig; charset=utf-8")],
            buf,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Extract the `Accept` header as a `&str` (empty string if absent/invalid).
fn accept_header(headers: &HeaderMap) -> &str {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
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

    // ── POST /update ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_update_insert_data() {
        let store = make_store();
        let router = Server::new(Arc::clone(&store)).into_router();

        let sparql = r#"INSERT DATA { <http://ex/carol> <http://ex/name> "Carol" }"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/sparql-update")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            store
                .contains(&Quad::new(
                    NamedNode::new_unchecked("http://ex/carol"),
                    NamedNode::new_unchecked("http://ex/name"),
                    Term::Literal(Literal::new_simple_literal("Carol")),
                    GraphName::DefaultGraph,
                ))
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_update_delete_data() {
        let store = make_store();
        let router = Server::new(Arc::clone(&store)).into_router();

        let sparql = r#"DELETE DATA { <http://ex/alice> <http://ex/name> "Alice" }"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/sparql-update")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            !store
                .contains(&Quad::new(
                    NamedNode::new_unchecked("http://ex/alice"),
                    NamedNode::new_unchecked("http://ex/name"),
                    Term::Literal(Literal::new_simple_literal("Alice")),
                    GraphName::DefaultGraph,
                ))
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_update_delete_where() {
        let store = make_store();
        let router = Server::new(Arc::clone(&store)).into_router();

        let sparql = r#"DELETE WHERE { ?s <http://ex/name> ?n }"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/sparql-update")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // only the alice-age quad should remain
        assert_eq!(store.len().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_update_insert_delete_where() {
        let store = make_store();
        let router = Server::new(Arc::clone(&store)).into_router();

        let sparql = r#"DELETE { ?s <http://ex/age> ?a }
                         INSERT { ?s <http://ex/ageOld> ?a }
                         WHERE { ?s <http://ex/age> ?a }"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/sparql-update")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            store
                .contains(&Quad::new(
                    NamedNode::new_unchecked("http://ex/alice"),
                    NamedNode::new_unchecked("http://ex/ageOld"),
                    Term::Literal(Literal::new_typed_literal(
                        "30",
                        NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer"),
                    )),
                    GraphName::DefaultGraph,
                ))
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_update_clear_default() {
        let store = make_store();
        let router = Server::new(Arc::clone(&store)).into_router();

        let sparql = "CLEAR DEFAULT";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/sparql-update")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(store.len().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_update_create_and_drop_graph() {
        let store = make_store();
        let router = Server::new(Arc::clone(&store)).into_router();

        let sparql = "CREATE GRAPH <http://ex/g1>";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/sparql-update")
            .body(Body::from(sparql))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let graphs: Vec<_> = store
            .known_named_graphs()
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            graphs.contains(&GraphName::NamedNode(NamedNode::new_unchecked(
                "http://ex/g1"
            )))
        );

        let router2 = Server::new(Arc::clone(&store)).into_router();
        let sparql2 = "DROP GRAPH <http://ex/g1>";
        let req2 = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/sparql-update")
            .body(Body::from(sparql2))
            .unwrap();
        let resp2 = router2.oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_update_form_urlencoded() {
        let store = make_store();
        let router = Server::new(Arc::clone(&store)).into_router();

        // update=INSERT+DATA+%7B+%3Chttp%3A%2F%2Fex%2Fd%3E+%3Chttp%3A%2F%2Fex%2Fp%3E+%22v%22+%7D
        let form = "update=INSERT+DATA+%7B+%3Chttp%3A%2F%2Fex%2Fd%3E+%3Chttp%3A%2F%2Fex%2Fp%3E+%22v%22+%7D";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            store
                .contains(&Quad::new(
                    NamedNode::new_unchecked("http://ex/d"),
                    NamedNode::new_unchecked("http://ex/p"),
                    Term::Literal(Literal::new_simple_literal("v")),
                    GraphName::DefaultGraph,
                ))
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_update_empty_body_returns_400() {
        let router = make_router();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/sparql-update")
            .body(Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_parse_error_returns_400() {
        let router = make_router();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/sparql-update")
            .body(Body::from("NOT AN UPDATE"))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_unsupported_content_type_returns_415() {
        let router = make_router();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn test_update_drop_missing_graph_errors() {
        let router = make_router();
        let sparql = "DROP GRAPH <http://ex/nonexistent>";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/sparql-update")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn test_update_drop_silent_missing_graph_ok() {
        let router = make_router();
        let sparql = "DROP SILENT GRAPH <http://ex/nonexistent>";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/update")
            .header("content-type", "application/sparql-update")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── Content-negotiated CONSTRUCT/DESCRIBE serialization ───────────────────

    #[tokio::test]
    async fn test_construct_default_ntriples() {
        let sparql = r#"CONSTRUCT { ?s <http://ex/name> ?n } WHERE { ?s <http://ex/name> ?n }"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.contains("n-triples"), "wrong content-type: {ct}");
        let body = body_text(resp).await;
        assert!(body.contains("<http://ex/alice>"));
        assert!(body.trim_end().ends_with('.'));
    }

    #[tokio::test]
    async fn test_construct_turtle_via_accept() {
        let sparql = r#"CONSTRUCT { ?s <http://ex/name> ?n } WHERE { ?s <http://ex/name> ?n }"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .header("accept", "text/turtle")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.contains("text/turtle"), "wrong content-type: {ct}");
        let body = body_text(resp).await;
        assert!(body.contains("http://ex/alice"));
    }

    #[tokio::test]
    async fn test_construct_jsonld_via_accept() {
        let sparql = r#"CONSTRUCT { ?s <http://ex/name> ?n } WHERE { ?s <http://ex/name> ?n }"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .header("accept", "application/ld+json")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            ct.contains("application/ld+json"),
            "wrong content-type: {ct}"
        );
        let json = body_json(resp).await;
        // With no `@context`/prefixes configured on the serializer, the output
        // is a bare top-level JSON array of expanded node objects (no `@graph`
        // wrapper — that only appears when a base IRI or prefix is set).
        let graph = json.as_array().expect("expected top-level JSON array");
        assert_eq!(graph.len(), 2, "expected 2 subjects with names");
        let body = serde_json::to_string(&json).unwrap();
        assert!(body.contains("http://ex/alice"));
    }

    #[tokio::test]
    async fn test_construct_rdfxml_via_accept() {
        let sparql = r#"CONSTRUCT { ?s <http://ex/name> ?n } WHERE { ?s <http://ex/name> ?n }"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .header("accept", "application/rdf+xml")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            ct.contains("application/rdf+xml"),
            "wrong content-type: {ct}"
        );
        let body = body_text(resp).await;
        assert!(body.contains("http://ex/alice"));
    }

    #[tokio::test]
    async fn test_construct_nquads_via_accept() {
        let sparql = r#"CONSTRUCT { ?s <http://ex/name> ?n } WHERE { ?s <http://ex/name> ?n }"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .header("accept", "application/n-quads")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            ct.contains("application/n-quads"),
            "wrong content-type: {ct}"
        );
        let body = body_text(resp).await;
        assert!(body.contains("http://ex/alice"));
    }

    #[tokio::test]
    async fn test_construct_trig_via_accept() {
        let sparql = r#"CONSTRUCT { ?s <http://ex/name> ?n } WHERE { ?s <http://ex/name> ?n }"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .header("accept", "application/trig")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.contains("application/trig"), "wrong content-type: {ct}");
        let body = body_text(resp).await;
        assert!(body.contains("http://ex/alice"));
    }

    #[tokio::test]
    async fn test_select_results_xml_via_accept() {
        let sparql = "SELECT ?s WHERE { ?s <http://ex/name> ?n }";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .header("accept", "application/sparql-results+xml")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            ct.contains("application/sparql-results+xml"),
            "wrong content-type: {ct}"
        );
        let body = body_text(resp).await;
        assert!(body.contains("<?xml"));
    }

    #[tokio::test]
    async fn test_select_results_csv_via_accept() {
        let sparql = "SELECT ?s WHERE { ?s <http://ex/name> ?n }";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .header("accept", "text/csv")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.contains("text/csv"), "wrong content-type: {ct}");
        let body = body_text(resp).await;
        assert!(body.starts_with('s'), "expected CSV header row: {body}");
        assert_eq!(body.lines().count(), 3, "header + 2 rows expected");
    }

    #[tokio::test]
    async fn test_select_results_tsv_via_accept() {
        let sparql = "SELECT ?s WHERE { ?s <http://ex/name> ?n }";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .header("accept", "text/tab-separated-values")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            ct.contains("text/tab-separated-values"),
            "wrong content-type: {ct}"
        );
        let body = body_text(resp).await;
        assert!(body.starts_with('?'), "expected TSV header row: {body}");
        assert_eq!(body.lines().count(), 3, "header + 2 rows expected");
    }

    #[tokio::test]
    async fn test_ask_results_xml_via_accept() {
        let sparql = r#"ASK { <http://ex/alice> <http://ex/name> "Alice" }"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .header("accept", "application/sparql-results+xml")
            .body(Body::from(sparql))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            ct.contains("application/sparql-results+xml"),
            "wrong content-type: {ct}"
        );
        let body = body_text(resp).await;
        assert!(body.contains("true"));
    }

    // ── JSON-LD bulk-load (PUT /store) ────────────────────────────────────────

    #[tokio::test]
    async fn test_store_put_jsonld_content_type() {
        let router = make_router();
        let jsonld = r#"{
            "@context": {"ex": "http://ex/"},
            "@id": "http://ex/grace",
            "ex:name": "Grace"
        }"#;

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/store?graph=http%3A%2F%2Fex%2Fjsonld")
            .header("content-type", "application/ld+json")
            .body(Body::from(jsonld))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_store_put_nquads_content_type() {
        let router = make_router();
        let nq = "<http://ex/heidi> <http://ex/name> \"Heidi\" <http://ex/somegraph> .\n";

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/store?graph=http%3A%2F%2Fex%2Fnq")
            .header("content-type", "application/n-quads")
            .body(Body::from(nq))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_store_put_trig_content_type() {
        let router = make_router();
        let trig = r#"<http://ex/somegraph> {
            <http://ex/ivan> <http://ex/name> "Ivan" .
        }"#;

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/store?graph=http%3A%2F%2Fex%2Ftrig")
            .header("content-type", "application/trig")
            .body(Body::from(trig))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // ── Graph Store HTTP Protocol: GET /store ─────────────────────────────────

    #[tokio::test]
    async fn test_store_get_default_graph() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/store?default")
            .body(Body::empty())
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        // 3 quads inserted into the default graph by make_store()
        assert_eq!(body.lines().filter(|l| !l.trim().is_empty()).count(), 3);
    }

    #[tokio::test]
    async fn test_store_get_missing_target_returns_400() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/store")
            .body(Body::empty())
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_store_get_named_graph_turtle_accept() {
        let store = make_store();
        store
            .insert(&Quad::new(
                NamedNode::new_unchecked("http://ex/dave"),
                NamedNode::new_unchecked("http://ex/name"),
                Term::Literal(Literal::new_simple_literal("Dave")),
                GraphName::NamedNode(NamedNode::new_unchecked("http://ex/g1")),
            ))
            .unwrap();
        let router = Server::new(store).into_router();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/store?graph=http%3A%2F%2Fex%2Fg1")
            .header("accept", "text/turtle")
            .body(Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.contains("text/turtle"));
        let body = body_text(resp).await;
        assert!(body.contains("Dave"));
    }

    // ── Graph Store HTTP Protocol: PUT /store ─────────────────────────────────

    #[tokio::test]
    async fn test_store_put_new_named_graph_returns_201() {
        let router = make_router();
        let ttl = "<http://ex/eve> <http://ex/name> \"Eve\" .";

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/store?graph=http%3A%2F%2Fex%2Fnew")
            .header("content-type", "text/turtle")
            .body(Body::from(ttl))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_store_put_existing_graph_returns_204_and_replaces() {
        let store = make_store();
        let g = GraphName::NamedNode(NamedNode::new_unchecked("http://ex/g1"));
        store
            .insert(&Quad::new(
                NamedNode::new_unchecked("http://ex/old"),
                NamedNode::new_unchecked("http://ex/p"),
                Term::Literal(Literal::new_simple_literal("old")),
                g.clone(),
            ))
            .unwrap();
        let router = Server::new(Arc::clone(&store)).into_router();

        let ttl = "<http://ex/newsubj> <http://ex/p> \"new\" .";
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/store?graph=http%3A%2F%2Fex%2Fg1")
            .header("content-type", "text/turtle")
            .body(Body::from(ttl))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Old triple should be gone (replace semantics), new triple present.
        assert!(
            !store
                .contains(&Quad::new(
                    NamedNode::new_unchecked("http://ex/old"),
                    NamedNode::new_unchecked("http://ex/p"),
                    Term::Literal(Literal::new_simple_literal("old")),
                    g.clone(),
                ))
                .unwrap()
        );
        assert!(
            store
                .contains(&Quad::new(
                    NamedNode::new_unchecked("http://ex/newsubj"),
                    NamedNode::new_unchecked("http://ex/p"),
                    Term::Literal(Literal::new_simple_literal("new")),
                    g,
                ))
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_store_put_ntriples_content_type() {
        let router = make_router();
        let nt = "<http://ex/frank> <http://ex/name> \"Frank\" .\n";

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/store?graph=http%3A%2F%2Fex%2Fnt")
            .header("content-type", "application/n-triples")
            .body(Body::from(nt))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_store_put_tolerates_extra_query_params() {
        let router = make_router();
        let ttl = "<http://ex/x> <http://ex/p> \"v\" .";

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/store?graph=http%3A%2F%2Fex%2Fextra&no_transaction&lenient")
            .header("content-type", "text/turtle")
            .body(Body::from(ttl))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // ── Graph Store HTTP Protocol: POST /store ────────────────────────────────

    #[tokio::test]
    async fn test_store_post_merges_without_clearing() {
        let store = make_store();
        let g = GraphName::NamedNode(NamedNode::new_unchecked("http://ex/g1"));
        store
            .insert(&Quad::new(
                NamedNode::new_unchecked("http://ex/existing"),
                NamedNode::new_unchecked("http://ex/p"),
                Term::Literal(Literal::new_simple_literal("keepme")),
                g.clone(),
            ))
            .unwrap();
        let router = Server::new(Arc::clone(&store)).into_router();

        let ttl = "<http://ex/added> <http://ex/p> \"added\" .";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/store?graph=http%3A%2F%2Fex%2Fg1")
            .header("content-type", "text/turtle")
            .body(Body::from(ttl))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        // Graph already existed → 204
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Both old and new triples should be present (merge, no clear).
        assert!(
            store
                .contains(&Quad::new(
                    NamedNode::new_unchecked("http://ex/existing"),
                    NamedNode::new_unchecked("http://ex/p"),
                    Term::Literal(Literal::new_simple_literal("keepme")),
                    g.clone(),
                ))
                .unwrap()
        );
        assert!(
            store
                .contains(&Quad::new(
                    NamedNode::new_unchecked("http://ex/added"),
                    NamedNode::new_unchecked("http://ex/p"),
                    Term::Literal(Literal::new_simple_literal("added")),
                    g,
                ))
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_store_post_new_graph_returns_201() {
        let router = make_router();
        let ttl = "<http://ex/g> <http://ex/p> \"v\" .";

        let req = Request::builder()
            .method(Method::POST)
            .uri("/store?graph=http%3A%2F%2Fex%2Fbrandnew")
            .header("content-type", "text/turtle")
            .body(Body::from(ttl))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // ── Graph Store HTTP Protocol: DELETE /store ──────────────────────────────

    #[tokio::test]
    async fn test_store_delete_default_graph_always_204() {
        let store = make_store();
        let router = Server::new(Arc::clone(&store)).into_router();

        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/store?default")
            .body(Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(store.len().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_store_delete_nonexistent_named_graph_returns_404() {
        let router = make_router();

        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/store?graph=http%3A%2F%2Fex%2Fnonexistent")
            .body(Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_store_delete_existing_named_graph_returns_204() {
        let store = make_store();
        let g = GraphName::NamedNode(NamedNode::new_unchecked("http://ex/g1"));
        store
            .insert(&Quad::new(
                NamedNode::new_unchecked("http://ex/s"),
                NamedNode::new_unchecked("http://ex/p"),
                Term::Literal(Literal::new_simple_literal("v")),
                g.clone(),
            ))
            .unwrap();
        let router = Server::new(Arc::clone(&store)).into_router();

        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/store?graph=http%3A%2F%2Fex%2Fg1")
            .body(Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(
            !store
                .contains(&Quad::new(
                    NamedNode::new_unchecked("http://ex/s"),
                    NamedNode::new_unchecked("http://ex/p"),
                    Term::Literal(Literal::new_simple_literal("v")),
                    g,
                ))
                .unwrap()
        );
    }
}
