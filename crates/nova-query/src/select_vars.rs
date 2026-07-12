//! Extracts the ordered SELECT projection variable list from a parsed query.
//!
//! Shared by every caller that needs to pair a `QueryResult::Solutions` with
//! its column headers without re-deriving them from each row (Nova's
//! `Solution` supports lookup by `&Variable` but not "list every variable in
//! this query's SELECT clause") — currently `nova-server`'s HTTP result
//! serialization and `nova-python`'s `PyStore::query`/`QuerySolutions`. Both
//! previously carried their own copy of this logic; centralizing it here
//! means there is exactly one place that has to track spargebra's `Query`/
//! `GraphPattern` shape.

use spargebra::Query;
use spargebra::algebra::GraphPattern;
use spargebra::term::Variable;

/// Extract the ordered SELECT variable list from the outermost `Project` node.
///
/// spargebra resolves `SELECT *` to an explicit `Project` with all WHERE-clause
/// variables during parsing, so this is always populated for valid SELECT queries.
/// Returns an empty list for ASK/CONSTRUCT/DESCRIBE queries, which have no
/// projection.
pub fn projected_variables(query: &Query) -> Vec<Variable> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use spargebra::SparqlParser;

    #[test]
    fn select_star_projects_all_vars() {
        let query = SparqlParser::new()
            .parse_query("SELECT * WHERE { ?s ?p ?o }")
            .unwrap();
        let mut vars: Vec<String> = projected_variables(&query)
            .iter()
            .map(|v| v.as_str().to_string())
            .collect();
        vars.sort();
        assert_eq!(vars, vec!["o", "p", "s"]);
    }

    #[test]
    fn select_explicit_list_preserves_order() {
        let query = SparqlParser::new()
            .parse_query("SELECT ?o ?s WHERE { ?s ?p ?o }")
            .unwrap();
        let projected = projected_variables(&query);
        let vars: Vec<&str> = projected.iter().map(|v| v.as_str()).collect();
        assert_eq!(vars, vec!["o", "s"]);
    }


    #[test]
    fn ask_query_has_no_projection() {
        let query = SparqlParser::new().parse_query("ASK { ?s ?p ?o }").unwrap();
        assert!(projected_variables(&query).is_empty());
    }

    #[test]
    fn select_with_order_by_and_limit_still_projects() {
        let query = SparqlParser::new()
            .parse_query("SELECT ?s WHERE { ?s ?p ?o } ORDER BY ?s LIMIT 10")
            .unwrap();
        let projected = projected_variables(&query);
        let vars: Vec<&str> = projected.iter().map(|v| v.as_str()).collect();
        assert_eq!(vars, vec!["s"]);
    }

}
