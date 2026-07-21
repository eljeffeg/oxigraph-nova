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
//! Also exposes an experimental openCypher query surface (see
//! `oxigraph_nova_cypher`'s crate doc comment for the supported subset —
//! read-only `MATCH`/`WHERE`/`RETURN`/`ORDER BY`/`SKIP`/`LIMIT` plus the
//! write clauses `CREATE`/`SET`/`DELETE`/`DETACH DELETE`/`REMOVE`), each
//! endpoint a thin wrapper translating Cypher into the equivalent SPARQL
//! algebra (`oxigraph_nova_cypher::parse_and_lower`/`parse_and_lower_update`)
//! and then reusing exactly the same evaluation/serialization machinery as
//! `/sparql`/`/update` above:
//!
//!   GET  `/cypher?query=<encoded>`                              — read query via URL param
//!   POST `/cypher`  Content-Type: text/plain (or unset)         — read query in body
//!   POST `/cypher`  Content-Type: application/x-www-form-urlencoded; `query=` field
//!   POST `/cypher/update`  Content-Type: text/plain (or unset)  — write clauses in body
//!   POST `/cypher/update`  Content-Type: application/x-www-form-urlencoded; `update=` field
//!
//! Result serialization for `/cypher` is content-negotiated identically to
//! `/sparql` (see below); `/cypher/update` returns `200 OK` on success like
//! `/update`. `/cypher/update` honors the same `read_only` server flag as
//! `/update`.
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
//!   CONSTRUCT/DESCRIBE   → content-negotiated via `Accept`: `application/ld+json`,
//!                          `text/turtle`, `application/rdf+xml`, `application/n-quads`,
//!                          `application/trig`, or `application/n-triples` (default, and
//!                          used whenever `Accept` doesn't ask for one of the others
//!                          specifically) — all served via `oxrdfio::RdfSerializer`
//!                          (see `serialize_triples`). N-Quads/TriG output places every
//!                          triple in the default graph, since CONSTRUCT/DESCRIBE results
//!                          are graph-agnostic in this server's `QueryResult::Triples`
//!                          representation.
//!
//! Bulk-load / Graph Store PUT/POST bodies are parsed by Content-Type via
//! `oxrdfio::RdfFormat::from_media_type` + `oxrdfio::RdfParser` (see
//! `parse_body_triples`), falling back to Turtle for an unrecognised/absent
//! Content-Type.
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
//! use oxigraph_nova_engine_memory::MemoryStore;
//! use oxigraph_nova_server::Server;
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
//! single-`Mutex<LoudsStoreInner>` design (see
//! `oxigraph_nova_engine_ring`'s `store.rs` module doc comment, "Isolation
//! semantics", for the storage-level explanation), demonstrated directly by
//! `crates/server/nova-server/tests/isolation.rs`'s two integration tests.

mod service_description;

/// mimalloc purge tuning (bulk-load/compaction transient-memory fix).
///
/// `LoudsStore::bulk_load()`/`compact_locked()` build the Ring's 6 LOUDS
/// tries via a paired-construction algorithm (`cltj::build_cltj_data`) that
/// allocates several large (tens-to-hundreds-of-MiB, at multi-million-triple
/// scale) transient `Vec<[u32;3]>` sort/dedup scratch buffers. mimalloc —
/// like every segmented allocator — does not return a freed *segment* to
/// the OS the instant it becomes empty; by default it delays 10ms
/// (`mi_option_purge_delay`) before actually decommitting/munmap-ing it, on
/// the theory that a segment freed now is likely to be needed again soon.
/// That default is a fine trade-off for steady-state request/response
/// workloads, but it actively fights us here: dozens of these giant scratch
/// buffers are allocated and freed back-to-back in a tight loop while
/// building all 6 orderings for every graph, so mimalloc's 10ms retention
/// window means most of them are all still "on hold" simultaneously right
/// as the *next* one is allocated — inflating the process's peak physical
/// footprint far past the sum of what's actually alive at any single
/// instant (measured: ~2.7-3.3 GiB peak/steady-state physical footprint for
/// a 500K-entity/12.5M-triple in-memory `nova_serve`, versus
/// `memory_breakdown()`'s self-reported ~494 MiB of real, live data).
///
/// This module provides the two-part fix, shared by both `nova_serve` and
/// `oxigraph` (nova-cli), so it's applied uniformly wherever a `LoudsStore`
/// can be bulk-loaded:
///
///   1. [`tune_mimalloc_purge_delay`] — sets `mi_option_purge_delay` to 0
///      ("immediate purging"), so freed scratch-buffer segments become
///      purge-eligible instantly instead of being held for 10ms. **Must be
///      called as the very first statement in `main()`**, before any other
///      allocation happens (including `tracing_subscriber::fmt::init()`) —
///      mimalloc reads its options lazily on first use, and once a first
///      allocation has locked in a default, later `mi_option_set` calls for
///      that option are ignored.
///   2. [`mimalloc_collect_now`] — call once right after a bulk-load/compact
///      finishes, to eagerly walk mimalloc's per-thread heaps and push any
///      remaining purge-eligible-but-not-yet-purged segments back to the OS
///      immediately, rather than waiting for mimalloc's own
///      background/next-allocation-triggered sweep.
///
/// Measured effect (500K-entity/12.5M-triple in-memory `nova_serve`, `vmmap
/// -summary`): steady-state physical footprint 2.7 GiB → 569.5 MiB (~4.8x
/// reduction); peak physical footprint 2.7-3.3 GiB → 2.2 GiB.
///
/// The equivalent `MIMALLOC_PURGE_DELAY=0` environment variable produces
/// identical results, but wiring the `mi_option_set` call directly into
/// `main()` makes the tuning load-bearing in the binary itself rather than
/// something every operator/script launching it must remember to set.
pub mod mimalloc_tuning {
    /// The raw C `mi_option_e` ordinal for `mi_option_purge_delay`.
    ///
    /// Not exposed as a named Rust constant by `libmimalloc-sys` 0.1.49's
    /// hand-written `extended.rs` bindings (only a subset of mimalloc's
    /// `mi_option_e` enum is). Cross-referencing the vendored C headers
    /// confirms `mi_option_purge_delay` sits at ordinal **15** in *both* the
    /// `v2` and `v3` mimalloc headers this crate vendors (`c_src/mimalloc/
    /// {v2,v3}/include/mimalloc.h`) — matching the position immediately
    /// after `libmimalloc-sys`'s own named constant
    /// `mi_option_eager_commit_delay = 14`, confirming the numbering lines
    /// up. `libmimalloc-sys`/`mimalloc` are pinned to exact versions in
    /// `Cargo.toml`, so this ordinal cannot silently drift out from under us
    /// on a routine `cargo update`.
    const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

    /// Set mimalloc's purge delay to 0 (immediate purging). **Call this as
    /// the very first statement in `main()`** — see the module doc comment
    /// for why timing matters.
    pub fn tune_mimalloc_purge_delay() {
        // SAFETY: `mi_option_set` is documented as safe to call at any time
        // from any thread; it just writes an internal mimalloc option value.
        unsafe {
            libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0);
        }
    }

    /// Eagerly walk mimalloc's heaps and purge/release any freed-but-not-yet
    /// -returned segments back to the OS right now. Call once, right after a
    /// bulk-load/compact's burst of large transient scratch-buffer
    /// allocations finishes.
    pub fn mimalloc_collect_now() {
        // SAFETY: `mi_collect` is documented as safe to call at any time
        // from any thread.
        unsafe {
            libmimalloc_sys::mi_collect(true);
        }
    }
}

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Query as AxumQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use oxigraph_nova_core::{QuadStore, TextSearch};
use oxigraph_nova_cypher::{parse_and_lower, parse_and_lower_update};
use oxigraph_nova_query::{
    CancellationToken, EvalLimitError, Evaluator, QueryOptions, QueryResult, ServiceHandler,
    Solutions, StoreDataset, clear_graph, execute_update,
};
use oxigraph_nova_reasoning::{ReasoningEngine, ReasoningState};
use oxigraph_nova_shacl::{NativeValidator, ShaclValidator};
use oxrdf::{GraphName, NamedNode, NamedOrBlankNode, Quad, Term, Triple, Variable};
use oxrdfio::{JsonLdProfileSet, RdfFormat, RdfParser, RdfSerializer};
use serde::Deserialize;
use service_description::generate_service_description_graph;
use sparesults::{QueryResultsFormat, QueryResultsSerializer};
use spargebra::SparqlParser;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::CorsLayer;

// ── Server-level metrics (process-wide) ────────────────────────────────────────
//
// Counters for the hand-rolled `/metrics` endpoint (Prometheus text format;
// Deliberately simple `AtomicU64`s rather than a metrics crate dependency
// — this process
// normally runs exactly one `Server`, so process-wide statics are sufficient
// and avoid adding a new dependency for a handful of counters.
static QUERIES_TOTAL: AtomicU64 = AtomicU64::new(0);
static QUERY_ERRORS_TOTAL: AtomicU64 = AtomicU64::new(0);
static QUERY_TIMEOUTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static QUERY_RESULT_LIMIT_EXCEEDED_TOTAL: AtomicU64 = AtomicU64::new(0);
static QUERY_REJECTED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Default [`ServiceHandler`] for a freshly-constructed [`Server`]: with the
/// `http-client` feature enabled, a real HTTP-backed
/// `oxigraph_nova_query::HttpServiceHandler`; without it, `None` (`SERVICE`
/// clauses are unsupported until [`Server::with_service_handler`] is called).
#[cfg(feature = "http-client")]
fn default_service_handler() -> Option<Arc<dyn ServiceHandler>> {
    Some(Arc::new(oxigraph_nova_query::HttpServiceHandler::new()))
}

#[cfg(not(feature = "http-client"))]
fn default_service_handler() -> Option<Arc<dyn ServiceHandler>> {
    None
}

// ── Application state ─────────────────────────────────────────────────────────

/// Shared server state.  Holds only an `Arc` to the backing store so axum can
/// cheaply clone it for every request without requiring `S: Clone`.
pub struct AppState<S: QuadStore + ?Sized + 'static> {
    pub store: Arc<S>,
    /// Per-query wall-clock timeout, if configured via
    /// `Server::with_query_timeout` / `--query-timeout-s`.
    pub query_timeout: Option<Duration>,
    /// Cap on the number of result rows/triples a single query may produce,
    /// if configured via `Server::with_max_results` / `--max-results`.
    pub max_results: Option<usize>,
    /// Bounds the number of query evaluations running concurrently, if
    /// configured via `Server::with_max_parallel_queries` /
    /// `--max-parallel-queries`. `None` means unbounded.
    pub query_semaphore: Option<Arc<Semaphore>>,
    /// The configured permit count backing `query_semaphore`, kept alongside
    /// it so `/metrics` can report a true in-flight count
    /// (`max_parallel_queries - available_permits()`) rather than just the
    /// raw available-permits figure. Always `Some` iff `query_semaphore` is.
    pub max_parallel_queries: Option<usize>,
    /// Opt-in full-text search backend, if configured via
    /// `Server::with_text_search` / `--fulltext`. Threaded into every
    /// query's `QueryOptions` so `text:query`/`text:contains` extension
    /// functions can be dispatched by the evaluator (see
    /// `oxigraph_nova_query::options::QueryOptions::text_search`).
    pub text_search: Option<Arc<dyn TextSearch>>,
    /// Opt-in SPARQL 1.1 Federated Query (`SERVICE`) handler, if configured
    /// via `Server::with_service_handler`. Threaded into every query's
    /// `QueryOptions` so `SERVICE` clauses can be dispatched (see
    /// `oxigraph_nova_query::options::QueryOptions::service_handler`).
    pub service_handler: Option<Arc<dyn ServiceHandler>>,
    /// Opt-in OWL 2 RL reasoning overlay cache, if configured via
    /// `Server::with_reasoning` / `--reasoning`. When present, `/sparql`
    /// evaluates every query over the cached
    /// `oxigraph_nova_reasoning::ReasoningDataset` overlay (rebuilt lazily
    /// when the store's compaction generation advances — see
    /// `reasoning_state`'s module doc comment) instead of the raw store.
    pub reasoning: Option<Arc<ReasoningState<S>>>,
    /// If `true`, every write-capable handler (`/update`, and `PUT`/`POST`/
    /// `DELETE` on `/store`) rejects the request immediately with `403
    /// Forbidden` instead of executing it. Configured via
    /// `Server::read_only` / `oxigraph serve-read-only`. See that method's
    /// doc comment for the important caveat about what this does and does
    /// not guarantee.
    pub read_only: bool,
    /// Server-wide default equivalent to upstream Oxigraph's
    /// `serve --union-default-graph`, configured via
    /// `Server::with_union_default_graph` / `--union-default-graph`.
    /// Threaded into every query's `QueryOptions` (see
    /// `oxigraph_nova_query::options::QueryOptions::union_default_graph`).
    pub union_default_graph: bool,
}

/// Manual `Clone` impl — `Arc<S>` is always `Clone` regardless of whether `S`
/// is `Clone`, so we do not impose that bound on the user's store type.
impl<S: QuadStore + ?Sized + 'static> Clone for AppState<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            query_timeout: self.query_timeout,
            max_results: self.max_results,
            query_semaphore: self.query_semaphore.clone(),
            max_parallel_queries: self.max_parallel_queries,
            text_search: self.text_search.clone(),
            service_handler: self.service_handler.clone(),
            reasoning: self.reasoning.clone(),
            read_only: self.read_only,
            union_default_graph: self.union_default_graph,
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
pub struct Server<S: QuadStore + ?Sized + 'static> {
    store: Arc<S>,
    query_timeout: Option<Duration>,
    max_results: Option<usize>,
    max_parallel_queries: Option<usize>,
    text_search: Option<Arc<dyn TextSearch>>,
    service_handler: Option<Arc<dyn ServiceHandler>>,
    reasoning: Option<Arc<ReasoningState<S>>>,
    read_only: bool,
    union_default_graph: bool,
}

impl<S: QuadStore + ?Sized + Send + Sync + 'static> Server<S> {
    /// Construct a new server around `store`.
    ///
    /// When this crate is built with the `http-client` feature, `SERVICE`
    /// clauses are supported out of the box: `service_handler` defaults to
    /// an `oxigraph_nova_query::HttpServiceHandler`, a real HTTP-backed
    /// implementation that issues SPARQL 1.1 Protocol requests to the
    /// service's IRI (see its docs). Without that feature (the default),
    /// `service_handler` starts `None` and `SERVICE` is unsupported — call
    /// [`Self::with_service_handler`] to override either way.
    pub fn new(store: Arc<S>) -> Self {
        Self {
            store,
            query_timeout: None,
            max_results: None,
            max_parallel_queries: None,
            text_search: None,
            service_handler: default_service_handler(),
            reasoning: None,
            read_only: false,
            union_default_graph: false,
        }
    }

    /// Enforce a wall-clock timeout on `/sparql` query evaluation. Queries
    /// running longer than `timeout` are cancelled cooperatively (via
    /// `oxigraph_nova_query::CancellationToken`) and the request fails with
    /// `504 Gateway Timeout`. Mirrors upstream Oxigraph's `--timeout` flag.
    pub fn with_query_timeout(mut self, timeout: Duration) -> Self {
        self.query_timeout = Some(timeout);
        self
    }

    /// Cap the number of result rows (SELECT) / triples (CONSTRUCT/DESCRIBE)
    /// a single `/sparql` query may produce. Exceeding the cap aborts
    /// evaluation early and the request fails with `413 Payload Too Large`.
    pub fn with_max_results(mut self, max: usize) -> Self {
        self.max_results = Some(max);
        self
    }

    /// Bound the number of `/sparql` query evaluations that may run
    /// concurrently. A request arriving while `max` evaluations are already
    /// in flight is rejected immediately with `503 Service Unavailable`
    /// rather than being queued, so a burst of expensive queries can't pile
    /// up unboundedly waiting on the single `Mutex<LoudsStoreInner>`.
    pub fn with_max_parallel_queries(mut self, max: usize) -> Self {
        self.max_parallel_queries = Some(max);
        self
    }

    /// Attach a full-text search backend (see `oxigraph_nova_core::TextSearch`),
    /// enabling `text:query`/`text:contains` extension-function dispatch for
    /// every query this server evaluates. Typically `store.clone() as
    /// Arc<dyn TextSearch>` when `S` is `LoudsStore` built with the
    /// `fulltext` cargo feature and `LoudsStore::enable_fulltext` has been
    /// called — see `--fulltext` in `nova_serve`/`oxigraph serve`.
    pub fn with_text_search(mut self, ts: Arc<dyn TextSearch>) -> Self {
        self.text_search = Some(ts);
        self
    }

    /// Attach a [`ServiceHandler`], enabling SPARQL 1.1 Federated Query
    /// `SERVICE` clause evaluation for every query this server evaluates.
    /// Without this, a non-`SILENT` `SERVICE` clause errors and a
    /// `SILENT` one evaluates to zero solutions (see
    /// `oxigraph_nova_query::service::ServiceHandler` and
    /// `QueryOptions::with_service_handler`).
    pub fn with_service_handler(mut self, handler: Arc<dyn ServiceHandler>) -> Self {
        self.service_handler = Some(handler);
        self
    }

    /// Enable OWL 2 RL reasoning: every `/sparql` query is evaluated over an
    /// in-memory [`oxigraph_nova_reasoning::ReasoningDataset`] overlay built
    /// by `engine`, rebuilt lazily whenever the store's compaction
    /// generation advances (see [`reasoning_state::ReasoningState`]'s module
    /// doc comment for the full staleness policy). Also exposes `GET
    /// /reasoning/diagnostics` and advertises the OWL-RL entailment regime
    /// on the SPARQL Service Description (`GET /`).
    pub fn with_reasoning(mut self, engine: Arc<dyn ReasoningEngine>) -> Self {
        self.reasoning = Some(Arc::new(ReasoningState::new(engine)));
        self
    }

    /// Make this server read-only: every write-capable handler (`/update`,
    /// and `PUT`/`POST`/`DELETE` on `/store`) rejects the request
    /// immediately with `403 Forbidden` instead of executing it.
    ///
    /// **Important caveat**: this only guarantees that *this server
    /// process* will never write to the store — it is an HTTP-layer gate,
    /// not a storage-level guarantee. `LoudsStore` still uses a
    /// single-`Mutex<LoudsStoreInner>`, single-writer WAL design (see
    /// `oxigraph_nova_engine_ring::store`'s module doc, "Isolation
    /// semantics"); this flag does **not** by itself make it safe to run
    /// this server concurrently against a `--location` directory that
    /// another process is actively writing to. Used by `oxigraph
    /// serve-read-only`.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Enable a server-wide default equivalent to upstream Oxigraph's
    /// `serve --union-default-graph`: a query with *no* `FROM`/`FROM NAMED`
    /// dataset clause of its own then uses the RDF merge of the default
    /// graph and every named graph as its effective default graph, instead
    /// of just the store's actual default graph. A query that specifies its
    /// own `FROM`/`FROM NAMED` clause is unaffected either way. See
    /// `oxigraph_nova_query::options::QueryOptions::union_default_graph`'s
    /// doc comment for the full semantics.
    pub fn with_union_default_graph(mut self, on: bool) -> Self {
        self.union_default_graph = on;
        self
    }

    /// Build the axum `Router`.
    ///
    /// Returns a router that can be used directly in integration tests via
    /// `tower::ServiceExt::oneshot`, or passed to `axum::serve`.
    pub fn into_router(self) -> Router {
        let state = AppState {
            store: self.store,
            query_timeout: self.query_timeout,
            max_results: self.max_results,
            query_semaphore: self
                .max_parallel_queries
                .map(|n| Arc::new(Semaphore::new(n))),
            max_parallel_queries: self.max_parallel_queries,
            text_search: self.text_search,
            service_handler: self.service_handler,
            reasoning: self.reasoning,
            read_only: self.read_only,
            union_default_graph: self.union_default_graph,
        };

        Router::new()
            .route("/sparql", get(sparql_get::<S>).post(sparql_post::<S>))
            // `/query` is an alias matching upstream Oxigraph's endpoint
            // naming (`oxigraph serve` exposes `POST /query`), so clients
            // written against Oxigraph's conventions work against Nova
            // unmodified.
            .route("/query", get(sparql_get::<S>).post(sparql_post::<S>))
            .route("/update", post(sparql_update::<S>))
            // Experimental openCypher query surface — see the module doc
            // comment above for the supported subset and dispatch rules.
            .route("/cypher", get(cypher_get::<S>).post(cypher_post::<S>))
            .route("/cypher/update", post(cypher_update_post::<S>))
            .route(

                "/store",
                get(store_get::<S>)
                    .put(store_put::<S>)
                    .post(store_post::<S>)
                    .delete(store_delete::<S>),
            )
            // Machine-readable SPARQL Service Description (`sd:` vocabulary),
            // matching upstream Oxigraph's `oxigraph serve`. Some SPARQL
            // tooling fetches this before sending real requests.
            .route("/", get(service_description_get::<S>))
            // Prometheus text-format observability endpoint (dataset size,
            // delta size, compaction count/duration, query counters, LFTJ
            // vs nested-loop fallback rate — see `metrics_get`'s doc comment).
            .route("/metrics", get(metrics_get::<S>))
            // OWL 2 RL reasoning diagnostics (only meaningful when
            // `Server::with_reasoning` was configured — see
            // `reasoning_diagnostics_get`'s doc comment).
            .route(
                "/reasoning/diagnostics",
                get(reasoning_diagnostics_get::<S>),
            )
            // SHACL validation of the store's data against a shapes graph
            // supplied in the request body (see `validate_post`'s doc
            // comment).
            .route("/validate", post(validate_post::<S>))
            // Permissive CORS so browser-based SPARQL clients (e.g. YASGUI,
            // or any web app served from a different origin/port) can query
            // this endpoint directly via `fetch`/XHR without a proxy.
            .layer(CorsLayer::permissive())
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
async fn sparql_get<S: QuadStore + ?Sized + 'static>(
    State(state): State<AppState<S>>,
    AxumQuery(params): AxumQuery<SparqlQueryParams>,
    headers: HeaderMap,
) -> Response {
    match params.query {
        None => (StatusCode::BAD_REQUEST, "Missing ?query= parameter").into_response(),
        Some(q) => execute_sparql_query(&state, &q, accept_header(&headers)).await,
    }
}

/// `POST /sparql`
///
/// Accepted content-types (SPARQL 1.1 Protocol § 2.1):
///   - `application/sparql-query`          — body is the SPARQL text
///   - `application/x-www-form-urlencoded` — body contains `query=<encoded>`
///   - `text/plain` / no content-type      — lenient: body treated as SPARQL text
async fn sparql_post<S: QuadStore + ?Sized + 'static>(
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

    execute_sparql_query(&state, &query_str, accept_header(&headers)).await
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
async fn sparql_update<S: QuadStore + ?Sized + 'static>(
    State(state): State<AppState<S>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if state.read_only {
        return (StatusCode::FORBIDDEN, "This server is read-only").into_response();
    }
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

/// `GET /cypher?query=<percent-encoded Cypher>`
async fn cypher_get<S: QuadStore + ?Sized + 'static>(
    State(state): State<AppState<S>>,
    AxumQuery(params): AxumQuery<SparqlQueryParams>,
    headers: HeaderMap,
) -> Response {
    match params.query {
        None => (StatusCode::BAD_REQUEST, "Missing ?query= parameter").into_response(),
        Some(q) => execute_cypher_query(&state, &q, accept_header(&headers)).await,
    }
}

/// `POST /cypher`
///
/// Accepted content-types (mirrors `/sparql`'s leniency — there is no
/// registered `application/cypher-query` media type to require):
///   - `application/x-www-form-urlencoded` — body contains `query=<encoded>`
///   - `text/plain` / no content-type      — lenient: body treated as Cypher text
async fn cypher_post<S: QuadStore + ?Sized + 'static>(
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

    let query_str: String = if ct.starts_with("application/x-www-form-urlencoded") {
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
        // Lenient: accept untyped or plain-text bodies as direct Cypher.
        body
    } else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Use Content-Type: text/plain or application/x-www-form-urlencoded",
        )
            .into_response();
    };

    execute_cypher_query(&state, &query_str, accept_header(&headers)).await
}

/// `POST /cypher/update` — openCypher write clauses
/// (`CREATE`/`SET`/`DELETE`/`DETACH DELETE`/`REMOVE`).
///
/// Accepted content-types: same leniency as `POST /cypher` above.
async fn cypher_update_post<S: QuadStore + ?Sized + 'static>(
    State(state): State<AppState<S>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if state.read_only {
        return (StatusCode::FORBIDDEN, "This server is read-only").into_response();
    }
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "Empty request body").into_response();
    }

    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let update_str: String = if ct.starts_with("application/x-www-form-urlencoded") {
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
            "Use Content-Type: text/plain or application/x-www-form-urlencoded",
        )
            .into_response();
    };

    execute_cypher_update(&state.store, &update_str)
}

/// `GET /` — SPARQL Service Description.
///
/// Returns a machine-readable capabilities document (the `sd:` vocabulary,
/// `http://www.w3.org/ns/sparql-service-description#`) describing this
/// endpoint, content-negotiated via `Accept` the same way as CONSTRUCT/
/// DESCRIBE results (see `serialize_triples`). Some SPARQL tooling (e.g.
/// query builders) fetches this before sending real requests, to discover
/// which result/RDF formats and query/update languages are supported rather
/// than guessing.
///
/// `sd:endpoint` is derived from the request's `Host` header (falling back
/// to a relative-URL-friendly empty value if absent), naming the `/sparql`
/// query endpoint.
async fn service_description_get<S: QuadStore + ?Sized + 'static>(
    State(state): State<AppState<S>>,
    headers: HeaderMap,
) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let endpoint_url = format!("http://{host}/sparql");
    let graph = generate_service_description_graph(
        &endpoint_url,
        state.text_search.is_some(),
        state.reasoning.is_some(),
        service_description::GEOSPARQL_COMPILED_IN,
        state.union_default_graph,
    );

    serialize_triples(&graph, accept_header(&headers))
}

/// `GET /reasoning/diagnostics` — OWL 2 RL reasoning overlay diagnostics.
///
/// Returns `404 Not Found` if `Server::with_reasoning` was never configured
/// (there is no overlay to report on). Otherwise, returns the current
/// (possibly lazily-rebuilt — see [`reasoning_state::ReasoningState`]'s
/// module doc comment) overlay's diagnostics as JSON:
///
/// ```json
/// {
///   "enabled": true,
///   "inferred_len": 1234,
///   "diagnostics": [
///     {"rule": "decode", "message": "...", "severity": "info"},
///     {"rule": "prp-asyp", "message": "...", "severity": "violation"},
///     ...
///   ]
/// }
/// ```
///
/// **Blocking.** Building/rebuilding the overlay runs on a
/// `spawn_blocking` thread, matching `execute_sparql_query`'s handling of
/// the same call (see `ReasoningState::current`'s doc comment).
async fn reasoning_diagnostics_get<S: QuadStore + ?Sized + 'static>(
    State(state): State<AppState<S>>,
) -> Response {
    let Some(rs) = state.reasoning.clone() else {
        return (
            StatusCode::NOT_FOUND,
            "Reasoning is not enabled on this server (see --reasoning)",
        )
            .into_response();
    };

    let store = Arc::clone(&state.store);
    let result = tokio::task::spawn_blocking(move || rs.current(&store)).await;

    match result {
        Err(join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("internal error: {join_err}"),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to build reasoning overlay: {e}"),
        )
            .into_response(),
        Ok(Ok(overlay)) => {
            let diagnostics: Vec<serde_json::Value> = overlay
                .diagnostics()
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "rule": d.rule,
                        "message": d.message,
                        "severity": if d.is_violation() { "violation" } else { "info" },
                    })
                })
                .collect();
            let body = serde_json::json!({
                "enabled": true,
                "inferred_len": overlay.inferred_len(),
                "diagnostics": diagnostics,
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                body.to_string(),
            )
                .into_response()
        }
    }
}

/// `POST /validate` — SHACL validation of the store's data against a shapes
/// graph supplied in the request body.
///
/// The shapes graph is content-negotiated the same way as Graph Store
/// Protocol/bulk-load bodies (see `parse_body_triples`): `Content-Type` is
/// resolved to an [`RdfFormat`] via `RdfFormat::from_media_type`, falling
/// back to Turtle if absent/unrecognised. The data graph being validated is
/// always this server's own store (every quad, across every graph) —
/// there is no way to scope validation to a subset of the store via this
/// endpoint.
///
/// Uses [`NativeValidator`] (Nova's dependency-free SHACL Core subset — see
/// `oxigraph_nova_shacl`'s crate docs for current constraint/target
/// coverage). Returns a JSON report:
///
/// ```json
/// {
///   "conforms": false,
///   "violation_count": 1,
///   "warning_count": 0,
///   "results": [
///     {
///       "focus_node": "http://ex/alice",
///       "path": "http://ex/age",
///       "source_shape": "http://ex/PersonShape",
///       "source_constraint_component": "http://www.w3.org/ns/shacl#MinCountConstraintComponent",
///       "severity": "Violation",
///       "message": "...",
///       "value": null
///     },
///     ...
///   ]
/// }
/// ```
///
/// `focus_node`/`source_shape`/`value` are rendered via each `Term`'s
/// `to_string()` (an IRI's `<...>`-free form for `NamedNode`s, an N-Triples
/// literal for `Literal`s, `_:...` for blank nodes — matches `Term`'s
/// `Display` impl).
///
/// **Blocking.** Shape compilation + validation runs on a `spawn_blocking`
/// thread, matching every other potentially-expensive handler in this file.
async fn validate_post<S: QuadStore + ?Sized + 'static>(
    State(state): State<AppState<S>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let shape_triples = match parse_body_triples(ct, &body) {
        Ok(t) => t,
        Err(resp) => return *resp,
    };
    let shapes: Vec<Quad> = shape_triples
        .into_iter()
        .map(|t| Quad::new(t.subject, t.predicate, t.object, GraphName::DefaultGraph))
        .collect();

    let store = Arc::clone(&state.store);
    let result = tokio::task::spawn_blocking(move || {
        let dataset = StoreDataset::new(store);
        NativeValidator::new().validate(&shapes, &dataset)
    })
    .await;

    match result {
        Err(join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("internal error: {join_err}"),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("SHACL validation error: {e}"),
        )
            .into_response(),
        Ok(Ok(report)) => {
            let results: Vec<serde_json::Value> = report
                .results
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "focus_node": r.focus_node.to_string(),
                        "path": r.path.as_ref().map(|p| p.to_string()),
                        "source_shape": r.source_shape.to_string(),
                        "source_constraint_component": r.source_constraint_component.to_string(),
                        "severity": format!("{:?}", r.severity),
                        "message": r.message,
                        "value": r.value.as_ref().map(|v| v.to_string()),
                    })
                })
                .collect();
            let body = serde_json::json!({
                "conforms": report.conforms,
                "violation_count": report.violation_count(),
                "warning_count": report.warning_count(),
                "results": results,
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                body.to_string(),
            )
                .into_response()
        }
    }
}

/// `GET /metrics` — Prometheus text-exposition-format observability endpoint.
///
/// Hand-rolled rather than pulled in via a metrics crate dependency — a
/// deliberate choice to avoid a new dependency for a handful of gauges/
/// counters. Exposes:
///
///   - `nova_store_triples`               (gauge) — total quads in the store.
///   - `nova_store_delta_entries`         (gauge) — live entries in the
///     uncompacted write buffer, if the backend has one (omitted otherwise).
///   - `nova_compactions_total`           (counter) — completed compactions
///     (manual + automatic), if the backend supports compaction.
///   - `nova_compaction_duration_seconds_total` (counter) — cumulative time
///     spent inside compaction, if the backend supports compaction.
///   - `nova_queries_total`               (counter) — `/sparql` requests received.
///   - `nova_query_errors_total`          (counter) — evaluation errors (parse
///     errors are not counted here; they never reach the evaluator).
///   - `nova_query_timeouts_total`        (counter) — queries aborted by
///     `--query-timeout-s`.
///   - `nova_query_result_limit_exceeded_total` (counter) — queries aborted by
///     `--max-results`.
///   - `nova_queries_rejected_total`      (counter) — queries rejected outright
///     by `--max-parallel-queries` (503 Service Unavailable).
///   - `nova_queries_in_flight`           (gauge) — currently-executing queries,
///     derived from the configured semaphore's available permits (omitted if
///     `--max-parallel-queries` is not set, since there is then no semaphore
///     to derive it from).
///   - `nova_lftj_queries_total` / `nova_lftj_fallback_total` (counters) —
///     how many BGP evaluations took the Leapfrog Triejoin fast path vs.
///     fell back to nested-loop join, process-wide (see
///     `oxigraph_nova_query::lftj`'s module doc comment, "LFTJ fallback
///     conditions", for why a fallback might happen).
///
/// All counters are process-wide (not per-request-scoped), consistent with
/// normal Prometheus semantics — a scraper polls this endpoint periodically
/// and computes rates itself.
async fn metrics_get<S: QuadStore + ?Sized + 'static>(
    State(state): State<AppState<S>>,
) -> Response {
    let mut out = String::new();

    macro_rules! metric {
        ($name:expr, $help:expr, $type:expr, $value:expr) => {
            let _ = writeln!(out, "# HELP {} {}", $name, $help);
            let _ = writeln!(out, "# TYPE {} {}", $name, $type);
            let _ = writeln!(out, "{} {}", $name, $value);
        };
    }

    match state.store.len() {
        Ok(n) => {
            metric!(
                "nova_store_triples",
                "Total number of quads currently in the store.",
                "gauge",
                n
            );
        }
        Err(e) => {
            tracing::warn!("metrics: failed to read store.len(): {e}");
        }
    }

    if let Some(n) = state.store.delta_len() {
        metric!(
            "nova_store_delta_entries",
            "Live entries (inserts + tombstones) in the uncompacted write buffer.",
            "gauge",
            n
        );
    }
    if let Some(n) = state.store.compaction_count() {
        metric!(
            "nova_compactions_total",
            "Total number of completed compactions (manual + automatic).",
            "counter",
            n
        );
    }
    if let Some(s) = state.store.compaction_duration_seconds_total() {
        metric!(
            "nova_compaction_duration_seconds_total",
            "Cumulative wall-clock time spent inside compaction, in seconds.",
            "counter",
            s
        );
    }

    metric!(
        "nova_queries_total",
        "Total number of /sparql (SELECT/ASK/CONSTRUCT/DESCRIBE) requests received.",
        "counter",
        QUERIES_TOTAL.load(Ordering::Relaxed)
    );
    metric!(
        "nova_query_errors_total",
        "Total number of query evaluation errors (excludes parse errors and limit/cancellation outcomes).",
        "counter",
        QUERY_ERRORS_TOTAL.load(Ordering::Relaxed)
    );
    metric!(
        "nova_query_timeouts_total",
        "Total number of queries aborted by the configured --query-timeout-s.",
        "counter",
        QUERY_TIMEOUTS_TOTAL.load(Ordering::Relaxed)
    );
    metric!(
        "nova_query_result_limit_exceeded_total",
        "Total number of queries aborted by the configured --max-results.",
        "counter",
        QUERY_RESULT_LIMIT_EXCEEDED_TOTAL.load(Ordering::Relaxed)
    );
    metric!(
        "nova_queries_rejected_total",
        "Total number of queries rejected outright by --max-parallel-queries (503 Service Unavailable).",
        "counter",
        QUERY_REJECTED_TOTAL.load(Ordering::Relaxed)
    );
    if let (Some(sem), Some(max)) = (&state.query_semaphore, state.max_parallel_queries) {
        // `Semaphore` doesn't track "in use" directly; derive a true
        // in-flight count as `max_parallel_queries - available_permits()`.
        let in_flight = max.saturating_sub(sem.available_permits());
        metric!(
            "nova_queries_in_flight",
            "Currently-executing /sparql query evaluations (max_parallel_queries - available permits).",
            "gauge",
            in_flight
        );
    }

    metric!(
        "nova_lftj_queries_total",
        "Total number of BGP evaluations that used the Leapfrog Triejoin fast path.",
        "counter",
        oxigraph_nova_query::lftj_used_total()
    );
    metric!(
        "nova_lftj_fallback_total",
        "Total number of BGP evaluations that fell back to nested-loop join.",
        "counter",
        oxigraph_nova_query::lftj_fallback_total()
    );

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        out,
    )
        .into_response()
}

/// Parse → execute a SPARQL Update request against the store.
fn execute_sparql_update<S: QuadStore + ?Sized + 'static>(
    store: &Arc<S>,
    sparql: &str,
) -> Response {
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

/// Parse (as openCypher, lowering to the equivalent SPARQL Update algebra
/// via `oxigraph_nova_cypher::parse_and_lower_update`) → execute a write
/// request against the store. Mirrors `execute_sparql_update` exactly,
/// swapping only the parse step — everything downstream (the actual
/// `execute_update` call and its error handling) is identical.
fn execute_cypher_update<S: QuadStore + ?Sized + 'static>(
    store: &Arc<S>,
    cypher: &str,
) -> Response {
    let update = match parse_and_lower_update(cypher) {
        Ok(u) => u,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("Cypher parse error: {e}")).into_response();
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
fn graph_exists<S: QuadStore + ?Sized>(
    store: &Arc<S>,
    g: &GraphName,
) -> Result<bool, Box<Response>> {
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

/// Parse an RDF body into `Triple`s, resolving `Content-Type` to an
/// [`RdfFormat`] via `oxrdfio::RdfFormat::from_media_type` (falling back to
/// Turtle for an unrecognised or absent Content-Type — matches the previous
/// hand-rolled dispatch's "default: Turtle" behavior), then parsing via
/// `oxrdfio::RdfParser`.
///
/// Dataset formats (N-Quads/TriG/JSON-LD) parse to `Quad`s (they can express
/// named graphs); since this function only returns `Triple`s (per the Graph
/// Store Protocol / bulk-load callers, which always target one specific
/// graph), any non-default-graph component on a parsed quad is simply
/// dropped and only its triple is kept — mirroring how e.g. a JSON-LD
/// document's own `@graph` nesting still ultimately gets inserted into the
/// one target graph named by the request.
fn parse_body_triples(content_type: &str, body: &[u8]) -> Result<Vec<Triple>, Box<Response>> {
    let err = |e: String| {
        Box::new((StatusCode::BAD_REQUEST, format!("RDF parse error: {e}")).into_response())
    };

    let format = RdfFormat::from_media_type(content_type).unwrap_or(RdfFormat::Turtle);
    RdfParser::from_format(format)
        .for_reader(body)
        .map(|r| r.map(|q| Triple::new(q.subject, q.predicate, q.object)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| err(e.to_string()))
}

/// Insert parsed `Triple`s into `graph` as `Quad`s.
fn insert_triples_into_graph<S: QuadStore + ?Sized>(
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
async fn store_get<S: QuadStore + ?Sized + 'static>(
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
async fn store_put<S: QuadStore + ?Sized + 'static>(
    State(state): State<AppState<S>>,
    AxumQuery(params): AxumQuery<GraphStoreParams>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if state.read_only {
        return (StatusCode::FORBIDDEN, "This server is read-only").into_response();
    }
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
async fn store_post<S: QuadStore + ?Sized + 'static>(
    State(state): State<AppState<S>>,
    AxumQuery(params): AxumQuery<GraphStoreParams>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if state.read_only {
        return (StatusCode::FORBIDDEN, "This server is read-only").into_response();
    }
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
async fn store_delete<S: QuadStore + ?Sized + 'static>(
    State(state): State<AppState<S>>,
    AxumQuery(params): AxumQuery<GraphStoreParams>,
) -> Response {
    if state.read_only {
        return (StatusCode::FORBIDDEN, "This server is read-only").into_response();
    }
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
/// Evaluation itself is synchronous (the evaluator is sync), so it always
/// runs on a blocking-pool thread via `tokio::task::spawn_blocking` — this
/// keeps a single expensive query from stalling the async runtime's worker
/// threads, and is what makes the timeout/cancellation racing below
/// possible (the async task doing the racing needs to stay responsive to
/// the `tokio::time::sleep` future while the blocking evaluation runs
/// concurrently on another thread).
///
/// Resource limits applied here, all configured via `Server`/CLI flags and
/// stored on `AppState` (see the module doc's "Operational hardening" note
/// and `oxigraph_nova_query::options`):
///   - `query_semaphore`: bounds the number of concurrent evaluations. A
///     request arriving when the semaphore is saturated is rejected
///     immediately with `503 Service Unavailable` (no queueing).
///   - `query_timeout`: races evaluation against a `tokio::time::sleep`. On
///     timeout, the evaluator's `CancellationToken` is flipped (cooperative
///     cancellation — see `oxigraph_nova_query::options::CancellationToken`
///     and the per-loop checks in `lftj.rs`/`path.rs`/`evaluator.rs`) and the
///     request fails with `504 Gateway Timeout` once the blocking task
///     actually observes the cancellation and returns.
///   - `max_results`: caps the number of result rows/triples; exceeding it
///     aborts evaluation early with `413 Payload Too Large`.
async fn execute_sparql_query<S: QuadStore + ?Sized + 'static>(
    state: &AppState<S>,
    sparql: &str,
    accept: &str,
) -> Response {
    QUERIES_TOTAL.fetch_add(1, Ordering::Relaxed);

    // 0. Concurrency limit — reject immediately rather than queue, so a
    //    burst of expensive queries can't pile up waiting on the store.
    let _permit = if let Some(sem) = &state.query_semaphore {
        match Arc::clone(sem).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                QUERY_REJECTED_TOTAL.fetch_add(1, Ordering::Relaxed);
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Too many concurrent queries; please try again later",
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    // 1. Parse
    let query = {
        let _span = tracing::info_span!("parse_query").entered();
        match SparqlParser::new().parse_query(sparql) {
            Ok(q) => q,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, format!("SPARQL parse error: {e}"))
                    .into_response();
            }
        }
    };

    // 2. Build per-query options (cancellation token only allocated if a
    //    timeout is actually configured) and hand evaluation to a blocking
    //    thread so this async task stays free to race it against a timer.
    let mut options = QueryOptions::new();
    if let Some(max) = state.max_results {
        options = options.with_max_results(max);
    }
    if let Some(ts) = &state.text_search {
        options = options.with_text_search(Arc::clone(ts));
    }
    if let Some(handler) = &state.service_handler {
        options = options.with_service_handler(Arc::clone(handler));
    }
    if state.union_default_graph {
        options = options.with_union_default_graph(true);
    }
    let cancellation = state.query_timeout.map(|_| CancellationToken::new());
    if let Some(token) = &cancellation {
        options = options.with_cancellation(token.clone());
    }

    let store = Arc::clone(&state.store);
    let reasoning = state.reasoning.clone();
    let query_for_eval = query.clone();
    let mut join = tokio::task::spawn_blocking(move || {
        if let Some(rs) = reasoning {
            let overlay = rs.current(&store)?;
            let evaluator = Evaluator::with_options(&*overlay, options);
            evaluator.evaluate(&query_for_eval)
        } else {
            let dataset = StoreDataset::new(store);
            let evaluator = Evaluator::with_options(&dataset, options);
            evaluator.evaluate(&query_for_eval)
        }
    });

    // 3. Evaluate, racing against the configured timeout (if any).
    let outcome = if let Some(timeout) = state.query_timeout {
        tokio::select! {
            res = &mut join => res,
            _ = tokio::time::sleep(timeout) => {
                if let Some(token) = &cancellation {
                    token.cancel();
                }
                // Wait for the blocking task to observe cancellation and
                // return, so its result (now an `EvalLimitError::Cancelled`)
                // is still reported below rather than the task being
                // silently detached.
                (&mut join).await
            }
        }
    } else {
        (&mut join).await
    };

    // 4. Serialise (or map an evaluation/limit error to an HTTP status).
    match outcome {
        Err(join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("internal error: {join_err}"),
        )
            .into_response(),
        Ok(Err(e)) => match e.downcast_ref::<EvalLimitError>() {
            Some(EvalLimitError::Cancelled) => {
                QUERY_TIMEOUTS_TOTAL.fetch_add(1, Ordering::Relaxed);
                (StatusCode::GATEWAY_TIMEOUT, "Query timed out").into_response()
            }
            Some(EvalLimitError::ResultLimitExceeded(n)) => {
                QUERY_RESULT_LIMIT_EXCEEDED_TOTAL.fetch_add(1, Ordering::Relaxed);
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("Query exceeded the result limit of {n} row(s)"),
                )
                    .into_response()
            }
            None => {
                QUERY_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Evaluation error: {e}"),
                )
                    .into_response()
            }
        },
        Ok(Ok(QueryResult::Boolean(b))) => serialize_boolean(b, accept),
        Ok(Ok(qr @ QueryResult::Solutions { .. })) => match qr.into_solutions_vec() {
            Ok((vars_arc, solutions)) => {
                let vars: Vec<Variable> = vars_arc.to_vec();
                serialize_solutions(&vars, solutions, accept)
            }
            Err(e) => {
                QUERY_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("error collecting results: {e}"),
                )
                    .into_response()
            }
        },

        Ok(Ok(QueryResult::Triples(stream))) => match stream.collect::<anyhow::Result<Vec<_>>>() {
            Ok(triples) => serialize_triples(&triples, accept),
            Err(e) => {
                QUERY_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("error collecting results: {e}"),
                )
                    .into_response()
            }
        },
    }
}

/// Parse (as openCypher, lowering to the equivalent SPARQL algebra via
/// `oxigraph_nova_cypher::parse_and_lower`) → adapt store → evaluate →
/// serialise. Mirrors `execute_sparql_query` exactly, swapping only the
/// parse step (step 1) — every downstream concern (concurrency limiting,
/// timeout/cancellation racing, `max_results`, content-negotiated
/// serialization, metrics) is identical and fully reused since
/// `parse_and_lower` produces the same `spargebra::Query` type the
/// evaluator already consumes.
async fn execute_cypher_query<S: QuadStore + ?Sized + 'static>(
    state: &AppState<S>,
    cypher: &str,
    accept: &str,
) -> Response {
    QUERIES_TOTAL.fetch_add(1, Ordering::Relaxed);

    // 0. Concurrency limit — reject immediately rather than queue.
    let _permit = if let Some(sem) = &state.query_semaphore {
        match Arc::clone(sem).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                QUERY_REJECTED_TOTAL.fetch_add(1, Ordering::Relaxed);
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Too many concurrent queries; please try again later",
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    // 1. Parse (Cypher → SPARQL algebra).
    let query = {
        let _span = tracing::info_span!("parse_cypher_query").entered();
        match parse_and_lower(cypher) {
            Ok(q) => q,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, format!("Cypher parse error: {e}"))
                    .into_response();
            }
        }
    };

    // 2. Build per-query options and hand evaluation to a blocking thread.
    let mut options = QueryOptions::new();
    if let Some(max) = state.max_results {
        options = options.with_max_results(max);
    }
    if let Some(ts) = &state.text_search {
        options = options.with_text_search(Arc::clone(ts));
    }
    if let Some(handler) = &state.service_handler {
        options = options.with_service_handler(Arc::clone(handler));
    }
    if state.union_default_graph {
        options = options.with_union_default_graph(true);
    }
    let cancellation = state.query_timeout.map(|_| CancellationToken::new());
    if let Some(token) = &cancellation {
        options = options.with_cancellation(token.clone());
    }

    let store = Arc::clone(&state.store);
    let reasoning = state.reasoning.clone();
    let query_for_eval = query.clone();
    let mut join = tokio::task::spawn_blocking(move || {
        if let Some(rs) = reasoning {
            let overlay = rs.current(&store)?;
            let evaluator = Evaluator::with_options(&*overlay, options);
            evaluator.evaluate(&query_for_eval)
        } else {
            let dataset = StoreDataset::new(store);
            let evaluator = Evaluator::with_options(&dataset, options);
            evaluator.evaluate(&query_for_eval)
        }
    });

    // 3. Evaluate, racing against the configured timeout (if any).
    let outcome = if let Some(timeout) = state.query_timeout {
        tokio::select! {
            res = &mut join => res,
            _ = tokio::time::sleep(timeout) => {
                if let Some(token) = &cancellation {
                    token.cancel();
                }
                (&mut join).await
            }
        }
    } else {
        (&mut join).await
    };

    // 4. Serialise (or map an evaluation/limit error to an HTTP status).
    match outcome {
        Err(join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("internal error: {join_err}"),
        )
            .into_response(),
        Ok(Err(e)) => match e.downcast_ref::<EvalLimitError>() {
            Some(EvalLimitError::Cancelled) => {
                QUERY_TIMEOUTS_TOTAL.fetch_add(1, Ordering::Relaxed);
                (StatusCode::GATEWAY_TIMEOUT, "Query timed out").into_response()
            }
            Some(EvalLimitError::ResultLimitExceeded(n)) => {
                QUERY_RESULT_LIMIT_EXCEEDED_TOTAL.fetch_add(1, Ordering::Relaxed);
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("Query exceeded the result limit of {n} row(s)"),
                )
                    .into_response()
            }
            None => {
                QUERY_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Evaluation error: {e}"),
                )
                    .into_response()
            }
        },
        Ok(Ok(QueryResult::Boolean(b))) => serialize_boolean(b, accept),
        Ok(Ok(qr @ QueryResult::Solutions { .. })) => match qr.into_solutions_vec() {
            Ok((vars_arc, solutions)) => {
                let vars: Vec<Variable> = vars_arc.to_vec();
                serialize_solutions(&vars, solutions, accept)
            }
            Err(e) => {
                QUERY_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("error collecting results: {e}"),
                )
                    .into_response()
            }
        },
        Ok(Ok(QueryResult::Triples(stream))) => match stream.collect::<anyhow::Result<Vec<_>>>() {
            Ok(triples) => serialize_triples(&triples, accept),
            Err(e) => {
                QUERY_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("error collecting results: {e}"),
                )
                    .into_response()
            }
        },
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

/// Resolve the `Accept` header to an [`RdfFormat`] for CONSTRUCT/DESCRIBE
/// (and Graph Store Protocol `GET`) serialization:
///   - `Accept` contains `application/ld+json` → JSON-LD
///   - `Accept` contains `text/turtle`         → Turtle
///   - `Accept` contains `application/rdf+xml` → RDF/XML
///   - `Accept` contains `application/n-quads` → N-Quads
///   - `Accept` contains `application/trig`    → TriG
///   - anything else (default)                 → N-Triples
fn triples_format_for_accept(accept: &str) -> RdfFormat {
    if accept.contains("application/ld+json") {
        RdfFormat::JsonLd {
            profile: JsonLdProfileSet::empty(),
        }
    } else if accept.contains("text/turtle") {
        RdfFormat::Turtle
    } else if accept.contains("application/rdf+xml") {
        RdfFormat::RdfXml
    } else if accept.contains("application/n-quads") {
        RdfFormat::NQuads
    } else if accept.contains("application/trig") {
        RdfFormat::TriG
    } else {
        RdfFormat::NTriples
    }
}

/// Serialize a CONSTRUCT/DESCRIBE result, content-negotiated via `Accept`
/// (see `triples_format_for_accept`), via `oxrdfio::RdfSerializer`.
/// `RdfSerializer::serialize_triple` works uniformly across every
/// `RdfFormat` variant, including the dataset formats (N-Quads/TriG/JSON-LD)
/// — for those, every triple is placed in the default graph, since
/// CONSTRUCT/DESCRIBE results are graph-agnostic in this server's
/// `QueryResult::Triples` representation.
///
/// Also used directly by the Graph Store Protocol's `GET /store` handler so
/// both code paths share one serializer.
fn serialize_triples(triples: &[Triple], accept: &str) -> Response {
    let format = triples_format_for_accept(accept);
    let mut writer = RdfSerializer::from_format(format).for_writer(Vec::new());
    for t in triples {
        if let Err(e) = writer.serialize_triple(t) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    match writer.finish() {
        Ok(buf) => {
            let content_type = format!("{}; charset=utf-8", format.media_type());
            (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], buf).into_response()
        }
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
    use oxigraph_nova_engine_memory::MemoryStore;
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

    // ── /cypher and /cypher/update ─────────────────────────────────────────────

    /// Insert two Cypher-native nodes (via `CREATE (n {name: ...})`, which
    /// lowers `name` to a `<PROP_NS>name` triple — see
    /// `oxigraph_nova_cypher::PROP_NS`) into `router`'s store, for use by
    /// `MATCH (n) WHERE n.name = ...`-style read tests below. Cypher's data
    /// model (`LABEL_NS`/`REL_NS`/`PROP_NS`-namespaced triples) is distinct
    /// from arbitrary SPARQL-inserted triples like `make_store()`'s
    /// `http://ex/name`, so these read tests seed their own Cypher-created
    /// fixture data rather than reusing `make_store()`.
    async fn seed_cypher_names(router: &Router) {
        for name in ["Alice", "Bob"] {
            let cypher = format!(r#"CREATE (n {{name: "{name}"}})"#);
            let req = Request::builder()
                .method(Method::POST)
                .uri("/cypher/update")
                .body(Body::from(cypher))
                .unwrap();
            let resp = router.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn test_cypher_get_returns_two_names() {
        let router = Server::new(Arc::new(MemoryStore::new())).into_router();
        seed_cypher_names(&router).await;

        // MATCH (n) RETURN n.name — URL-encoded.
        let q = "MATCH+%28n%29+RETURN+n.name";

        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("/cypher?query={q}"))
            .body(Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "expected 200 OK");
        let json = body_json(resp).await;
        let bindings = json["results"]["bindings"].as_array().unwrap();
        assert_eq!(
            bindings.len(),
            2,
            "expected 2 bindings, got {}",
            bindings.len()
        );
    }

    #[tokio::test]
    async fn test_cypher_get_missing_query_returns_400() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/cypher")
            .body(Body::empty())
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_cypher_post_text_plain_match_returns_names() {
        let router = Server::new(Arc::new(MemoryStore::new())).into_router();
        seed_cypher_names(&router).await;

        let cypher = "MATCH (n) RETURN n.name";

        let req = Request::builder()
            .method(Method::POST)
            .uri("/cypher")
            .header("content-type", "text/plain")
            .body(Body::from(cypher))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let bindings = json["results"]["bindings"].as_array().unwrap();
        assert_eq!(bindings.len(), 2);
    }

    #[tokio::test]
    async fn test_cypher_post_no_content_type_is_lenient() {
        let router = Server::new(Arc::new(MemoryStore::new())).into_router();
        seed_cypher_names(&router).await;

        let cypher = "MATCH (n) RETURN n.name";

        let req = Request::builder()
            .method(Method::POST)
            .uri("/cypher")
            .body(Body::from(cypher))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_cypher_post_form_urlencoded() {
        let router = Server::new(Arc::new(MemoryStore::new())).into_router();
        seed_cypher_names(&router).await;

        // query=MATCH (n) RETURN n.name
        let form = "query=MATCH+%28n%29+RETURN+n.name";

        let req = Request::builder()
            .method(Method::POST)
            .uri("/cypher")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let bindings = json["results"]["bindings"].as_array().unwrap();
        assert_eq!(bindings.len(), 2);
    }

    #[tokio::test]
    async fn test_cypher_post_empty_body_returns_400() {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/cypher")
            .body(Body::empty())
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_cypher_post_parse_error_returns_400() {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/cypher")
            .body(Body::from("THIS IS NOT CYPHER"))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_text(resp).await;
        assert!(
            body.contains("Cypher parse error"),
            "expected Cypher parse error message, got: {body}"
        );
    }

    #[tokio::test]
    async fn test_cypher_post_unsupported_content_type_returns_415() {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/cypher")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn test_cypher_update_create_node() {
        let store = make_store();
        let router = Server::new(Arc::clone(&store)).into_router();

        let cypher = r#"CREATE (n {name: "Carol"})"#;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/cypher/update")
            .body(Body::from(cypher))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // The store should have grown by (at least) the new node's name triple.
        let len_after = store.len().unwrap();
        assert!(
            len_after > 3,
            "expected store to have grown past the initial 3 quads, got {len_after}"
        );
    }

    #[tokio::test]
    async fn test_cypher_update_form_urlencoded() {
        let store = make_store();
        let router = Server::new(Arc::clone(&store)).into_router();

        // update=CREATE (n {name: "Dave"})
        let form = "update=CREATE+%28n+%7Bname%3A+%22Dave%22%7D%29";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/cypher/update")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(store.len().unwrap() > 3);
    }

    #[tokio::test]
    async fn test_cypher_update_empty_body_returns_400() {
        let router = make_router();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/cypher/update")
            .body(Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_cypher_update_parse_error_returns_400() {
        let router = make_router();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/cypher/update")
            .body(Body::from("NOT A CYPHER UPDATE {{{"))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_cypher_update_read_only_returns_403() {
        let router = Server::new(make_store()).read_only(true).into_router();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/cypher/update")
            .body(Body::from(r#"CREATE (n {name: "X"})"#))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
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

    // ── SPARQL Service Description: GET / ─────────────────────────────────────

    #[tokio::test]
    async fn test_service_description_default_turtle() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
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
        assert!(
            body.contains("http://www.w3.org/ns/sparql-service-description#Service"),
            "missing sd:Service triple: {body}"
        );
        assert!(
            body.contains("http://www.w3.org/ns/sparql-service-description#endpoint"),
            "missing sd:endpoint triple: {body}"
        );
        assert!(
            body.contains("http://www.w3.org/ns/sparql-service-description#supportedLanguage"),
            "missing sd:supportedLanguage triple: {body}"
        );
        assert!(
            body.contains("http://www.w3.org/ns/sparql-service-description#SPARQLQuery"),
            "missing sd:SPARQLQuery language declaration: {body}"
        );
        assert!(
            body.contains("http://www.w3.org/ns/sparql-service-description#SPARQLUpdate"),
            "missing sd:SPARQLUpdate language declaration: {body}"
        );
        assert!(
            body.contains("http://www.w3.org/ns/sparql-service-description#supportedVersion"),
            "missing sd:supportedVersion triple: {body}"
        );
        // Note: matched with the closing `>` so this doesn't just pass as a
        // substring of the `version-1.2-basic` assertion below.
        assert!(
            body.contains("http://www.w3.org/ns/sparql#version-1.2>"),
            "missing SPARQL 1.2 version declaration: {body}"
        );
        assert!(
            body.contains("http://www.w3.org/ns/sparql#version-1.2-basic"),
            "missing SPARQL 1.2-basic version declaration: {body}"
        );

        assert!(
            body.contains("http://www.w3.org/ns/sparql-service-description#resultFormat"),
            "missing sd:resultFormat triple: {body}"
        );

        assert!(
            body.contains("http://www.w3.org/ns/sparql-service-description#feature"),
            "missing sd:feature triple: {body}"
        );
        assert!(
            body.contains(
                "http://www.w3.org/ns/sparql-service-description#defaultEntailmentRegime"
            ),
            "missing sd:defaultEntailmentRegime triple: {body}"
        );
    }

    #[tokio::test]
    async fn test_service_description_turtle_via_accept() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header("accept", "text/turtle")
            .header("host", "example.org")
            .body(Body::empty())
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
        assert!(
            body.contains("http://example.org/sparql"),
            "expected sd:endpoint to use Host header: {body}"
        );
    }

    // ── serve --union-default-graph ───────────────────────────────────────────

    #[tokio::test]
    async fn test_service_description_omits_union_default_graph_by_default() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header("accept", "application/n-triples")
            .body(Body::empty())
            .unwrap();

        let resp = make_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(
            !body.contains("http://www.w3.org/ns/sparql-service-description#UnionDefaultGraph"),
            "must not advertise sd:UnionDefaultGraph when the server was not built with \
             Server::with_union_default_graph, got:\n{body}"
        );
    }

    #[tokio::test]
    async fn test_service_description_advertises_union_default_graph_when_enabled() {
        let router = Server::new(make_store())
            .with_union_default_graph(true)
            .into_router();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header("accept", "application/n-triples")
            .body(Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(
            body.contains("http://www.w3.org/ns/sparql-service-description#UnionDefaultGraph"),
            "expected sd:UnionDefaultGraph to be advertised when \
             Server::with_union_default_graph(true) was used, got:\n{body}"
        );
    }

    /// A store with one triple in the default graph and another, distinct
    /// triple only in a named graph — used to distinguish "see only the
    /// actual default graph" from "see the RDF merge of default + every
    /// named graph" (`GraphSelector::Union`).
    fn make_store_with_named_graph() -> Arc<MemoryStore> {
        let store = make_store();
        store
            .insert(&Quad::new(
                NamedNode::new_unchecked("http://ex/onlyinnamed"),
                NamedNode::new_unchecked("http://ex/name"),
                Term::Literal(Literal::new_simple_literal("OnlyInNamed")),
                GraphName::NamedNode(NamedNode::new_unchecked("http://ex/g1")),
            ))
            .unwrap();
        store
    }

    /// A FROM-less query must NOT see a named-graph-only triple by default
    /// (the store's actual default graph is used, matching pre-existing
    /// behavior exactly).
    #[tokio::test]
    async fn from_less_query_does_not_see_named_graph_by_default() {
        let router = Server::new(make_store_with_named_graph()).into_router();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(
                "ASK { <http://ex/onlyinnamed> <http://ex/name> \"OnlyInNamed\" }",
            ))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(
            json["boolean"], false,
            "without --union-default-graph, a FROM-less query must only see the \
             store's actual default graph, got: {json}"
        );
    }

    /// The same FROM-less query DOES see the named-graph-only triple once
    /// `Server::with_union_default_graph(true)` is set — the effective
    /// default graph becomes the RDF merge of the default graph and every
    /// named graph.
    #[tokio::test]
    async fn from_less_query_sees_named_graph_when_union_default_graph_enabled() {
        let router = Server::new(make_store_with_named_graph())
            .with_union_default_graph(true)
            .into_router();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(
                "ASK { <http://ex/onlyinnamed> <http://ex/name> \"OnlyInNamed\" }",
            ))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(
            json["boolean"], true,
            "with --union-default-graph, a FROM-less query must see the RDF merge \
             of the default graph and every named graph, got: {json}"
        );
    }

    /// A query with its OWN `FROM`/`FROM NAMED` dataset clause must be
    /// unaffected by `--union-default-graph` either way — the query's own
    /// clause always takes precedence (see
    /// `oxigraph_nova_query::evaluator::dataset_clause_selector`).
    #[tokio::test]
    async fn explicit_from_clause_unaffected_by_union_default_graph_flag() {
        let sparql =
            "ASK FROM <http://ex/g1> { <http://ex/onlyinnamed> <http://ex/name> \"OnlyInNamed\" }";

        for union_default_graph in [false, true] {
            let mut builder = Server::new(make_store_with_named_graph());
            if union_default_graph {
                builder = builder.with_union_default_graph(true);
            }
            let router = builder.into_router();
            let req = Request::builder()
                .method(Method::POST)
                .uri("/sparql")
                .header("content-type", "application/sparql-query")
                .body(Body::from(sparql))
                .unwrap();

            let resp = router.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let json = body_json(resp).await;
            assert_eq!(
                json["boolean"], true,
                "explicit FROM <http://ex/g1> must see the triple regardless of \
                 union_default_graph={union_default_graph}, got: {json}"
            );
        }
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

    // ── Operational hardening: timeout / max-results / max-parallel-queries ──

    /// A `QuadStore` wrapper that sleeps for a configurable duration on every
    /// `quads_for_pattern` call before delegating to the inner store — used
    /// to make timeout-cancellation deterministic in tests (see
    /// `test_query_timeout_returns_504`) without racing real wall-clock
    /// scheduling against a trivially-fast in-memory query.
    struct SlowStore {
        inner: Arc<MemoryStore>,
        delay: std::time::Duration,
    }

    impl oxigraph_nova_core::LftjSource for SlowStore {}

    impl oxigraph_nova_core::QuadStore for SlowStore {
        fn insert(&self, quad: &oxigraph_nova_core::Quad) -> oxigraph_nova_core::Result<bool> {
            self.inner.insert(quad)
        }
        fn remove(&self, quad: &oxigraph_nova_core::Quad) -> oxigraph_nova_core::Result<bool> {
            self.inner.remove(quad)
        }
        fn quads_for_pattern(
            &self,
            subject: Option<&Term>,
            predicate: Option<&NamedNode>,
            object: Option<&Term>,
            graph_name: Option<&oxigraph_nova_core::GraphName>,
        ) -> oxigraph_nova_core::Result<
            Box<
                dyn Iterator<Item = oxigraph_nova_core::Result<oxigraph_nova_core::StoredQuad>>
                    + '_,
            >,
        > {
            std::thread::sleep(self.delay);
            self.inner
                .quads_for_pattern(subject, predicate, object, graph_name)
        }
        fn len(&self) -> oxigraph_nova_core::Result<usize> {
            self.inner.len()
        }
        fn contains(&self, quad: &oxigraph_nova_core::Quad) -> oxigraph_nova_core::Result<bool> {
            self.inner.contains(quad)
        }
    }

    #[tokio::test]
    async fn test_query_timeout_returns_504() {
        // The store sleeps 200ms on every pattern scan; the second triple
        // pattern's between-pattern cancellation check (see
        // `Evaluator::eval_bgp`'s nested-loop fallback) observes the
        // timeout-triggered cancellation and aborts before completing —
        // deterministic regardless of machine speed, since the configured
        // timeout (20ms) is far shorter than the store's fixed 200ms delay.
        let store = Arc::new(SlowStore {
            inner: make_store(),
            delay: Duration::from_millis(200),
        });
        let router = Server::new(store)
            .with_query_timeout(Duration::from_millis(20))
            .into_router();

        let sparql = "SELECT ?s ?p ?o ?s2 ?p2 ?o2 WHERE { ?s ?p ?o . ?s2 ?p2 ?o2 }";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn test_query_without_timeout_still_succeeds() {
        // Sanity check: a generous timeout doesn't interfere with a normal query.
        let store = make_store();
        let router = Server::new(store)
            .with_query_timeout(Duration::from_secs(60))
            .into_router();

        let sparql = "SELECT ?s WHERE { ?s <http://ex/name> ?n }";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let bindings = json["results"]["bindings"].as_array().unwrap();
        assert_eq!(bindings.len(), 2);
    }

    #[tokio::test]
    async fn test_max_results_exceeded_returns_413() {
        // The result cap is enforced between triple-pattern joins in the
        // nested-loop fallback (see `Evaluator::eval_bgp`), so a two-pattern
        // BGP is used: the 3 rows produced by the first pattern are checked
        // against `max_results` before the second pattern (a cross join) is
        // ever evaluated.
        let store = make_store();
        let router = Server::new(store).with_max_results(1).into_router();

        let sparql = "SELECT ?s ?p ?o ?s2 ?p2 ?o2 WHERE { ?s ?p ?o . ?s2 ?p2 ?o2 }";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn test_max_results_not_exceeded_succeeds() {
        let store = make_store();
        let router = Server::new(store).with_max_results(10).into_router();

        let sparql = "SELECT ?s ?p ?o WHERE { ?s ?p ?o }";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_max_parallel_queries_zero_returns_503() {
        // A semaphore with 0 permits is permanently saturated — every query
        // is rejected immediately with 503, without ever touching the store.
        let store = make_store();
        let router = Server::new(store)
            .with_max_parallel_queries(0)
            .into_router();

        let sparql = "SELECT ?s WHERE { ?s ?p ?o }";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_max_parallel_queries_allows_within_limit() {
        let store = make_store();
        let router = Server::new(store)
            .with_max_parallel_queries(4)
            .into_router();

        let sparql = "SELECT ?s WHERE { ?s <http://ex/name> ?n }";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/sparql")
            .header("content-type", "application/sparql-query")
            .body(Body::from(sparql))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
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

    #[tokio::test]
    async fn test_metrics_endpoint_returns_200_with_expected_metric_names() {
        let store = make_store();
        let router = Server::new(store).into_router();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; version=0.0.4"
        );
        let text = body_text(resp).await;
        // Always-present metrics (store size, query counters, LFTJ counters).
        for name in [
            "nova_store_triples",
            "nova_queries_total",
            "nova_query_errors_total",
            "nova_query_timeouts_total",
            "nova_query_result_limit_exceeded_total",
            "nova_queries_rejected_total",
            "nova_lftj_queries_total",
            "nova_lftj_fallback_total",
        ] {
            assert!(
                text.contains(&format!("# TYPE {name} ")),
                "missing metric {name} in:\n{text}"
            );
        }
        // `MemoryStore` doesn't track delta/compaction stats, and no
        // `--max-parallel-queries` was configured, so these should be
        // omitted rather than reported as misleading zeros.
        for name in [
            "nova_store_delta_entries",
            "nova_compactions_total",
            "nova_compaction_duration_seconds_total",
            "nova_queries_in_flight",
        ] {
            assert!(
                !text.contains(name),
                "unexpected metric {name} present for a backend/config that doesn't support it:\n{text}"
            );
        }
    }

    /// Extract the value of a single-line Prometheus metric (`name value`)
    /// from exposition-format text, ignoring `# HELP`/`# TYPE` lines.
    fn metric_value(text: &str, name: &str) -> u64 {
        text.lines()
            .find(|l| l.starts_with(&format!("{name} ")))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("metric {name} not found in:\n{text}"))
    }

    #[tokio::test]
    async fn test_metrics_query_counters_increment_after_queries() {
        // `nova_queries_total` is a process-wide counter shared across every
        // test in this binary (they run concurrently in the same process),
        // so this asserts a *delta* across 3 queries rather than an absolute
        // value.
        let store = make_store();
        let router = Server::new(store).into_router();

        let scrape = |router: Router| async {
            let req = Request::builder()
                .method(Method::GET)
                .uri("/metrics")
                .body(Body::empty())
                .unwrap();
            let resp = router.oneshot(req).await.unwrap();
            body_text(resp).await
        };
        let before = metric_value(&scrape(router.clone()).await, "nova_queries_total");

        let sparql = "SELECT ?s WHERE { ?s <http://ex/name> ?n }";
        for _ in 0..3 {
            let req = Request::builder()
                .method(Method::POST)
                .uri("/sparql")
                .header("content-type", "application/sparql-query")
                .body(Body::from(sparql))
                .unwrap();
            let resp = router.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        // `>=` rather than `==`: `nova_queries_total` is a single
        // process-wide counter shared by every test in this binary, which
        // `cargo test` runs concurrently on multiple threads, so other
        // tests' queries may also land between the two scrapes. The
        // counter is monotonically increasing, so this test's 3 queries are
        // guaranteed to be reflected in the delta regardless of what else
        // is running.
        let after = metric_value(&scrape(router).await, "nova_queries_total");
        assert!(
            after - before >= 3,
            "expected nova_queries_total to increase by at least 3, went from {before} to {after}"
        );
    }

    #[tokio::test]
    async fn test_metrics_reports_in_flight_gauge_when_max_parallel_queries_configured() {
        let store = make_store();
        let router = Server::new(store)
            .with_max_parallel_queries(4)
            .into_router();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let text = body_text(resp).await;
        // No queries in flight at the time of the scrape itself.
        assert!(
            text.contains("nova_queries_in_flight 0"),
            "expected nova_queries_in_flight 0 in:\n{text}"
        );
    }

    #[tokio::test]
    async fn test_metrics_reports_ring_store_delta_and_compaction_stats() {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("nova_server_metrics_test_{pid}"));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(oxigraph_nova_engine_ring::LoudsStore::open(&dir).unwrap());
        let router = Server::new(store).into_router();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let text = body_text(resp).await;
        for name in [
            "nova_store_delta_entries",
            "nova_compactions_total",
            "nova_compaction_duration_seconds_total",
        ] {
            assert!(
                text.contains(&format!("# TYPE {name} ")),
                "missing metric {name} for LoudsStore in:\n{text}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── POST /validate ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_validate_conforming_returns_conforms_true() {
        let router = make_router();
        // Every ex:name value in make_store() is a plain string literal, so
        // a shape requiring datatype xsd:string on ex:name should conform.
        // `sh:targetSubjectsOf` is not yet supported by `NativeValidator`
        // (see nova-shacl's `shape.rs` module doc comment), so target
        // alice/bob explicitly via `sh:targetNode`.
        let shapes = r#"
            @prefix sh: <http://www.w3.org/ns/shacl#> .
            @prefix ex: <http://ex/> .
            @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

            ex:NameShape a sh:NodeShape ;
                sh:targetNode ex:alice, ex:bob ;
                sh:property [
                    sh:path ex:name ;
                    sh:datatype xsd:string ;
                ] .
        "#;

        let req = Request::builder()
            .method(Method::POST)
            .uri("/validate")
            .header("content-type", "text/turtle")
            .body(Body::from(shapes))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(
            json["conforms"], true,
            "expected conforming validation, got: {json}"
        );
        assert_eq!(json["violation_count"], 0);
    }

    #[tokio::test]
    async fn test_validate_violating_returns_conforms_false() {
        let router = make_router();
        // make_store()'s data has no ex:email triples at all, so a shape
        // requiring minCount 1 on ex:email for alice/bob must report
        // violations for both. `sh:targetSubjectsOf` is not yet supported
        // by `NativeValidator` (see nova-shacl's `shape.rs` module doc
        // comment), so target alice/bob explicitly via `sh:targetNode`.
        let shapes = r#"
            @prefix sh: <http://www.w3.org/ns/shacl#> .
            @prefix ex: <http://ex/> .

            ex:EmailShape a sh:NodeShape ;
                sh:targetNode ex:alice, ex:bob ;
                sh:property [
                    sh:path ex:email ;
                    sh:minCount 1 ;
                ] .
        "#;

        let req = Request::builder()
            .method(Method::POST)
            .uri("/validate")
            .header("content-type", "text/turtle")
            .body(Body::from(shapes))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(
            json["conforms"], false,
            "expected non-conforming validation, got: {json}"
        );
        let violation_count = json["violation_count"].as_u64().unwrap();
        assert!(
            violation_count >= 2,
            "expected at least 2 violations (alice + bob missing ex:email), got: {json}"
        );
        let results = json["results"].as_array().unwrap();
        assert!(!results.is_empty(), "expected non-empty results: {json}");
        assert!(
            results[0]["source_constraint_component"]
                .as_str()
                .unwrap()
                .contains("MinCountConstraintComponent"),
            "expected a MinCountConstraintComponent violation, got: {json}"
        );
    }

    #[tokio::test]
    async fn test_validate_malformed_shapes_body_returns_400() {
        let router = make_router();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/validate")
            .header("content-type", "text/turtle")
            .body(Body::from("THIS IS NOT TURTLE {{{"))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
