//! openCypher frontend for Oxigraph Nova.
//!
//! Provides a lexer, recursive-descent parser, and a lowering pass that
//! translates the supported Cypher subset directly into either a
//! [`spargebra::Query`] (read-only `MATCH`/`RETURN` statements) or a
//! [`spargebra::Update`] (write statements: `CREATE`/`SET`/`DELETE`/
//! `REMOVE`) — the same representations Nova's SPARQL parser and SPARQL
//! Update parser produce. This means any caller that already knows how to
//! evaluate a `spargebra::Query` (e.g. `oxigraph_nova_query::Evaluator`) or
//! execute a `spargebra::Update` (e.g.
//! `oxigraph_nova_query::update::execute_update`) can run a Cypher
//! query/statement with no further integration work: just call
//! [`parse_and_lower`] or [`parse_and_lower_update`] and hand the result to
//! the evaluator/executor.
//!
//! # Supported subset
//!
//! ## Reads
//!
//! `MATCH` (node + relationship patterns, including unbounded
//! variable-length relationships `-[:TYPE*]->`/`-[:TYPE*1..]->`) / `WHERE`
//! / `RETURN` (with `AS` aliases and `DISTINCT`) / `ORDER BY` / `SKIP` /
//! `LIMIT`. See [`lower`] for the full RDF↔property-graph mapping this
//! crate uses, and [`parser`] for the exact grammar accepted.
//!
//! ## Writes
//!
//! An optional `MATCH [WHERE ...]` followed by one or more write clauses:
//! `CREATE` (nodes/relationships), `SET n.prop = expr` / `SET n:Label`,
//! `DELETE` / `DETACH DELETE`, and `REMOVE n.prop` / `REMOVE n:Label`. Each
//! write statement lowers to a `spargebra::Update` made of one or more
//! `GraphUpdateOperation::DeleteInsert` operations — see [`lower`]'s module
//! documentation for the exact lowering strategy per clause.
//!
//! Not supported (rejected with a clear error, never a panic): `MERGE`,
//! `WITH`, `OPTIONAL MATCH`, `UNION`, multiple `MATCH` clauses, bounded
//! variable-length relationships (`*min..max` with an explicit max),
//! chained property access (`a.b.c`), property access on a relationship
//! variable in expressions (e.g. `WHERE r.since > 2000`), and referencing a
//! variable introduced by one write clause (e.g. a `CREATE`) from a later
//! write clause in the same statement.
//!
//! Relationship *properties* on a pattern (`-[r:KNOWS {since: 2020}]->`) are
//! partially supported: `MATCH` lowers them to RDF 1.2 quoted-triple
//! annotations (`<< ?from :TYPE ?to >> :since 2020`); `CREATE` rejects them
//! because inserting a quad whose subject is a quoted triple cannot be
//! expressed with the current `oxrdf::Quad` API — see [`lower`]'s module docs.
//!
//! # Example
//!
//! ```
//! use oxigraph_nova_cypher::parse_and_lower;
//!
//! let query = parse_and_lower("MATCH (n:Person) WHERE n.age > 30 RETURN n.name AS name")
//!     .expect("valid Cypher Phase 1 query");
//! // `query` is a `spargebra::Query::Select` ready for
//! // `oxigraph_nova_query::Evaluator::evaluate`.
//! ```
//!
//! ```
//! use oxigraph_nova_cypher::parse_and_lower_update;
//!
//! let update = parse_and_lower_update("CREATE (n:Person {name: \"Alice\"})")
//!     .expect("valid Cypher Phase 2 write statement");
//! // `update` is a `spargebra::Update` ready for
//! // `oxigraph_nova_query::update::execute_update`.
//! ```

mod ast;
mod lexer;
mod lower;
mod parser;

pub use ast::{CypherQuery, CypherStatement};
pub use lower::{LABEL_NS, PROP_NS, REL_NS};

/// Parses and lowers a Cypher Phase 1 query string directly into a
/// `spargebra::Query`, ready to be evaluated by
/// `oxigraph_nova_query::Evaluator::evaluate`.
///
/// Returns a plain error message (not a structured error type) describing
/// the first syntax or semantic problem encountered — this crate never
/// panics on malformed input.
pub fn parse_and_lower(cypher: &str) -> anyhow::Result<spargebra::Query> {
    let ast = parser::parse(cypher).map_err(anyhow::Error::msg)?;
    lower::lower(&ast).map_err(anyhow::Error::msg)
}

/// Parses a Cypher Phase 1 query string into its AST without lowering it.
/// Exposed mainly for testing/tooling; most callers should use
/// [`parse_and_lower`] instead.
pub fn parse(cypher: &str) -> anyhow::Result<CypherQuery> {
    parser::parse(cypher).map_err(anyhow::Error::msg)
}

/// Parses and lowers a Cypher Phase 2 write statement directly into a
/// `spargebra::Update`, ready to be executed by
/// `oxigraph_nova_query::update::execute_update`.
///
/// Returns a plain error message (not a structured error type) describing
/// the first syntax or semantic problem encountered — this crate never
/// panics on malformed input.
pub fn parse_and_lower_update(cypher: &str) -> anyhow::Result<spargebra::Update> {
    let ast = parser::parse_statement(cypher).map_err(anyhow::Error::msg)?;
    lower::lower_statement(&ast).map_err(anyhow::Error::msg)
}

/// Parses a Cypher Phase 2 write statement into its AST without lowering
/// it. Exposed mainly for testing/tooling; most callers should use
/// [`parse_and_lower_update`] instead.
pub fn parse_statement(cypher: &str) -> anyhow::Result<CypherStatement> {
    parser::parse_statement(cypher).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_lower_smoke_test() {
        let query = parse_and_lower("MATCH (n:Person) WHERE n.age > 30 RETURN n.name AS name")
            .expect("should parse and lower");
        assert!(matches!(query, spargebra::Query::Select { .. }));
    }

    #[test]
    fn parse_and_lower_reports_syntax_errors() {
        assert!(parse_and_lower("CREATE (n) RETURN n").is_err());
    }

    #[test]
    fn parse_and_lower_update_smoke_test() {
        let update = parse_and_lower_update("CREATE (n:Person {name: \"Alice\"})")
            .expect("should parse and lower a write statement");
        assert_eq!(update.operations.len(), 1);
    }

    #[test]
    fn parse_and_lower_update_reports_syntax_errors() {
        assert!(parse_and_lower_update("MATCH (n) RETURN n").is_err());
    }
}
