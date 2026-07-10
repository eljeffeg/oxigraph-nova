//! `ServiceHandler` — the extension point for SPARQL 1.1 Federated Query
//! (`SERVICE`) — <https://www.w3.org/TR/sparql11-federated-query/#defn_evalService>.
//!
//! By default the evaluator has no way to actually reach out over the
//! network for a `SERVICE <endpoint> { ... }` clause — that would require
//! pulling in an HTTP client and a SPARQL-results parser, neither of which
//! belong in `nova-query`'s dependency graph by default. Instead,
//! `nova-query` defines this trait as a seam: any downstream crate/
//! application that wants `SERVICE` support implements [`ServiceHandler`]
//! (or uses the built-in [`HttpServiceHandler`], gated behind this crate's
//! opt-in `http-client` feature) and registers it via
//! [`crate::QueryOptions::with_service_handler`].
//!
//! This mirrors the existing [`crate::options::QueryOptions::text_search`]
//! pattern exactly: a single `Option<Arc<dyn Trait>>` field on
//! `QueryOptions`, defaulting to `None` (meaning "not supported"), with a
//! `with_*` builder method to opt in.
//!
//! ## Example
//!
//! ```ignore
//! use oxigraph_nova_query::{QueryOptions, ServiceHandler};
//! use std::sync::Arc;
//!
//! struct MyHandler;
//! impl ServiceHandler for MyHandler {
//!     fn handle(
//!         &self,
//!         service_name: &oxrdf::NamedNode,
//!         pattern: &spargebra::algebra::GraphPattern,
//!         base_iri: Option<&str>,
//!     ) -> anyhow::Result<oxigraph_nova_query::Solutions> {
//!         // ... issue an HTTP SPARQL request to `service_name`, using
//!         // `spargebra::algebra::GraphPattern::Display` (or a hand-rolled
//!         // SPARQL serializer) to turn `pattern` back into query text, then
//!         // parse the SPARQL-results response into `Solutions`.
//!         todo!()
//!     }
//! }
//!
//! let options = QueryOptions::new().with_service_handler(Arc::new(MyHandler));
//! ```
//!
//! ## Built-in HTTP handler
//!
//! With the `http-client` feature enabled, [`HttpServiceHandler`] provides a
//! ready-to-use, real HTTP-backed implementation: it re-serializes the
//! `SERVICE` clause's inner pattern as a synthetic `SELECT * WHERE { ... }`
//! query (via `spargebra`'s `Query::Display`), `POST`s it to the service
//! endpoint per the SPARQL 1.1 Protocol, and parses the SPARQL-results
//! response via `sparesults`.

use crate::solution::Solutions;
use oxrdf::NamedNode;
use spargebra::algebra::GraphPattern;

/// Handles one `SERVICE <name> { pattern }` clause, returning the solutions
/// the remote (or otherwise external) endpoint produced.
///
/// Implementations are free to:
/// - Issue a real HTTP SPARQL 1.1 Protocol request to `service_name` and
///   parse the SPARQL-results response (see [`HttpServiceHandler`] for a
///   ready-made implementation of exactly this, behind the `http-client`
///   feature).
/// - Dispatch to another in-process [`crate::Dataset`]/[`crate::Evaluator`]
///   (useful for testing, or for a multi-store setup where "services" are
///   just other local stores known by IRI).
/// - Return a fixed/mocked result set.
///
/// `pattern` is the inner `GraphPattern` of the `SERVICE` clause — the
/// implementation is responsible for turning it into whatever
/// request/query format its transport needs (e.g. re-serializing it to
/// SPARQL text via `GraphPattern`'s `Display` impl, wrapped in a
/// synthetic `SELECT * WHERE { ... }`).
///
/// Returning `Err` causes the whole query to fail *unless* the original
/// `SERVICE` clause was written with the `SILENT` keyword, in which case
/// the evaluator treats the error as "zero solutions" instead (per the
/// SPARQL 1.1 Federated Query spec's `SERVICE SILENT` semantics) —
/// implementations do not need to know or care about `SILENT` themselves.
pub trait ServiceHandler: Send + Sync {
    /// Evaluate `pattern` against the named service and return its
    /// solutions.
    ///
    /// `base_iri`, if present, is the query's base IRI (useful for
    /// resolving any relative IRIs that might appear if `pattern` is
    /// re-serialized to SPARQL text).
    fn handle(
        &self,
        service_name: &NamedNode,
        pattern: &GraphPattern,
        base_iri: Option<&str>,
    ) -> anyhow::Result<Solutions>;
}

// ── Built-in HTTP-backed handler ──────────────────────────────────────────

/// Real HTTP-backed [`ServiceHandler`], only compiled with this crate's
/// opt-in `http-client` feature.
///
/// GET/POST semantics per the [SPARQL 1.1 Protocol]: the `SERVICE` clause's
/// inner pattern is wrapped in a synthetic `SELECT * WHERE { pattern }`
/// query (constructed as a `spargebra::Query::Select` and rendered to text
/// via its `Display` impl — see [`spargebra::algebra`]'s
/// `SparqlGraphRootPattern`, which falls back to `SELECT *` when the
/// pattern carries no explicit `Project` wrapper, which is always the case
/// for a bare `SERVICE { ... }` inner pattern), `POST`ed as
/// `application/sparql-query` with an `Accept` header listing SPARQL-results
/// formats, and the response is parsed via `sparesults`.
///
/// [SPARQL 1.1 Protocol]: https://www.w3.org/TR/sparql11-protocol/
#[cfg(feature = "http-client")]
pub struct HttpServiceHandler {
    client: oxhttp::Client,
}

#[cfg(feature = "http-client")]
impl std::fmt::Debug for HttpServiceHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpServiceHandler").finish_non_exhaustive()
    }
}


#[cfg(feature = "http-client")]
impl Default for HttpServiceHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "http-client")]
impl HttpServiceHandler {
    /// Create a new handler. Constructing an `oxhttp::Client` is cheap (no
    /// connection pooling to warm up), so one is created once here rather
    /// than per-request.
    pub fn new() -> Self {
        Self {
            client: oxhttp::Client::new().with_redirection_limit(5),
        }
    }
}

#[cfg(feature = "http-client")]
impl ServiceHandler for HttpServiceHandler {
    fn handle(
        &self,
        service_name: &NamedNode,
        pattern: &GraphPattern,
        base_iri: Option<&str>,
    ) -> anyhow::Result<Solutions> {
        http_client::handle(&self.client, service_name, pattern, base_iri)
    }
}

#[cfg(feature = "http-client")]
mod http_client {
    use crate::solution::{Solution, Solutions};
    use anyhow::{Result, anyhow};
    use oxrdf::{NamedNode, Variable};
    use sparesults::{QueryResultsFormat, QueryResultsParser, SliceQueryResultsParserOutput};
    use spargebra::Query;
    use spargebra::algebra::GraphPattern;
    use std::sync::Arc;

    /// Media types accepted for the SPARQL-results response, in preference
    /// order — mirrors `do_load`'s `Accept`-header approach in `update.rs`,
    /// but for query *results* rather than RDF graph data.
    const ACCEPT: &str = "application/sparql-results+json, application/sparql-results+xml;q=0.9, \
                          text/tab-separated-values;q=0.8, text/csv;q=0.7";

    pub(super) fn handle(
        client: &oxhttp::Client,
        service_name: &NamedNode,
        pattern: &GraphPattern,
        base_iri: Option<&str>,
    ) -> Result<Solutions> {
        let base_iri = base_iri
            .map(|iri| {
                oxiri::Iri::parse(iri.to_owned()).map_err(|e| {
                    anyhow!(
                        "SERVICE <{}>: invalid base IRI: {e}",
                        service_name.as_str()
                    )
                })
            })
            .transpose()?;
        let synthetic = Query::Select {
            dataset: None,
            pattern: pattern.clone(),
            base_iri,
        };
        let query_text = synthetic.to_string();

        let request = oxhttp::model::Request::builder()
            .method(oxhttp::model::Method::POST)
            .uri(service_name.as_str())
            .header(oxhttp::model::header::ACCEPT, ACCEPT)
            .header(
                oxhttp::model::header::CONTENT_TYPE,
                "application/sparql-query",
            )
            .body(query_text)
            .map_err(|e| {
                anyhow!(
                    "SERVICE <{}>: invalid request: {e}",
                    service_name.as_str()
                )
            })?;
        let response = client.request(request).map_err(|e| {
            anyhow!(
                "SERVICE <{}>: request failed: {e}",
                service_name.as_str()
            )
        })?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "SERVICE <{}>: server returned HTTP {}",
                service_name.as_str(),
                response.status()
            ));
        }
        let content_type = response
            .headers()
            .get(oxhttp::model::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = response.into_body().to_vec().map_err(|e| {
            anyhow!(
                "SERVICE <{}>: error reading response body: {e}",
                service_name.as_str()
            )
        })?;

        let format = content_type
            .as_deref()
            .and_then(QueryResultsFormat::from_media_type)
            .unwrap_or(QueryResultsFormat::Json);

        let parsed = QueryResultsParser::from_format(format)
            .for_slice(&body)
            .map_err(|e| {
                anyhow!(
                    "SERVICE <{}>: SPARQL results parse error: {e}",
                    service_name.as_str()
                )
            })?;
        let solutions_parser = match parsed {
            SliceQueryResultsParserOutput::Solutions(s) => s,
            SliceQueryResultsParserOutput::Boolean(_) => {
                return Err(anyhow!(
                    "SERVICE <{}>: expected a SELECT-shaped (row-based) result, got a boolean \
                     (ASK-style) result",
                    service_name.as_str()
                ));
            }
        };
        let vars: Arc<[Variable]> = solutions_parser.variables().into();
        let mut solutions = Solutions::new();
        for row in solutions_parser {
            let row = row.map_err(|e| {
                anyhow!(
                    "SERVICE <{}>: SPARQL results parse error: {e}",
                    service_name.as_str()
                )
            })?;
            solutions.push(Solution::positional(
                Arc::clone(&vars),
                row.values().to_vec(),
            ));
        }
        Ok(solutions)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "http-client"))]
mod tests {
    use super::*;
    use oxhttp::Server;
    use oxhttp::model::header::CONTENT_TYPE;
    use oxhttp::model::{Body, Response, StatusCode};
    use oxrdf::Variable as OxVariable;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::thread::sleep;
    use std::time::Duration;

    fn iri(s: &str) -> NamedNode {
        NamedNode::new_unchecked(s)
    }

    fn parse_pattern(pattern: &str) -> GraphPattern {
        let query = spargebra::SparqlParser::new()
            .parse_query(&format!("SELECT * WHERE {{ {pattern} }}"))
            .unwrap();
        match query {
            spargebra::Query::Select { pattern, .. } => pattern,
            _ => unreachable!(),
        }
    }

    fn spawn_fixture(
        port: u16,
        content_type: &'static str,
        body: &'static str,
    ) -> oxhttp::ListeningServer {
        let server = Server::new(move |_request| {
            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, content_type)
                .body(Body::from(body))
                .unwrap()
        })
        .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .spawn()
        .unwrap();
        sleep(Duration::from_millis(100));
        server
    }

    #[test]
    fn service_fetches_and_parses_json_results() {
        let _fixture = spawn_fixture(
            18881,
            "application/sparql-results+json",
            r#"{"head":{"vars":["s","name"]},"results":{"bindings":[
                {"s":{"type":"uri","value":"http://ex/alice"},
                 "name":{"type":"literal","value":"Alice"}}
            ]}}"#,
        );
        let handler = HttpServiceHandler::new();
        let pattern = parse_pattern("?s ?p ?name");
        let solutions = handler
            .handle(&iri("http://127.0.0.1:18881/sparql"), &pattern, None)
            .unwrap();
        assert_eq!(solutions.len(), 1);
        let sol = &solutions[0];
        assert_eq!(
            sol.get(&OxVariable::new_unchecked("s")),
            Some(&oxrdf::Term::NamedNode(iri("http://ex/alice")))
        );
        assert_eq!(
            sol.get(&OxVariable::new_unchecked("name")),
            Some(&oxrdf::Term::Literal(oxrdf::Literal::new_simple_literal(
                "Alice"
            )))
        );
    }

    #[test]
    fn service_unreachable_errors() {
        let handler = HttpServiceHandler::new();
        let pattern = parse_pattern("?s ?p ?o");
        // Port 18883 has no fixture bound to it.
        assert!(
            handler
                .handle(&iri("http://127.0.0.1:18883/sparql"), &pattern, None)
                .is_err()
        );
    }

    #[test]
    fn service_falls_back_to_json_when_no_content_type() {
        let server = Server::new(move |_request| {
            Response::builder()
                .status(StatusCode::OK)
                .body(Body::from(
                    r#"{"head":{"vars":["s"]},"results":{"bindings":[
                        {"s":{"type":"uri","value":"http://ex/bob"}}
                    ]}}"#,
                ))
                .unwrap()
        })
        .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 18884)))
        .spawn()
        .unwrap();
        sleep(Duration::from_millis(100));
        let _fixture = server;

        let handler = HttpServiceHandler::new();
        let pattern = parse_pattern("?s ?p ?o");
        let solutions = handler
            .handle(&iri("http://127.0.0.1:18884/sparql"), &pattern, None)
            .unwrap();
        assert_eq!(solutions.len(), 1);
    }
}
