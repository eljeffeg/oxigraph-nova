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
//! `LoudsStore`'s single-`Mutex<LoudsStoreInner>` design does not currently
//! expose a way to hold the lock across multiple calls, and adding that is a
//! larger architectural change (see `LoudsStore`'s module doc comment on its
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
//! `LOAD <iri> [INTO GRAPH <g>]` fetches the remote IRI over HTTP (content
//! negotiation via an `Accept` header listing every RDF format `oxrdfio`
//! understands), resolves the response's `Content-Type` to an
//! [`oxrdfio::RdfFormat`] (falling back to the IRI's own extension, then to
//! Turtle), parses it into quads tagged with the destination graph, and
//! inserts them. This requires this crate's opt-in `http-client` feature
//! (see [`do_load`]); without it, `LOAD` always errors (`LOAD SILENT` still
//! succeeds as a no-op either way). Bulk data loading is also available
//! through the SPARQL 1.1 Graph Store HTTP Protocol (`PUT`/`POST /store`).

use crate::dataset::StoreDataset;
use crate::evaluator::Evaluator;
use crate::solution::Solution;
use anyhow::{Result, anyhow};
use oxigraph_nova_core::{QuadOp, QuadStore, QuadStoreExt};
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
pub fn execute_update<S: QuadStore + ?Sized + 'static>(
    store: &Arc<S>,
    update: &Update,
) -> Result<()> {
    for op in &update.operations {
        execute_operation(store, op)?;
    }
    Ok(())
}

fn execute_operation<S: QuadStore + ?Sized + 'static>(
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
            destination,
        } => do_load(store, source, destination).or_else(|e| if *silent { Ok(()) } else { Err(e) }),
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

/// `LOAD <source> [INTO GRAPH <destination>]`: fetch, parse, and insert.
///
/// Real HTTP-backed implementation, only compiled with the `http-client`
/// feature. GET `source` with an `Accept` header listing every RDF media
/// type `oxrdfio` understands (so a well-behaved server can return whatever
/// format it prefers to serialize), resolve the response's `Content-Type` to
/// an [`oxrdfio::RdfFormat`] — falling back to the source IRI's own file
/// extension, then to Turtle if neither resolves — parse the body into
/// quads tagged with `destination` as their (default) graph, and insert them
/// as one batch.
#[cfg(feature = "http-client")]
fn do_load<S: QuadStore + ?Sized + 'static>(
    store: &Arc<S>,
    source: &NamedNode,
    destination: &spargebra::term::GraphName,
) -> Result<()> {
    use oxrdfio::{RdfFormat, RdfParser};

    const ACCEPT: &str = "text/turtle, application/n-triples, application/n-quads, \
                           application/trig, application/rdf+xml, application/ld+json, \
                           text/n3;q=0.9, */*;q=0.1";

    let request = oxhttp::model::Request::builder()
        .method(oxhttp::model::Method::GET)
        .uri(source.as_str())
        .header(oxhttp::model::header::ACCEPT, ACCEPT)
        .body(())
        .map_err(|e| anyhow!("LOAD <{}>: invalid request: {e}", source.as_str()))?;
    let response = oxhttp::Client::new()
        .with_redirection_limit(5)
        .request(request)
        .map_err(|e| anyhow!("LOAD <{}>: fetch failed: {e}", source.as_str()))?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "LOAD <{}>: server returned HTTP {}",
            source.as_str(),
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
            "LOAD <{}>: error reading response body: {e}",
            source.as_str()
        )
    })?;

    let format = content_type
        .as_deref()
        .and_then(RdfFormat::from_media_type)
        .or_else(|| {
            // Isolate the last path segment (ignoring any query string/
            // fragment) before looking for a file extension, so a dot in the
            // authority (e.g. `example.com`) is never mistaken for one: for
            // `http://example.com/data.ttl` this yields `data.ttl` -> `ttl`,
            // and for `http://example.com/data` (no path extension at all)
            // this correctly yields `None` rather than spuriously matching
            // `com/data`.
            source
                .as_str()
                .rsplit(['?', '#'])
                .next_back()
                .and_then(|path_and_beyond| path_and_beyond.rsplit('/').next())
                .and_then(|last_segment| last_segment.rsplit('.').next())
                .and_then(RdfFormat::from_extension)
        })
        .unwrap_or(RdfFormat::Turtle);

    let destination_graph = spargebra_graph_name_to_oxrdf(destination);
    let quads = RdfParser::from_format(format)
        .with_default_graph(destination_graph)
        .for_reader(body.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("LOAD <{}>: RDF parse error: {e}", source.as_str()))?;
    store.extend(quads).map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

/// Stub compiled when the `http-client` feature is disabled: `LOAD` always
/// errors (its caller still lets `LOAD SILENT` swallow this and succeed as a
/// no-op).
#[cfg(not(feature = "http-client"))]
fn do_load<S: QuadStore + ?Sized + 'static>(
    _store: &Arc<S>,
    source: &NamedNode,
    _destination: &spargebra::term::GraphName,
) -> Result<()> {
    Err(anyhow!(
        "LOAD <{}> is not supported by this build (compiled without the `http-client` feature); \
         use the Graph Store HTTP Protocol (PUT/POST /store) to upload data instead, or use \
         LOAD SILENT to ignore this error",
        source.as_str()
    ))
}

fn insert_data<S: QuadStore + ?Sized>(
    store: &Arc<S>,
    data: &[spargebra::term::Quad],
) -> Result<()> {
    // All-insert batch: `QuadStoreExt::extend` boxes the iterator and hands
    // it to `QuadStore::extend_boxed`, which backends with a WAL/lock (e.g.
    // `LoudsStore`) override to append the whole batch in one `fsync` instead
    // of one per quad — see `QuadStore::apply_batch`'s doc comment for the
    // same rationale applied to mixed insert/remove batches below.
    let quads = data.iter().map(|q| {
        Quad::new(
            q.subject.clone(),
            q.predicate.clone(),
            q.object.clone(),
            spargebra_graph_name_to_oxrdf(&q.graph_name),
        )
    });
    store.extend(quads).map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

fn delete_data<S: QuadStore + ?Sized>(store: &Arc<S>, data: &[GroundQuad]) -> Result<()> {
    // All-remove batch: use `apply_batch` so a WAL-backed store applies the
    // whole batch under a single lock acquisition / fsync instead of one per
    // quad (see `QuadStore::apply_batch`'s doc comment).
    let ops: Vec<QuadOp> = data
        .iter()
        .map(|q| {
            QuadOp::Remove(Quad::new(
                NamedOrBlankNode::NamedNode(q.subject.clone()),
                q.predicate.clone(),
                ground_term_to_term(&q.object),
                spargebra_graph_name_to_oxrdf(&q.graph_name),
            ))
        })
        .collect();
    store.apply_batch(&ops).map_err(|e| anyhow!("{e}"))?;
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

fn delete_insert<S: QuadStore + ?Sized + 'static>(
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
    let (_, solutions) = Evaluator::new(&dataset)
        .evaluate(&synthetic)?
        .into_solutions_vec()?;

    // Collect every row's deletes-then-inserts into a single ops batch and
    // apply it in one `apply_batch` call, instead of issuing one `remove`/
    // `insert` call per instantiated quad. This preserves the existing
    // per-row DELETE-before-INSERT order (each row's deletes are pushed
    // before that row's inserts), which continues to guarantee a
    // just-inserted quad from row N can't be observed by row N's own delete
    // pass — that guarantee never actually depended on deletes/inserts being
    // applied to the store *immediately* (the WHERE clause was already fully
    // evaluated into `solutions` before any mutation begins), so batching
    // every row into one call is behavior-preserving while cutting the
    // number of lock acquisitions/fsyncs on a WAL-backed store from O(ops)
    // to 1. See `QuadStore::apply_batch`'s doc comment for the
    // durability/atomicity contract this relies on.
    let mut ops: Vec<QuadOp> = Vec::new();
    for sol in &solutions {
        for gqp in delete {
            if let Some(quad) = instantiate_ground_quad_pattern(gqp, sol) {
                ops.push(QuadOp::Remove(quad));
            }
        }
        // Fresh blank-node map per solution row (SPARQL 1.1 § 3.1.3, the
        // INSERT-template analogue of CONSTRUCT's § 18.2.4 semantics).
        let mut bnode_map: HashMap<BlankNode, BlankNode> = HashMap::new();
        for qp in insert {
            if let Some(quad) = instantiate_quad_pattern(qp, sol, &mut bnode_map) {
                ops.push(QuadOp::Insert(quad));
            }
        }
    }
    store.apply_batch(&ops).map_err(|e| anyhow!("{e}"))?;
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
        TermPattern::Triple(inner) => instantiate_triple_pattern_for_insert(inner, sol, bnode_map)
            .map(|t| Term::Triple(Box::new(t))),
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

fn create_graph<S: QuadStore + ?Sized>(store: &Arc<S>, graph: &NamedNode) -> Result<()> {
    store
        .register_named_graph(&GraphName::NamedNode(graph.clone()))
        .map_err(|e| anyhow!("{e}"))
}

/// Delete every quad in the graph(s) named by `target` (implements both
/// `CLEAR` and `DROP` — see module docs for why they're currently identical).
fn clear_graph_target<S: QuadStore + ?Sized>(store: &Arc<S>, target: &GraphTarget) -> Result<()> {
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
pub fn clear_graph<S: QuadStore + ?Sized>(store: &Arc<S>, g: &GraphName) -> Result<()> {
    clear_one_graph(store, g)
}

fn clear_one_graph<S: QuadStore + ?Sized>(store: &Arc<S>, g: &GraphName) -> Result<()> {
    let stored: Vec<_> = store
        .quads_for_pattern(None, None, None, Some(g))
        .map_err(|e| anyhow!("{e}"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("{e}"))?;
    // All-remove batch: apply every removal for this graph in one
    // `apply_batch` call rather than one `remove` call per quad (see
    // `QuadStore::apply_batch`'s doc comment) — matters most for `CLEAR
    // DEFAULT`/`CLEAR ALL`/`DROP ALL` on a large graph.
    let mut ops: Vec<QuadOp> = Vec::with_capacity(stored.len());
    for sq in stored {
        let subject = match sq.subject.as_ref() {
            Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n.clone()),
            Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b.clone()),
            // Quoted-triple subject: not representable in a plain `Quad`
            // (write-path limitation of oxrdf 0.3), so it can't be removed
            // via `QuadStore::remove` — skip it.
            _ => continue,
        };
        let object = Arc::unwrap_or_clone(sq.object);
        ops.push(QuadOp::Remove(Quad::new(
            subject,
            sq.predicate,
            object,
            sq.graph_name,
        )));
    }
    store.apply_batch(&ops).map_err(|e| anyhow!("{e}"))?;

    Ok(())
}

/// Returns `true` if `g` is either explicitly registered or has at least one
/// quad — used to decide whether a non-`SILENT` `CLEAR GRAPH <iri>` /
/// `DROP GRAPH <iri>` on a specific named graph should error.
fn named_graph_exists<S: QuadStore + ?Sized>(store: &Arc<S>, g: &GraphName) -> Result<bool> {
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
fn resolve_graph_target<S: QuadStore + ?Sized>(
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
fn all_named_graphs<S: QuadStore + ?Sized>(store: &Arc<S>) -> Result<Vec<GraphName>> {
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
    use oxigraph_nova_engine_memory::MemoryStore;
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

    // Without the `http-client` feature, `LOAD` always fails (no fetch
    // client compiled in) — verify the unreachable-network-free stub
    // behavior directly rather than depending on `example.com` being up.
    #[cfg(not(feature = "http-client"))]
    #[test]
    fn load_without_silent_errors() {
        let store = Arc::new(MemoryStore::new());
        let update = parse("LOAD <http://example.com/data.ttl>");
        assert!(execute_update(&store, &update).is_err());
    }

    #[cfg(not(feature = "http-client"))]
    #[test]
    fn load_silent_ok() {
        let store = Arc::new(MemoryStore::new());
        let update = parse("LOAD SILENT <http://example.com/data.ttl>");
        assert!(execute_update(&store, &update).is_ok());
    }

    // With the `http-client` feature, exercise the real fetch+parse+insert
    // path against a local `oxhttp::Server` fixture instead of the network.
    #[cfg(feature = "http-client")]
    mod http_client_tests {
        use super::*;
        use oxhttp::Server;
        use oxhttp::model::header::CONTENT_TYPE;
        use oxhttp::model::{Body, Response, StatusCode};
        use std::net::{Ipv4Addr, SocketAddr};
        use std::thread::sleep;
        use std::time::Duration;

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
            sleep(Duration::from_millis(100)); // let the listener thread start
            server
        }

        /// Like [`spawn_fixture`], but the response carries **no**
        /// `Content-Type` header at all, forcing `do_load` to fall back to
        /// resolving the RDF format from `source`'s own URL path extension.
        fn spawn_fixture_no_content_type(port: u16, body: &'static str) -> oxhttp::ListeningServer {
            let server = Server::new(move |_request| {
                Response::builder()
                    .status(StatusCode::OK)
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
        fn load_fetches_parses_and_inserts() {
            let _fixture = spawn_fixture(
                18781,
                "text/turtle",
                r#"<http://ex/alice> <http://ex/name> "Alice" ."#,
            );
            let store = Arc::new(MemoryStore::new());
            let update = parse("LOAD <http://127.0.0.1:18781/data.ttl>");
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
        fn load_into_graph_tags_destination() {
            let _fixture = spawn_fixture(
                18782,
                "text/turtle",
                r#"<http://ex/bob> <http://ex/name> "Bob" ."#,
            );
            let store = Arc::new(MemoryStore::new());
            let update = parse("LOAD <http://127.0.0.1:18782/data.ttl> INTO GRAPH <http://ex/g>");
            execute_update(&store, &update).unwrap();
            assert!(
                store
                    .contains(&CoreQuad::new(
                        iri("http://ex/bob"),
                        iri("http://ex/name"),
                        Term::Literal(Literal::new_simple_literal("Bob")),
                        OxGraphName::NamedNode(iri("http://ex/g")),
                    ))
                    .unwrap()
            );
        }

        #[test]
        fn load_falls_back_to_url_extension_when_no_content_type() {
            // No `Content-Type` header at all, and the URL path's own
            // extension (`.ttl`) must be used to resolve the RDF format —
            // this also exercises that the fallback correctly ignores the
            // dot in `127.0.0.1`'s port-bearing authority and only looks at
            // the last path segment.
            let _fixture = spawn_fixture_no_content_type(
                18784,
                r#"<http://ex/carol> <http://ex/name> "Carol" ."#,
            );
            let store = Arc::new(MemoryStore::new());
            let update = parse("LOAD <http://127.0.0.1:18784/path/data.ttl>");
            execute_update(&store, &update).unwrap();
            assert!(
                store
                    .contains(&CoreQuad::new(
                        iri("http://ex/carol"),
                        iri("http://ex/name"),
                        Term::Literal(Literal::new_simple_literal("Carol")),
                        OxGraphName::DefaultGraph,
                    ))
                    .unwrap()
            );
        }

        #[test]
        fn load_unreachable_without_silent_errors() {
            let store = Arc::new(MemoryStore::new());
            // Port 18783 has no fixture bound to it.
            let update = parse("LOAD <http://127.0.0.1:18783/data.ttl>");
            assert!(execute_update(&store, &update).is_err());
        }

        #[test]
        fn load_unreachable_silent_ok() {
            let store = Arc::new(MemoryStore::new());
            let update = parse("LOAD SILENT <http://127.0.0.1:18783/data.ttl>");
            assert!(execute_update(&store, &update).is_ok());
        }
    }
}
