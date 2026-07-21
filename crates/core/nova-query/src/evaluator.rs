//! SPARQL evaluator.
//!
//! The evaluator receives a [`spargebra::Query`] algebra AST and evaluates it
//! against a [`Dataset`].  All storage access goes through `Dataset::find_quads` —
//! the evaluator never touches a `QuadStore` directly.
//!
//! # Parsing entry point (spargebra 0.4.x)
//!
//! `Query::parse()` is **deprecated** in spargebra 0.4.  Use instead:
//! ```ignore
//! use spargebra::SparqlParser;
//! let query = SparqlParser::new().parse_query(sparql_str)?;
//! ```
//!
//! # Implementation notes
//!
//! All `GraphPattern` arms are implemented, including property paths.
//! `Service` dispatches to a configurable [`crate::service::ServiceHandler`]
//! (see [`crate::options::QueryOptions::with_service_handler`]); without one
//! configured, a non-`SILENT` `SERVICE` clause errors and a `SILENT` one
//! evaluates to zero solutions.
//! Expression evaluation uses `Option<Term>` — `None` represents a SPARQL
//! type error (the filter/expression silently fails).

use crate::dataset::{Dataset, GraphSelector, PatternTerm, QuadPattern};
use crate::select_vars::projected_variables;
use crate::solution::{Solution, Solutions};
use anyhow::Result;
use oxiri::{Iri, IriRef};
use oxrdf::{BaseDirection, BlankNode, GraphName, Literal, NamedNode, Term, Variable};
use oxsdatatypes::{
    Date as XsdDate, DateTime as XsdDateTime, Decimal as XsdDec, Double as XsdDbl,
    Float as XsdFloat, Integer as XsdInt, Time as XsdTime,
};
use regex::Regex;
use spargebra::Query;
use spargebra::algebra::{
    AggregateExpression, AggregateFunction, Expression, Function, GraphPattern as GP,
    OrderExpression, PropertyPathExpression as PPE,
};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern, TriplePattern};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

/// Short label for a query's top-level kind, used as a `tracing` span field
/// in `Evaluator::evaluate` (`SELECT` / `ASK` / `CONSTRUCT` / `DESCRIBE`).
fn query_kind_label(query: &Query) -> &'static str {
    match query {
        Query::Select { .. } => "SELECT",
        Query::Ask { .. } => "ASK",
        Query::Construct { .. } => "CONSTRUCT",
        Query::Describe { .. } => "DESCRIBE",
    }
}

// ── XSD namespace helper ──────────────────────────────────────────────────────

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

fn xsd_nn(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{XSD}{local}"))
}

// ── Full-text search extension functions (`text:query`/`text:contains`) ─────────
//
// Non-standard SPARQL extension functions, parsed by `spargebra` as
// `Function::Custom(NamedNode)` — the same convention used by Jena Text/
// GraphDB/Stardog for their full-text extensions (a dedicated function-IRI
// namespace, e.g. `text:query("...")`). Both functions are treated
// identically for v1 (a boolean membership test against a bound object
// variable): `text:query(?var, "search string")` runs the underlying
// `TextSearch::search`'s implementation-defined query syntax (Tantivy's
// query-parser syntax for the reference implementation);
// `text:contains(?var, "term")` is currently just an alias. Distinguishing
// them (e.g. `contains` as a plain substring/phrase match vs. `query` as
// full query-syntax) is a later extension, not required for v1.
//
// Dispatched from two places:
// - `eval_fn`'s `Function::Custom` arm (`eval_text_scalar`): a per-solution-row
//   scalar fallback, used whenever the call can't be pushed down (e.g. the
//   object variable isn't otherwise bound by the surrounding BGP, or the
//   surrounding pattern isn't a plain BGP `Filter` sits directly over).
// - `eval_pattern`'s `GP::Filter` arm (`try_text_search_pushdown`): real
//   pushdown — when a `text:query`/`text:contains` FILTER wraps a BGP that
//   itself binds the same variable (as a triple pattern's object), the
//   search runs *first* to produce a candidate `TermId` set, seeding the
//   BGP evaluation so the object variable's domain is constrained before
//   the join runs, rather than materializing the full unconstrained join
//   and filtering afterward.
const TEXT_NS: &str = "http://oxigraph-nova.dev/fn/text#";

/// Default cap on the number of Tantivy hits fetched for the BGP-pushdown
/// path ([`Evaluator::try_text_search_pushdown`]). Not currently
/// configurable via [`crate::QueryOptions`] -- a query whose search string
/// matches more than this many literal objects will silently only consider
/// the top-scoring `DEFAULT_TEXT_SEARCH_LIMIT` of them, potentially omitting
/// some otherwise-valid solution rows. Acceptable for now because full-text
/// predicates are expected to be highly selective in practice; revisit if
/// this proves too small for real workloads (e.g. by threading a
/// `text_search_limit: Option<usize>` through `QueryOptions`).
const DEFAULT_TEXT_SEARCH_LIMIT: usize = 10_000;

/// Which of the two full-text extension functions a `text:` call site used
/// -- they share the same pushdown/scalar-fallback machinery but differ in
/// how the search-string argument is turned into a Tantivy query (see
/// [`Self::effective_query`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextFn {
    /// `text:query(?var, "...")`: the string is passed to Tantivy's
    /// `QueryParser` as-is, so callers can use its full query syntax
    /// (`AND`/`OR`/field queries/wildcards/etc.).
    Query,
    /// `text:contains(?var, "...")`: the string is treated as a literal
    /// substring/phrase to search for -- escaped and wrapped in double
    /// quotes so any `QueryParser` syntax characters in it (`"`, `\`, `AND`,
    /// `*`, ...) are matched literally rather than parsed as query syntax.
    Contains,
}

impl TextFn {
    /// Turn the raw search-string argument into the actual string handed to
    /// [`oxigraph_nova_core::TextSearch::search`], per this variant's
    /// semantics.
    fn effective_query(self, raw: &str) -> String {
        match self {
            TextFn::Query => raw.to_string(),
            TextFn::Contains => {
                let escaped = raw.replace('\\', "\\\\").replace('"', "\\\"");
                format!("\"{escaped}\"")
            }
        }
    }
}

/// `Some(TextFn)` if `nn_str` is one of this crate's full-text extension
/// function IRIs; `None` for anything else (including other
/// `Function::Custom` IRIs, e.g. XSD casts).
fn text_function_local(nn_str: &str) -> Option<TextFn> {
    let local = nn_str.strip_prefix(TEXT_NS)?;
    match local {
        "query" => Some(TextFn::Query),
        "contains" => Some(TextFn::Contains),
        _ => None,
    }
}

/// If `expr` is exactly `text:query(?var, "literal string")` or
/// `text:contains(?var, "literal string")` (a direct `FunctionCall`, not
/// nested inside `And`/`Or`/etc. -- see `try_text_search_pushdown`'s doc
/// comment for why only this shape is recognized for v1), return
/// `(var, search_string, which_fn)`. Used by `eval_pattern`'s `GP::Filter`
/// arm to decide whether to attempt pushdown before falling back to plain
/// per-row filtering.
fn text_search_call(expr: &Expression) -> Option<(Variable, String, TextFn)> {
    let Expression::FunctionCall(Function::Custom(nn), args) = expr else {
        return None;
    };
    let mode = text_function_local(nn.as_str())?;
    if args.len() != 2 {
        return None;
    }
    let Expression::Variable(var) = &args[0] else {
        return None;
    };
    let Expression::Literal(lit) = &args[1] else {
        return None;
    };
    Some((var.clone(), lit.value().to_string(), mode))
}

// ── Dataset clause (FROM / FROM NAMED) → GraphSelector ────────────────────────

/// Compute the top-level graph selector to use for pattern matching, per
/// SPARQL 1.1 §13.1/§13.2 dataset-clause semantics.
///
/// - No `dataset` clause at all (no `FROM`/`FROM NAMED` anywhere in the
///   query) → the store's actual default graph is used, as before
///   (`GraphSelector::Default`) — unless the server-wide
///   `union_default_graph` toggle is enabled (equivalent to upstream
///   Oxigraph's `serve --union-default-graph`), in which case the RDF merge
///   of the default graph and every named graph is used instead
///   (`GraphSelector::Union`).
/// - A `dataset` clause is present (i.e. the query has at least one `FROM`
///   or `FROM NAMED`) → the *effective* default graph for evaluating the
///   query's top-level pattern becomes the RDF merge of exactly the graphs
///   named in `FROM` clauses (`dataset.default`) — represented here via
///   `GraphSelector::UnionOf`. Per spec this is true even when `dataset.default`
///   is empty (i.e., only `FROM NAMED` was specified): the effective default
///   graph is then empty, matching no quads, which `UnionOf(vec![])` does
///   correctly (`graph_matches` returns `false` for every graph). A query's
///   own dataset clause always takes precedence over the server-wide
///   `union_default_graph` toggle.
fn dataset_clause_selector(
    dataset: Option<&spargebra::algebra::QueryDataset>,
    union_default_graph: bool,
) -> GraphSelector {
    match dataset {
        None if union_default_graph => GraphSelector::Union,
        None => GraphSelector::Default,
        Some(ds) => GraphSelector::UnionOf(
            ds.default
                .iter()
                .map(|n| GraphName::NamedNode(n.clone()))
                .collect(),
        ),
    }
}

// ── sparopt algebra optimization (filter pushdown, join reordering) ──────────
//
// sparopt operates on its own algebra (`sparopt::algebra::GraphPattern` /
// `Expression`), distinct from `spargebra`'s.  Both directions of conversion
// are public (`From<&spargebra::algebra::GraphPattern> for sparopt::algebra::GraphPattern`
// and the reverse), so we round-trip: convert in, optimize, convert back to
// `spargebra::algebra::GraphPattern`.  This lets the entire existing evaluator
// (expression evaluation, XSD numeric tower, aggregates, property paths) stay
// untouched — it only ever sees `spargebra` types, just a rewritten tree with
// filters pushed down and joins reordered.
//
// The reverse conversion necessarily drops sparopt's `JoinAlgorithm` /
// `LeftJoinAlgorithm` / `MinusAlgorithm` hints (spargebra has no field for
// them) — Nova doesn't consume those anyway, since join execution and the
// CLTJ*/WCOJ variable ordering are driven by `lftj.rs`'s own adaptive VEO,
// not by sparopt's static join order.
//
// `sparopt` 0.3.6 has confirmed optimizer bugs around `GRAPH ?var { ... }`
// (a variable-named graph clause): it can hoist a `Group`/aggregation out
// from inside the `Graph` node so aggregation runs once over the whole
// dataset instead of once per named graph ("COUNT: no GROUP BY inside of
// GRAPH"), drop the `Graph` wrapper entirely around a `Values` clause so
// `?g` loses its graph-name binding ("VALUES inside GRAPH binding the same
// variable as the graph name"), and distribute a single outer `Graph` node
// into *both* branches of a `Minus` so the left and right sides spuriously
// share the `?g` variable and are no longer compared for disjointness
// correctly ("outer GRAPH operator does not affect MINUS disjointness") —
// all three are W3C SPARQL 1.1 conformance-test regressions traced (via a
// minimal `sparopt`-only repro, independent of this evaluator) to the
// optimizer's round-trip, not to anything in this crate. Since a
// variable-named `GRAPH` clause anywhere in the pattern is the common
// trigger, skip sparopt optimization for the entire query in that case
// (falling back to the unoptimized pattern, which this evaluator has always
// executed correctly) rather than trying to patch sparopt's internals.
#[cfg(feature = "sparopt")]
fn optimize_pattern(pattern: &GP) -> GP {
    if contains_variable_graph(pattern) {
        return pattern.clone();
    }
    use sparopt::Optimizer;
    use sparopt::algebra::GraphPattern as OptGP;
    let opt = Optimizer::optimize_graph_pattern(OptGP::from(pattern));
    GP::from(&opt)
}

#[cfg(not(feature = "sparopt"))]
fn optimize_pattern(pattern: &GP) -> GP {
    pattern.clone()
}

/// Recursively check whether `pattern` contains a `GRAPH ?var { ... }` clause
/// (a variable-named graph, as opposed to `GRAPH <iri> { ... }`) anywhere —
/// see the doc comment on `optimize_pattern` above for why this disables the
/// sparopt optimization pass for the whole query.
#[cfg(feature = "sparopt")]
fn contains_variable_graph(pattern: &GP) -> bool {
    match pattern {
        GP::Graph {
            name: NamedNodePattern::Variable(_),
            ..
        } => true,
        GP::Graph { inner, .. } => contains_variable_graph(inner),
        GP::Join { left, right } | GP::Union { left, right } | GP::Minus { left, right } => {
            contains_variable_graph(left) || contains_variable_graph(right)
        }
        GP::LeftJoin { left, right, .. } => {
            contains_variable_graph(left) || contains_variable_graph(right)
        }
        GP::Filter { inner, .. }
        | GP::Extend { inner, .. }
        | GP::OrderBy { inner, .. }
        | GP::Project { inner, .. }
        | GP::Distinct { inner }
        | GP::Reduced { inner }
        | GP::Slice { inner, .. }
        | GP::Group { inner, .. } => contains_variable_graph(inner),
        GP::Bgp { .. } | GP::Path { .. } | GP::Values { .. } | GP::Service { .. } => false,
    }
}

// ── XSD numeric tower ─────────────────────────────────────────────────────────

/// Parsed XSD numeric literal — carries the full type so that comparison and
/// arithmetic respect W3C XSD promotion rules (integer → decimal → float → double).
#[derive(Clone, Debug)]
enum Numeric {
    Integer(XsdInt),
    Decimal(XsdDec),
    Float(XsdFloat),
    Double(XsdDbl),
}

impl Numeric {
    fn parse(l: &Literal) -> Option<Self> {
        let local = l.datatype().as_str().strip_prefix(XSD)?;
        match local {
            "integer" | "int" | "long" | "short" | "byte" | "nonNegativeInteger"
            | "positiveInteger" | "negativeInteger" | "nonPositiveInteger" | "unsignedLong"
            | "unsignedInt" | "unsignedShort" | "unsignedByte" => {
                XsdInt::from_str(l.value()).ok().map(Numeric::Integer)
            }
            "decimal" => XsdDec::from_str(l.value()).ok().map(Numeric::Decimal),
            "float" => XsdFloat::from_str(l.value()).ok().map(Numeric::Float),
            "double" => XsdDbl::from_str(l.value()).ok().map(Numeric::Double),
            _ => None,
        }
    }

    fn partial_cmp_xsd(&self, other: &Self) -> Option<std::cmp::Ordering> {
        use Numeric::*;
        match (self, other) {
            (Integer(a), Integer(b)) => Some(a.cmp(b)),
            (Decimal(a), Decimal(b)) => Some(a.cmp(b)),
            (Float(a), Float(b)) => a.partial_cmp(b),
            (Double(a), Double(b)) => a.partial_cmp(b),
            (Integer(a), Decimal(b)) => Some(XsdDec::from(*a).cmp(b)),
            (Decimal(a), Integer(b)) => Some(a.cmp(&XsdDec::from(*b))),
            (Integer(a), Float(b)) => XsdDbl::from(*a).partial_cmp(&XsdDbl::from(*b)),
            (Float(a), Integer(b)) => XsdDbl::from(*a).partial_cmp(&XsdDbl::from(*b)),
            (Integer(a), Double(b)) => XsdDbl::from(*a).partial_cmp(b),
            (Double(a), Integer(b)) => a.partial_cmp(&XsdDbl::from(*b)),
            (Decimal(a), Float(b)) => {
                let af: f64 = a.to_string().parse().ok()?;
                af.partial_cmp(&f64::from(*b))
            }
            (Float(a), Decimal(b)) => {
                let bf: f64 = b.to_string().parse().ok()?;
                f64::from(*a).partial_cmp(&bf)
            }
            (Decimal(a), Double(b)) => {
                let af: f64 = a.to_string().parse().ok()?;
                af.partial_cmp(&f64::from(*b))
            }
            (Double(a), Decimal(b)) => {
                let bf: f64 = b.to_string().parse().ok()?;
                f64::from(*a).partial_cmp(&bf)
            }
            (Float(a), Double(b)) => XsdDbl::from(*a).partial_cmp(b),
            (Double(a), Float(b)) => a.partial_cmp(&XsdDbl::from(*b)),
        }
    }

    fn eq_xsd(&self, other: &Self) -> Option<bool> {
        self.partial_cmp_xsd(other)
            .map(|o| o == std::cmp::Ordering::Equal)
    }

    fn to_f64(&self) -> Option<f64> {
        match self {
            Numeric::Integer(i) => Some(f64::from(XsdDbl::from(*i))),
            Numeric::Decimal(d) => d.to_string().parse().ok(),
            Numeric::Float(f) => Some(f64::from(*f)),
            Numeric::Double(d) => Some(f64::from(*d)),
        }
    }

    fn into_term(self) -> Term {
        match self {
            Numeric::Integer(i) => make_integer_literal(i64::from(i)),
            Numeric::Decimal(d) => {
                Term::Literal(Literal::new_typed_literal(d.to_string(), xsd_nn("decimal")))
            }
            Numeric::Float(f) => {
                Term::Literal(Literal::new_typed_literal(f.to_string(), xsd_nn("float")))
            }
            Numeric::Double(d) => {
                Term::Literal(Literal::new_typed_literal(d.to_string(), xsd_nn("double")))
            }
        }
    }
}

// ── Public result type ────────────────────────────────────────────────────────

/// The result of evaluating a SPARQL query.
pub type SolutionStream = Box<dyn Iterator<Item = Result<Solution>> + Send>;
pub type TripleStream = Box<dyn Iterator<Item = Result<oxrdf::Triple>> + Send>;

pub enum QueryResult {
    Solutions {
        vars: Arc<[Variable]>,
        stream: SolutionStream,
    },
    Boolean(bool),
    Triples(TripleStream),
}

impl QueryResult {
    pub fn into_solutions_vec(self) -> Result<(Arc<[Variable]>, Vec<Solution>)> {
        match self {
            QueryResult::Solutions { vars, stream } => {
                let sols = stream.collect::<Result<Vec<_>>>()?;
                Ok((vars, sols))
            }
            _ => Err(anyhow::anyhow!(
                "into_solutions_vec() called on a non-Solutions QueryResult"
            )),
        }
    }

    pub fn into_triples_vec(self) -> Result<Vec<oxrdf::Triple>> {
        match self {
            QueryResult::Triples(stream) => stream.collect::<Result<Vec<_>>>(),
            _ => Err(anyhow::anyhow!(
                "into_triples_vec() called on a non-Triples QueryResult"
            )),
        }
    }
}

// ── Evaluator ────────────────────────────────────────────────────────────────

pub struct Evaluator<'a, D: Dataset> {
    dataset: &'a D,
    /// BASE IRI extracted from the current query; used by IRI()/URI() to resolve relative strings.
    base_iri: std::cell::RefCell<Option<String>>,
    /// Per-query cancellation/result-cap limits; see `crate::options`.
    options: crate::options::QueryOptions,
}

impl<'a, D: Dataset> Evaluator<'a, D> {
    pub fn new(dataset: &'a D) -> Self {
        Self {
            dataset,
            base_iri: std::cell::RefCell::new(None),
            options: crate::options::QueryOptions::default(),
        }
    }

    /// Construct an evaluator with explicit execution limits (cancellation
    /// token and/or a result-row cap) — see `crate::options::QueryOptions`.
    pub fn with_options(dataset: &'a D, options: crate::options::QueryOptions) -> Self {
        Self {
            dataset,
            base_iri: std::cell::RefCell::new(None),
            options,
        }
    }

    // ── Top-level entry point ─────────────────────────────────────────────

    /// Evaluate a parsed query against `self.dataset`.
    ///
    /// Wrapped in a `tracing` span (`query_kind` field distinguishes
    /// SELECT/ASK/CONSTRUCT/DESCRIBE) so `nova-server`'s tracing-subscriber
    /// output can show evaluation latency per query. `skip_all` avoids requiring
    /// `Query`/`Self` to implement `Debug` just for span field capture.
    #[tracing::instrument(name = "evaluate", skip_all, fields(query_kind = query_kind_label(query)))]
    pub fn evaluate(&self, query: &Query) -> Result<QueryResult> {
        // Extract BASE IRI from the query so IRI()/URI() can resolve relative strings.
        *self.base_iri.borrow_mut() = match query {
            Query::Select { base_iri, .. } => base_iri.as_ref().map(|i| i.as_str().to_string()),
            Query::Ask { base_iri, .. } => base_iri.as_ref().map(|i| i.as_str().to_string()),
            Query::Construct { base_iri, .. } => base_iri.as_ref().map(|i| i.as_str().to_string()),
            Query::Describe { base_iri, .. } => base_iri.as_ref().map(|i| i.as_str().to_string()),
        };

        match query {
            Query::Select { pattern, .. } => {
                let active_graph =
                    dataset_clause_selector(query.dataset(), self.options.union_default_graph);
                let pattern = optimize_pattern(pattern);
                let solutions = self.eval_pattern(&pattern, &active_graph)?;
                let vars: Arc<[Variable]> = projected_variables(query).into();
                let stream: SolutionStream = Box::new(solutions.into_iter().map(Ok));
                Ok(QueryResult::Solutions { vars, stream })
            }
            Query::Ask { pattern, .. } => {
                let active_graph =
                    dataset_clause_selector(query.dataset(), self.options.union_default_graph);
                let pattern = optimize_pattern(pattern);
                let solutions = self.eval_pattern(&pattern, &active_graph)?;
                Ok(QueryResult::Boolean(!solutions.is_empty()))
            }
            Query::Construct {
                template, pattern, ..
            } => {
                let active_graph =
                    dataset_clause_selector(query.dataset(), self.options.union_default_graph);
                let pattern = optimize_pattern(pattern);
                let solutions = self.eval_pattern(&pattern, &active_graph)?;

                let mut triples = Vec::new();
                for sol in &solutions {
                    // Per SPARQL 1.1 §18.2.4: blank nodes in a CONSTRUCT template
                    // must be instantiated with a *fresh* blank node for every
                    // query solution, but the same fresh blank node for every
                    // occurrence of the same template label *within* that one
                    // solution's instantiation. This map is therefore rebuilt
                    // per-solution (never shared across solutions) and reused
                    // across all triple-pattern instantiations of this solution.
                    let mut bnode_map: HashMap<BlankNode, BlankNode> = HashMap::new();
                    for tp in template {
                        if let Some(t) = instantiate_triple_pattern(tp, sol, &mut bnode_map) {
                            triples.push(t);
                        }
                    }
                }
                triples.sort_by_key(|t| t.to_string());
                triples.dedup_by_key(|t| t.to_string());
                let stream: TripleStream = Box::new(triples.into_iter().map(Ok));
                Ok(QueryResult::Triples(stream))
            }

            Query::Describe { .. } => Err(anyhow::anyhow!("DESCRIBE evaluation not implemented")),
        }
    }

    // ── Core recursive algebra walk ───────────────────────────────────────

    fn eval_pattern(&self, pattern: &GP, active_graph: &GraphSelector) -> Result<Solutions> {
        match pattern {
            GP::Bgp { patterns } => self.eval_bgp(patterns, active_graph),

            GP::Join { left, right } => {
                let ls = self.eval_pattern(left, active_graph)?;
                let rs = self.eval_pattern(right, active_graph)?;
                Ok(join_solutions(ls, rs))
            }

            GP::LeftJoin {
                left,
                right,
                expression,
            } => {
                let ls = self.eval_pattern(left, active_graph)?;
                let rs = self.eval_pattern(right, active_graph)?;
                self.left_join(ls, rs, expression.as_ref(), active_graph)
            }

            GP::Filter { expr, inner } => {
                // Full-text search pushdown: if `expr` is a
                // `text:query`/`text:contains(?var, "...")` call and `?var`
                // is bound by `inner` (a top-level BGP) as an object, run
                // the search first to constrain `?var`'s domain before the
                // join, rather than joining everything and then filtering.
                // Falls through to plain evaluation if no pushdown
                // opportunity is found (e.g. no `TextSearch` backend
                // configured, `inner` isn't a plain BGP, or the FILTER
                // expression isn't this exact shape).
                if let Some(result) = self.try_text_search_pushdown(expr, inner, active_graph) {
                    return result;
                }

                let solutions = self.eval_pattern(inner, active_graph)?;
                Ok(solutions
                    .into_iter()
                    .filter(|sol| {
                        self.eval_expr(expr, sol, active_graph)
                            .and_then(|t| to_ebv(&t))
                            .unwrap_or(false)
                    })
                    .collect())
            }

            GP::Union { left, right } => {
                let mut ls = self.eval_pattern(left, active_graph)?;
                let rs = self.eval_pattern(right, active_graph)?;
                ls.extend(rs);
                Ok(ls)
            }

            GP::Graph { name, inner } => self.eval_graph(name, inner),

            GP::Extend {
                inner,
                variable,
                expression,
            } => {
                let mut solutions = self.eval_pattern(inner, active_graph)?;
                for sol in &mut solutions {
                    if let Some(val) = self.eval_expr(expression, sol, active_graph)
                        && !sol.contains(variable)
                    {
                        sol.insert(variable.clone(), val);
                    }
                }
                Ok(solutions)
            }

            GP::Minus { left, right } => {
                let ls = self.eval_pattern(left, active_graph)?;
                let rs = self.eval_pattern(right, active_graph)?;
                Ok(ls
                    .into_iter()
                    .filter(|l| !rs.iter().any(|r| minus_compatible(l, r)))
                    .collect())
            }

            GP::Values {
                variables,
                bindings,
            } => Ok(bindings
                .iter()
                .map(|row| {
                    let mut sol = Solution::new();
                    for (var, opt) in variables.iter().zip(row.iter()) {
                        if let Some(gt) = opt {
                            sol.insert(var.clone(), ground_term_to_term(gt));
                        }
                    }
                    sol
                })
                .collect()),

            GP::OrderBy { inner, expression } => {
                let mut solutions = self.eval_pattern(inner, active_graph)?;
                let ag = active_graph;
                solutions.sort_by(|a, b| {
                    for oe in expression {
                        let (e, asc) = match oe {
                            OrderExpression::Asc(e) => (e, true),
                            OrderExpression::Desc(e) => (e, false),
                        };
                        let av = self.eval_expr(e, a, ag);
                        let bv = self.eval_expr(e, b, ag);
                        let ord = compare_terms_opt(av.as_ref(), bv.as_ref());
                        let ord = if asc { ord } else { ord.reverse() };
                        if ord != std::cmp::Ordering::Equal {
                            return ord;
                        }
                    }
                    std::cmp::Ordering::Equal
                });
                Ok(solutions)
            }

            GP::Project { inner, variables } => {
                let solutions = self.eval_pattern(inner, active_graph)?;
                Ok(solutions
                    .into_iter()
                    .map(|sol| sol.project(variables.iter()))
                    .collect())
            }

            GP::Distinct { inner } => {
                let solutions = self.eval_pattern(inner, active_graph)?;
                let mut seen = std::collections::HashSet::new();
                let mut out = Vec::new();
                for sol in solutions {
                    if seen.insert(solution_key(&sol)) {
                        out.push(sol);
                    }
                }
                Ok(out)
            }

            GP::Reduced { inner } => self.eval_pattern(inner, active_graph),

            GP::Slice {
                inner,
                start,
                length,
            } => {
                let solutions = self.eval_pattern(inner, active_graph)?;
                Ok(solutions
                    .into_iter()
                    .skip(*start)
                    .take(length.unwrap_or(usize::MAX))
                    .collect())
            }

            GP::Group {
                inner,
                variables,
                aggregates,
            } => self.eval_group(inner, variables, aggregates, active_graph),

            GP::Service {
                name,
                inner,
                silent,
            } => {
                let result = self.eval_service(name, inner);
                match result {
                    Ok(solutions) => Ok(solutions),
                    Err(e) => {
                        if *silent {
                            Ok(vec![])
                        } else {
                            Err(e)
                        }
                    }
                }
            }

            // ── Property paths ────────────────────────────────────────────
            GP::Path {
                subject,
                path,
                object,
            } => self.eval_path(subject, path, object, active_graph),
        }
    }

    // =========================================================================
    // SERVICE (SPARQL 1.1 Federated Query)
    // =========================================================================

    /// Dispatch a `SERVICE <name> { inner }` clause to the configured
    /// [`crate::service::ServiceHandler`] (if any). Returns an error if no
    /// handler is configured (`self.options.service_handler.is_none()`) or
    /// `name` is a variable (unsupported — SPARQL 1.1 Federated Query's
    /// "variable service name" form requires a prior binding source, which
    /// this evaluator does not yet implement); the caller (`eval_pattern`'s
    /// `GP::Service` arm) turns that error into zero solutions when the
    /// clause was written with `SILENT`.
    fn eval_service(&self, name: &NamedNodePattern, inner: &GP) -> Result<Solutions> {
        let handler = self.options.service_handler.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "SERVICE (federated queries) not supported: no ServiceHandler configured \
                 (see QueryOptions::with_service_handler)"
            )
        })?;
        let service_name = match name {
            NamedNodePattern::NamedNode(n) => n,
            NamedNodePattern::Variable(_) => {
                return Err(anyhow::anyhow!(
                    "SERVICE with a variable service name is not supported"
                ));
            }
        };
        handler.handle(service_name, inner, self.base_iri.borrow().as_deref())
    }

    // =========================================================================
    // BGP
    // =========================================================================

    fn eval_bgp(
        &self,
        patterns: &[TriplePattern],
        active_graph: &GraphSelector,
    ) -> Result<Solutions> {
        // ── Fast path: Leapfrog Triejoin ──────────────────────────────────────
        // Try LFTJ first; falls back to nested-loop if the dataset doesn't
        // support it, the delta is non-empty, or the BGP has blank nodes / unknown terms.
        if let Some(result) = crate::lftj::eval_bgp_lftj_cancellable(
            self.dataset,
            patterns,
            active_graph,
            self.options.cancellation.as_ref(),
        ) {
            crate::lftj::record_lftj_used();
            return result;
        }
        crate::lftj::record_lftj_fallback();

        // ── Fallback: nested-loop evaluation ──────────────────────────────────
        let mut solutions: Solutions = vec![Solution::new()];

        for tp in patterns {
            self.options
                .check(solutions.len())
                .map_err(anyhow::Error::from)?;
            let mut next: Solutions = Vec::new();

            for current in &solutions {
                let (s_pt, s_var) = term_pattern_with_sol(&tp.subject, current);
                let (p_pt, p_var) = nn_pattern_with_sol(&tp.predicate, current);
                let (o_pt, o_var) = term_pattern_with_sol(&tp.object, current);

                let qp = QuadPattern {
                    subject: s_pt,
                    predicate: p_pt,
                    object: o_pt,
                    graph: active_graph.clone(),
                };

                for qr in self.dataset.find_quads(&qp)? {
                    let q = qr?;
                    let mut sol = current.clone();
                    let mut ok = true;

                    if ok {
                        ok = bind_var_arc(&mut sol, &s_var, &q.subject);
                    }
                    if ok {
                        ok = bind_var_arc(&mut sol, &p_var, &q.predicate);
                    }
                    if ok {
                        ok = bind_var_arc(&mut sol, &o_var, &q.object);
                    }

                    // ── RDF-star: structural match for quoted-triple patterns ──
                    // term_pattern_with_sol() returns (Wildcard, None) for
                    // TermPattern::Triple, so the storage scan is unconstrained.
                    // We post-filter and bind inner variables here.
                    if ok && let TermPattern::Triple(inner_tp) = &tp.subject {
                        ok = bind_triple_pattern(&mut sol, inner_tp, q.subject.as_ref());
                    }
                    if ok && let TermPattern::Triple(inner_tp) = &tp.object {
                        ok = bind_triple_pattern(&mut sol, inner_tp, q.object.as_ref());
                    }

                    if ok {
                        next.push(sol);
                    }
                }
            }
            solutions = next;
        }

        Ok(solutions)
    }

    // =========================================================================
    // GRAPH clause
    // =========================================================================

    fn eval_graph(&self, name: &NamedNodePattern, inner: &GP) -> Result<Solutions> {
        match name {
            NamedNodePattern::NamedNode(n) => {
                let sel = GraphSelector::Named(GraphName::NamedNode(n.clone()));
                self.eval_pattern(inner, &sel)
            }
            NamedNodePattern::Variable(v) => {
                let named: Vec<GraphName> = self
                    .dataset
                    .named_graphs()?
                    .collect::<anyhow::Result<Vec<_>>>()?;

                let mut result = Vec::new();
                for g in named {
                    let graph_term = match &g {
                        GraphName::NamedNode(n) => Term::NamedNode(n.clone()),
                        GraphName::DefaultGraph => continue,
                        #[allow(unreachable_patterns)]
                        _ => continue,
                    };
                    let sel = GraphSelector::Named(g);
                    let sols = self.eval_pattern(inner, &sel)?;
                    for mut sol in sols {
                        // Per SPARQL 1.1 §18.5 (Evaluation semantics for
                        // `GRAPH ?g { P }`): the inner pattern `P` is
                        // evaluated against each named graph `g` in turn, and
                        // `?g` is *joined* with the graph name — not simply
                        // overwritten. If `P` itself already binds `?g`
                        // (e.g. a nested subquery projecting a variable also
                        // named `?g`), that binding must be *compatible*
                        // with the currently-iterated graph name or the
                        // solution is discarded; it must not be silently
                        // clobbered by the graph name (which would keep rows
                        // whose inner binding actually disagrees with the
                        // active graph).
                        match sol.get(v) {
                            None => sol.insert(v.clone(), graph_term.clone()),
                            Some(bound) if *bound == graph_term => {}
                            Some(_) => continue, // incompatible — drop this solution
                        }
                        result.push(sol);
                    }
                }
                Ok(result)
            }
        }
    }

    // =========================================================================
    // LeftJoin (OPTIONAL)
    // =========================================================================

    fn left_join(
        &self,
        left: Solutions,
        right: Solutions,
        condition: Option<&Expression>,
        active_graph: &GraphSelector,
    ) -> Result<Solutions> {
        let mut result = Vec::new();
        for ls in &left {
            let mut joined = false;
            for rs in &right {
                if let Some(merged) = ls.merge_compatible(rs) {
                    let passes = condition.is_none_or(|expr| {
                        self.eval_expr(expr, &merged, active_graph)
                            .and_then(|t| to_ebv(&t))
                            .unwrap_or(false)
                    });
                    if passes {
                        result.push(merged);
                        joined = true;
                    }
                }
            }
            if !joined {
                result.push(ls.clone());
            }
        }
        Ok(result)
    }

    // =========================================================================
    // Group / Aggregates
    // =========================================================================

    fn eval_group(
        &self,
        inner: &GP,
        variables: &[Variable],
        aggregates: &[(Variable, AggregateExpression)],
        active_graph: &GraphSelector,
    ) -> Result<Solutions> {
        let inner_sols = self.eval_pattern(inner, active_graph)?;

        let mut groups: HashMap<Vec<Option<String>>, Vec<Solution>> = HashMap::new();
        for sol in inner_sols {
            let key: Vec<Option<String>> = variables
                .iter()
                .map(|v| sol.get(v).map(|t| t.to_string()))
                .collect();
            groups.entry(key).or_default().push(sol);
        }

        if groups.is_empty() && variables.is_empty() {
            groups.insert(vec![], vec![]);
        }

        let mut result = Vec::new();
        for group in groups.values() {
            let mut sol = Solution::new();

            if let Some(first) = group.first() {
                for v in variables {
                    if let Some(t) = first.get(v) {
                        sol.insert(v.clone(), t.clone());
                    }
                }
            }

            for (agg_var, agg_expr) in aggregates {
                if let Some(val) = self.eval_aggregate(agg_expr, group, active_graph) {
                    sol.insert(agg_var.clone(), val);
                }
            }
            result.push(sol);
        }
        Ok(result)
    }

    fn eval_aggregate(
        &self,
        agg: &AggregateExpression,
        group: &[Solution],
        active_graph: &GraphSelector,
    ) -> Option<Term> {
        match agg {
            AggregateExpression::CountSolutions { distinct } => {
                let count = if *distinct {
                    use std::collections::HashSet;
                    group
                        .iter()
                        .map(|s| format!("{s:?}"))
                        .collect::<HashSet<_>>()
                        .len() as i64
                } else {
                    group.len() as i64
                };
                Some(make_integer_literal(count))
            }
            AggregateExpression::FunctionCall {
                name,
                expr,
                distinct,
            } => {
                match name {
                    AggregateFunction::Count => {
                        let vals: Vec<String> = group
                            .iter()
                            .filter_map(|s| self.eval_expr(expr, s, active_graph))
                            .map(|t| t.to_string())
                            .collect();
                        let count = if *distinct {
                            use std::collections::HashSet;
                            vals.into_iter().collect::<HashSet<_>>().len() as i64
                        } else {
                            vals.len() as i64
                        };
                        Some(make_integer_literal(count))
                    }
                    AggregateFunction::Sum => {
                        let mut nums: Vec<Numeric> = group
                            .iter()
                            .filter_map(|s| {
                                let t = self.eval_expr(expr, s, active_graph)?;
                                if let Term::Literal(l) = &t {
                                    Numeric::parse(l)
                                } else {
                                    None
                                }
                            })
                            .collect();

                        if *distinct {
                            let mut seen = std::collections::HashSet::new();
                            nums.retain(|n| seen.insert(n.clone().into_term().to_string()));
                        }

                        if nums.is_empty() {
                            return Some(make_integer_literal(0));
                        }

                        // Determine result type by XSD promotion
                        let all_int = nums.iter().all(|n| matches!(n, Numeric::Integer(_)));
                        let any_double = nums.iter().any(|n| matches!(n, Numeric::Double(_)));
                        let any_float = nums.iter().any(|n| matches!(n, Numeric::Float(_)));

                        if all_int {
                            let sum: i64 = nums
                                .iter()
                                .filter_map(|n| {
                                    if let Numeric::Integer(i) = n {
                                        Some(i64::from(*i))
                                    } else {
                                        None
                                    }
                                })
                                .sum();
                            Some(make_integer_literal(sum))
                        } else if any_double {
                            let sum: f64 = nums.iter().filter_map(|n| n.to_f64()).sum();
                            Some(Term::Literal(Literal::new_typed_literal(
                                format_xsd_double(sum),
                                xsd_nn("double"),
                            )))
                        } else if any_float {
                            let sum: f64 = nums.iter().filter_map(|n| n.to_f64()).sum();
                            Some(Term::Literal(Literal::new_typed_literal(
                                (sum as f32).to_string(),
                                xsd_nn("float"),
                            )))
                        } else {
                            // decimal (or int+decimal mixed) — exact XsdDec arithmetic
                            let mut acc = XsdDec::from(0i64);
                            for n in &nums {
                                let addend: XsdDec = match n {
                                    Numeric::Integer(i) => XsdDec::from(*i),
                                    Numeric::Decimal(d) => *d,
                                    _ => return None,
                                };
                                acc = acc.checked_add(addend)?;
                            }
                            Some(Term::Literal(Literal::new_typed_literal(
                                acc.to_string(),
                                xsd_nn("decimal"),
                            )))
                        }
                    }
                    AggregateFunction::Avg => {
                        // Collect values with error propagation:
                        // - unbound (None from eval_expr) → skip (per SPARQL spec)
                        // - bound non-numeric literal or non-literal → error (return None)
                        let mut nums: Vec<Numeric> = Vec::new();
                        for sol in group {
                            match self.eval_expr(expr, sol, active_graph) {
                                None => {} // unbound, skip
                                Some(t) => {
                                    if let Term::Literal(ref l) = t {
                                        {
                                            let n = Numeric::parse(l)?;
                                            nums.push(n)
                                        }
                                    } else {
                                        return None; // non-literal (IRI, blank node) → error
                                    }
                                }
                            }
                        }

                        // Per SPARQL 1.1 spec: AVG over empty set = 0^^xsd:integer
                        if nums.is_empty() {
                            return Some(make_integer_literal(0));
                        }

                        let count = nums.len();
                        let any_double = nums.iter().any(|n| matches!(n, Numeric::Double(_)));
                        let any_float = nums.iter().any(|n| matches!(n, Numeric::Float(_)));

                        if any_double {
                            let sum: f64 = nums.iter().filter_map(|n| n.to_f64()).sum();
                            let avg = sum / count as f64;
                            Some(Term::Literal(Literal::new_typed_literal(
                                format_xsd_double(avg),
                                xsd_nn("double"),
                            )))
                        } else if any_float {
                            let sum: f64 = nums.iter().filter_map(|n| n.to_f64()).sum();
                            let avg = sum / count as f64;
                            Some(Term::Literal(Literal::new_typed_literal(
                                (avg as f32).to_string(),
                                xsd_nn("float"),
                            )))
                        } else {
                            // integer or decimal input → decimal result with exact arithmetic
                            let mut sum_dec = XsdDec::from(0i64);
                            for n in &nums {
                                let addend: XsdDec = match n {
                                    Numeric::Integer(i) => XsdDec::from(*i),
                                    Numeric::Decimal(d) => *d,
                                    _ => return None,
                                };
                                sum_dec = sum_dec.checked_add(addend)?;
                            }
                            let count_dec = XsdDec::from(count as i64);
                            let avg_dec = sum_dec.checked_div(count_dec)?;
                            Some(Term::Literal(Literal::new_typed_literal(
                                avg_dec.to_string(),
                                xsd_nn("decimal"),
                            )))
                        }
                    }
                    AggregateFunction::Min => group
                        .iter()
                        .filter_map(|s| self.eval_expr(expr, s, active_graph))
                        .min_by(|a, b| compare_terms(a, b).unwrap_or(std::cmp::Ordering::Equal))
                        .map(normalize_double_term),
                    AggregateFunction::Max => group
                        .iter()
                        .filter_map(|s| self.eval_expr(expr, s, active_graph))
                        .max_by(|a, b| compare_terms(a, b).unwrap_or(std::cmp::Ordering::Equal))
                        .map(normalize_double_term),
                    AggregateFunction::Sample => group
                        .iter()
                        .find_map(|s| self.eval_expr(expr, s, active_graph)),
                    AggregateFunction::GroupConcat { separator } => {
                        let sep = separator.as_deref().unwrap_or(" ");
                        let mut vals: Vec<String> = group
                            .iter()
                            .filter_map(|s| {
                                let t = self.eval_expr(expr, s, active_graph)?;
                                match t {
                                    Term::Literal(l) => Some(l.value().to_string()),
                                    _ => None,
                                }
                            })
                            .collect();
                        if *distinct {
                            vals.sort();
                            vals.dedup();
                        }
                        Some(Term::Literal(Literal::new_simple_literal(vals.join(sep))))
                    }
                    AggregateFunction::Custom(_) => None,
                }
            }
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    // =========================================================================
    // Expression evaluation
    // =========================================================================

    fn eval_expr(&self, expr: &Expression, sol: &Solution, ag: &GraphSelector) -> Option<Term> {
        match expr {
            Expression::NamedNode(n) => Some(Term::NamedNode(n.clone())),
            Expression::Literal(l) => Some(Term::Literal(l.clone())),
            Expression::Variable(v) => sol.get(v).cloned(),

            Expression::Or(a, b) => {
                let ae = self.eval_expr(a, sol, ag).and_then(|t| to_ebv(&t));
                let be = self.eval_expr(b, sol, ag).and_then(|t| to_ebv(&t));
                match (ae, be) {
                    (Some(true), _) | (_, Some(true)) => Some(bool_term(true)),
                    (Some(false), Some(false)) => Some(bool_term(false)),
                    _ => None,
                }
            }
            Expression::And(a, b) => {
                let ae = self.eval_expr(a, sol, ag).and_then(|t| to_ebv(&t));
                let be = self.eval_expr(b, sol, ag).and_then(|t| to_ebv(&t));
                match (ae, be) {
                    (Some(false), _) | (_, Some(false)) => Some(bool_term(false)),
                    (Some(true), Some(true)) => Some(bool_term(true)),
                    _ => None,
                }
            }
            Expression::Not(inner) => {
                let ebv = self.eval_expr(inner, sol, ag).and_then(|t| to_ebv(&t))?;
                Some(bool_term(!ebv))
            }

            Expression::Equal(a, b) => {
                let av = self.eval_expr(a, sol, ag)?;
                let bv = self.eval_expr(b, sol, ag)?;
                Some(bool_term(rdf_equal(&av, &bv)?))
            }
            Expression::SameTerm(a, b) => {
                let av = self.eval_expr(a, sol, ag)?;
                let bv = self.eval_expr(b, sol, ag)?;
                Some(bool_term(av == bv))
            }
            Expression::Greater(a, b) => {
                let av = self.eval_expr(a, sol, ag)?;
                let bv = self.eval_expr(b, sol, ag)?;
                Some(bool_term(
                    compare_terms(&av, &bv)? == std::cmp::Ordering::Greater,
                ))
            }
            Expression::GreaterOrEqual(a, b) => {
                let av = self.eval_expr(a, sol, ag)?;
                let bv = self.eval_expr(b, sol, ag)?;
                Some(bool_term(
                    compare_terms(&av, &bv)? != std::cmp::Ordering::Less,
                ))
            }
            Expression::Less(a, b) => {
                let av = self.eval_expr(a, sol, ag)?;
                let bv = self.eval_expr(b, sol, ag)?;
                Some(bool_term(
                    compare_terms(&av, &bv)? == std::cmp::Ordering::Less,
                ))
            }
            Expression::LessOrEqual(a, b) => {
                let av = self.eval_expr(a, sol, ag)?;
                let bv = self.eval_expr(b, sol, ag)?;
                Some(bool_term(
                    compare_terms(&av, &bv)? != std::cmp::Ordering::Greater,
                ))
            }

            Expression::In(val, list) => {
                let v = self.eval_expr(val, sol, ag)?;
                let mut had_err = false;
                for item in list {
                    match self.eval_expr(item, sol, ag) {
                        None => had_err = true,
                        Some(iv) => {
                            if rdf_equal(&v, &iv) == Some(true) {
                                return Some(bool_term(true));
                            }
                        }
                    }
                }
                if had_err {
                    None
                } else {
                    Some(bool_term(false))
                }
            }

            Expression::Add(a, b) => numeric_binop(
                &self.eval_expr(a, sol, ag)?,
                &self.eval_expr(b, sol, ag)?,
                |x, y| x + y,
            ),
            Expression::Subtract(a, b) => numeric_binop(
                &self.eval_expr(a, sol, ag)?,
                &self.eval_expr(b, sol, ag)?,
                |x, y| x - y,
            ),
            Expression::Multiply(a, b) => numeric_binop(
                &self.eval_expr(a, sol, ag)?,
                &self.eval_expr(b, sol, ag)?,
                |x, y| x * y,
            ),
            Expression::Divide(a, b) => {
                let bv = self.eval_expr(b, sol, ag)?;
                let bn = term_as_f64(&bv)?;
                if bn == 0.0 {
                    return None;
                }
                // SPARQL 1.1: integer / integer → xsd:decimal (not integer)
                numeric_binop_div(&self.eval_expr(a, sol, ag)?, &bv)
            }
            Expression::UnaryPlus(inner) => {
                let v = self.eval_expr(inner, sol, ag)?;
                term_as_f64(&v)?;
                Some(v)
            }
            Expression::UnaryMinus(inner) => {
                let v = self.eval_expr(inner, sol, ag)?;
                let n = term_as_f64(&v)?;
                numeric_unary(&v, -n)
            }

            Expression::Exists(pat) => {
                let candidates = self.eval_pattern(pat, ag).ok()?;
                Some(bool_term(
                    candidates.iter().any(|s| sol.merge_compatible(s).is_some()),
                ))
            }
            Expression::Bound(v) => Some(bool_term(sol.contains(v))),

            Expression::If(cond, then_e, else_e) => {
                match self.eval_expr(cond, sol, ag).and_then(|t| to_ebv(&t)) {
                    Some(true) => self.eval_expr(then_e, sol, ag),
                    Some(false) => self.eval_expr(else_e, sol, ag),
                    None => None,
                }
            }
            Expression::Coalesce(exprs) => {
                for e in exprs {
                    if let Some(v) = self.eval_expr(e, sol, ag) {
                        return Some(v);
                    }
                }
                None
            }
            Expression::FunctionCall(func, args) => self.eval_fn(func, args, sol, ag),

            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    // =========================================================================
    // Built-in function dispatch
    // =========================================================================

    fn eval_fn(
        &self,
        func: &Function,
        args: &[Expression],
        sol: &Solution,
        ag: &GraphSelector,
    ) -> Option<Term> {
        let ev: Vec<Option<Term>> = args.iter().map(|e| self.eval_expr(e, sol, ag)).collect();

        let arg = |i: usize| -> Option<Term> { ev.get(i)?.clone() };
        let lit = |i: usize| -> Option<Literal> {
            match arg(i)? {
                Term::Literal(l) => Some(l),
                _ => None,
            }
        };
        let str_val = |i: usize| -> Option<String> {
            match arg(i)? {
                Term::NamedNode(n) => Some(n.as_str().to_string()),
                Term::Literal(l) => Some(l.value().to_string()),
                _ => None,
            }
        };

        match func {
            // ── Conversion ────────────────────────────────────────────────
            Function::Str => Some(Term::Literal(Literal::new_simple_literal(str_val(0)?))),
            Function::Lang => {
                let l = lit(0)?;
                Some(Term::Literal(Literal::new_simple_literal(
                    l.language().unwrap_or(""),
                )))
            }
            Function::LangMatches => {
                let tag = lit(0)?;
                let range = lit(1)?;
                // LANG() returns a simple literal whose *value* is the language tag string,
                // not a lang-tagged literal itself.  We must use .value(), not .language().
                let tag_s = tag.value();
                let range_s = range.value();
                let ok = if range_s == "*" {
                    !tag_s.is_empty()
                } else {
                    tag_s.to_lowercase().starts_with(&range_s.to_lowercase())
                };
                Some(bool_term(ok))
            }
            Function::Datatype => Some(Term::NamedNode(lit(0)?.datatype().into())),
            Function::Iri => {
                let s = str_val(0)?;
                if s.is_empty() {
                    return None;
                }
                resolve_iri_against_base(&s, self.base_iri.borrow().as_deref()).map(Term::NamedNode)
            }
            Function::BNode => {
                // SPARQL 1.1 §17.4.2.7: BNODE(strExpr) — repeated calls with the
                // same string value *within one solution* must yield the same
                // blank node, but different solutions (even with the same
                // string value) must get fresh, non-colliding blank nodes.
                // `Solution::bnode_for` implements exactly this via a
                // per-row (Rc-shared) cache keyed by the argument's string
                // value — never by reusing the string itself as the label.
                if args.is_empty() {
                    Some(Term::BlankNode(BlankNode::default()))
                } else {
                    let s = str_val(0)?;
                    Some(Term::BlankNode(sol.bnode_for(&s)))
                }
            }
            Function::StrLang => {
                let l0 = lit(0)?;
                // First arg must be plain/xsd:string (no language tag, no other typed literals)
                if l0.language().is_some() {
                    return None;
                }
                if l0.datatype().as_str().strip_prefix(XSD) != Some("string") {
                    return None;
                }
                let lex = l0.value().to_string();
                // Language tags are case-insensitive; normalize to lowercase per RDF 1.1 / BCP47
                let lang = lit(1)?.value().to_lowercase();
                if lang.is_empty() {
                    return None;
                }
                Some(Term::Literal(
                    Literal::new_language_tagged_literal_unchecked(lex, lang),
                ))
            }
            Function::StrDt => {
                let l0 = lit(0)?;
                // First arg must be plain/xsd:string (no language tag, no other typed literals)
                if l0.language().is_some() {
                    return None;
                }
                if l0.datatype().as_str().strip_prefix(XSD) != Some("string") {
                    return None;
                }
                let lex = l0.value().to_string();
                let dt = match arg(1)? {
                    Term::NamedNode(n) => n,
                    _ => return None,
                };
                Some(Term::Literal(Literal::new_typed_literal(lex, dt)))
            }

            // ── Type predicates ───────────────────────────────────────────
            Function::IsIri => Some(bool_term(matches!(arg(0)?, Term::NamedNode(_)))),
            Function::IsBlank => Some(bool_term(matches!(arg(0)?, Term::BlankNode(_)))),
            Function::IsLiteral => Some(bool_term(matches!(arg(0)?, Term::Literal(_)))),
            Function::IsNumeric => {
                let ok = match arg(0)? {
                    Term::Literal(l) => is_numeric_dt(l.datatype().as_str()),
                    _ => false,
                };
                Some(bool_term(ok))
            }

            // ── Numeric ───────────────────────────────────────────────────
            Function::Abs => {
                let l = lit(0)?;
                Some(numeric_like_literal(literal_as_f64(&l)?.abs(), &l))
            }
            Function::Ceil => {
                let l = lit(0)?;
                Some(numeric_like_literal(literal_as_f64(&l)?.ceil(), &l))
            }
            Function::Floor => {
                let l = lit(0)?;
                Some(numeric_like_literal(literal_as_f64(&l)?.floor(), &l))
            }
            Function::Round => {
                let l = lit(0)?;
                Some(numeric_like_literal(literal_as_f64(&l)?.round(), &l))
            }

            // ── String ────────────────────────────────────────────────────
            Function::Concat => {
                // SPARQL 1.1 §17.4.3.10 (extended by RDF 1.2 for directional
                // language-tagged strings): all args must be string literals
                // (xsd:string, lang-tagged, or dir-lang-tagged); if ALL args
                // share the exact same language tag AND the exact same base
                // direction (including "no direction" as a state that must
                // also match across every arg), the result inherits both the
                // language tag and the direction. If the language tags agree
                // but the directions disagree (or vice versa), the result
                // must be a fully plain literal — neither language tag nor
                // direction is preserved.
                let mut buf = String::new();
                let mut common_lang: Option<String> = None;
                let mut common_dir: Option<BaseDirection> = None;
                let mut lang_disagreed = false;
                let mut dir_disagreed = false;
                let mut first = true;
                for opt in &ev {
                    match opt {
                        Some(Term::Literal(l)) => {
                            // Non-string literals (e.g. xsd:integer) are type errors
                            if !is_string_literal(l) {
                                return None;
                            }
                            buf.push_str(l.value());
                            let this_lang = l.language().map(|s| s.to_string());
                            let this_dir = l.direction();
                            if first {
                                common_lang = this_lang;
                                common_dir = this_dir;
                                first = false;
                            } else {
                                if common_lang != this_lang {
                                    lang_disagreed = true;
                                }
                                if common_dir != this_dir {
                                    dir_disagreed = true;
                                }
                            }
                        }
                        _ => return None,
                    }
                }
                let lit_out = if lang_disagreed || dir_disagreed {
                    Literal::new_simple_literal(buf)
                } else {
                    match (common_lang, common_dir) {
                        (Some(tag), Some(dir)) => {
                            Literal::new_directional_language_tagged_literal_unchecked(
                                buf, tag, dir,
                            )
                        }
                        (Some(tag), None) => {
                            Literal::new_language_tagged_literal_unchecked(buf, tag)
                        }
                        _ => Literal::new_simple_literal(buf),
                    }
                };
                Some(Term::Literal(lit_out))
            }

            Function::SubStr => {
                let l0 = lit(0)?;
                let src: Vec<char> = l0.value().chars().collect();
                // SPARQL SUBSTR is 1-based; positions can be fractional (rounded)
                let start = term_as_f64(&arg(1)?)?.round() as i64;
                let start_idx = (start - 1).max(0) as usize;
                let result: String = if let Some(len_term) = arg(2) {
                    let len = term_as_f64(&len_term)?.round().max(0.0) as usize;
                    src.iter().skip(start_idx).take(len).collect()
                } else {
                    src.iter().skip(start_idx).collect()
                };
                Some(Term::Literal(preserve_string_type(&l0, result)))
            }
            Function::StrLen => Some(make_integer_literal(lit(0)?.value().chars().count() as i64)),
            Function::Replace => {
                let l0 = lit(0)?;
                // First arg must be a string literal (xsd:string or lang-tagged)
                if !is_string_literal(&l0) {
                    return None;
                }
                let pat = str_val(1)?;
                let repl = str_val(2)?;
                let flags = str_val(3).unwrap_or_default();
                let re = build_regex(&pat, &flags)?;
                let result = re.replace_all(l0.value(), repl.as_str()).into_owned();
                Some(Term::Literal(preserve_string_type(&l0, result)))
            }
            Function::UCase => {
                let l = lit(0)?;
                Some(Term::Literal(preserve_string_type(
                    &l,
                    l.value().to_uppercase(),
                )))
            }
            Function::LCase => {
                let l = lit(0)?;
                Some(Term::Literal(preserve_string_type(
                    &l,
                    l.value().to_lowercase(),
                )))
            }
            Function::EncodeForUri => Some(Term::Literal(Literal::new_simple_literal(
                encode_for_uri(&str_val(0)?),
            ))),
            Function::Contains => {
                let s = lit(0)?.value().to_string();
                let needle = lit(1)?.value().to_string();
                Some(bool_term(s.contains(needle.as_str())))
            }
            Function::StrStarts => {
                let s = lit(0)?.value().to_string();
                let prefix = lit(1)?.value().to_string();
                Some(bool_term(s.starts_with(prefix.as_str())))
            }
            Function::StrEnds => {
                let s = lit(0)?.value().to_string();
                let suffix = lit(1)?.value().to_string();
                Some(bool_term(s.ends_with(suffix.as_str())))
            }
            Function::StrBefore => {
                let l0 = lit(0)?;
                let l1 = lit(1)?;
                // Both args must be string literals (xsd:string or lang-tagged)
                if !is_string_literal(&l0) {
                    return None;
                }
                if !is_string_literal(&l1) {
                    return None;
                }
                // If arg2 has a lang tag, arg1 must have the same lang tag
                if let Some(lang2) = l1.language()
                    && l0.language() != Some(lang2)
                {
                    return None;
                }
                let s = l0.value();
                let delim = l1.value();
                match s.find(delim) {
                    // Found: return substring preserving the type of arg1
                    Some(i) => Some(Term::Literal(preserve_string_type(&l0, s[..i].to_string()))),
                    // Not found: always return plain empty string per SPARQL spec
                    None => Some(Term::Literal(Literal::new_simple_literal(""))),
                }
            }
            Function::StrAfter => {
                let l0 = lit(0)?;
                let l1 = lit(1)?;
                // Both args must be string literals (xsd:string or lang-tagged)
                if !is_string_literal(&l0) {
                    return None;
                }
                if !is_string_literal(&l1) {
                    return None;
                }
                // If arg2 has a lang tag, arg1 must have the same lang tag
                if let Some(lang2) = l1.language()
                    && l0.language() != Some(lang2)
                {
                    return None;
                }
                let s = l0.value();
                let delim = l1.value();
                match s.find(delim) {
                    // Found: return substring preserving the type of arg1
                    Some(i) => Some(Term::Literal(preserve_string_type(
                        &l0,
                        s[i + delim.len()..].to_string(),
                    ))),
                    // Not found: always return plain empty string per SPARQL spec
                    None => Some(Term::Literal(Literal::new_simple_literal(""))),
                }
            }

            // ── Regex ─────────────────────────────────────────────────────
            Function::Regex => {
                let s = lit(0)?.value().to_string();
                let pattern = lit(1)?.value().to_string();
                let flags = str_val(2).unwrap_or_default();
                let re = build_regex(&pattern, &flags)?;
                Some(bool_term(re.is_match(&s)))
            }

            // ── Date/Time ─────────────────────────────────────────────────
            Function::Year => {
                let l = lit(0)?;
                let y = dt_year(l.value(), l.datatype().as_str())?;
                Some(make_integer_literal(y))
            }
            Function::Month => {
                let l = lit(0)?;
                Some(make_integer_literal(
                    dt_month(l.value(), l.datatype().as_str())? as i64,
                ))
            }
            Function::Day => {
                let l = lit(0)?;
                Some(make_integer_literal(
                    dt_day(l.value(), l.datatype().as_str())? as i64,
                ))
            }
            Function::Hours => {
                let l = lit(0)?;
                Some(make_integer_literal(
                    dt_hour(l.value(), l.datatype().as_str())? as i64,
                ))
            }
            Function::Minutes => {
                let l = lit(0)?;
                Some(make_integer_literal(
                    dt_minute(l.value(), l.datatype().as_str())? as i64,
                ))
            }
            Function::Seconds => {
                let l = lit(0)?;
                let secs = dt_second(l.value(), l.datatype().as_str())?;
                Some(Term::Literal(Literal::new_typed_literal(
                    secs.to_string(),
                    xsd_nn("decimal"),
                )))
            }
            Function::Timezone => {
                let l = lit(0)?;
                // Returns None if the literal has no timezone → type error
                let tz_str = dt_timezone_str(l.value(), l.datatype().as_str())??;
                let dur = tz_str_to_duration(&tz_str);
                Some(Term::Literal(Literal::new_typed_literal(
                    dur,
                    xsd_nn("dayTimeDuration"),
                )))
            }
            Function::Tz => {
                let l = lit(0)?;
                // Returns Some(None) if no timezone (empty string result)
                let tz_opt = dt_timezone_str(l.value(), l.datatype().as_str())?;
                let tz_str = tz_opt.unwrap_or_default();
                Some(Term::Literal(Literal::new_simple_literal(tz_str)))
            }
            Function::Now => Some(Term::Literal(Literal::new_typed_literal(
                current_datetime_string(),
                xsd_nn("dateTime"),
            ))),

            // ── Hash functions ────────────────────────────────────────────
            Function::Md5 => {
                let s = lit(0)?.value().to_string();
                let hash = {
                    use md5::{Digest, Md5};
                    Md5::digest(s.as_bytes())
                };
                Some(Term::Literal(Literal::new_simple_literal(bytes_to_hex(
                    &hash,
                ))))
            }
            Function::Sha1 => {
                let s = lit(0)?.value().to_string();
                let hash = {
                    use sha1::{Digest, Sha1};
                    Sha1::digest(s.as_bytes())
                };
                Some(Term::Literal(Literal::new_simple_literal(bytes_to_hex(
                    &hash,
                ))))
            }
            Function::Sha256 => {
                let s = lit(0)?.value().to_string();
                let hash = {
                    use sha2::{Digest, Sha256};
                    Sha256::digest(s.as_bytes())
                };
                Some(Term::Literal(Literal::new_simple_literal(bytes_to_hex(
                    &hash,
                ))))
            }
            Function::Sha384 => {
                let s = lit(0)?.value().to_string();
                let hash = {
                    use sha2::{Digest, Sha384};
                    Sha384::digest(s.as_bytes())
                };
                Some(Term::Literal(Literal::new_simple_literal(bytes_to_hex(
                    &hash,
                ))))
            }
            Function::Sha512 => {
                let s = lit(0)?.value().to_string();
                let hash = {
                    use sha2::{Digest, Sha512};
                    Sha512::digest(s.as_bytes())
                };
                Some(Term::Literal(Literal::new_simple_literal(bytes_to_hex(
                    &hash,
                ))))
            }

            // ── Custom functions (XSD casts + user extensions) ────────────
            Function::Custom(nn) => {
                if let Some(mode) = text_function_local(nn.as_str()) {
                    // `text:query(?var, "search string")` /
                    // `text:contains(?var, "term")`: arg 0 is the (already
                    // solution-bound) object term being tested, arg 1 is the
                    // search string. This is the scalar-fallback path (used
                    // when the pushdown in `eval_pattern`'s `GP::Filter` arm
                    // didn't apply) — it re-runs the search per row and
                    // checks whether `arg(0)`'s TermId is among the hits.
                    self.eval_text_scalar(arg(0).as_ref(), str_val(1).as_deref(), mode)
                } else if let Some(term_fn) = geosparql_function_local(nn) {
                    let call_args: Vec<Term> = ev.iter().cloned().collect::<Option<Vec<_>>>()?;
                    term_fn(&call_args)
                } else {
                    eval_xsd_cast(nn.as_str(), arg(0).as_ref())
                }
            }

            // ── UUID / STRUUID ────────────────────────────────────────────
            Function::Uuid => {
                let id = uuid::Uuid::new_v4();
                Some(Term::NamedNode(NamedNode::new_unchecked(format!(
                    "urn:uuid:{id}"
                ))))
            }
            Function::StrUuid => {
                let id = uuid::Uuid::new_v4();
                Some(Term::Literal(Literal::new_simple_literal(id.to_string())))
            }

            // ── RAND ───────────────────────────────────────────────────────
            // SPARQL 1.1 §17.4.2.2: returns a pseudo-random xsd:double in the
            // range [0.0, 1.0). rand::random::<f64>() samples uniformly from
            // exactly that half-open range, so no extra scaling is needed.
            Function::Rand => {
                let r: f64 = rand::random::<f64>();
                Some(Term::Literal(Literal::new_typed_literal(
                    format_xsd_double(r),
                    NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#double"),
                )))
            }

            // ── RDF 1.2: quoted triple terms ───────────────────────────────
            Function::Triple => {
                let s = arg(0)?;
                let p = arg(1)?;
                let o = arg(2)?;
                oxrdf::Triple::from_terms(s, p, o)
                    .ok()
                    .map(|t| Term::Triple(Box::new(t)))
            }
            Function::Subject => match arg(0)? {
                Term::Triple(t) => Some(Term::from(t.subject.clone())),
                _ => None,
            },
            Function::Predicate => match arg(0)? {
                Term::Triple(t) => Some(Term::NamedNode(t.predicate.clone())),
                _ => None,
            },
            Function::Object => match arg(0)? {
                Term::Triple(t) => Some(t.object.clone()),
                _ => None,
            },
            Function::IsTriple => Some(bool_term(matches!(arg(0)?, Term::Triple(_)))),

            // ── RDF 1.2: directional language-tagged strings ───────────────
            Function::HasLang => {
                let ok = matches!(arg(0)?, Term::Literal(l) if l.language().is_some());
                Some(bool_term(ok))
            }
            Function::HasLangDir => {
                let ok = matches!(arg(0)?, Term::Literal(l) if l.direction().is_some());
                Some(bool_term(ok))
            }

            Function::LangDir => {
                let l = lit(0)?;
                Some(Term::Literal(Literal::new_simple_literal(
                    l.direction().map(|d| d.to_string()).unwrap_or_default(),
                )))
            }
            Function::StrLangDir => {
                let l0 = lit(0)?;
                // First arg must be plain/xsd:string (no language tag, no other typed literals)
                if l0.language().is_some() {
                    return None;
                }
                if l0.datatype().as_str().strip_prefix(XSD) != Some("string") {
                    return None;
                }
                let lex = l0.value().to_string();
                let lang = lit(1)?.value().to_lowercase();
                if lang.is_empty() {
                    return None;
                }
                let dir = match lit(2)?.value() {
                    "ltr" => BaseDirection::Ltr,
                    "rtl" => BaseDirection::Rtl,
                    _ => return None,
                };
                Some(Term::Literal(
                    Literal::new_directional_language_tagged_literal_unchecked(lex, lang, dir),
                ))
            }

            // ── Catch-all: unknown custom extensions ──────────────────────
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// Scalar-fallback evaluation of `text:query`/`text:contains`: re-run
    /// the search and check whether `bound_term`'s interned `TermId` is
    /// among the hits. Used by `eval_fn`'s `Function::Custom` arm whenever
    /// the pushdown path (`try_text_search_pushdown`, invoked from
    /// `eval_pattern`'s `GP::Filter` arm) didn't apply.
    ///
    /// Returns `None` (SPARQL type error / unbound) if no `TextSearch`
    /// backend is configured (`self.options.text_search`), `bound_term`
    /// isn't a literal, or the search string argument is missing/non-string
    /// -- otherwise `Some(bool_term(...))`.
    fn eval_text_scalar(
        &self,
        bound_term: Option<&Term>,
        query: Option<&str>,
        mode: TextFn,
    ) -> Option<Term> {
        let ts = self.options.text_search.as_ref()?;
        let bound_term = bound_term?;
        let query = query?;
        if !matches!(bound_term, Term::Literal(_)) {
            return Some(bool_term(false));
        }
        let target_id = self
            .dataset
            .lftj_intern_term(bound_term, &GraphSelector::Union)?;
        let effective = mode.effective_query(query);
        let hits = ts.search(&effective, None, DEFAULT_TEXT_SEARCH_LIMIT);
        Some(bool_term(hits.iter().any(|m| m.object_id == target_id)))
    }

    /// Real pushdown: if `expr` is a `text:query`/`text:contains(?var, "...")`
    /// call, and `?var` is bound by `inner` as the object of some triple
    /// pattern in a top-level BGP, run the search *first* to get a
    /// candidate `TermId` set, and evaluate `inner` once per candidate
    /// binding rather than materializing the full unconstrained join and
    /// filtering afterward. Returns `None` if no pushdown opportunity is
    /// found (caller falls back to plain `eval_pattern` + scalar filtering).
    fn try_text_search_pushdown(
        &self,
        expr: &Expression,
        inner: &GP,
        active_graph: &GraphSelector,
    ) -> Option<Result<Solutions>> {
        let ts = self.options.text_search.as_ref()?;
        let (var, query_str, mode) = text_search_call(expr)?;
        let GP::Bgp { patterns } = inner else {
            return None;
        };
        let predicate_id = patterns.iter().find_map(|tp| match &tp.object {
            TermPattern::Variable(v) if v == &var => match &tp.predicate {
                NamedNodePattern::NamedNode(p) => self
                    .dataset
                    .lftj_intern_term(&Term::NamedNode(p.clone()), active_graph),
                NamedNodePattern::Variable(_) => None,
            },
            _ => None,
        });
        let object_var_bound = patterns
            .iter()
            .any(|tp| matches!(&tp.object, TermPattern::Variable(v) if v == &var));
        if !object_var_bound {
            return None;
        }

        let effective_query = mode.effective_query(&query_str);
        let hits = ts.search(&effective_query, predicate_id, DEFAULT_TEXT_SEARCH_LIMIT);
        if hits.is_empty() {
            return Some(Ok(Vec::new()));
        }
        let mut seen_ids = std::collections::HashSet::new();
        let mut result = Vec::new();
        for hit in &hits {
            if !seen_ids.insert(hit.object_id) {
                continue;
            }
            let Some(term) = self.dataset.lftj_decode_term(hit.object_id) else {
                continue;
            };
            match self.eval_bgp_with_bound_var(patterns, &var, term.as_ref(), active_graph) {
                Ok(sols) => result.extend(sols),
                Err(e) => return Some(Err(e)),
            }
        }
        Some(Ok(result))
    }

    /// Evaluate `patterns` as a BGP with every occurrence of `TermPattern::
    /// Variable(var)` replaced by the ground `term` first -- used by
    /// `try_text_search_pushdown` to evaluate one candidate binding at a
    /// time.
    ///
    /// This is the real pushdown: rather than evaluating the full
    /// unconstrained BGP once per search hit and then joining against a
    /// single-row seed (`O(hits × full-BGP)`), the ground term is
    /// substituted directly into the pattern so `eval_bgp` (and therefore
    /// LFTJ, when applicable) sees a BGP that is already constrained by
    /// `var`'s binding -- e.g. `?s <p> ?o` with `?o := "fox"` substituted
    /// becomes `?s <p> "fox"`, which LFTJ can join in O(deg) rather than
    /// O(|graph|). `var` is re-inserted into every resulting solution
    /// afterward, since a ground `TermPattern` does not itself bind
    /// anything in the returned `Solution`s.
    ///
    /// `var` may also occur elsewhere in `patterns` in a position that
    /// substitution does not touch -- e.g. as a *predicate*
    /// (`NamedNodePattern::Variable`), which only ever unifies with an IRI,
    /// never with the literal `term` a text-search hit produces. In that
    /// case `eval_bgp` on the substituted patterns can still bind `var`
    /// itself (to some unrelated IRI it matched as a predicate elsewhere in
    /// the BGP), which would be *incompatible* with the hit's binding. Such
    /// rows must be dropped rather than kept, so any solution that already
    /// binds `var` is checked for compatibility with `term` and discarded
    /// on mismatch instead of being silently accepted.
    fn eval_bgp_with_bound_var(
        &self,
        patterns: &[TriplePattern],
        var: &Variable,
        term: &Term,
        active_graph: &GraphSelector,
    ) -> Result<Solutions> {
        let substituted: Vec<TriplePattern> = patterns
            .iter()
            .map(|tp| substitute_var_in_triple_pattern(tp, var, term))
            .collect();
        let mut sols = self.eval_bgp(&substituted, active_graph)?;
        sols.retain_mut(|sol| match sol.get(var) {
            None => {
                sol.insert(var.clone(), term.clone()); // Term -> Arc via Into
                true
            }
            Some(bound) => bound == term,
        });
        Ok(sols)
    }

    // =========================================================================
    // Property path evaluation
    // =========================================================================

    fn eval_path(
        &self,
        subject: &TermPattern,
        path: &PPE,
        object: &TermPattern,
        active_graph: &GraphSelector,
    ) -> Result<Solutions> {
        // Endpoint-aware Ring fast path: for `p+`/`p*` where at least one of
        // subject/object is a bound constant, use bidirectional (both bound)
        // or single-direction backward (target-only bound) BFS instead of
        // materializing the whole transitive closure.
        if let Some(result) = self.try_bound_path_ring(subject, path, object, active_graph) {
            let pairs = result?;
            let mut sols = Vec::new();
            for (s_term, o_term) in pairs {
                let mut sol = Solution::new();
                let ok_s = match_term_pattern(subject, &s_term, &mut sol);
                let ok_o = match_term_pattern(object, &o_term, &mut sol);
                if ok_s && ok_o {
                    sols.push(sol);
                }
            }
            let mut seen = std::collections::HashSet::new();
            sols.retain(|sol| seen.insert(solution_key(sol)));
            return Ok(sols);
        }

        // Product-automaton fast path for composed RPQs (sequence/alternative/
        // negated-property-set/nested Kleene stars) — evaluates the whole
        // expression in a single (node, state) BFS instead of materializing a
        // full Vec<(Arc<Term>, Arc<Term>)> per operator via `path_pairs`.
        if let Some(result) = self.try_rpq_product_automaton(subject, path, object, active_graph) {
            let pairs = result?;
            let mut sols = Vec::new();
            for (s_term, o_term) in pairs {
                let mut sol = Solution::new();
                let ok_s = match_term_pattern(subject, &s_term, &mut sol);
                let ok_o = match_term_pattern(object, &o_term, &mut sol);
                if ok_s && ok_o {
                    sols.push(sol);
                }
            }
            let mut seen = std::collections::HashSet::new();
            sols.retain(|sol| seen.insert(solution_key(sol)));
            return Ok(sols);
        }

        let mut pairs = self.path_pairs(path, active_graph)?;

        // For ZeroOrMore (*) and ZeroOrOne (?), identity pairs (x, x) are always
        // valid regardless of the dataset contents.  On an empty dataset (or when a
        // constant endpoint is not otherwise reachable), we must still emit the
        // identity pair for every constant term in the subject/object patterns.
        if matches!(path, PPE::ZeroOrMore(_) | PPE::ZeroOrOne(_)) {
            let mut extra_nodes: Vec<Term> = Vec::new();
            if let TermPattern::NamedNode(n) = subject {
                extra_nodes.push(Term::NamedNode(n.clone()));
            }
            if let TermPattern::Literal(l) = subject {
                extra_nodes.push(Term::Literal(l.clone()));
            }
            if let TermPattern::NamedNode(n) = object {
                extra_nodes.push(Term::NamedNode(n.clone()));
            }
            if let TermPattern::Literal(l) = object {
                extra_nodes.push(Term::Literal(l.clone()));
            }
            let existing: std::collections::HashSet<String> =
                pairs.iter().map(|(s, o)| format!("{s}\x00{o}")).collect();
            for node in extra_nodes {
                let k = format!("{node}\x00{node}");
                if !existing.contains(&k) {
                    pairs.push((Arc::new(node.clone()), Arc::new(node)));
                }
            }
        }

        let mut result = Vec::new();
        for (s_term, o_term) in pairs {
            let mut sol = Solution::new();
            let ok_s = match_term_pattern(subject, s_term.as_ref(), &mut sol);
            let ok_o = match_term_pattern(object, o_term.as_ref(), &mut sol);
            if ok_s && ok_o {
                result.push(sol);
            }
        }
        // Deduplicate by solution key
        let mut seen = std::collections::HashSet::new();
        result.retain(|sol| seen.insert(solution_key(sol)));
        Ok(result)
    }

    /// Endpoint-aware Ring fast path for `p+`/`p*` where subject and/or
    /// object is a bound constant term. Returns `None` if not applicable
    /// (falls through to the generic `path_pairs` path).
    fn try_bound_path_ring(
        &self,
        subject: &TermPattern,
        path: &PPE,
        object: &TermPattern,
        ag: &GraphSelector,
    ) -> Option<Result<Vec<(Arc<Term>, Arc<Term>)>>> {
        let (pred, include_identity) = match path {
            PPE::OneOrMore(inner) => match inner.as_ref() {
                PPE::NamedNode(p) => (p, false),
                _ => return None,
            },
            PPE::ZeroOrMore(inner) => match inner.as_ref() {
                PPE::NamedNode(p) => (p, true),
                _ => return None,
            },
            _ => return None,
        };
        if !self.dataset.supports_lftj() || self.dataset.lftj_has_delta() {
            return None;
        }
        match ag {
            GraphSelector::Default | GraphSelector::Named(_) => {}
            _ => return None,
        }

        let subj_bound = match subject {
            TermPattern::NamedNode(n) => Some(Term::NamedNode(n.clone())),
            TermPattern::Literal(l) => Some(Term::Literal(l.clone())),
            _ => None,
        };
        let obj_bound = match object {
            TermPattern::NamedNode(n) => Some(Term::NamedNode(n.clone())),
            TermPattern::Literal(l) => Some(Term::Literal(l.clone())),
            _ => None,
        };
        // If neither endpoint is bound, there's nothing to specialize.
        if subj_bound.is_none() && obj_bound.is_none() {
            return None;
        }

        let p_id = self
            .dataset
            .lftj_intern_term(&Term::NamedNode(pred.clone()), ag)?;
        let source_id = match &subj_bound {
            Some(t) => Some(self.dataset.lftj_intern_term(t, ag)?),
            None => None,
        };
        let target_id = match &obj_bound {
            Some(t) => Some(self.dataset.lftj_intern_term(t, ag)?),
            None => None,
        };

        crate::path::ring_bfs_transitive_bound_cancellable(
            self.dataset,
            p_id,
            source_id,
            target_id,
            include_identity,
            ag,
            self.options.cancellation.as_ref(),
        )
    }

    /// Product-automaton fast path for composed property-path expressions
    /// (`Sequence`, `Alternative`, `NegatedPropertySet`, nested Kleene stars,
    /// `Reverse` over anything other than a bare `NamedNode`). Returns `None`
    /// for the simple single-predicate shapes already handled by
    /// `try_bound_path_ring` / `path_transitive_ring` (falls through to
    /// `path_pairs` for those, and for any dataset that doesn't support LFTJ).
    fn try_rpq_product_automaton(
        &self,
        subject: &TermPattern,
        path: &PPE,
        object: &TermPattern,
        ag: &GraphSelector,
    ) -> Option<Result<Vec<(Arc<Term>, Arc<Term>)>>> {
        if !self.dataset.supports_lftj() || self.dataset.lftj_has_delta() {
            return None;
        }
        match ag {
            GraphSelector::Default | GraphSelector::Named(_) => {}
            _ => return None,
        }

        let subj_bound = match subject {
            TermPattern::NamedNode(n) => Some(Term::NamedNode(n.clone())),
            TermPattern::Literal(l) => Some(Term::Literal(l.clone())),
            _ => None,
        };
        let obj_bound = match object {
            TermPattern::NamedNode(n) => Some(Term::NamedNode(n.clone())),
            TermPattern::Literal(l) => Some(Term::Literal(l.clone())),
            _ => None,
        };

        let source_id = match &subj_bound {
            Some(t) => Some(self.dataset.lftj_intern_term(t, ag)?),
            None => None,
        };
        let target_id = match &obj_bound {
            Some(t) => Some(self.dataset.lftj_intern_term(t, ag)?),
            None => None,
        };

        crate::path::ring_eval_rpq_cancellable(
            self.dataset,
            path,
            source_id,
            target_id,
            ag,
            self.options.cancellation.as_ref(),
        )
    }

    /// Enumerate all (subject, object) pairs reachable via `path` in `ag`.
    fn path_pairs(&self, path: &PPE, ag: &GraphSelector) -> Result<Vec<(Arc<Term>, Arc<Term>)>> {
        match path {
            PPE::NamedNode(p) => {
                // Ring fast path: O(edges_for_p) via lftj_join_scan vs O(total_triples)
                // via find_quads. Reference: "BWT Indexes for Optimal Joins" (Arroyuelo/Navarro).
                if self.dataset.supports_lftj()
                    && !self.dataset.lftj_has_delta()
                    && matches!(ag, GraphSelector::Default | GraphSelector::Named(_))
                    && let Some(result) = crate::path::ring_pairs_for_pred(self.dataset, p, ag)
                {
                    return result;
                }
                // Fallback: generic find_quads scan.
                let qp = QuadPattern {
                    subject: PatternTerm::Variable,
                    predicate: PatternTerm::bound(Term::NamedNode(p.clone())),
                    object: PatternTerm::Variable,
                    graph: ag.clone(),
                };
                let mut pairs = Vec::new();
                for qr in self.dataset.find_quads(&qp)? {
                    let q = qr?;
                    pairs.push((q.subject, q.object));
                }
                Ok(pairs)
            }
            PPE::Reverse(inner) => {
                let pairs = self.path_pairs(inner, ag)?;
                Ok(pairs.into_iter().map(|(s, o)| (o, s)).collect())
            }
            PPE::Sequence(left, right) => {
                let lp = self.path_pairs(left, ag)?;
                let rp = self.path_pairs(right, ag)?;
                // Index right by intermediate-node term content (no Display).
                let mut right_idx: HashMap<Term, Vec<Arc<Term>>> = HashMap::new();
                for (m, o) in rp {
                    right_idx.entry(m.as_ref().clone()).or_default().push(o);
                }
                let mut result = Vec::new();
                let mut seen: std::collections::HashSet<(u64, u64)> =
                    std::collections::HashSet::new();
                fn fp(t: &Term) -> u64 {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut h = DefaultHasher::new();
                    t.hash(&mut h);
                    h.finish()
                }
                for (s, m) in lp {
                    if let Some(os) = right_idx.get(m.as_ref()) {
                        let sh = fp(&s);
                        for o in os {
                            if seen.insert((sh, fp(o))) {
                                result.push((Arc::clone(&s), Arc::clone(o)));
                            }
                        }
                    }
                }
                Ok(result)
            }
            PPE::Alternative(left, right) => {
                let mut pairs = self.path_pairs(left, ag)?;
                pairs.extend(self.path_pairs(right, ag)?);
                let mut seen: std::collections::HashSet<(u64, u64)> =
                    std::collections::HashSet::new();
                fn fp(t: &Term) -> u64 {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut h = DefaultHasher::new();
                    t.hash(&mut h);
                    h.finish()
                }
                pairs.retain(|(s, o)| seen.insert((fp(s), fp(o))));
                Ok(pairs)
            }
            PPE::ZeroOrOne(inner) => {
                let mut pairs = self.path_pairs(inner, ag)?;
                let mut seen: std::collections::HashSet<(u64, u64)> =
                    std::collections::HashSet::new();
                fn fp(t: &Term) -> u64 {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut h = DefaultHasher::new();
                    t.hash(&mut h);
                    h.finish()
                }
                for (s, o) in &pairs {
                    seen.insert((fp(s), fp(o)));
                }
                // Add (x, x) identity for all graph nodes
                for node in self.all_terms(ag)? {
                    let arc = Arc::new(node);
                    let h = fp(&arc);
                    if seen.insert((h, h)) {
                        pairs.push((Arc::clone(&arc), arc));
                    }
                }
                Ok(pairs)
            }
            PPE::ZeroOrMore(inner) => self.path_transitive(inner, ag, true),
            PPE::OneOrMore(inner) => self.path_transitive(inner, ag, false),
            PPE::NegatedPropertySet(preds) => {
                let qp = QuadPattern {
                    subject: PatternTerm::Variable,
                    predicate: PatternTerm::Variable,
                    object: PatternTerm::Variable,
                    graph: ag.clone(),
                };
                let pred_set: std::collections::HashSet<String> =
                    preds.iter().map(|n| n.as_str().to_string()).collect();
                let mut pairs = Vec::new();
                let mut seen: std::collections::HashSet<(u64, u64)> =
                    std::collections::HashSet::new();
                fn fp(t: &Term) -> u64 {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut h = DefaultHasher::new();
                    t.hash(&mut h);
                    h.finish()
                }
                for qr in self.dataset.find_quads(&qp)? {
                    let q = qr?;
                    let skip = matches!(q.predicate.as_ref(),
                        Term::NamedNode(p) if pred_set.contains(p.as_str()));
                    if !skip && seen.insert((fp(&q.subject), fp(&q.object))) {
                        pairs.push((q.subject, q.object));
                    }
                }
                Ok(pairs)
            }
            #[allow(unreachable_patterns)]
            _ => Ok(vec![]),
        }
    }

    /// BFS transitive closure.
    ///
    /// `include_identity = true`  → ZeroOrMore (`*`): adds (x, x) for every node.
    /// `include_identity = false` → OneOrMore (`+`): does NOT add (x, x).
    fn path_transitive(
        &self,
        path: &PPE,
        ag: &GraphSelector,
        include_identity: bool,
    ) -> Result<Vec<(Arc<Term>, Arc<Term>)>> {
        // Ring-accelerated BFS for simple predicate paths.
        // Reference: "BWT Indexes for Optimal Joins" (OASIcs, Arroyuelo/Navarro et al.)
        if let PPE::NamedNode(pred) = path
            && let Some(result) = self.path_transitive_ring(pred, ag, include_identity)
        {
            return result;
        }
        let direct = self.path_pairs(path, ag)?;

        // Forward adjacency keyed by Term content (Hash+Eq) — no Display
        // formatting / String allocations on the BFS hot path.
        let mut adj: HashMap<Term, Vec<Arc<Term>>> = HashMap::new();
        let mut term_map: HashMap<Term, Arc<Term>> = HashMap::new();

        for (s, o) in &direct {
            adj.entry(s.as_ref().clone())
                .or_default()
                .push(Arc::clone(o));
            term_map
                .entry(s.as_ref().clone())
                .or_insert_with(|| Arc::clone(s));
            term_map
                .entry(o.as_ref().clone())
                .or_insert_with(|| Arc::clone(o));
        }

        // For ZeroOrMore we start from ALL nodes; for OneOrMore just path endpoints.
        let start_nodes: Vec<Arc<Term>> = if include_identity {
            self.all_terms(ag)?.into_iter().map(Arc::new).collect()
        } else {
            term_map.values().cloned().collect()
        };

        let mut result: Vec<(Arc<Term>, Arc<Term>)> = Vec::new();
        // Pair dedup by content hash of (start, nbr) — avoids format! strings.
        let mut global_seen: std::collections::HashSet<(u64, u64)> =
            std::collections::HashSet::new();

        fn term_fp(t: &Term) -> u64 {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            t.hash(&mut h);
            h.finish()
        }

        for start in &start_nodes {
            let start_h = term_fp(start);

            // Identity pair
            if include_identity && global_seen.insert((start_h, start_h)) {
                result.push((Arc::clone(start), Arc::clone(start)));
            }

            // BFS over Arc terms, tracking visited by content fingerprint.
            let mut visited: std::collections::HashSet<u64> = std::collections::HashSet::new();
            visited.insert(start_h);
            let mut queue: std::collections::VecDeque<Arc<Term>> =
                std::collections::VecDeque::new();
            queue.push_back(Arc::clone(start));

            while let Some(curr) = queue.pop_front() {
                if let Some(neighbors) = adj.get(curr.as_ref()) {
                    for nbr in neighbors {
                        let nh = term_fp(nbr);
                        if visited.insert(nh) {
                            queue.push_back(Arc::clone(nbr));
                            if global_seen.insert((start_h, nh)) {
                                result.push((Arc::clone(start), Arc::clone(nbr)));
                            }
                        }
                    }
                }
            }
        }
        Ok(result)
    }

    /// Ring-accelerated transitive closure for `PPE::NamedNode(pred)` paths.
    ///
    /// Uses `lftj_join_scan` for lazy neighbor enumeration (O(deg) per step) instead
    /// of materializing all edges upfront via `find_quads`.
    ///
    /// Reference: "BWT Indexes for Optimal Joins" (OASIcs, Arroyuelo/Navarro et al.)
    fn path_transitive_ring(
        &self,
        pred: &NamedNode,
        ag: &GraphSelector,
        include_identity: bool,
    ) -> Option<Result<Vec<(Arc<Term>, Arc<Term>)>>> {
        if !self.dataset.supports_lftj() || self.dataset.lftj_has_delta() {
            return None;
        }
        match ag {
            GraphSelector::Default | GraphSelector::Named(_) => {}
            _ => return None,
        }
        let p_id = self
            .dataset
            .lftj_intern_term(&Term::NamedNode(pred.clone()), ag)?;
        let start_ids: Vec<u64> = if include_identity {
            match crate::path::ring_all_node_ids(self.dataset, ag)? {
                Ok(ids) => ids,
                Err(e) => return Some(Err(e)),
            }
        } else {
            match crate::path::ring_subjects_for_pred(self.dataset, p_id, ag)? {
                Ok(ids) => ids,
                Err(e) => return Some(Err(e)),
            }
        };
        crate::path::ring_bfs_transitive_cancellable(
            self.dataset,
            p_id,
            &start_ids,
            include_identity,
            ag,
            self.options.cancellation.as_ref(),
        )
    }

    /// Collect all distinct subjects and objects in the active graph.
    fn all_terms(&self, ag: &GraphSelector) -> Result<Vec<Term>> {
        let qp = QuadPattern {
            subject: PatternTerm::Variable,
            predicate: PatternTerm::Variable,
            object: PatternTerm::Variable,
            graph: ag.clone(),
        };
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for qr in self.dataset.find_quads(&qp)? {
            let q = qr?;
            for t in [q.subject, q.object] {
                if seen.insert(t.to_string()) {
                    result.push(Arc::unwrap_or_clone(t));
                }
            }
        }
        Ok(result)
    }
}

// =============================================================================
// Free helper functions
// =============================================================================

// ── Join helpers ──────────────────────────────────────────────────────────────

fn join_solutions(left: Solutions, right: Solutions) -> Solutions {
    let mut result = Vec::new();
    for ls in &left {
        for rs in &right {
            if let Some(m) = ls.merge_compatible(rs) {
                result.push(m);
            }
        }
    }
    result
}

fn minus_compatible(ls: &Solution, rs: &Solution) -> bool {
    let mut has_shared = false;
    for (var, lt) in ls.iter() {
        if let Some(rt) = rs.get(var) {
            has_shared = true;
            if lt != rt {
                return false;
            }
        }
    }
    has_shared
}

// ── Deduplication key ─────────────────────────────────────────────────────────

/// Dedup key for DISTINCT / path pair sets: variable name + term content.
///
/// Avoids the previous `t.to_string()` path (which allocated a Display form
/// per binding per row). `Term` implements `Hash + Eq`, so we hash the
/// structured term directly. Variable names are interned as owned `String`
/// once per distinct variable occurrence in the key (typically few vars).
fn solution_key(sol: &Solution) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut pairs: Vec<(&str, &Term)> = sol.iter().map(|(v, t)| (v.as_str(), t)).collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let mut h = DefaultHasher::new();
    pairs.len().hash(&mut h);
    for (v, t) in pairs {
        v.hash(&mut h);
        t.hash(&mut h);
    }
    h.finish()
}

// ── BGP term helpers ──────────────────────────────────────────────────────────

fn term_pattern_with_sol(tp: &TermPattern, sol: &Solution) -> (PatternTerm, Option<Variable>) {
    match tp {
        TermPattern::NamedNode(n) => (PatternTerm::bound(Term::NamedNode(n.clone())), None),
        TermPattern::Literal(l) => (PatternTerm::bound(Term::Literal(l.clone())), None),
        TermPattern::BlankNode(b) => {
            let var = Variable::new_unchecked(format!("__bn_{}", b.as_str()));
            if let Some(t) = sol.get_arc(&var) {
                (PatternTerm::Bound(t), None)
            } else {
                (PatternTerm::Variable, Some(var))
            }
        }
        TermPattern::Variable(v) => {
            if let Some(t) = sol.get_arc(v) {
                (PatternTerm::Bound(t), None)
            } else {
                (PatternTerm::Variable, Some(v.clone()))
            }
        }
        TermPattern::Triple(_) => {
            // Quoted-triple pattern in subject/object: use wildcard for the
            // storage scan; structural binding is applied post-retrieval in
            // eval_bgp via bind_triple_pattern().
            (PatternTerm::Variable, None)
        }
    }
}

fn nn_pattern_with_sol(nnp: &NamedNodePattern, sol: &Solution) -> (PatternTerm, Option<Variable>) {
    match nnp {
        NamedNodePattern::NamedNode(n) => (PatternTerm::bound(Term::NamedNode(n.clone())), None),
        NamedNodePattern::Variable(v) => {
            if let Some(t) = sol.get_arc(v) {
                (PatternTerm::Bound(t), None)
            } else {
                (PatternTerm::Variable, Some(v.clone()))
            }
        }
    }
}

/// Replace every occurrence of `TermPattern::Variable(v) if v == var` (in
/// subject or object position -- predicate position uses `NamedNodePattern`,
/// which can never hold a *literal* full-text search result, so it's left
/// untouched) with `term` as a ground `TermPattern`, for use by
/// [`Evaluator::eval_bgp_with_bound_var`]'s pushdown substitution. Leaves
/// every other part of `tp` unchanged (including nested `TermPattern::
/// Triple` patterns' own variable occurrences, which are also substituted
/// recursively via `substitute_var_in_term_pattern`).
fn substitute_var_in_triple_pattern(
    tp: &TriplePattern,
    var: &Variable,
    term: &Term,
) -> TriplePattern {
    TriplePattern {
        subject: substitute_var_in_term_pattern(&tp.subject, var, term),
        predicate: tp.predicate.clone(),
        object: substitute_var_in_term_pattern(&tp.object, var, term),
    }
}

fn substitute_var_in_term_pattern(tp: &TermPattern, var: &Variable, term: &Term) -> TermPattern {
    match tp {
        TermPattern::Variable(v) if v == var => TermPattern::from(term.clone()),
        TermPattern::Triple(inner) => TermPattern::Triple(Box::new(TriplePattern {
            subject: substitute_var_in_term_pattern(&inner.subject, var, term),
            predicate: inner.predicate.clone(),
            object: substitute_var_in_term_pattern(&inner.object, var, term),
        })),
        other => other.clone(),
    }
}

fn bind_var_arc(sol: &mut Solution, var: &Option<Variable>, value: &Arc<Term>) -> bool {
    match var {
        None => true,
        Some(v) => {
            if let Some(existing) = sol.get(v) {
                existing == value.as_ref()
            } else {
                sol.insert(v.clone(), Arc::clone(value));
                true
            }
        }
    }
}

// ── RDF-star BGP helpers ──────────────────────────────────────────────────────

/// Structurally match a quoted-triple pattern `<< s p o >>` against `term`.
///
/// The storage scan uses a wildcard for the subject/object when the pattern
/// contains `TermPattern::Triple`; this function is called post-retrieval to
/// bind any inner variables and to reject triples that don't structurally match.
fn bind_triple_pattern(sol: &mut Solution, inner: &TriplePattern, term: &Term) -> bool {
    let t = match term {
        Term::Triple(t) => t.as_ref(),
        _ => return false,
    };
    let s_term = Term::from(t.subject.clone());
    let p_term = Term::NamedNode(t.predicate.clone());
    let o_term = t.object.clone();
    bind_term_pattern(sol, &inner.subject, &s_term)
        && bind_nn_pattern(sol, &inner.predicate, &p_term)
        && bind_term_pattern(sol, &inner.object, &o_term)
}

fn bind_term_pattern(sol: &mut Solution, tp: &TermPattern, term: &Term) -> bool {
    match tp {
        TermPattern::Variable(v) => {
            if let Some(existing) = sol.get(v) {
                existing == term
            } else {
                sol.insert(v.clone(), term.clone());
                true
            }
        }
        TermPattern::NamedNode(n) => *term == Term::NamedNode(n.clone()),
        TermPattern::Literal(l) => *term == Term::Literal(l.clone()),
        TermPattern::BlankNode(b) => {
            let vname = Variable::new_unchecked(format!("__bn_{}", b.as_str()));
            if let Some(existing) = sol.get(&vname) {
                existing == term
            } else {
                sol.insert(vname, term.clone());
                true
            }
        }
        TermPattern::Triple(inner_tp) => bind_triple_pattern(sol, inner_tp, term),
        #[allow(unreachable_patterns)]
        _ => false,
    }
}

fn bind_nn_pattern(sol: &mut Solution, nnp: &NamedNodePattern, term: &Term) -> bool {
    match nnp {
        NamedNodePattern::NamedNode(n) => *term == Term::NamedNode(n.clone()),
        NamedNodePattern::Variable(v) => {
            if let Some(existing) = sol.get(v) {
                existing == term
            } else {
                sol.insert(v.clone(), term.clone());
                true
            }
        }
    }
}

// ── Property-path term matching ───────────────────────────────────────────────

/// Match `term` against `tp`, binding variables in `sol`.
/// Returns false if the pattern is a constant that doesn't equal `term`.
fn match_term_pattern(tp: &TermPattern, term: &Term, sol: &mut Solution) -> bool {
    match tp {
        TermPattern::Variable(v) => {
            if let Some(bound) = sol.get(v) {
                bound == term
            } else {
                sol.insert(v.clone(), term.clone());
                true
            }
        }
        TermPattern::NamedNode(n) => Term::NamedNode(n.clone()) == *term,
        TermPattern::Literal(l) => Term::Literal(l.clone()) == *term,
        TermPattern::BlankNode(b) => {
            // Anonymous variable scoped to this path
            let vname = Variable::new_unchecked(format!("__bnp_{}", b.as_str()));
            if let Some(bound) = sol.get(&vname) {
                bound == term
            } else {
                sol.insert(vname, term.clone());
                true
            }
        }
        #[allow(unreachable_patterns)]
        _ => false,
    }
}

// ── Ordering ──────────────────────────────────────────────────────────────────

fn compare_terms_opt(a: Option<&Term>, b: Option<&Term>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(at), Some(bt)) => compare_terms(at, bt).unwrap_or(std::cmp::Ordering::Equal),
    }
}

fn compare_terms(a: &Term, b: &Term) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering::*;
    match (a, b) {
        (Term::NamedNode(an), Term::NamedNode(bn)) => Some(an.as_str().cmp(bn.as_str())),
        (Term::BlankNode(ab), Term::BlankNode(bb)) => Some(ab.as_str().cmp(bb.as_str())),
        (Term::Literal(al), Term::Literal(bl)) => compare_literals(al, bl),
        // SPARQL ORDER BY term ordering (ascending): unbound < blank nodes <
        // IRIs < RDF literals < RDF 1.2 embedded triple terms. Blank nodes
        // sort *before* IRIs, not after — confirmed against the W3C SPARQL
        // 1.2 `data-order-kind.ttl` fixture's own explicit comment listing
        // this exact order.
        (Term::BlankNode(_), Term::NamedNode(_)) => Some(Less),
        (Term::NamedNode(_), Term::BlankNode(_)) => Some(Greater),
        (Term::BlankNode(_), Term::Literal(_)) => Some(Less),
        (Term::Literal(_), Term::BlankNode(_)) => Some(Greater),
        (Term::NamedNode(_), Term::Literal(_)) => Some(Less),
        (Term::Literal(_), Term::NamedNode(_)) => Some(Greater),

        // RDF 1.2 §... / SPARQL 1.2 ORDER BY: quoted triple terms sort after
        // IRIs, blank nodes, and literals (the SPARQL 1.1 ordering already
        // covers those three), and two quoted triples are ordered
        // component-wise: subject first, then predicate, then object —
        // exactly like comparing a 3-tuple, short-circuiting on the first
        // component that differs.
        (Term::Triple(_), Term::NamedNode(_))
        | (Term::Triple(_), Term::BlankNode(_))
        | (Term::Triple(_), Term::Literal(_)) => Some(Greater),
        (Term::NamedNode(_), Term::Triple(_))
        | (Term::BlankNode(_), Term::Triple(_))
        | (Term::Literal(_), Term::Triple(_)) => Some(Less),
        (Term::Triple(at), Term::Triple(bt)) => {
            let a_s = Term::from(at.subject.clone());
            let b_s = Term::from(bt.subject.clone());
            match compare_terms(&a_s, &b_s) {
                Some(Equal) => {}
                other => return other,
            }
            let a_p = Term::NamedNode(at.predicate.clone());
            let b_p = Term::NamedNode(bt.predicate.clone());
            match compare_terms(&a_p, &b_p) {
                Some(Equal) => {}
                other => return other,
            }
            compare_terms(&at.object, &bt.object)
        }
    }
}

fn compare_literals(a: &Literal, b: &Literal) -> Option<std::cmp::Ordering> {
    if let (Some(an), Some(bn)) = (Numeric::parse(a), Numeric::parse(b)) {
        return an.partial_cmp_xsd(&bn);
    }

    let a_dt = a.datatype();
    let b_dt = b.datatype();
    let a_loc = a_dt.as_str().strip_prefix(XSD);
    let b_loc = b_dt.as_str().strip_prefix(XSD);

    if let (Some(al), Some(bl)) = (a_loc, b_loc) {
        if al == "boolean" && bl == "boolean" {
            let ab = a.value() == "true" || a.value() == "1";
            let bb = b.value() == "true" || b.value() == "1";
            return Some(ab.cmp(&bb));
        }
        if al == "dateTime" && bl == "dateTime" {
            return Some(a.value().cmp(b.value()));
        }
    }
    if a_dt == b_dt {
        return Some(a.value().cmp(b.value()));
    }
    None
}

// ── Effective Boolean Value ───────────────────────────────────────────────────

fn to_ebv(term: &Term) -> Option<bool> {
    match term {
        Term::Literal(l) => {
            // Per SPARQL 1.1 §17.2.2 (Effective Boolean Value): only
            // xsd:boolean, numeric types, and plain/xsd:string literals have
            // a defined EBV. Language-tagged literals (including RDF 1.2
            // directional language-tagged strings, rdf:dirLangString) are
            // NOT xsd:string, so they must produce a type error (None),
            // *not* be coerced via their non-empty-ness.
            if l.language().is_some() {
                return None;
            }
            let dt = l.datatype();
            match dt.as_str().strip_prefix(XSD) {
                Some("boolean") => match l.value() {
                    "true" | "1" => Some(true),
                    "false" | "0" => Some(false),
                    _ => None,
                },
                Some("string") => Some(!l.value().is_empty()),
                None => Some(!l.value().is_empty()),
                Some(loc) if is_numeric_local(loc) => {
                    let n = literal_as_f64(l)?;
                    Some(n != 0.0 && !n.is_nan())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

// ── RDF equality ─────────────────────────────────────────────────────────────

fn rdf_equal(a: &Term, b: &Term) -> Option<bool> {
    match (a, b) {
        (Term::NamedNode(an), Term::NamedNode(bn)) => Some(an == bn),
        (Term::BlankNode(ab), Term::BlankNode(bb)) => Some(ab == bb),
        (Term::Literal(al), Term::Literal(bl)) => literal_equal(al, bl),
        // RDF 1.2: quoted triples compare by structural (component-wise) equality.
        (Term::Triple(at), Term::Triple(bt)) => {
            let s_eq = rdf_equal(
                &Term::from(at.subject.clone()),
                &Term::from(bt.subject.clone()),
            )?;
            if !s_eq {
                return Some(false);
            }
            if at.predicate != bt.predicate {
                return Some(false);
            }
            rdf_equal(&at.object, &bt.object)
        }
        _ => Some(false),
    }
}

fn literal_equal(a: &Literal, b: &Literal) -> Option<bool> {
    if a == b {
        return Some(true);
    }
    if let (Some(an), Some(bn)) = (Numeric::parse(a), Numeric::parse(b)) {
        return an.eq_xsd(&bn);
    }
    if a.datatype() != b.datatype() {
        return None;
    }
    Some(a.value() == b.value())
}

// ── Numeric helpers ───────────────────────────────────────────────────────────

fn is_numeric_local(local: &str) -> bool {
    matches!(
        local,
        "integer"
            | "decimal"
            | "float"
            | "double"
            | "int"
            | "long"
            | "short"
            | "byte"
            | "nonNegativeInteger"
            | "positiveInteger"
            | "negativeInteger"
            | "nonPositiveInteger"
            | "unsignedLong"
            | "unsignedInt"
            | "unsignedShort"
            | "unsignedByte"
    )
}

fn is_numeric_dt(dt_str: &str) -> bool {
    dt_str
        .strip_prefix(XSD)
        .map(is_numeric_local)
        .unwrap_or(false)
}

/// Return true if `l` is a SPARQL string literal: plain/xsd:string OR lang-tagged.
/// Non-string typed literals (xsd:integer, xsd:dateTime, etc.) return false.
fn is_string_literal(l: &Literal) -> bool {
    l.language().is_some() || l.datatype().as_str().strip_prefix(XSD) == Some("string")
}

fn literal_as_f64(l: &Literal) -> Option<f64> {
    let local = l.datatype().as_str().strip_prefix(XSD)?;
    match local {
        "boolean" => match l.value() {
            "true" | "1" => Some(1.0),
            "false" | "0" => Some(0.0),
            _ => None,
        },
        "integer" | "int" | "long" | "short" | "byte" | "nonNegativeInteger"
        | "positiveInteger" | "negativeInteger" | "nonPositiveInteger" | "unsignedLong"
        | "unsignedInt" | "unsignedShort" | "unsignedByte" => {
            l.value().parse::<i64>().ok().map(|n| n as f64)
        }
        "decimal" | "float" | "double" => l.value().parse::<f64>().ok(),
        _ => None,
    }
}

fn term_as_f64(t: &Term) -> Option<f64> {
    match t {
        Term::Literal(l) => literal_as_f64(l),
        _ => None,
    }
}

fn make_integer_literal(n: i64) -> Term {
    Term::Literal(Literal::new_typed_literal(n.to_string(), xsd_nn("integer")))
}

fn make_decimal_literal(f: f64) -> Term {
    Term::Literal(Literal::new_typed_literal(
        format_decimal_f64(f),
        xsd_nn("decimal"),
    ))
}

/// Format a decimal value: whole numbers without trailing `.0`,
/// fractional values with minimum necessary digits.
/// Format a float as XSD canonical double notation (e.g. 32100.0 → "3.21E4").
fn format_xsd_double(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v == f64::INFINITY {
        return "INF".to_string();
    }
    if v == f64::NEG_INFINITY {
        return "-INF".to_string();
    }
    if v == 0.0 {
        return "0.0E0".to_string();
    }

    // Rust {:e} gives shortest scientific notation with lowercase e
    let s = format!("{:e}", v);
    let (mantissa_str, exp_str) = s.split_once('e').unwrap();
    let exp_num: i32 = exp_str.parse().unwrap_or(0);

    // Ensure at least one digit after the decimal point, strip trailing zeros
    let mantissa_canonical = if mantissa_str.contains('.') {
        let trimmed = mantissa_str.trim_end_matches('0');
        if trimmed.ends_with('.') {
            format!("{}0", trimmed)
        } else {
            trimmed.to_string()
        }
    } else {
        format!("{}.0", mantissa_str)
    };

    format!("{}E{}", mantissa_canonical, exp_num)
}

/// Normalize a double literal's lexical form to XSD canonical notation.
/// Non-double terms are returned unchanged.
fn normalize_double_term(t: Term) -> Term {
    if let Term::Literal(ref l) = t {
        let dt = l.datatype();
        if dt.as_str() == "http://www.w3.org/2001/XMLSchema#double"
            && let Ok(d) = XsdDbl::from_str(l.value())
        {
            return Term::Literal(Literal::new_typed_literal(
                format_xsd_double(f64::from(d)),
                dt,
            ));
        }
    }
    t
}

fn format_decimal_f64(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

fn numeric_binop(a: &Term, b: &Term, op: impl Fn(f64, f64) -> f64) -> Option<Term> {
    let al = match a {
        Term::Literal(l) => l,
        _ => return None,
    };
    let bl = match b {
        Term::Literal(l) => l,
        _ => return None,
    };
    let an = Numeric::parse(al)?;
    let bn = Numeric::parse(bl)?;
    let result_type = match (&an, &bn) {
        (Numeric::Integer(_), Numeric::Integer(_)) => "integer",
        (Numeric::Double(_), _) | (_, Numeric::Double(_)) => "double",
        (Numeric::Float(_), _) | (_, Numeric::Float(_)) => "float",
        _ => "decimal",
    };
    let af = an.to_f64()?;
    let bf = bn.to_f64()?;
    let result = op(af, bf);
    Some(make_typed_numeric(result, result_type))
}

/// SPARQL 1.1 §17.3: integer/integer division yields xsd:decimal (not integer).
fn numeric_binop_div(a: &Term, b: &Term) -> Option<Term> {
    let al = match a {
        Term::Literal(l) => l,
        _ => return None,
    };
    let bl = match b {
        Term::Literal(l) => l,
        _ => return None,
    };
    let an = Numeric::parse(al)?;
    let bn = Numeric::parse(bl)?;
    let result_type = match (&an, &bn) {
        (Numeric::Double(_), _) | (_, Numeric::Double(_)) => "double",
        (Numeric::Float(_), _) | (_, Numeric::Float(_)) => "float",
        _ => "decimal", // integer/integer → decimal per SPARQL 1.1 spec
    };
    let af = an.to_f64()?;
    let bf = bn.to_f64()?;
    let result = af / bf;
    if result_type == "decimal" {
        Some(Term::Literal(Literal::new_typed_literal(
            format_decimal_div(result),
            xsd_nn("decimal"),
        )))
    } else {
        Some(make_typed_numeric(result, result_type))
    }
}

/// Format a decimal division result, always ensuring a decimal point is present.
/// e.g. 0.0 → "0.0", 2.0 → "2.0", 0.5 → "0.5", 1.333... → "1.3333333333333333"
fn format_decimal_div(f: f64) -> String {
    let s = format!("{f}");
    // If no decimal point (and not NaN/Inf/scientific notation), append ".0"
    if s.contains('.') || s.contains('e') || s.contains('E') || s.contains('n') || s.contains('i') {
        s
    } else {
        format!("{s}.0")
    }
}

fn numeric_unary(a: &Term, result: f64) -> Option<Term> {
    let l = match a {
        Term::Literal(l) => l,
        _ => return None,
    };
    let local = l.datatype().as_str().strip_prefix(XSD)?;
    Some(make_typed_numeric(result, local))
}

fn make_typed_numeric(value: f64, type_local: &str) -> Term {
    let is_int = matches!(
        type_local,
        "integer"
            | "int"
            | "long"
            | "short"
            | "byte"
            | "nonNegativeInteger"
            | "positiveInteger"
            | "negativeInteger"
            | "nonPositiveInteger"
            | "unsignedLong"
            | "unsignedInt"
            | "unsignedShort"
            | "unsignedByte"
    );
    let lex = if is_int {
        format!("{}", value as i64)
    } else if type_local == "decimal" {
        format_decimal_div(value)
    } else {
        format!("{value}")
    };
    Term::Literal(Literal::new_typed_literal(lex, xsd_nn(type_local)))
}

fn numeric_like_literal(value: f64, orig: &Literal) -> Term {
    let dt: NamedNode = orig.datatype().into();
    let local = dt.as_str().strip_prefix(XSD).unwrap_or("decimal");
    let is_int = matches!(
        local,
        "integer"
            | "int"
            | "long"
            | "short"
            | "byte"
            | "nonNegativeInteger"
            | "positiveInteger"
            | "negativeInteger"
            | "nonPositiveInteger"
            | "unsignedLong"
            | "unsignedInt"
            | "unsignedShort"
            | "unsignedByte"
    );
    let lex = if is_int {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    };
    Term::Literal(Literal::new_typed_literal(lex, dt))
}

// ── Boolean term constructor ──────────────────────────────────────────────────

fn bool_term(b: bool) -> Term {
    Term::Literal(Literal::new_typed_literal(
        if b { "true" } else { "false" },
        xsd_nn("boolean"),
    ))
}

// ── Ground term conversion ────────────────────────────────────────────────────

fn ground_term_to_term(gt: &GroundTerm) -> Term {
    match gt {
        GroundTerm::NamedNode(n) => Term::NamedNode(n.clone()),
        GroundTerm::Literal(l) => Term::Literal(l.clone()),
        // GroundTriple → oxrdf::Triple via the From impl in spargebra.
        // `From<GroundTriple> for Triple` converts subject/predicate/object
        // recursively; nested quoted-triple subjects are handled up to the
        // depth allowed by oxrdf 0.3.3.
        GroundTerm::Triple(t) => Term::Triple(Box::new(oxrdf::Triple::from(*t.clone()))),
    }
}

// ── CONSTRUCT template instantiation ─────────────────────────────────────────

/// Look up (or allocate) the fresh blank node that a CONSTRUCT template's
/// blank-node label `b` maps to for the *current solution*. Per SPARQL 1.1
/// §18.2.4.2 ("Constructing the Output RDF Graph"), each solution gets its
/// own fresh set of blank nodes, but every occurrence of the same template
/// label within one solution's instantiation must map to the *same* fresh
/// blank node. `bnode_map` is therefore reset by the caller once per
/// solution and threaded through every `TriplePattern` of that solution's
/// template instantiation.
fn fresh_bnode_for(b: &BlankNode, bnode_map: &mut HashMap<BlankNode, BlankNode>) -> BlankNode {
    bnode_map.entry(b.clone()).or_default().clone()
}

fn instantiate_triple_pattern(
    tp: &TriplePattern,
    sol: &Solution,
    bnode_map: &mut HashMap<BlankNode, BlankNode>,
) -> Option<oxrdf::Triple> {
    use oxrdf::NamedOrBlankNode;

    let subject: NamedOrBlankNode = match &tp.subject {
        TermPattern::NamedNode(n) => NamedOrBlankNode::NamedNode(n.clone()),
        TermPattern::BlankNode(b) => NamedOrBlankNode::BlankNode(fresh_bnode_for(b, bnode_map)),
        TermPattern::Variable(v) => match sol.get(v)? {
            Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n.clone()),
            Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b.clone()),
            _ => return None,
        },
        #[allow(unreachable_patterns)]
        _ => return None,
    };

    let predicate: NamedNode = match &tp.predicate {
        NamedNodePattern::NamedNode(n) => n.clone(),
        NamedNodePattern::Variable(v) => match sol.get(v)? {
            Term::NamedNode(n) => n.clone(),
            _ => return None,
        },
    };

    let object: Term = match &tp.object {
        TermPattern::NamedNode(n) => Term::NamedNode(n.clone()),
        TermPattern::BlankNode(b) => Term::BlankNode(fresh_bnode_for(b, bnode_map)),
        TermPattern::Literal(l) => Term::Literal(l.clone()),
        TermPattern::Variable(v) => sol.get(v)?.clone(),
        // RDF 1.2: a quoted triple term `<< s p o >>` in a CONSTRUCT
        // template's object position is instantiated recursively, sharing
        // the same per-solution blank-node map so that a template label
        // used both inside and outside the quoted triple still resolves to
        // the same fresh blank node within one solution's instantiation.
        TermPattern::Triple(inner_tp) => Term::Triple(Box::new(instantiate_triple_pattern(
            inner_tp, sol, bnode_map,
        )?)),
        #[allow(unreachable_patterns)]
        _ => return None,
    };

    Some(oxrdf::Triple::new(subject, predicate, object))
}

// ── encodeForURI ──────────────────────────────────────────────────────────────

fn encode_for_uri(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

// ── IRI resolution ────────────────────────────────────────────────────────────

/// Resolve `s` (a possibly-relative IRI reference) against `base` using
/// oxiri's RFC 3986/3987-compliant resolution algorithm.
///
/// Returns `None` if:
/// - `s` fails to parse as a valid IRI reference,
/// - a `base` is given but fails to parse as an absolute IRI,
/// - resolution against `base` fails (e.g. produces an invalid IRI), or
/// - no `base` is given and `s` is not itself already an absolute IRI.
///
/// `oxiri::Iri::resolve` correctly implements RFC 3986 §5.3 relative
/// reference resolution, including dot-segment removal (`../`, `./`),
/// fragment-only (`#frag`) and query-only (`?q`) references, and
/// authority-less bases (e.g. `urn:`) — all cases the previous hand-rolled
/// implementation only partially handled.
fn resolve_iri_against_base(s: &str, base: Option<&str>) -> Option<NamedNode> {
    let iri_ref = IriRef::parse(s.to_string()).ok()?;
    match base {
        Some(base_str) => {
            let base_iri = Iri::parse(base_str.to_string()).ok()?;
            let resolved = base_iri.resolve(&iri_ref).ok()?;
            Some(NamedNode::new_unchecked(resolved.into_inner()))
        }
        None => {
            // No base IRI available: s must already be an absolute IRI.
            let s_owned = iri_ref.into_inner();
            Iri::parse(s_owned)
                .ok()
                .map(|i| NamedNode::new_unchecked(i.into_inner()))
        }
    }
}

// ── String type preservation ──────────────────────────────────────────────────

/// Return a new `Literal` with the same language tag or datatype as `orig`
/// but with a different string value.  Used by UCASE, LCASE, SUBSTR, etc.
fn preserve_string_type(orig: &Literal, new_value: String) -> Literal {
    if let Some(lang) = orig.language() {
        Literal::new_language_tagged_literal_unchecked(new_value, lang)
    } else {
        let dt: NamedNode = orig.datatype().into();
        Literal::new_typed_literal(new_value, dt)
    }
}

// ── Regex helper ──────────────────────────────────────────────────────────────

/// Build a `Regex` from a SPARQL pattern string and flag string.
/// Flags: `i` = case-insensitive, `s` = dot-all, `m` = multi-line, `x` = extended.
fn build_regex(pattern: &str, flags: &str) -> Option<Regex> {
    let mut full = String::new();
    for ch in flags.chars() {
        match ch {
            'i' => full.push_str("(?i)"),
            's' => full.push_str("(?s)"),
            'm' => full.push_str("(?m)"),
            'x' => full.push_str("(?x)"),
            _ => {}
        }
    }
    full.push_str(pattern);
    Regex::new(&full).ok()
}

// ── Hash helper ───────────────────────────────────────────────────────────────

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── Date/time helpers ─────────────────────────────────────────────────────────

fn dt_year(value: &str, dt_iri: &str) -> Option<i64> {
    match dt_iri.strip_prefix(XSD)? {
        "dateTime" => Some(XsdDateTime::from_str(value).ok()?.year()),
        "date" => Some(XsdDate::from_str(value).ok()?.year()),
        _ => None,
    }
}

fn dt_month(value: &str, dt_iri: &str) -> Option<u8> {
    match dt_iri.strip_prefix(XSD)? {
        "dateTime" => Some(XsdDateTime::from_str(value).ok()?.month()),
        "date" => Some(XsdDate::from_str(value).ok()?.month()),
        _ => None,
    }
}

fn dt_day(value: &str, dt_iri: &str) -> Option<u8> {
    match dt_iri.strip_prefix(XSD)? {
        "dateTime" => Some(XsdDateTime::from_str(value).ok()?.day()),
        "date" => Some(XsdDate::from_str(value).ok()?.day()),
        _ => None,
    }
}

fn dt_hour(value: &str, dt_iri: &str) -> Option<u8> {
    match dt_iri.strip_prefix(XSD)? {
        "dateTime" => Some(XsdDateTime::from_str(value).ok()?.hour()),
        "time" => Some(XsdTime::from_str(value).ok()?.hour()),
        _ => None,
    }
}

fn dt_minute(value: &str, dt_iri: &str) -> Option<u8> {
    match dt_iri.strip_prefix(XSD)? {
        "dateTime" => Some(XsdDateTime::from_str(value).ok()?.minute()),
        "time" => Some(XsdTime::from_str(value).ok()?.minute()),
        _ => None,
    }
}

fn dt_second(value: &str, dt_iri: &str) -> Option<XsdDec> {
    match dt_iri.strip_prefix(XSD)? {
        "dateTime" => Some(XsdDateTime::from_str(value).ok()?.second()),
        "time" => Some(XsdTime::from_str(value).ok()?.second()),
        _ => None,
    }
}

/// Returns:
/// - `None` if the literal is not a dateTime/date/time type (type error)
/// - `Some(None)` if it IS a datetime but has no timezone (TZ returns "")
/// - `Some(Some(tz_str))` where `tz_str` is the timezone like `"+05:30"` or `"Z"`
fn dt_timezone_str(value: &str, dt_iri: &str) -> Option<Option<String>> {
    let tz = match dt_iri.strip_prefix(XSD)? {
        "dateTime" => XsdDateTime::from_str(value).ok()?.timezone_offset(),
        "date" => XsdDate::from_str(value).ok()?.timezone_offset(),
        "time" => XsdTime::from_str(value).ok()?.timezone_offset(),
        _ => return None,
    };
    Some(tz.map(|t| t.to_string()))
}

/// Convert a timezone display string ("+HH:MM", "-HH:MM", "Z") to
/// the xsd:dayTimeDuration format required by SPARQL TIMEZONE().
fn tz_str_to_duration(tz: &str) -> String {
    if tz == "Z" || tz == "+00:00" || tz == "-00:00" {
        return "PT0S".to_string();
    }
    let neg = tz.starts_with('-');
    let rest = &tz[1..]; // strip sign
    let mut parts = rest.splitn(2, ':');
    let h: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let m: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    if h == 0 && m == 0 {
        return "PT0S".to_string();
    }
    let sign = if neg { "-" } else { "" };
    match (h, m) {
        (h, 0) => format!("{sign}PT{h}H"),
        (0, m) => format!("{sign}PT{m}M"),
        (h, m) => format!("{sign}PT{h}H{m}M"),
    }
}

/// Return a minimal UTC dateTime string for the current moment.
///
/// `std::time::SystemTime::now()` panics on `wasm32-unknown-unknown` (no OS
/// clock is available there) — use the `Date.now()` ECMAScript API via
/// `js_sys` instead on that target, matching how upstream oxigraph-js's
/// `oxsdatatypes` handles this.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
fn current_datetime_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    unix_secs_to_datetime(secs)
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn current_datetime_string() -> String {
    let secs = (js_sys::Date::now() / 1000.) as i64;
    unix_secs_to_datetime(secs)
}

/// Convert Unix epoch seconds to an ISO-8601 UTC dateTime string.
fn unix_secs_to_datetime(secs: i64) -> String {
    let days = secs / 86400;
    let rem = secs % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (y, mo, d) = jd_to_ymd(days + 2440588); // JD for 1970-01-01 is 2440588
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Standard Julian-Day → Gregorian calendar conversion.
fn jd_to_ymd(jd: i64) -> (i64, i64, i64) {
    let l = jd + 68569;
    let n = (4 * l) / 146097;
    let l = l - (146097 * n + 3) / 4;
    let i = (4000 * (l + 1)) / 1461001;
    let l = l - (1461 * i) / 4 + 31;
    let j = (80 * l) / 2447;
    let d = l - (2447 * j) / 80;
    let l = j / 11;
    let m = j + 2 - 12 * l;
    let y = 100 * (n - 49) + i + l;
    (y, m, d)
}

// ── GeoSPARQL extension functions (`geof:distance`, `sf:intersects`, …) ───────
//
// Opt-in via the `geosparql` cargo feature, which pulls in `spargeo` — a
// partial GeoSPARQL 1.1 implementation exposing 43 `geof:`/`sf:`/`eh:`/
// `rcc8:` functions (distance/area/length/centroid/convexHull/envelope, the
// Simple Features / Egenhofer / RCC8 topological relation families, and
// WKT/GeoJSON literal conversion) as plain `fn(&[Term]) -> Option<Term>`
// pairs keyed by function IRI. Parsed by `spargebra` the same way as the
// full-text extension functions above: `Function::Custom(NamedNode)`.
// Dispatched only from `eval_fn`'s `Function::Custom` arm — GeoSPARQL
// functions are pure (no pushdown/index-assisted evaluation is attempted).
#[cfg(feature = "geosparql")]
fn geosparql_function_local(nn: &NamedNode) -> Option<fn(&[Term]) -> Option<Term>> {
    use std::sync::LazyLock;
    static TABLE: LazyLock<HashMap<NamedNode, fn(&[Term]) -> Option<Term>>> = LazyLock::new(|| {
        spargeo::GEOSPARQL_EXTENSION_FUNCTIONS
            .iter()
            .map(|(name, f)| (name.into_owned(), *f))
            .collect()
    });
    TABLE.get(nn).copied()
}

/// No-op stand-in used when the `geosparql` cargo feature is disabled, so
/// the `eval_fn` call site above needs no `#[cfg(...)]` of its own.
#[cfg(not(feature = "geosparql"))]
fn geosparql_function_local(_nn: &NamedNode) -> Option<fn(&[Term]) -> Option<Term>> {
    None
}

// ── XSD cast functions ────────────────────────────────────────────────────────

/// Dispatch XSD cast function calls (`xsd:integer(?x)`, `xsd:boolean(?x)`, …).
/// Called for `Function::Custom(nn)` where `nn` is an XSD datatype IRI.
fn eval_xsd_cast(nn_str: &str, arg: Option<&Term>) -> Option<Term> {
    let local = nn_str.strip_prefix(XSD)?;
    let arg = arg?;
    match local {
        "boolean" => cast_to_boolean(arg),
        "integer" => cast_to_integer(arg),
        "decimal" => cast_to_decimal(arg),
        "float" => cast_to_float(arg),
        "double" => cast_to_double(arg),
        "string" => cast_to_xsd_string(arg),
        "dateTime" | "date" | "time" => {
            // Re-tag a string/literal as the target type (minimal validation)
            let lex = match arg {
                Term::Literal(l) if l.language().is_none() => l.value().to_string(),
                _ => return None,
            };
            Some(Term::Literal(Literal::new_typed_literal(
                lex,
                xsd_nn(local),
            )))
        }
        _ => None,
    }
}

fn cast_to_boolean(arg: &Term) -> Option<Term> {
    let l = match arg {
        Term::Literal(l) => l,
        _ => return None,
    };
    let local = l.datatype().as_str().strip_prefix(XSD).unwrap_or("");
    let b = match local {
        "boolean" => match l.value() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return None,
        },
        loc if is_numeric_local(loc) || matches!(loc, "decimal" | "float" | "double") => {
            let n = literal_as_f64(l)?;
            n != 0.0 && !n.is_nan()
        }
        "string" | "" => match l.value().trim() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return None,
        },
        _ => return None,
    };
    Some(bool_term(b))
}

fn cast_to_integer(arg: &Term) -> Option<Term> {
    let l = match arg {
        Term::Literal(l) => l,
        _ => return None,
    };
    let local = l.datatype().as_str().strip_prefix(XSD).unwrap_or("");
    let n: i64 = match local {
        "boolean" => match l.value() {
            "true" | "1" => 1,
            _ => 0,
        },
        loc if is_numeric_local(loc) || matches!(loc, "decimal" | "float" | "double") => {
            literal_as_f64(l)?.trunc() as i64
        }
        "string" | "" => l.value().trim().parse().ok()?,
        _ => return None,
    };
    Some(make_integer_literal(n))
}

fn cast_to_decimal(arg: &Term) -> Option<Term> {
    let l = match arg {
        Term::Literal(l) => l,
        _ => return None,
    };
    let local = l.datatype().as_str().strip_prefix(XSD).unwrap_or("");
    let f: f64 = match local {
        "boolean" => match l.value() {
            "true" | "1" => 1.0,
            _ => 0.0,
        },
        loc if is_numeric_local(loc) || matches!(loc, "decimal" | "float" | "double") => {
            literal_as_f64(l)?
        }
        "string" | "" => {
            let trimmed = l.value().trim();
            if trimmed.contains('e') || trimmed.contains('E') {
                return None;
            }
            trimmed.parse().ok()?
        }
        _ => return None,
    };
    Some(make_decimal_literal(f))
}

fn cast_to_float(arg: &Term) -> Option<Term> {
    let l = match arg {
        Term::Literal(l) => l,
        _ => return None,
    };
    let local = l.datatype().as_str().strip_prefix(XSD).unwrap_or("");
    let f: f64 = match local {
        "boolean" => match l.value() {
            "true" | "1" => 1.0,
            _ => 0.0,
        },
        loc if is_numeric_local(loc) || matches!(loc, "decimal" | "float" | "double") => {
            literal_as_f64(l)?
        }
        "string" | "" => l.value().trim().parse().ok()?,
        _ => return None,
    };
    Some(Term::Literal(Literal::new_typed_literal(
        (f as f32).to_string(),
        xsd_nn("float"),
    )))
}

fn cast_to_double(arg: &Term) -> Option<Term> {
    let l = match arg {
        Term::Literal(l) => l,
        _ => return None,
    };
    let local = l.datatype().as_str().strip_prefix(XSD).unwrap_or("");
    let f: f64 = match local {
        "boolean" => match l.value() {
            "true" | "1" => 1.0,
            _ => 0.0,
        },
        loc if is_numeric_local(loc) || matches!(loc, "decimal" | "float" | "double") => {
            literal_as_f64(l)?
        }
        "string" | "" => l.value().trim().parse().ok()?,
        _ => return None,
    };
    Some(Term::Literal(Literal::new_typed_literal(
        format_xsd_double(f),
        xsd_nn("double"),
    )))
}

fn cast_to_xsd_string(arg: &Term) -> Option<Term> {
    let s = match arg {
        Term::NamedNode(n) => n.as_str().to_string(),
        Term::Literal(l) => {
            let local = l.datatype().as_str().strip_prefix(XSD).unwrap_or("");
            match local {
                "boolean" => match l.value() {
                    "true" | "1" => "true".to_string(),
                    _ => "false".to_string(),
                },
                "decimal" | "double" => {
                    // Parse as f64 and produce integer string if whole, decimal otherwise
                    let f: f64 = l.value().parse().ok()?;
                    if f.fract() == 0.0 && f.abs() < 1e15 {
                        format!("{}", f as i64)
                    } else {
                        format!("{f}")
                    }
                }
                "float" => {
                    // Use f32 precision for float
                    let f: f32 = l.value().parse().ok()?;
                    if f.fract() == 0.0 && f.abs() < 1e15f32 {
                        format!("{}", f as i64)
                    } else {
                        format!("{f}")
                    }
                }
                _ => l.value().to_string(),
            }
        }
        _ => return None,
    };
    Some(Term::Literal(Literal::new_typed_literal(
        s,
        xsd_nn("string"),
    )))
}

// ── term → string value ───────────────────────────────────────────────────────

#[allow(dead_code)]
fn term_to_string_value(t: &Term) -> String {
    match t {
        Term::NamedNode(n) => n.as_str().to_string(),
        Term::Literal(l) => l.value().to_string(),
        Term::BlankNode(b) => b.as_str().to_string(),
        _ => String::new(),
    }
}

// =============================================================================
// Update stub
// =============================================================================

pub fn apply_update<S: oxigraph_nova_core::QuadStore>(
    _store: &S,
    _update: &spargebra::Update,
) -> Result<()> {
    Err(anyhow::anyhow!("SPARQL Update not yet implemented"))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::InMemoryDataset;
    use oxrdf::{Literal, NamedNode};
    use spargebra::SparqlParser;

    fn iri(s: &str) -> Term {
        Term::NamedNode(NamedNode::new_unchecked(s))
    }
    fn lit(s: &str) -> Term {
        Term::Literal(Literal::new_simple_literal(s))
    }

    fn make_dataset() -> InMemoryDataset {
        let mut d = InMemoryDataset::new();
        d.add_default(iri("http://ex/alice"), iri("http://ex/name"), lit("Alice"));
        d.add_default(
            iri("http://ex/alice"),
            iri("http://ex/age"),
            Term::Literal(Literal::new_typed_literal("30", xsd_nn("integer"))),
        );
        d.add_default(iri("http://ex/bob"), iri("http://ex/name"), lit("Bob"));
        d.add_default(
            iri("http://ex/bob"),
            iri("http://ex/age"),
            Term::Literal(Literal::new_typed_literal("25", xsd_nn("integer"))),
        );
        d
    }

    #[test]
    fn select_all() {
        let d = make_dataset();
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query("SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 4);
    }

    #[test]
    fn ask_true() {
        let d = make_dataset();
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query("ASK { <http://ex/alice> <http://ex/name> \"Alice\" }")
            .unwrap();
        let QueryResult::Boolean(b) = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        assert!(b);
    }

    #[test]
    fn ask_false() {
        let d = make_dataset();
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query("ASK { <http://ex/alice> <http://ex/name> \"Nobody\" }")
            .unwrap();
        let QueryResult::Boolean(b) = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        assert!(!b);
    }

    #[test]
    fn filter_by_name() {
        let d = make_dataset();
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query(r#"SELECT ?s WHERE { ?s <http://ex/name> ?n FILTER(?n = "Alice") }"#)
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 1);
        assert_eq!(
            sols[0].get(&Variable::new_unchecked("s")),
            Some(&iri("http://ex/alice"))
        );
    }

    #[test]
    fn optional_join() {
        let mut d = make_dataset();
        d.add_default(iri("http://ex/carol"), iri("http://ex/name"), lit("Carol"));
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query(
                r#"
                SELECT ?s ?age WHERE {
                    ?s <http://ex/name> ?n .
                    OPTIONAL { ?s <http://ex/age> ?age }
                }"#,
            )
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 3);
        let carol_sol = sols
            .iter()
            .find(|s| s.get(&Variable::new_unchecked("s")) == Some(&iri("http://ex/carol")))
            .unwrap();
        assert!(carol_sol.get(&Variable::new_unchecked("age")).is_none());
    }

    #[test]
    fn union_query() {
        let d = make_dataset();
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query(
                r#"
                SELECT ?s WHERE {
                    { ?s <http://ex/name> "Alice" }
                    UNION
                    { ?s <http://ex/name> "Bob" }
                }"#,
            )
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 2);
    }

    #[test]
    fn distinct_query() {
        let mut d = make_dataset();
        d.add_default(iri("http://ex/alice"), iri("http://ex/name"), lit("Alice"));
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query("SELECT DISTINCT ?s WHERE { ?s <http://ex/name> ?n }")
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 2);
    }

    #[test]
    fn limit_offset() {
        let d = make_dataset();
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query("SELECT ?s ?p ?o WHERE { ?s ?p ?o } ORDER BY ?s LIMIT 2 OFFSET 1")
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 2);
    }

    #[test]
    fn count_aggregate() {
        let d = make_dataset();
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query("SELECT (COUNT(?s) AS ?cnt) WHERE { ?s <http://ex/name> ?n }")
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 1);
        let cnt = sols[0].get(&Variable::new_unchecked("cnt")).unwrap();
        assert_eq!(cnt, &make_integer_literal(2));
    }

    #[test]
    fn values_inline() {
        let d = make_dataset();
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query(
                r#"
                SELECT ?s ?n WHERE {
                    VALUES ?s { <http://ex/alice> }
                    ?s <http://ex/name> ?n
                }"#,
            )
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 1);
    }

    #[test]
    fn bind_extend() {
        let d = make_dataset();
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query(
                r#"
                SELECT ?s ?upper WHERE {
                    ?s <http://ex/name> ?n .
                    BIND(UCASE(?n) AS ?upper)
                }"#,
            )
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 2);
        for sol in &sols {
            let upper = sol.get(&Variable::new_unchecked("upper")).unwrap();
            match upper {
                Term::Literal(l) => assert!(l.value() == "ALICE" || l.value() == "BOB"),
                _ => panic!("expected literal"),
            }
        }
    }

    #[test]
    fn hash_md5() {
        let d = InMemoryDataset::new();
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query(r#"SELECT (MD5("abc") AS ?h) WHERE {}"#)
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 1);
        let h = sols[0].get(&Variable::new_unchecked("h")).unwrap();
        assert_eq!(h.to_string(), "\"900150983cd24fb0d6963f7d28e17f72\"");
    }

    #[test]
    fn property_path_one_or_more() {
        let mut d = InMemoryDataset::new();
        d.add_default(iri("http://ex/a"), iri("http://ex/p"), iri("http://ex/b"));
        d.add_default(iri("http://ex/b"), iri("http://ex/p"), iri("http://ex/c"));
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query("SELECT ?x WHERE { <http://ex/a> <http://ex/p>+ ?x }")
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        let mut got: Vec<String> = sols
            .iter()
            .map(|s| s.get(&Variable::new_unchecked("x")).unwrap().to_string())
            .collect();
        got.sort();
        assert_eq!(got, vec!["<http://ex/b>", "<http://ex/c>"]);
    }

    #[test]
    fn property_path_zero_or_more() {
        let mut d = InMemoryDataset::new();
        d.add_default(iri("http://ex/a"), iri("http://ex/p"), iri("http://ex/b"));
        d.add_default(iri("http://ex/b"), iri("http://ex/p"), iri("http://ex/c"));
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query("SELECT ?x WHERE { <http://ex/a> <http://ex/p>* ?x }")
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        let mut got: Vec<String> = sols
            .iter()
            .map(|s| s.get(&Variable::new_unchecked("x")).unwrap().to_string())
            .collect();
        got.sort();
        assert_eq!(got, vec!["<http://ex/a>", "<http://ex/b>", "<http://ex/c>"]);
    }

    #[test]
    fn property_path_reverse() {
        let mut d = InMemoryDataset::new();
        d.add_default(iri("http://ex/a"), iri("http://ex/p"), iri("http://ex/b"));
        let ev = Evaluator::new(&d);
        // SPARQL: `<b> ^<p> ?x` ≡ `?x <p> <b>`, i.e. find x where (x, p, b) ∈ graph → x = a.
        let q = SparqlParser::new()
            .parse_query("SELECT ?x WHERE { <http://ex/b> ^<http://ex/p> ?x }")
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 1);
        assert_eq!(
            sols[0].get(&Variable::new_unchecked("x")),
            Some(&iri("http://ex/a"))
        );
    }

    #[test]
    fn property_path_alternative() {
        let mut d = InMemoryDataset::new();
        d.add_default(iri("http://ex/a"), iri("http://ex/p"), iri("http://ex/b"));
        d.add_default(iri("http://ex/a"), iri("http://ex/q"), iri("http://ex/c"));
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query("SELECT ?x WHERE { <http://ex/a> <http://ex/p>|<http://ex/q> ?x }")
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        let mut got: Vec<String> = sols
            .iter()
            .map(|s| s.get(&Variable::new_unchecked("x")).unwrap().to_string())
            .collect();
        got.sort();
        assert_eq!(got, vec!["<http://ex/b>", "<http://ex/c>"]);
    }

    #[test]
    fn property_path_sequence() {
        let mut d = InMemoryDataset::new();
        let p = iri("http://ex/p");
        let q_iri = iri("http://ex/q");
        d.add_default(iri("http://ex/a"), p.clone(), iri("http://ex/b"));
        d.add_default(iri("http://ex/b"), q_iri.clone(), iri("http://ex/c"));
        let ev = Evaluator::new(&d);
        let q = SparqlParser::new()
            .parse_query("SELECT ?x WHERE { <http://ex/a> <http://ex/p>/<http://ex/q> ?x }")
            .unwrap();
        let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
            panic!()
        };
        let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(sols.len(), 1);
        assert_eq!(
            sols[0].get(&Variable::new_unchecked("x")),
            Some(&iri("http://ex/c"))
        );
    }

    // ── Full-text search dispatch (text:query / text:contains) ─────────────

    mod fulltext_dispatch {
        use super::*;
        use crate::dataset::StoreDataset;
        use crate::options::QueryOptions;
        use oxigraph_nova_core::QuadStore;
        use oxigraph_nova_engine_ring::LoudsStore;
        use std::sync::Arc;

        fn make_store_with_text() -> Arc<LoudsStore> {
            let store = Arc::new(LoudsStore::new());
            store.enable_fulltext().unwrap();
            store
                .insert(&oxrdf::Quad::new(
                    NamedNode::new_unchecked("http://ex/s1"),
                    NamedNode::new_unchecked("http://ex/name"),
                    Literal::new_simple_literal("the quick brown fox"),
                    GraphName::DefaultGraph,
                ))
                .unwrap();
            store
                .insert(&oxrdf::Quad::new(
                    NamedNode::new_unchecked("http://ex/s2"),
                    NamedNode::new_unchecked("http://ex/name"),
                    Literal::new_simple_literal("a lazy dog sleeps"),
                    GraphName::DefaultGraph,
                ))
                .unwrap();
            store.compact().unwrap();
            store
        }

        #[test]
        fn pushdown_filter_over_bgp_finds_match() {
            let store = make_store_with_text();
            let dataset = StoreDataset::new(Arc::clone(&store));
            let options = QueryOptions::new()
                .with_text_search(store.clone() as Arc<dyn oxigraph_nova_core::TextSearch>);
            let ev = Evaluator::with_options(&dataset, options);
            let q = SparqlParser::new()
                .parse_query(
                    r#"PREFIX text: <http://oxigraph-nova.dev/fn/text#>
                       SELECT ?s ?o WHERE {
                           ?s <http://ex/name> ?o .
                           FILTER(text:query(?o, "fox"))
                       }"#,
                )
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert_eq!(sols.len(), 1);
            assert_eq!(
                sols[0].get(&Variable::new_unchecked("s")),
                Some(&iri("http://ex/s1"))
            );
        }

        #[test]
        fn pushdown_filter_no_match_returns_empty() {
            let store = make_store_with_text();
            let dataset = StoreDataset::new(Arc::clone(&store));
            let options = QueryOptions::new()
                .with_text_search(store.clone() as Arc<dyn oxigraph_nova_core::TextSearch>);
            let ev = Evaluator::with_options(&dataset, options);
            let q = SparqlParser::new()
                .parse_query(
                    r#"PREFIX text: <http://oxigraph-nova.dev/fn/text#>
                       SELECT ?s WHERE {
                           ?s <http://ex/name> ?o .
                           FILTER(text:contains(?o, "nonexistentword"))
                       }"#,
                )
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert!(sols.is_empty());
        }

        #[test]
        fn no_text_search_backend_configured_yields_empty() {
            // No `with_text_search` call at all: text:query must behave as
            // an always-false filter (SPARQL type error -> filtered out),
            // not panic or error.
            let store = make_store_with_text();
            let dataset = StoreDataset::new(Arc::clone(&store));
            let ev = Evaluator::new(&dataset); // default QueryOptions, no text_search
            let q = SparqlParser::new()
                .parse_query(
                    r#"PREFIX text: <http://oxigraph-nova.dev/fn/text#>
                       SELECT ?s WHERE {
                           ?s <http://ex/name> ?o .
                           FILTER(text:query(?o, "fox"))
                       }"#,
                )
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert!(sols.is_empty());
        }

        /// Regression test for the var-as-predicate compatibility fix in
        /// `eval_bgp_with_bound_var`: when the text-search variable also
        /// occurs in *predicate* position of another triple pattern in the
        /// same BGP, substitution can't touch that occurrence (predicates
        /// are `NamedNodePattern`, never literals), so `eval_bgp` is free to
        /// bind it to some unrelated predicate IRI there. Such rows must be
        /// dropped as incompatible with the search hit's literal binding,
        /// not silently kept.
        #[test]
        fn pushdown_drops_rows_where_var_rebound_incompatibly_as_predicate() {
            let store = make_store_with_text();
            // Add a second predicate whose *name* is used as a subject in an
            // unrelated triple, and reuse the search variable `?o` in
            // predicate position of a second triple pattern so it can be
            // bound to an IRI incompatible with the literal hit.
            store
                .insert(&oxrdf::Quad::new(
                    NamedNode::new_unchecked("http://ex/s1"),
                    NamedNode::new_unchecked("http://ex/other"),
                    NamedNode::new_unchecked("http://ex/irrelevant"),
                    GraphName::DefaultGraph,
                ))
                .unwrap();
            store.compact().unwrap();

            let dataset = StoreDataset::new(Arc::clone(&store));
            let options = QueryOptions::new()
                .with_text_search(store.clone() as Arc<dyn oxigraph_nova_core::TextSearch>);
            let ev = Evaluator::with_options(&dataset, options);
            // `?o` is bound as the object of <name> (a literal, matched by
            // the search) AND as the predicate of a second triple pattern
            // `?a ?o ?b` -- since no quad in the store actually uses the
            // literal "the quick brown fox" as a predicate, `?a ?o ?b`
            // cannot match with `?o` bound to that literal, so the
            // pushdown-evaluated BGP must yield zero rows once
            // `eval_bgp_with_bound_var`'s compatibility check runs -- not a
            // spurious row with `?o` incompatibly rebound to some other IRI
            // matched via the unconstrained `?a ?o ?b` pattern.
            let q = SparqlParser::new()
                .parse_query(
                    r#"PREFIX text: <http://oxigraph-nova.dev/fn/text#>
                       SELECT ?s ?o ?a ?b WHERE {
                           ?s <http://ex/name> ?o .
                           ?a ?o ?b .
                           FILTER(text:query(?o, "fox"))
                       }"#,
                )
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert!(
                sols.is_empty(),
                "rows with ?o incompatibly rebound as a predicate must be dropped, got: {sols:?}"
            );
        }

        #[test]
        fn scalar_fallback_when_not_direct_filter_child() {
            // Wrapping in a BIND/nested boolean expression bypasses the
            // pushdown pattern-match (`text_search_call` only recognizes a
            // direct FunctionCall), exercising the `eval_fn` scalar-fallback
            // path (`Function::Custom` arm) instead.
            let store = make_store_with_text();
            let dataset = StoreDataset::new(Arc::clone(&store));
            let options = QueryOptions::new()
                .with_text_search(store.clone() as Arc<dyn oxigraph_nova_core::TextSearch>);
            let ev = Evaluator::with_options(&dataset, options);
            let q = SparqlParser::new()
                .parse_query(
                    r#"PREFIX text: <http://oxigraph-nova.dev/fn/text#>
                       SELECT ?s WHERE {
                           ?s <http://ex/name> ?o .
                           FILTER(text:query(?o, "fox") && true)
                       }"#,
                )
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert_eq!(sols.len(), 1);
            assert_eq!(
                sols[0].get(&Variable::new_unchecked("s")),
                Some(&iri("http://ex/s1"))
            );
        }
    }

    // ── SERVICE (ServiceHandler extension point) ────────────────────────

    mod service_dispatch {
        use super::*;
        use crate::options::QueryOptions;
        use crate::service::ServiceHandler;
        use spargebra::algebra::GraphPattern;
        use std::sync::Arc;

        /// A mock handler that always returns one fixed solution binding
        /// `?x` to `"mocked"`, regardless of `service_name`/`pattern`.
        struct FixedHandler;
        impl ServiceHandler for FixedHandler {
            fn handle(
                &self,
                _service_name: &NamedNode,
                _pattern: &GraphPattern,
                _base_iri: Option<&str>,
            ) -> anyhow::Result<Solutions> {
                let mut sol = Solution::new();
                sol.insert(Variable::new_unchecked("x"), lit("mocked"));
                Ok(vec![sol])
            }
        }

        /// A mock handler that always fails -- used to exercise both the
        /// hard-error (non-SILENT) and zero-solutions (SILENT) paths.
        struct FailingHandler;
        impl ServiceHandler for FailingHandler {
            fn handle(
                &self,
                _service_name: &NamedNode,
                _pattern: &GraphPattern,
                _base_iri: Option<&str>,
            ) -> anyhow::Result<Solutions> {
                Err(anyhow::anyhow!("mock service failure"))
            }
        }

        #[test]
        fn no_handler_configured_errors_without_silent() {
            let d = InMemoryDataset::new();
            let ev = Evaluator::new(&d); // default QueryOptions: no service_handler
            let q = SparqlParser::new()
                .parse_query("SELECT ?x WHERE { SERVICE <http://example.org/sparql> { ?s ?p ?x } }")
                .unwrap();
            assert!(ev.evaluate(&q).is_err());
        }

        #[test]
        fn no_handler_configured_silent_yields_empty() {
            let d = InMemoryDataset::new();
            let ev = Evaluator::new(&d); // default QueryOptions: no service_handler
            let q = SparqlParser::new()
                .parse_query(
                    "SELECT ?x WHERE { SERVICE SILENT <http://example.org/sparql> { ?s ?p ?x } }",
                )
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert!(sols.is_empty());
        }

        #[test]
        fn configured_handler_dispatches_and_returns_its_solutions() {
            let d = InMemoryDataset::new();
            let options = QueryOptions::new().with_service_handler(Arc::new(FixedHandler));
            let ev = Evaluator::with_options(&d, options);
            let q = SparqlParser::new()
                .parse_query("SELECT ?x WHERE { SERVICE <http://example.org/sparql> { ?s ?p ?x } }")
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert_eq!(sols.len(), 1);
            assert_eq!(
                sols[0].get(&Variable::new_unchecked("x")),
                Some(&lit("mocked"))
            );
        }

        #[test]
        fn failing_handler_errors_without_silent() {
            let d = InMemoryDataset::new();
            let options = QueryOptions::new().with_service_handler(Arc::new(FailingHandler));
            let ev = Evaluator::with_options(&d, options);
            let q = SparqlParser::new()
                .parse_query("SELECT ?x WHERE { SERVICE <http://example.org/sparql> { ?s ?p ?x } }")
                .unwrap();
            assert!(ev.evaluate(&q).is_err());
        }

        #[test]
        fn failing_handler_silent_yields_empty() {
            let d = InMemoryDataset::new();
            let options = QueryOptions::new().with_service_handler(Arc::new(FailingHandler));
            let ev = Evaluator::with_options(&d, options);
            let q = SparqlParser::new()
                .parse_query(
                    "SELECT ?x WHERE { SERVICE SILENT <http://example.org/sparql> { ?s ?p ?x } }",
                )
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert!(sols.is_empty());
        }

        #[test]
        fn variable_service_name_is_unsupported() {
            let d = InMemoryDataset::new();
            let options = QueryOptions::new().with_service_handler(Arc::new(FixedHandler));
            let ev = Evaluator::with_options(&d, options);
            let q = SparqlParser::new()
                .parse_query("SELECT ?x WHERE { SERVICE ?svc { ?s ?p ?x } }")
                .unwrap();
            assert!(ev.evaluate(&q).is_err());
        }
    }

    // ── GeoSPARQL (geof:distance, sf:intersects, geof:area, …) ──────────

    #[cfg(feature = "geosparql")]
    mod geosparql_dispatch {
        use super::*;

        const GEO: &str = "http://www.opengis.net/ont/geosparql#";
        const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";
        const UOM: &str = "http://www.opengis.net/def/uom/OGC/1.0/";

        fn wkt(s: &str) -> Term {
            Term::Literal(Literal::new_typed_literal(
                s,
                NamedNode::new_unchecked(format!("{GEO}wktLiteral")),
            ))
        }

        fn make_geo_dataset() -> InMemoryDataset {
            let mut d = InMemoryDataset::new();
            d.add_default(
                iri("http://ex/origin"),
                iri("http://ex/geom"),
                wkt("POINT(0 0)"),
            );
            d.add_default(
                iri("http://ex/one_degree_north"),
                iri("http://ex/geom"),
                wkt("POINT(0 1)"),
            );
            d
        }

        fn as_f64(t: &Term) -> f64 {
            let Term::Literal(l) = t else {
                panic!("expected a literal, got {t:?}")
            };
            l.value().parse().unwrap()
        }

        fn as_bool(t: &Term) -> bool {
            let Term::Literal(l) = t else {
                panic!("expected a literal, got {t:?}")
            };
            l.value() == "true"
        }

        #[test]
        fn distance_between_bound_points_dispatches_through_bgp() {
            let d = make_geo_dataset();
            let ev = Evaluator::new(&d);
            let q = SparqlParser::new()
                .parse_query(&format!(
                    r#"PREFIX geof: <{GEOF}>
                       PREFIX uom: <{UOM}>
                       SELECT ?d WHERE {{
                           <http://ex/origin> <http://ex/geom> ?ga .
                           <http://ex/one_degree_north> <http://ex/geom> ?gb .
                           BIND(geof:distance(?ga, ?gb, uom:metre) AS ?d)
                       }}"#
                ))
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert_eq!(sols.len(), 1);
            let d = as_f64(sols[0].get(&Variable::new_unchecked("d")).unwrap());
            // ~111.2 km per degree of latitude via Haversine.
            assert!((109_000.0..113_000.0).contains(&d), "got {d}");
        }

        #[test]
        fn distance_between_identical_points_is_zero() {
            let d = InMemoryDataset::new();
            let ev = Evaluator::new(&d);
            let q = SparqlParser::new()
                .parse_query(&format!(
                    r#"PREFIX geof: <{GEOF}>
                       PREFIX geo: <{GEO}>
                       PREFIX uom: <{UOM}>
                       SELECT ?d WHERE {{
                           BIND(geof:distance("POINT(3 4)"^^geo:wktLiteral, "POINT(3 4)"^^geo:wktLiteral, uom:metre) AS ?d)
                       }}"#
                ))
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert_eq!(sols.len(), 1);
            let d = as_f64(sols[0].get(&Variable::new_unchecked("d")).unwrap());
            assert_eq!(d, 0.0);
        }

        #[test]
        fn sf_intersects_true_for_overlapping_polygons() {
            let d = InMemoryDataset::new();
            let ev = Evaluator::new(&d);
            let q = SparqlParser::new()
                .parse_query(&format!(
                    r#"PREFIX geof: <{GEOF}>
                       PREFIX geo: <{GEO}>
                       SELECT ?b WHERE {{
                           BIND(geof:sfIntersects(
                               "POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))"^^geo:wktLiteral,
                               "POLYGON((2 2, 6 2, 6 6, 2 6, 2 2))"^^geo:wktLiteral
                           ) AS ?b)
                       }}"#
                ))
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert_eq!(sols.len(), 1);
            assert!(as_bool(sols[0].get(&Variable::new_unchecked("b")).unwrap()));
        }

        #[test]
        fn sf_intersects_false_for_disjoint_polygons() {
            let d = InMemoryDataset::new();
            let ev = Evaluator::new(&d);
            let q = SparqlParser::new()
                .parse_query(&format!(
                    r#"PREFIX geof: <{GEOF}>
                       PREFIX geo: <{GEO}>
                       SELECT ?b WHERE {{
                           BIND(geof:sfIntersects(
                               "POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))"^^geo:wktLiteral,
                               "POLYGON((100 100, 101 100, 101 101, 100 101, 100 100))"^^geo:wktLiteral
                           ) AS ?b)
                       }}"#
                ))
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert_eq!(sols.len(), 1);
            assert!(!as_bool(
                sols[0].get(&Variable::new_unchecked("b")).unwrap()
            ));
        }

        #[test]
        fn area_of_a_polygon_is_positive() {
            let d = InMemoryDataset::new();
            let ev = Evaluator::new(&d);
            let q = SparqlParser::new()
                .parse_query(&format!(
                    r#"PREFIX geof: <{GEOF}>
                       PREFIX geo: <{GEO}>
                       PREFIX uom: <{UOM}>
                       SELECT ?a WHERE {{
                           BIND(geof:area(
                               "POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))"^^geo:wktLiteral,
                               uom:square_metre
                           ) AS ?a)
                       }}"#
                ))
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert_eq!(sols.len(), 1);
            let a = as_f64(sols[0].get(&Variable::new_unchecked("a")).unwrap());
            assert!(a > 0.0, "got {a}");
        }

        #[test]
        fn unrecognized_geosparql_function_iri_yields_unbound() {
            // Not one of the 43 registered GeoSPARQL function IRIs: falls
            // through to `eval_xsd_cast`, which also doesn't recognize this
            // IRI (wrong namespace) and returns `None` -- BIND leaves the
            // variable unbound rather than erroring.
            let d = InMemoryDataset::new();
            let ev = Evaluator::new(&d);
            let q = SparqlParser::new()
                .parse_query(&format!(
                    r#"PREFIX geof: <{GEOF}>
                       SELECT ?x WHERE {{
                           BIND(geof:notAFunction("x") AS ?x)
                       }}"#
                ))
                .unwrap();
            let QueryResult::Solutions { stream, .. } = ev.evaluate(&q).unwrap() else {
                panic!()
            };
            let sols: Vec<_> = stream.collect::<Result<Vec<_>>>().unwrap();
            assert_eq!(sols.len(), 1);
            assert!(sols[0].get(&Variable::new_unchecked("x")).is_none());
        }
    }
}
