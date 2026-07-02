//! SPARQL 1.2 Update evaluator.
//!
//! Executes a parsed [`spargebra::Update`] (a sequence of
//! [`GraphUpdateOperation`]s) against any [`QuadStore`]. This module sits
//! alongside `evaluator.rs` (which handles read-only `Query` evaluation) and
//! *reuses* it for WHERE-clause evaluation in `DeleteInsert` operations by
//! wrapping the WHERE pattern in a synthetic `Query::Select` — this lets the
//! already-tested dataset-clause resolution logic in `Evaluator::evaluate`
//! handle `USING` / `USING NAMED` for free, since it plays exactly the role
//! `FROM` / `FROM NAMED` plays for ordinary queries (same `QueryDataset` type).
//!
//! # Atomicity
//!
//! Each individual [`QuadStore::insert`]/[`QuadStore::remove`] call is atomic
//! (the backing store's internal lock, if any, is held for exactly that
//! call), but a multi-statement `Update` request as a whole is **not**
//! atomic: concurrent readers/writers against the same store may observe
//! partial progress of an in-flight Update, and a later statement in the same
//! request that errors will leave earlier statements' effects applied.
//! `RingStore`'s single-`Mutex<RingStoreInner>` design does not currently
//! expose a way to hold the lock across multiple calls, and adding that is a
//! larger architectural change (see `RingStore`'s module doc comment on its
//! "production evolution path"). This is an accepted, documented limitation.
//!
//! # CLEAR / DROP / CREATE
//!
//! [`QuadStore`] has no dedicated "delete all quads in a graph" or
//! "unregister a named graph" primitive, so `CLEAR`/`DROP` are implemented by
//! scanning `quads_for_pattern` for the target graph(s) and calling `remove`
//! on every quad found. `DROP` is therefore currently identical to `CLEAR`
//! (both just empty the graph) — neither can make `known_named_graphs()`
//! forget a graph's registration, since no backend exposes an "unregister"
//! operation. `CREATE` maps directly onto `register_named_graph`, which is
//! already idempotent, so both `CREATE` and `CREATE SILENT` simply succeed
//! even if the graph already exists (a minor deviation from the strict
//! non-SILENT-CREATE-on-existing-graph-is-an-error wording of the spec, in
//! exchange for not needing a separate graph-existence-tracking mechanism).
//!
//! # LOAD
//!
//! `LOAD <iri>` requires fetching remote RDF data over HTTP; `nova-server`
//! has no HTTP client dependency wired up (out of scope for the primary
//! BSBM-compatibility motivation, which only exercises `INSERT DATA` /
//! `DELETE WHERE`). Non-`SILENT` `LOAD` therefore returns an error;
//! `LOAD SILENT` succeeds as a no-op. Bulk data loading is available instead
//! through the SPARQL 1.1 Graph Store HTTP Protocol (`PUT`/`POST /store`).

use crate::dataset::StoreDataset;
use crate::evaluator::{Evaluator, QueryResult};
use crate::solution::Solution;
use anyhow::{Result, anyhow};
use oxigraph_nova_core::QuadStore;
use oxrdf::{BlankNode, GraphName, NamedNode, NamedOrBlankNode, Quad, Term};
use spargebra::algebra::{GraphPattern, GraphTarget, QueryDataset};
use spargebra::term::{
    GraphNamePattern, GroundQuad, GroundQuadPattern, GroundTerm, GroundTermPattern,
    GroundTriplePattern, NamedNodePattern, QuadPattern, TermPattern, TriplePattern,
};
use spargebra::{GraphUpdateOperation, Query, Update};
use std::collections::HashMap;
use std::sync::Arc;

// ── Public entry point ────────────────────────────────────────────────────

/// Execute every operation in `update` against `store`, in order.
///
/// Per SPARQL 1.1 Update § 3.2.1, operations execute sequentially. If a
/// non-`SILENT` operation fails, this returns immediately with an error
/// (leaving any earlier operations' effects applied — see the module-level
/// docs on atomicity). A `SILENT` operation that would otherwise error is
/// simply skipped.
pub fn execute_update<S: QuadStore + 'static>(store: &Arc<S>, update: &Update) -> Result<()> {
    for op in &update.operations {
        execute_operation(store, op)?;
    }
    Ok(())
}

fn execute_operation<S: QuadStore + 'static>(
    store: &Arc<S>,
    op: &GraphUpdateOperation,
) -> Result<()> {
    match op {
        GraphUpdateOperation::InsertData { data } => insert_data(store, data),
        GraphUpdateOperation::DeleteData { data } => delete_data(store, data),
        GraphUpdateOperation::DeleteInsert {
            delete,
            insert,
            using,
            pattern,
        } => delete_insert(store, delete, insert, using.as_ref(), pattern),
        GraphUpdateOperation::Load {
            silent,
            source,
            destination: _,
        } => {
            if *silent {
                Ok(())
            } else {
                Err(anyhow!(
                    "LOAD <{}> is not supported by this server (no HTTP fetch client is wired \
                     up); use the Graph Store HTTP Protocol (PUT/POST /store) to upload data \
                     instead, or use LOAD SILENT to ignore this error",
                    source.as_str()
                ))
            }
        }
        GraphUpdateOperation::Clear { silent, graph } => {
            clear_graph_target(store, graph).or_else(|e| if *silent { Ok(()) } else { Err(e) })
        }
        GraphUpdateOperation::Create { silent, graph } => {
            create_graph(store, graph).or_else(|e| if *silent { Ok(()) } else { Err(e) })
        }
        GraphUpdateOperation::Drop { silent, graph } => {
            clear_graph_target(store, graph).or_else(|e| if *silent { Ok(()) } else { Err(e) })
        }
    }
}

// ── INSERT DATA / DELETE DATA ─────────────────────────────────────────────

/// `spargebra::term::Quad`/`GroundQuad` use `spargebra::term::GraphName`
/// (only `NamedNode`/`DefaultGraph` — no `BlankNode` variant, since a
/// concrete data quad's graph can't be a blank node per the SPARQL grammar),
/// which is a *different type* than `oxrdf::GraphName` (used by
/// `QuadStore`/`Quad`) despite having a similarly-named `NamedNode` variant.
/// There's no `From` impl between them in either crate, so convert manually.
fn spargebra_graph_name_to_oxrdf(g: &spargebra::term::GraphName) -> GraphName {
    match g {
        spargebra::term::GraphName::NamedNode(n) => GraphName::NamedNode(n.clone()),
        spargebra::term::GraphName::DefaultGraph => GraphName::DefaultGraph,
    }
}

fn insert_data<S: QuadStore>(store: &Arc<S>, data: &[spargebra::term::Quad]) -> Result<()> {
    for q in data {
        let quad = Quad::new(
            q.subject.clone(),
            q.predicate.clone(),
            q.object.clone(),
            spargebra_graph_name_to_oxrdf(&q.graph_name),
        );
        store.insert(&quad).map_err(|e| anyhow!("{e}"))?;
    }
    Ok(())
}

fn delete_data<S: QuadStore>(store: &Arc<S>, data: &[GroundQuad]) -> Result<()> {
    for q in data {
        let quad = Quad::new(
            NamedOrBlankNode::NamedNode(q.subject.clone()),
            q.predicate.clone(),
            ground_term_to_term(&q.object),
            spargebra_graph_name_to_oxrdf(&q.graph_name),
        );
        store.remove(&quad).map_err(|e| anyhow!("{e}"))?;
    }
    Ok(())
}

fn ground_term_to_term(gt: &GroundTerm) -> Term {
    match gt {
        GroundTerm::NamedNode(n) => Term::NamedNode(n.clone()),
        GroundTerm::Literal(l) => Term::Literal(l.clone()),
        GroundTerm::Triple(t) => Term::Triple(Box::new(oxrdf::Triple::from(*t.clone()))),
    }
}

// ── DELETE/INSERT ... WHERE (and DELETE WHERE, which spargebra desugars to
// this same variant with `insert` empty) ────────────────────────────────────

fn delete_insert<S: QuadStore + 'static>(
    store: &Arc<S>,
    delete: &[GroundQuadPattern],
    insert: &[QuadPattern],
    using: Option<&QueryDataset>,
    pattern: &GraphPattern,
) -> Result<()> {
    // Reuse the read-only Evaluator to run the WHERE clause. `USING` /
    // `USING NAMED` play exactly the role `FROM` / `FROM NAMED` play for
    // queries, so we smuggle the WHERE pattern through a synthetic
    // `Query::Select` and let `Evaluator::evaluate`'s existing dataset-clause
    // handling do the work — no duplicated FROM/FROM NAMED logic here.
    let dataset = StoreDataset::new(Arc::clone(store));
    let synthetic = Query::Select {
        dataset: using.cloned(),
        pattern: pattern.clone(),
        base_iri: None,
    };
    let solutions = match Evaluator::new(&dataset).evaluate(&synthetic)? {
        QueryResult::Solutions(s) => s,
        _ => unreachable!("Query::Select always evaluates to QueryResult::Solutions"),
    };

    for sol in &solutions {
        // DELETE before INSERT for each solution row: the WHERE clause was
        // already fully evaluated above, so deletions can't affect which
        // solutions exist, but ordering DELETE-then-INSERT per row avoids a
        // just-inserted quad being visible to this same row's delete pass.
        for gqp in delete {
            if let Some(quad) = instantiate_ground_quad_pattern(gqp, sol) {
                store.remove(&quad).map_err(|e| anyhow!("{e}"))?;
            }
        }
        // Fresh blank-node map per solution row (SPARQL 1.1 § 3.1.3, the
        // INSERT-template analogue of CONSTRUCT's § 18.2.4 semantics).
        let mut bnode_map: HashMap<BlankNode, BlankNode> = HashMap::new();
        for qp in insert {
            if let Some(quad) = instantiate_quad_pattern(qp, sol, &mut bnode_map) {
                store.insert(&quad).map_err(|e| anyhow!("{e}"))?;
            }
        }
    }
    Ok(())
}

// ── Template instantiation helpers ────────────────────────────────────────

fn nn_pattern_to_named_node(nnp: &NamedNodePattern, sol: &Solution) -> Option<NamedNode> {
    match nnp {
        NamedNodePattern::NamedNode(n) => Some(n.clone()),
        NamedNodePattern::Variable(v) => match sol.get(v)? {
            Term::NamedNode(n) => Some(n.clone()),
            _ => None,
        },
    }
}

fn graph_name_pattern_to_graph(gnp: &GraphNamePattern, sol: &Solution) -> Option<GraphName> {
    match gnp {
        GraphNamePattern::DefaultGraph => Some(GraphName::DefaultGraph),
        GraphNamePattern::NamedNode(n) => Some(GraphName::NamedNode(n.clone())),
        GraphNamePattern::Variable(v) => match sol.get(v)? {
            Term::NamedNode(n) => Some(GraphName::NamedNode(n.clone())),
            Term::BlankNode(b) => Some(GraphName::BlankNode(b.clone())),
            _ => None,
        },
    }
}

fn ground_term_pattern_to_term(gtp: &GroundTermPattern, sol: &Solution) -> Option<Term> {
    match gtp {
        GroundTermPattern::NamedNode(n) => Some(Term::NamedNode(n.clone())),
        GroundTermPattern::Literal(l) => Some(Term::Literal(l.clone())),
        GroundTermPattern::Variable(v) => sol.get(v).cloned(),
        GroundTermPattern::Triple(inner) => {
            instantiate_ground_triple_pattern(inner, sol).map(|t| Term::Triple(Box::new(t)))
        }
    }
}

fn instantiate_ground_triple_pattern(
    gtp: &GroundTriplePattern,
    sol: &Solution,
) -> Option<oxrdf::Triple> {
    let subject = match ground_term_pattern_to_term(&gtp.subject, sol)? {
        Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n),
        Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b),
        _ => return None,
    };
    let predicate = nn_pattern_to_named_node(&gtp.predicate, sol)?;
    let object = ground_term_pattern_to_term(&gtp.object, sol)?;
    Some(oxrdf::Triple::new(subject, predicate, object))
}

fn instantiate_ground_quad_pattern(gqp: &GroundQuadPattern, sol: &Solution) -> Option<Quad> {
    let subject = match ground_term_pattern_to_term(&gqp.subject, sol)? {
        Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n),
        Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b),
        _ => return None,
    };
    let predicate = nn_pattern_to_named_node(&gqp.predicate, sol)?;
    let object = ground_term_pattern_to_term(&gqp.object, sol)?;
    let graph_name = graph_name_pattern_to_graph(&gqp.graph_name, sol)?;
    Some(Quad::new(subject, predicate, object, graph_name))
}

/// Get-or-allocate the fresh blank node an INSERT template's blank-node
/// label `b` maps to for the *current solution row* — mirrors
/// `evaluator::fresh_bnode_for`'s semantics.
fn fresh_bnode_for(b: &BlankNode, bnode_map: &mut HashMap<BlankNode, BlankNode>) -> BlankNode {
    bnode_map.entry(b.clone()).or_default().clone()
}

fn term_pattern_to_term(
    tp: &TermPattern,
    sol: &Solution,
    bnode_map: &mut HashMap<BlankNode, BlankNode>,
) -> Option<Term> {
    match tp {
        TermPattern::NamedNode(n) => Some(Term::NamedNode(n.clone())),
        TermPattern::BlankNode(b) => Some(Term::BlankNode(fresh_bnode_for(b, bnode_map))),
        TermPattern::Literal(l) => Some(Term::Literal(l.clone())),
        TermPattern::Variable(v) => sol.get(v).cloned(),
        TermPattern::Triple(inner) => {
            instantiate_triple_pattern_for_insert(inner, sol, bnode_map)
                .map(|t| Term::Triple(Box::new(t)))
        }
    }
}

fn instantiate_triple_pattern_for_insert(
    tp: &TriplePattern,
    sol: &Solution,
    bnode_map: &mut HashMap<BlankNode, BlankNode>,
) -> Option<oxrdf::Triple> {
    let subject = match term_pattern_to_term(&tp.subject, sol, bnode_map)? {
        Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n),
        Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b),
        _ => return None,
    };
    let predicate = nn_pattern_to_named_node(&tp.predicate, sol)?;
    let object = term_pattern_to_term(&tp.object, sol, bnode_map)?;
    Some(oxrdf::Triple::new(subject, predicate, object))
}

fn instantiate_quad_pattern(
    qp: &QuadPattern,
    sol: &Solution,
    bnode_map: &mut HashMap<BlankNode, BlankNode>,
) -> Option<Quad> {
    let subject = match term_pattern_to_term(&qp.subject, sol, bnode_map)? {
        Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n),
        Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b),
        _ => return None,
    };
    let predicate = nn_pattern_to_named_node(&qp.predicate, sol)?;
    let object = term_pattern_to_term(&qp.object, sol, bnode_map)?;
    let graph_name = graph_name_pattern_to_graph(&qp.graph_name, sol)?;
    Some(Quad::new(subject, predicate, object, graph_name))
}

// ── CLEAR / DROP / CREATE ─────────────────────────────────────────────────

fn create_graph<S: QuadStore>(store: &Arc<S>, graph: &NamedNode) -> Result<()> {
    store
        .register_named_graph(&GraphName::NamedNode(graph.clone()))
        .map_err(|e| anyhow!("{e}"))
}

/// Delete every quad in the graph(s) named by `target` (implements both
/// `CLEAR` and `DROP` — see module docs for why they're currently identical).
fn clear_graph_target<S: QuadStore>(store: &Arc<S>, target: &GraphTarget) -> Result<()> {
    if let GraphTarget::NamedNode(n) = target {
        let g = GraphName::NamedNode(n.clone());
        if !named_graph_exists(store, &g)? {
            return Err(anyhow!("no such graph <{}>", n.as_str()));
        }
    }
    for g in resolve_graph_target(store, target)? {
        clear_one_graph(store, &g)?;
    }
    Ok(())
}

/// Delete every quad in graph `g` — the primitive behind SPARQL `CLEAR`/`DROP`
/// (see [`clear_graph_target`]) and reused directly by the SPARQL 1.1 Graph
/// Store HTTP Protocol's `DELETE` verb and `PUT`'s replace-semantics
/// (`nova-server`'s `/store` handlers), so the graph-emptying logic lives in
/// exactly one place.
pub fn clear_graph<S: QuadStore>(store: &Arc<S>, g: &GraphName) -> Result<()> {
    clear_one_graph(store, g)
}

fn clear_one_graph<S: QuadStore>(store: &Arc<S>, g: &GraphName) -> Result<()> {

    let stored: Vec<_> = store
        .quads_for_pattern(None, None, None, Some(g))
        .map_err(|e| anyhow!("{e}"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("{e}"))?;
    for sq in stored {
        let subject = match sq.subject {
            Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n),
            Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b),
            // Quoted-triple subject: not representable in a plain `Quad`
            // (write-path limitation of oxrdf 0.3), so it can't be removed
            // via `QuadStore::remove` — skip it.
            _ => continue,
        };
        let quad = Quad::new(subject, sq.predicate, sq.object, sq.graph_name);
        store.remove(&quad).map_err(|e| anyhow!("{e}"))?;
    }
    Ok(())
}

/// Returns `true` if `g` is either explicitly registered or has at least one
/// quad — used to decide whether a non-`SILENT` `CLEAR GRAPH <iri>` /
/// `DROP GRAPH <iri>` on a specific named graph should error.
fn named_graph_exists<S: QuadStore>(store: &Arc<S>, g: &GraphName) -> Result<bool> {
    if matches!(g, GraphName::DefaultGraph) {
        return Ok(true);
    }
    for kg in store.known_named_graphs().map_err(|e| anyhow!("{e}"))? {
        if &kg.map_err(|e| anyhow!("{e}"))? == g {
            return Ok(true);
        }
    }
    Ok(store
        .quads_for_pattern(None, None, None, Some(g))
        .map_err(|e| anyhow!("{e}"))?
        .next()
        .is_some())
}

/// Expand a `GraphTarget` into the concrete graph(s) it refers to.
fn resolve_graph_target<S: QuadStore>(
    store: &Arc<S>,
    target: &GraphTarget,
) -> Result<Vec<GraphName>> {
    Ok(match target {
        GraphTarget::DefaultGraph => vec![GraphName::DefaultGraph],
        GraphTarget::NamedNode(n) => vec![GraphName::NamedNode(n.clone())],
        GraphTarget::NamedGraphs => all_named_graphs(store)?,
        GraphTarget::AllGraphs => {
            let mut graphs = vec![GraphName::DefaultGraph];
            graphs.extend(all_named_graphs(store)?);
            graphs
        }
    })
}

/// Enumerate every named graph, merging explicitly-registered graphs
/// (`known_named_graphs`) with graphs inferred from quad-scanning — mirrors
/// `StoreDataset::named_graphs`'s merge strategy in `dataset.rs`.
fn all_named_graphs<S: QuadStore>(store: &Arc<S>) -> Result<Vec<GraphName>> {
    let mut seen = std::collections::HashSet::new();
    let mut graphs = Vec::new();
    for g in store.known_named_graphs().map_err(|e| anyhow!("{e}"))? {
        let g = g.map_err(|e| anyhow!("{e}"))?;
        if !matches!(g, GraphName::DefaultGraph) && seen.insert(format!("{g}")) {
            graphs.push(g);
        }
    }
    let all = store
        .quads_for_pattern(None, None, None, None)
        .map_err(|e| anyhow!("{e}"))?;
    for sq in all.flatten() {
        if !matches!(sq.graph_name, GraphName::DefaultGraph)
            && seen.insert(format!("{}", sq.graph_name))
        {
            graphs.push(sq.graph_name);
        }
    }
    Ok(graphs)
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::Quad as CoreQuad;
    use oxigraph_nova_storage_memory::MemoryStore;
    use oxrdf::{GraphName as OxGraphName, Literal, NamedNode as OxNamedNode};
    use spargebra::SparqlParser;

    fn parse(update: &str) -> Update {
        SparqlParser::new().parse_update(update).unwrap()
    }

    fn iri(s: &str) -> OxNamedNode {
        OxNamedNode::new_unchecked(s)
    }

    #[test]
    fn insert_data_basic() {
        let store = Arc::new(MemoryStore::new());
        let update = parse(r#"INSERT DATA { <http://ex/alice> <http://ex/name> "Alice" }"#);
        execute_update(&store, &update).unwrap();
        assert!(
            store
                .contains(&CoreQuad::new(
                    iri("http://ex/alice"),
                    iri("http://ex/name"),
                    Term::Literal(Literal::new_simple_literal("Alice")),
                    OxGraphName::DefaultGraph,
                ))
                .unwrap()
        );
    }

    #[test]
    fn delete_data_basic() {
        let store = Arc::new(MemoryStore::new());
        store
            .insert(&CoreQuad::new(
                iri("http://ex/alice"),
                iri("http://ex/name"),
                Term::Literal(Literal::new_simple_literal("Alice")),
                OxGraphName::DefaultGraph,
            ))
            .unwrap();

        let update = parse(r#"DELETE DATA { <http://ex/alice> <http://ex/name> "Alice" }"#);
        execute_update(&store, &update).unwrap();
        assert_eq!(store.len().unwrap(), 0);
    }

    #[test]
    fn delete_where_removes_matching() {
        let store = Arc::new(MemoryStore::new());
        store
            .insert(&CoreQuad::new(
                iri("http://ex/alice"),
                iri("http://ex/name"),
                Term::Literal(Literal::new_simple_literal("Alice")),
                OxGraphName::DefaultGraph,
            ))
            .unwrap();
        store
            .insert(&CoreQuad::new(
                iri("http://ex/bob"),
                iri("http://ex/name"),
                Term::Literal(Literal::new_simple_literal("Bob")),
                OxGraphName::DefaultGraph,
            ))
            .unwrap();

        let update = parse(r#"DELETE WHERE { ?s <http://ex/name> "Alice" }"#);
        execute_update(&store, &update).unwrap();
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn insert_delete_where_moves_data() {
        let store = Arc::new(MemoryStore::new());
        store
            .insert(&CoreQuad::new(
                iri("http://ex/alice"),
                iri("http://ex/age"),
                Term::Literal(Literal::new_typed_literal(
                    "30",
                    iri("http://www.w3.org/2001/XMLSchema#integer"),
                )),
                OxGraphName::DefaultGraph,
            ))
            .unwrap();

        let update = parse(
            r#"DELETE { ?s <http://ex/age> ?a }
               INSERT { ?s <http://ex/ageOld> ?a }
               WHERE { ?s <http://ex/age> ?a }"#,
        );
        execute_update(&store, &update).unwrap();
        assert_eq!(store.len().unwrap(), 1);
        assert!(
            store
                .contains(&CoreQuad::new(
                    iri("http://ex/alice"),
                    iri("http://ex/ageOld"),
                    Term::Literal(Literal::new_typed_literal(
                        "30",
                        iri("http://www.w3.org/2001/XMLSchema#integer"),
                    )),
                    OxGraphName::DefaultGraph,
                ))
                .unwrap()
        );
    }

    #[test]
    fn clear_default_removes_only_default_graph() {
        let store = Arc::new(MemoryStore::new());
        store
            .insert(&CoreQuad::new(
                iri("http://ex/a"),
                iri("http://ex/p"),
                Term::Literal(Literal::new_simple_literal("v")),
                OxGraphName::DefaultGraph,
            ))
            .unwrap();
        store
            .insert(&CoreQuad::new(
                iri("http://ex/a"),
                iri("http://ex/p"),
                Term::Literal(Literal::new_simple_literal("v")),
                OxGraphName::NamedNode(iri("http://ex/g")),
            ))
            .unwrap();

        let update = parse("CLEAR DEFAULT");
        execute_update(&store, &update).unwrap();
        assert_eq!(store.len().unwrap(), 1); // named graph quad remains
    }

    #[test]
    fn create_graph_registers_it() {
        let store = Arc::new(MemoryStore::new());
        let update = parse("CREATE GRAPH <http://ex/newgraph>");
        execute_update(&store, &update).unwrap();
        let graphs: Vec<_> = store
            .known_named_graphs()
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(graphs.contains(&OxGraphName::NamedNode(iri("http://ex/newgraph"))));
    }

    #[test]
    fn drop_missing_graph_errors_without_silent() {
        let store = Arc::new(MemoryStore::new());
        let update = parse("DROP GRAPH <http://ex/missing>");
        assert!(execute_update(&store, &update).is_err());
    }

    #[test]
    fn drop_silent_missing_graph_ok() {
        let store = Arc::new(MemoryStore::new());
        let update = parse("DROP SILENT GRAPH <http://ex/missing>");
        assert!(execute_update(&store, &update).is_ok());
    }

    #[test]
    fn load_without_silent_errors() {
        let store = Arc::new(MemoryStore::new());
        let update = parse("LOAD <http://example.com/data.ttl>");
        assert!(execute_update(&store, &update).is_err());
    }

    #[test]
    fn load_silent_ok() {
        let store = Arc::new(MemoryStore::new());
        let update = parse("LOAD SILENT <http://example.com/data.ttl>");
        assert!(execute_update(&store, &update).is_ok());
    }
}
