//! MCP (Model Context Protocol) server exposing Oxigraph Nova's SPARQL
//! query/update and data-model-discovery capabilities to LLM agents.
//!
//! This is the "database" toolset that reserves but does not
//! implement, since it needs a running store's state. [`NovaMcpService`]
//! wraps a shared `Arc<RingStore>` — the same store handle a `nova-server`
//! HTTP process or the `oxigraph` CLI would use — and exposes four tools
//! over the official Rust MCP SDK (`rmcp`):
//!
//! - `sparql_query`: run a SPARQL query, returning SELECT/ASK results as
//!   SPARQL-results-JSON, or CONSTRUCT/DESCRIBE results as Turtle.
//! - `sparql_update`: run a SPARQL update, returning a triple-count
//!   before/after summary.
//! - `describe_data_model`: named graphs, distinct predicates, `rdf:type`
//!   classes, and a triple count — lets an agent orient itself before
//!   writing a query blind (Fluree's `get_data_model` analog).
//! - `list_graphs`: a cheap subset of `describe_data_model` (named graphs +
//!   triple count only, no full-store scan) for quick orientation.
//!
//! ## Transport
//!
//! `oxigraph_nova_cli`'s `mcp serve` subcommand, which constructs a
//! [`NovaMcpService`] and serves it via `rmcp::transport::stdio()`. An HTTP
//! `/mcp` transport is a possible future, deferred until a need arises.
//!
//! ## Concurrency
//!
//! The stdio server opens its own `RingStore` handle in its own OS
//! process. **Do not** point it at a `--location` directory that is
//! concurrently open in a `nova_serve`/`oxigraph serve` process —
//! `RingStore` is a single-writer-WAL design (see
//! `oxigraph_nova_storage_ring::store`'s module doc, "Isolation
//! semantics").

use anyhow::Result as AnyResult;
use oxigraph_nova_core::{GraphName, QuadStore, Term, TextSearch};
use oxigraph_nova_query::{Evaluator, QueryOptions, QueryResult, StoreDataset, execute_update};
use oxigraph_nova_reasoning::ReasoningState;
use oxigraph_nova_storage_ring::RingStore;
use oxrdfio::{RdfFormat, RdfSerializer};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use sparesults::{QueryResultsFormat, QueryResultsSerializer};
use spargebra::SparqlParser;
use std::collections::HashSet;
use std::sync::Arc;

/// `rdf:type`'s full IRI, used by [`build_data_model_summary`] to collect
/// distinct classes without a `spargebra`/SPARQL round-trip.
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

// ── Tool request parameter types ────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SparqlQueryRequest {
    /// The SPARQL query text (SELECT / ASK / CONSTRUCT / DESCRIBE).
    pub query: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SparqlUpdateRequest {
    /// The SPARQL 1.1 Update text.
    pub update: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CypherQueryRequest {
    /// The openCypher read query text (`MATCH`/`WHERE`/`RETURN`/`ORDER BY`/
    /// `SKIP`/`LIMIT`) — see `oxigraph_nova_cypher`'s crate doc comment for
    /// the supported subset.
    pub query: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CypherUpdateRequest {
    /// The openCypher write clause(s) (`CREATE`/`SET`/`DELETE`/`DETACH
    /// DELETE`/`REMOVE`) — see `oxigraph_nova_cypher`'s crate doc comment
    /// for the supported subset.
    pub update: String,
}

// ── Service struct ───────────────────────────────────────────────────────────

/// The MCP service: wraps a shared store handle and exposes SPARQL query/
/// update/data-model tools over it. See the module doc comment for the
/// full tool list and the concurrency caveat.
#[derive(Clone)]
pub struct NovaMcpService {
    store: Arc<RingStore>,
    reasoning: Option<Arc<ReasoningState<RingStore>>>,
    text_search: Option<Arc<dyn TextSearch>>,
    max_results: Option<usize>,
    // Read by the `#[tool_handler]`-generated `ServerHandler::list_tools`/
    // `call_tool` methods below (and directly by the router-listing test);
    // `dead_code` can't see through the macro expansion in a lib-only crate.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl NovaMcpService {
    pub fn new(store: Arc<RingStore>) -> Self {
        Self {
            store,
            reasoning: None,
            text_search: None,
            max_results: None,
            tool_router: Self::tool_router(),
        }
    }

    /// Attach an OWL 2 RL reasoning overlay: `sparql_query` will then
    /// evaluate against `reasoning.current(&store)` instead of the raw
    /// store, mirroring `nova-server`'s own `--reasoning` wiring.
    pub fn with_reasoning(mut self, reasoning: Arc<ReasoningState<RingStore>>) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// Attach a full-text search backend, enabling `text:query`/
    /// `text:contains` extension-function dispatch in `sparql_query`.
    pub fn with_text_search(mut self, text_search: Arc<dyn TextSearch>) -> Self {
        self.text_search = Some(text_search);
        self
    }

    /// Cap the number of result rows/triples a single `sparql_query` call
    /// may produce, mirroring `nova-server`'s `--max-results`.
    pub fn with_max_results(mut self, max_results: usize) -> Self {
        self.max_results = Some(max_results);
        self
    }
}

// ── Result serialization helpers ────────────────────────────────────────────

/// Serialize a [`QueryResult`] the same way `nova-server`'s `/sparql`
/// endpoint would, but hardcoded to a single fixed format per result kind
/// (not user-negotiable, unlike the HTTP endpoint or the CLI's `query`
/// subcommand): SELECT/ASK → SPARQL-results-JSON, CONSTRUCT/DESCRIBE →
/// Turtle.
fn serialize_query_result(query: &spargebra::Query, result: QueryResult) -> AnyResult<String> {
    match result {
        QueryResult::Boolean(b) => {
            let mut out = Vec::new();
            QueryResultsSerializer::from_format(QueryResultsFormat::Json)
                .serialize_boolean_to_writer(&mut out, b)?;
            Ok(String::from_utf8(out)?)
        }
        QueryResult::Solutions { stream, .. } => {
            let variables = oxigraph_nova_query::projected_variables(query);
            let mut out = Vec::new();
            let mut ser = QueryResultsSerializer::from_format(QueryResultsFormat::Json)
                .serialize_solutions_to_writer(&mut out, variables.clone())?;
            for sol in stream {
                let sol = sol?;
                ser.serialize(
                    variables
                        .iter()
                        .filter_map(|v| sol.get(v).map(|t| (v.as_ref(), t.as_ref()))),
                )?;
            }
            ser.finish()?;
            Ok(String::from_utf8(out)?)
        }
        QueryResult::Triples(stream) => {
            let mut out = Vec::new();
            let mut writer = RdfSerializer::from_format(RdfFormat::Turtle).for_writer(&mut out);
            for t in stream {
                let t = t?;
                writer.serialize_triple(&t)?;
            }
            writer.finish()?;
            Ok(String::from_utf8(out)?)
        }
    }
}

/// Full data-model summary: distinct named graphs, predicates, `rdf:type`
/// classes, and a total triple count — computed via a single scan pass
/// over the whole store (one `HashSet` each for graphs/predicates/classes)
/// rather than three separate scans.
fn build_data_model_summary(store: &Arc<RingStore>) -> AnyResult<String> {
    let mut graphs: HashSet<String> = HashSet::new();
    let mut predicates: HashSet<String> = HashSet::new();
    let mut classes: HashSet<String> = HashSet::new();

    for g in store
        .known_named_graphs()
        .map_err(|e| anyhow::anyhow!("{e}"))?
    {
        let g = g.map_err(|e| anyhow::anyhow!("{e}"))?;
        if let GraphName::NamedNode(n) = &g {
            graphs.insert(n.as_str().to_string());
        }
    }

    let total = store.len().map_err(|e| anyhow::anyhow!("{e}"))?;

    for sq in store
        .quads_for_pattern(None, None, None, None)
        .map_err(|e| anyhow::anyhow!("{e}"))?
    {
        let sq = sq.map_err(|e| anyhow::anyhow!("{e}"))?;
        predicates.insert(sq.predicate.as_str().to_string());
        if sq.predicate.as_str() == RDF_TYPE
            && let Term::NamedNode(c) = sq.object.as_ref()
        {
            classes.insert(c.as_str().to_string());
        }
        if let GraphName::NamedNode(n) = &sq.graph_name {
            graphs.insert(n.as_str().to_string());
        }
    }

    let mut graphs: Vec<String> = graphs.into_iter().collect();
    let mut predicates: Vec<String> = predicates.into_iter().collect();
    let mut classes: Vec<String> = classes.into_iter().collect();
    graphs.sort();
    predicates.sort();
    classes.sort();

    let summary = serde_json::json!({
        "triple_count": total,
        "named_graphs": graphs,
        "predicates": predicates,
        "classes": classes,
    });
    Ok(serde_json::to_string_pretty(&summary)?)
}

/// Cheap subset of [`build_data_model_summary`]: named graphs + triple
/// count only, no full-store scan.
fn build_graph_list(store: &Arc<RingStore>) -> AnyResult<String> {
    let mut graphs: Vec<String> = store
        .known_named_graphs()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .filter_map(|r| r.ok())
        .filter_map(|g| match g {
            GraphName::NamedNode(n) => Some(n.as_str().to_string()),
            _ => None,
        })
        .collect();
    graphs.sort();
    let total = store.len().map_err(|e| anyhow::anyhow!("{e}"))?;

    let summary = serde_json::json!({
        "triple_count": total,
        "named_graphs": graphs,
    });
    Ok(serde_json::to_string_pretty(&summary)?)
}

// ── Tools ────────────────────────────────────────────────────────────────────

#[tool_router]
impl NovaMcpService {
    #[tool(
        description = "Run a SPARQL query against the store. SELECT/ASK results are returned \
                        as SPARQL-results-JSON; CONSTRUCT/DESCRIBE results are returned as \
                        Turtle. Call describe_data_model first if you don't already know the \
                        store's graphs/predicates/classes."
    )]
    async fn sparql_query(
        &self,
        Parameters(SparqlQueryRequest { query }): Parameters<SparqlQueryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let parsed = match SparqlParser::new().parse_query(&query) {
            Ok(q) => q,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "SPARQL parse error: {e}"
                ))]));
            }
        };

        let store = Arc::clone(&self.store);
        let reasoning = self.reasoning.clone();
        let text_search = self.text_search.clone();
        let max_results = self.max_results;

        let outcome = tokio::task::spawn_blocking(move || -> AnyResult<String> {
            let mut options = QueryOptions::default();
            if let Some(ts) = text_search {
                options = options.with_text_search(ts);
            }
            if let Some(mr) = max_results {
                options = options.with_max_results(mr);
            }

            if let Some(rs) = reasoning {
                let overlay = rs.current(&store)?;
                let evaluator = Evaluator::with_options(overlay.as_ref(), options);
                let result = evaluator.evaluate(&parsed)?;
                serialize_query_result(&parsed, result)
            } else {
                let dataset = StoreDataset::new(Arc::clone(&store));
                let evaluator = Evaluator::with_options(&dataset, options);
                let result = evaluator.evaluate(&parsed)?;
                serialize_query_result(&parsed, result)
            }
        })
        .await;

        match outcome {
            Ok(Ok(text)) => Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
            Ok(Err(e)) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "query evaluation error: {e}"
            ))])),
            Err(join_err) => Err(McpError::internal_error(join_err.to_string(), None)),
        }
    }

    #[tool(
        description = "Run a SPARQL 1.1 Update against the store (INSERT/DELETE/LOAD/CLEAR/\
                        etc.). Returns a triple-count-before/after summary on success."
    )]
    async fn sparql_update(
        &self,
        Parameters(SparqlUpdateRequest { update }): Parameters<SparqlUpdateRequest>,
    ) -> Result<CallToolResult, McpError> {
        let parsed = match SparqlParser::new().parse_update(&update) {
            Ok(u) => u,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "SPARQL parse error: {e}"
                ))]));
            }
        };

        let store = Arc::clone(&self.store);
        let outcome = tokio::task::spawn_blocking(move || -> AnyResult<(usize, usize)> {
            let before = store.len().map_err(|e| anyhow::anyhow!("{e}"))?;
            execute_update(&store, &parsed)?;
            let after = store.len().map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((before, after))
        })
        .await;

        match outcome {
            Ok(Ok((before, after))) => {
                let delta = after as i64 - before as i64;
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Update applied successfully. Triple count: {before} -> {after} ({delta:+})"
                ))]))
            }
            Ok(Err(e)) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "update error: {e}"
            ))])),
            Err(join_err) => Err(McpError::internal_error(join_err.to_string(), None)),
        }
    }

    #[tool(
        description = "Run an experimental openCypher read query against the store (MATCH/\
                        WHERE/RETURN/ORDER BY/SKIP/LIMIT). Translated internally to SPARQL \
                        algebra and evaluated the same way as sparql_query; results are \
                        returned as SPARQL-results-JSON (or Turtle for a CONSTRUCT-shaped \
                        translation). Call describe_data_model first if you don't already \
                        know the store's graphs/predicates/classes."
    )]
    async fn cypher_query(
        &self,
        Parameters(CypherQueryRequest { query }): Parameters<CypherQueryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let parsed = match oxigraph_nova_cypher::parse_and_lower(&query) {
            Ok(q) => q,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Cypher parse error: {e}"
                ))]));
            }
        };

        let store = Arc::clone(&self.store);
        let reasoning = self.reasoning.clone();
        let text_search = self.text_search.clone();
        let max_results = self.max_results;

        let outcome = tokio::task::spawn_blocking(move || -> AnyResult<String> {
            let mut options = QueryOptions::default();
            if let Some(ts) = text_search {
                options = options.with_text_search(ts);
            }
            if let Some(mr) = max_results {
                options = options.with_max_results(mr);
            }

            if let Some(rs) = reasoning {
                let overlay = rs.current(&store)?;
                let evaluator = Evaluator::with_options(overlay.as_ref(), options);
                let result = evaluator.evaluate(&parsed)?;
                serialize_query_result(&parsed, result)
            } else {
                let dataset = StoreDataset::new(Arc::clone(&store));
                let evaluator = Evaluator::with_options(&dataset, options);
                let result = evaluator.evaluate(&parsed)?;
                serialize_query_result(&parsed, result)
            }
        })
        .await;

        match outcome {
            Ok(Ok(text)) => Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
            Ok(Err(e)) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "query evaluation error: {e}"
            ))])),
            Err(join_err) => Err(McpError::internal_error(join_err.to_string(), None)),
        }
    }

    #[tool(
        description = "Run an experimental openCypher write query against the store (CREATE/\
                        SET/DELETE/DETACH DELETE/REMOVE). Translated internally to a SPARQL \
                        Update and applied the same way as sparql_update. Returns a \
                        triple-count-before/after summary on success."
    )]
    async fn cypher_update(
        &self,
        Parameters(CypherUpdateRequest { update }): Parameters<CypherUpdateRequest>,
    ) -> Result<CallToolResult, McpError> {
        let parsed = match oxigraph_nova_cypher::parse_and_lower_update(&update) {
            Ok(u) => u,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Cypher parse error: {e}"
                ))]));
            }
        };

        let store = Arc::clone(&self.store);
        let outcome = tokio::task::spawn_blocking(move || -> AnyResult<(usize, usize)> {
            let before = store.len().map_err(|e| anyhow::anyhow!("{e}"))?;
            execute_update(&store, &parsed)?;
            let after = store.len().map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((before, after))
        })
        .await;

        match outcome {
            Ok(Ok((before, after))) => {
                let delta = after as i64 - before as i64;
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Update applied successfully. Triple count: {before} -> {after} ({delta:+})"
                ))]))
            }
            Ok(Err(e)) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "update error: {e}"
            ))])),
            Err(join_err) => Err(McpError::internal_error(join_err.to_string(), None)),
        }
    }

    #[tool(
        description = "Describe the store's data model: named graphs, distinct predicates, \
                        rdf:type classes, and a triple count. Call this first to orient \
                        yourself before writing a query blind."
    )]
    async fn describe_data_model(&self) -> Result<CallToolResult, McpError> {
        let store = Arc::clone(&self.store);
        let outcome = tokio::task::spawn_blocking(move || build_data_model_summary(&store)).await;
        match outcome {
            Ok(Ok(text)) => Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
            Ok(Err(e)) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "describe_data_model error: {e}"
            ))])),
            Err(join_err) => Err(McpError::internal_error(join_err.to_string(), None)),
        }
    }

    #[tool(
        description = "List the store's named graphs and total triple count — a cheap subset \
                        of describe_data_model for quick orientation (no full-store scan)."
    )]
    async fn list_graphs(&self) -> Result<CallToolResult, McpError> {
        let store = Arc::clone(&self.store);
        let outcome = tokio::task::spawn_blocking(move || build_graph_list(&store)).await;
        match outcome {
            Ok(Ok(text)) => Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
            Ok(Err(e)) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "list_graphs error: {e}"
            ))])),
            Err(join_err) => Err(McpError::internal_error(join_err.to_string(), None)),
        }
    }
}

// ── ServerHandler ────────────────────────────────────────────────────────────

#[tool_handler]
impl ServerHandler for NovaMcpService {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` (= `rmcp::model::InitializeResult`) is `#[non_exhaustive]`,
        // so it can't be built with a struct literal from this crate. Its
        // fields are `pub`, though, so `InitializeResult::new(caps)` (which
        // already defaults `protocol_version`/`server_info` sensibly) plus a
        // direct field assignment for `instructions` is the correct idiom.
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.instructions = Some(
            "Oxigraph Nova MCP server: query/update a running RDF/SPARQL store, and \
             discover its data model. Call `describe_data_model` (or the cheaper \
             `list_graphs`) first to orient yourself before writing a query blind."
                .to_string(),
        );
        info
    }
}

// ── stdio transport entry point ─────────────────────────────────────────────

/// Serve `service` over stdio until the client disconnects (or the process
/// receives a shutdown signal). This is the async fn `oxigraph_nova_cli`'s `mcp
/// serve` subcommand calls after constructing a [`NovaMcpService`].
///
/// **Important**: the stdio transport uses stdout for the JSON-RPC protocol
/// stream itself — callers must ensure nothing else writes to stdout (e.g.
/// route all diagnostic logging through `tracing`/stderr, never
/// `println!`).
pub async fn serve_stdio(service: NovaMcpService) -> AnyResult<()> {
    let running = service
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("failed to start MCP stdio server: {e}"))?;
    running
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP stdio server task panicked: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;
    use oxigraph_nova_core::{NamedNode, Quad};

    fn service() -> NovaMcpService {
        NovaMcpService::new(Arc::new(RingStore::new()))
    }

    #[test]
    fn tool_router_lists_exactly_the_expected_tools() {
        let svc = service();
        let mut names: Vec<String> = svc
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "cypher_query".to_string(),
                "cypher_update".to_string(),
                "describe_data_model".to_string(),
                "list_graphs".to_string(),
                "sparql_query".to_string(),
                "sparql_update".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn round_trip_insert_query_describe() {
        let store = Arc::new(RingStore::new());
        let svc = NovaMcpService::new(Arc::clone(&store));

        // Insert directly, bypassing the tool layer, to seed the store.
        let s = NamedNode::new("http://example.org/alice").unwrap();
        let p = NamedNode::new("http://example.org/knows").unwrap();
        let o = NamedNode::new("http://example.org/bob").unwrap();
        store
            .insert(&Quad::new(
                s.clone(),
                p.clone(),
                o.clone(),
                GraphName::DefaultGraph,
            ))
            .unwrap();
        store.compact().unwrap();

        let result = svc
            .sparql_query(Parameters(SparqlQueryRequest {
                query: "SELECT ?s ?p ?o WHERE { ?s ?p ?o }".to_string(),
            }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));

        let result = svc.describe_data_model().await.unwrap();
        assert_ne!(result.is_error, Some(true));

        let result = svc
            .sparql_update(Parameters(SparqlUpdateRequest {
                update: "INSERT DATA { <http://example.org/carol> \
                         <http://example.org/knows> <http://example.org/dave> }"
                    .to_string(),
            }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));

        assert_eq!(store.triple_count(), 2);
    }

    #[tokio::test]
    async fn cypher_round_trip_create_query() {
        let store = Arc::new(RingStore::new());
        let svc = NovaMcpService::new(Arc::clone(&store));

        let result = svc
            .cypher_update(Parameters(CypherUpdateRequest {
                update: "CREATE (n {name: \"Alice\"})".to_string(),
            }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));

        let result = svc
            .cypher_query(Parameters(CypherQueryRequest {
                query: "MATCH (n) RETURN n.name".to_string(),
            }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));

        let found_alice = result.content.iter().any(|block| {
            block
                .as_text()
                .map(|t| t.text.contains("Alice"))
                .unwrap_or(false)
        });
        assert!(found_alice, "expected result content to mention Alice");
    }
}
