//! `ServiceHandler` — the extension point for SPARQL 1.1 Federated Query
//! (`SERVICE`) — <https://www.w3.org/TR/sparql11-federated-query/#defn_evalService>.
//!
//! By default the evaluator has no way to actually reach out over the
//! network for a `SERVICE <endpoint> { ... }` clause — that would require
//! pulling in an HTTP client and a SPARQL-results parser, neither of which
//! belong in `nova-query`'s dependency graph. Instead, `nova-query` defines
//! this trait as a seam: any downstream crate/application that wants
//! `SERVICE` support implements [`ServiceHandler`] (typically backed by
//! `reqwest` + `sparesults`, or by dispatching to another local `Evaluator`
//! for "federated" queries that are actually just other in-process
//! datasets) and registers it via
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

use crate::solution::Solutions;
use oxrdf::NamedNode;
use spargebra::algebra::GraphPattern;

/// Handles one `SERVICE <name> { pattern }` clause, returning the solutions
/// the remote (or otherwise external) endpoint produced.
///
/// Implementations are free to:
/// - Issue a real HTTP SPARQL 1.1 Protocol request to `service_name` and
///   parse the SPARQL-results response.
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
