//! Dataset trait — the seam between storage and the SPARQL evaluator.
//!
//! # Design rationale
//!
//! The trait operates at the *quad* level (triple + graph name) rather than
//! pure triples, which is required to handle:
//!   - `GRAPH <iri> { ... }` patterns (SPARQL 1.1 § 13)
//!   - `FROM` / `FROM NAMED` dataset clauses
//!   - `GRAPH ?g { ... }` variable binding
//!
//! Omitting the graph dimension from this seam would make the W3C SPARQL 1.1
//! graph/dataset test groups fail and would require a breaking change later.
//!
//! # Iterator shape
//!
//! [`Dataset::find_quads`] returns a lazy [`QuadIter`] rather than a
//! materialised `Vec`.  This keeps the nested-loop evaluator working
//! with no extra allocation for small results, and allows WCOJ trie
//! iterators to be dropped in behind the same `QuadIter<'a>` alias —
//! adding a `seek` method would require only a new *supertrait*, not a
//! change to this interface.
//!
//! # Implementations
//!
//! | Type | Purpose |
//! |---|---|
//! | [`InMemoryDataset`] | Unit tests — no `QuadStore` overhead |
//! | [`StoreDataset`] | Bridges any [`QuadStore`] into this trait |

use anyhow::Result;
use oxigraph_nova_core::{GraphName, QuadStore, Term};
use std::sync::Arc;

// ── Pattern types ────────────────────────────────────────────────────────────

/// A single position in a triple/quad pattern: either a concrete RDF term or
/// an unbound variable slot (wildcard for lookup purposes).
#[derive(Debug, Clone, PartialEq)]
pub enum PatternTerm {
    /// A concrete, bound RDF term.
    Bound(Term),
    /// An unbound variable — matches any term at this position.
    Variable,
}

/// Specifies which graph(s) a [`QuadPattern`] searches over.
///
/// Named to avoid collision with [`spargebra::algebra::GraphPattern`], which
/// is the SPARQL algebra node type.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphSelector {
    /// Default graph only (query has no `GRAPH` clause, or uses `GRAPH <>`).
    Default,
    /// A specific named or default graph: `GRAPH <iri> { }` or `GRAPH _:b { }`.
    Named(GraphName),
    /// Any named (non-default) graph: `GRAPH ?g { }` — iterates all named graphs,
    /// binding the graph IRI to `?g` at the evaluator level.
    AnyNamed,
    /// All graphs combined (default ∪ every named graph).
    /// Used when spargebra's `dataset.union_default_graph` is set.
    Union,
    /// The RDF merge of exactly these named graphs, treated as *the* default
    /// graph for pattern matching. This implements the SPARQL 1.1 §13.1
    /// `FROM <iri>` dataset clause: when a query specifies one or more
    /// `FROM` graphs, the "default graph" used to evaluate the
    /// query's top-level pattern (everything outside an explicit `GRAPH`
    /// block) is the merge of exactly those graphs — not the store's actual
    /// default graph, and not every named graph in the store. An empty list
    /// (query specifies `FROM NAMED` only, no `FROM`) correctly matches no
    /// quads, per spec (the default graph is then empty).
    UnionOf(Vec<GraphName>),
}

/// A quad-level lookup pattern: three [`PatternTerm`]s plus a [`GraphSelector`].
#[derive(Debug, Clone)]
pub struct QuadPattern {
    pub subject: PatternTerm,
    pub predicate: PatternTerm,
    pub object: PatternTerm,
    pub graph: GraphSelector,
}

impl QuadPattern {
    /// Convenience constructor for default-graph BGP patterns.
    pub fn default_graph(s: PatternTerm, p: PatternTerm, o: PatternTerm) -> Self {
        Self {
            subject: s,
            predicate: p,
            object: o,
            graph: GraphSelector::Default,
        }
    }

    /// Fully-wildcard pattern over a specific graph.
    pub fn all_in(graph: GraphSelector) -> Self {
        Self {
            subject: PatternTerm::Variable,
            predicate: PatternTerm::Variable,
            object: PatternTerm::Variable,
            graph,
        }
    }
}

// ── Match result ─────────────────────────────────────────────────────────────

/// A single quad returned by [`Dataset::find_quads`].
#[derive(Debug, Clone, PartialEq)]
pub struct QuadMatch {
    pub subject: Term,
    pub predicate: Term,
    pub object: Term,
    pub graph_name: GraphName,
}

/// Lazy iterator over [`QuadMatch`] results from a dataset query.
/// Lifetime `'a` is tied to the borrow of the [`Dataset`].
pub type QuadIter<'a> = Box<dyn Iterator<Item = Result<QuadMatch>> + 'a>;

// ── DatasetLftjSource trait ────────────────────────────────────────────────────

/// Optional Leapfrog Triejoin (LFTJ) / Worst-Case-Optimal-Join acceleration
/// capability for a [`Dataset`].
///
/// This mirrors [`oxigraph_nova_core::LftjSource`] — the equivalent
/// supertrait on the storage-level [`QuadStore`] trait — but cannot literally
/// be the *same* trait: `Dataset`'s LFTJ methods key on a
/// [`GraphSelector`] (a query-level abstraction that can mean "the default
/// graph", "any named graph", or a `FROM`-clause union of several named
/// graphs), whereas `QuadStore`'s LFTJ methods key on a concrete numeric
/// `u8` graph id. [`StoreDataset`] is exactly the adapter that bridges the
/// two: it resolves a `GraphSelector` down to a `u8` graph id (via
/// `QuadStore::lftj_graph_id`) before delegating.
///
/// Every method here is defaulted to "not supported"/"unknown", so any
/// `Dataset` implementor can opt out entirely with an empty
/// `impl DatasetLftjSource for MyDataset {}` block. Only [`StoreDataset`]
/// wrapping an LFTJ-capable `QuadStore` (i.e. Ring-backed/CLTJ stores)
/// currently overrides these to enable the accelerated join path.
pub trait DatasetLftjSource: Send + Sync {
    /// Returns `true` if this dataset supports Leapfrog Triejoin acceleration.
    ///
    /// When `false`, the SPARQL evaluator uses the nested-loop path.
    fn supports_lftj(&self) -> bool {
        false
    }

    /// Returns `true` if the underlying store has uncompacted delta writes.
    ///
    /// LFTJ requires a fully-sorted Ring; if delta is non-empty the evaluator
    /// falls back to nested-loop.
    fn lftj_has_delta(&self) -> bool {
        false
    }

    /// Returns `true` if this backend provides meaningful (non-`u64::MAX`)
    /// estimates for [`lftj_estimate_count`][DatasetLftjSource::lftj_estimate_count].
    ///
    /// Non-CLTJ backends (MemoryStore, InMemoryDataset) should return `false`
    /// so `lftj_step` skips the VEO heap-allocation + sort entirely.
    /// Only CLTJ backends that implement
    /// at least a vocab-size heuristic (or the full §3.6 leaf-descendant
    /// predictor) should return `true`.
    fn supports_veo_estimates(&self) -> bool {
        false
    }

    /// Intern a term to its numeric TermId for LFTJ seek operations.
    ///
    /// `graph` is passed so implementations can handle graph-local namespaces
    /// (though the current Ring uses a global dictionary, so it is ignored).
    ///
    /// Returns `None` if the term is not in the dictionary (no matches possible).
    fn lftj_intern_term(&self, _term: &Term, _graph: &GraphSelector) -> Option<u64> {
        None
    }

    /// Decode a numeric TermId back to an RDF Term.
    fn lftj_decode_term(&self, _id: u64) -> Option<Term> {
        None
    }

    /// Estimate the number of distinct values for `target_field` (0=S, 1=P, 2=O)
    /// given the other bound fields — used by the adaptive VEO predictor in LFTJ.
    ///
    /// Returns `u64::MAX` when the estimate is unavailable (non-CLTJ backends or
    /// graph not found).  CLTJ backends return the vocabulary size of the target
    /// field scaled by selectivity.  See [`GraphRing::estimate_count`].
    fn lftj_estimate_count(
        &self,
        _s: Option<u64>,
        _p: Option<u64>,
        _o: Option<u64>,
        _target_field: usize,
        _graph: &GraphSelector,
    ) -> u64 {
        u64::MAX
    }

    /// Zero-allocation cardinality probe for adaptive VEO — see
    /// `QuadStore::lftj_real_count`. Returns `None` when unsupported
    /// (non-CLTJ backends, or `AnyNamed`/`Union` graph selectors), in which
    /// case the caller falls back to `lftj_estimate_count`.
    fn lftj_real_count(
        &self,
        _s: Option<u64>,
        _p: Option<u64>,
        _o: Option<u64>,
        _target_field: usize,
        _graph: &GraphSelector,
    ) -> Option<u64> {
        None
    }

    /// Return a seek-capable [`TrieIterator`][oxigraph_nova_core::TrieIterator] for
    /// one join variable, given currently-bound field values.
    ///
    /// `s`, `p`, `o`: `Some(id)` = bound to this TermId; `None` = unbound
    ///   (either the current target or an as-yet-unbound later variable).
    /// `target_field`: 0 = subject, 1 = predicate, 2 = object — identifies which
    ///   `None` slot to iterate.
    /// `graph`: the active graph selector.
    ///
    /// Returns `None` if LFTJ is not supported or the graph isn't available.
    fn lftj_join_scan(
        &self,
        _s: Option<u64>,
        _p: Option<u64>,
        _o: Option<u64>,
        _target_field: usize,
        _graph: &GraphSelector,
    ) -> Option<Box<dyn oxigraph_nova_core::TrieIterator>> {
        None
    }
}

// ── Dataset trait ─────────────────────────────────────────────────────────────

/// The storage-agnostic interface the SPARQL evaluator uses.
///
/// Every storage backend is accessed through this trait; the evaluator never
/// knows whether it is talking to a `Vec`, a hypertrie, or a remote endpoint.
pub trait Dataset: Send + Sync + DatasetLftjSource {
    // ── Core query interface ──────────────────────────────────────────────────

    /// Iterate lazily over all quads matching the given pattern.
    ///
    /// Pattern terms set to [`PatternTerm::Variable`] match any value.
    /// The [`GraphSelector`] controls which graph(s) are searched.
    fn find_quads<'a>(&'a self, pattern: &QuadPattern) -> Result<QuadIter<'a>>;

    /// Fast existence check for a fully-bound quad.
    ///
    /// Backends can override this with an indexed `contains` rather than
    /// iterating; the default delegates to `find_quads`.
    fn contains_quad(&self, s: &Term, p: &Term, o: &Term, g: &GraphName) -> Result<bool> {
        let graph = if *g == GraphName::DefaultGraph {
            GraphSelector::Default
        } else {
            GraphSelector::Named(g.clone())
        };
        let pattern = QuadPattern {
            subject: PatternTerm::Bound(s.clone()),
            predicate: PatternTerm::Bound(p.clone()),
            object: PatternTerm::Bound(o.clone()),
            graph,
        };
        Ok(self.find_quads(&pattern)?.next().is_some())
    }

    /// Enumerate the IRIs of all named graphs in this dataset.
    ///
    /// Does *not* include the default graph; returns an empty iterator if
    /// the dataset contains only default-graph triples.
    fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>>;
}

// ── Helper predicates ─────────────────────────────────────────────────────────

fn term_matches(pattern: &PatternTerm, value: &Term) -> bool {
    match pattern {
        PatternTerm::Variable => true,
        PatternTerm::Bound(v) => v == value,
    }
}

fn graph_matches(selector: &GraphSelector, graph: &GraphName) -> bool {
    match selector {
        GraphSelector::Default => *graph == GraphName::DefaultGraph,
        GraphSelector::Named(g) => g == graph,
        GraphSelector::AnyNamed => !matches!(graph, GraphName::DefaultGraph),
        GraphSelector::Union => true,
        GraphSelector::UnionOf(graphs) => graphs.contains(graph),
    }
}

// ── InMemoryDataset ──────────────────────────────────────────────────────────

/// Lightweight dataset for unit tests — stores quads in a `Vec`.
///
/// Bypass the full `QuadStore` stack when you just need to write a few quads
/// inline in a test.  For the W3C conformance harness, use [`StoreDataset`]
/// wrapping a `MemoryStore` so the full storage path is exercised.
#[derive(Debug, Clone, Default)]
pub struct InMemoryDataset {
    quads: Vec<(Term, Term, Term, GraphName)>,
}

impl InMemoryDataset {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a quad to any graph.
    pub fn add(&mut self, s: Term, p: Term, o: Term, g: GraphName) {
        self.quads.push((s, p, o, g));
    }

    /// Convenience: add a triple to the default graph.
    pub fn add_default(&mut self, s: Term, p: Term, o: Term) {
        self.add(s, p, o, GraphName::DefaultGraph);
    }
}

impl DatasetLftjSource for InMemoryDataset {}

impl Dataset for InMemoryDataset {
    fn find_quads<'a>(&'a self, pattern: &QuadPattern) -> Result<QuadIter<'a>> {
        let pat = pattern.clone();
        let iter = self
            .quads
            .iter()
            .filter(move |(s, p, o, g)| {
                term_matches(&pat.subject, s)
                    && term_matches(&pat.predicate, p)
                    && term_matches(&pat.object, o)
                    && graph_matches(&pat.graph, g)
            })
            .map(|(s, p, o, g)| {
                Ok(QuadMatch {
                    subject: s.clone(),
                    predicate: p.clone(),
                    object: o.clone(),
                    graph_name: g.clone(),
                })
            });
        Ok(Box::new(iter))
    }

    fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
        let mut seen = std::collections::HashSet::new();
        let graphs: Vec<GraphName> = self
            .quads
            .iter()
            .filter_map(|(_, _, _, g)| match g {
                GraphName::DefaultGraph => None,
                g if seen.insert(format!("{g}")) => Some(g.clone()),
                _ => None,
            })
            .collect();
        Ok(Box::new(graphs.into_iter().map(Ok)))
    }
}

// ── StoreDataset ──────────────────────────────────────────────────────────────

/// Bridges any [`QuadStore`] implementation into the [`Dataset`] trait.
///
/// This is the adapter that wires `MemoryStore` (and later `HypertrieStore`)
/// into the SPARQL evaluator — neither side needs to know about the other.
pub struct StoreDataset<S: QuadStore> {
    store: Arc<S>,
}

impl<S: QuadStore> StoreDataset<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self { store }
    }
}

// ── Private helpers for StoreDataset ─────────────────────────────────────────

fn to_named_node(pt: &PatternTerm) -> Option<oxrdf::NamedNode> {
    match pt {
        PatternTerm::Bound(Term::NamedNode(n)) => Some(n.clone()),
        _ => None,
    }
}

fn to_term(pt: &PatternTerm) -> Option<Term> {
    match pt {
        PatternTerm::Bound(t) => Some(t.clone()),
        PatternTerm::Variable => None,
    }
}

impl<S: QuadStore + 'static> DatasetLftjSource for StoreDataset<S> {
    // ── LFTJ delegates to the underlying QuadStore ────────────────────────────

    fn supports_lftj(&self) -> bool {
        self.store.supports_lftj()
    }

    fn lftj_has_delta(&self) -> bool {
        self.store.lftj_has_delta()
    }

    fn supports_veo_estimates(&self) -> bool {
        self.store.supports_veo_estimates()
    }

    fn lftj_intern_term(&self, term: &Term, _graph: &GraphSelector) -> Option<u64> {
        self.store.lftj_intern_term(term)
    }

    fn lftj_decode_term(&self, id: u64) -> Option<Term> {
        self.store.lftj_decode_term(id)
    }

    fn lftj_estimate_count(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
        graph: &GraphSelector,
    ) -> u64 {
        let graph_id: u8 = match graph {
            GraphSelector::Default => 0u8,
            GraphSelector::Named(gn) => match self.store.lftj_graph_id(gn) {
                Some(id) => id,
                None => return u64::MAX,
            },
            _ => return u64::MAX, // AnyNamed/Union → unknown
        };
        self.store
            .lftj_estimate_count(s, p, o, target_field, graph_id)
    }

    fn lftj_join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
        graph: &GraphSelector,
    ) -> Option<Box<dyn oxigraph_nova_core::TrieIterator>> {
        let graph_id: u8 = match graph {
            GraphSelector::Default => 0u8,
            GraphSelector::Named(gn) => self.store.lftj_graph_id(gn)?,
            _ => return None, // AnyNamed/Union → fallback
        };
        self.store.lftj_join_scan(s, p, o, target_field, graph_id)
    }

    fn lftj_real_count(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
        graph: &GraphSelector,
    ) -> Option<u64> {
        let graph_id: u8 = match graph {
            GraphSelector::Default => 0u8,
            GraphSelector::Named(gn) => self.store.lftj_graph_id(gn)?,
            _ => return None, // AnyNamed/Union → fallback
        };
        self.store.lftj_real_count(s, p, o, target_field, graph_id)
    }
}

impl<S: QuadStore + 'static> Dataset for StoreDataset<S> {
    fn find_quads<'a>(&'a self, pattern: &QuadPattern) -> Result<QuadIter<'a>> {
        // Use to_term for subject so Term::Triple patterns are passed through
        // to the store (which now accepts Option<&Term> for subject).
        let subject = to_term(&pattern.subject);
        let predicate = to_named_node(&pattern.predicate);
        let object = to_term(&pattern.object);

        // Pass a specific graph to the store when we can — lets indexed
        // backends avoid a full scan.  AnyNamed / Union pass None so the
        // store returns all graphs, then we post-filter below.
        let graph_filter: Option<GraphName> = match &pattern.graph {
            GraphSelector::Default => Some(GraphName::DefaultGraph),
            GraphSelector::Named(g) => Some(g.clone()),
            GraphSelector::AnyNamed => None,
            GraphSelector::Union => None,
            // A specific set of graphs — the store has no multi-graph
            // filter parameter, so pass None (scan everything) and let the
            // graph_matches() post-filter below restrict to this exact set.
            GraphSelector::UnionOf(_) => None,
        };

        let store_iter = self
            .store
            .quads_for_pattern(
                subject.as_ref(),
                predicate.as_ref(),
                object.as_ref(),
                graph_filter.as_ref(),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let graph_sel = pattern.graph.clone();
        let mapped = store_iter
            .filter(move |r| match r {
                // Always pass through errors so the caller can handle them.
                Err(_) => true,
                Ok(sq) => graph_matches(&graph_sel, &sq.graph_name),
            })
            .map(|r| {
                // StoredQuad.subject/object are Arc<Term>: the tight
                // quads_for_pattern loop shares dictionary Arc
                // allocations instead of deep-cloning every matched term. We
                // unwrap/clone back to an owned `Term` exactly once here, at
                // the StoredQuad → QuadMatch boundary, rather than doing the
                // (much more numerous) deep clones inside that hot loop.
                r.map(|sq| QuadMatch {
                    subject: Arc::unwrap_or_clone(sq.subject),
                    predicate: Term::NamedNode(sq.predicate),
                    object: Arc::unwrap_or_clone(sq.object),
                    graph_name: sq.graph_name,
                })
                .map_err(|e| anyhow::anyhow!("{e}"))
            });

        Ok(Box::new(mapped))
    }

    fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
        // Start with explicitly registered named graphs (includes empty ones).
        let known = self
            .store
            .known_named_graphs()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut seen = std::collections::HashSet::new();
        let mut graphs: Vec<GraphName> = known
            .filter_map(|r| r.ok())
            .filter_map(|g| match &g {
                GraphName::DefaultGraph => None,
                _ if seen.insert(format!("{g}")) => Some(g),
                _ => None,
            })
            .collect();

        // Then add any named graphs inferred from quad-scanning that weren't
        // already present in the registered set (e.g. graphs added directly
        // via insert without an explicit register_named_graph call).
        let store_iter = self
            .store
            .quads_for_pattern(None, None, None, None)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        for sq in store_iter.flatten() {
            if let GraphName::NamedNode(_) = &sq.graph_name
                && seen.insert(format!("{}", sq.graph_name))
            {
                graphs.push(sq.graph_name.clone());
            }
        }

        Ok(Box::new(graphs.into_iter().map(Ok)))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::{Literal, NamedNode};

    fn iri(s: &str) -> Term {
        Term::NamedNode(NamedNode::new_unchecked(s))
    }
    fn lit(s: &str) -> Term {
        Term::Literal(Literal::new_simple_literal(s))
    }
    fn ng(s: &str) -> GraphName {
        GraphName::NamedNode(NamedNode::new_unchecked(s))
    }

    fn make_dataset() -> InMemoryDataset {
        let mut d = InMemoryDataset::new();
        d.add_default(iri("http://ex/s1"), iri("http://ex/p"), lit("default"));
        d.add(
            iri("http://ex/s2"),
            iri("http://ex/p"),
            lit("named"),
            ng("http://ex/g1"),
        );
        d.add(
            iri("http://ex/s3"),
            iri("http://ex/p"),
            lit("named2"),
            ng("http://ex/g2"),
        );
        d
    }

    #[test]
    fn default_graph_wildcard() {
        let d = make_dataset();
        let results: Vec<_> = d
            .find_quads(&QuadPattern::default_graph(
                PatternTerm::Variable,
                PatternTerm::Variable,
                PatternTerm::Variable,
            ))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].graph_name, GraphName::DefaultGraph);
    }

    #[test]
    fn named_graph_specific() {
        let d = make_dataset();
        let results: Vec<_> = d
            .find_quads(&QuadPattern::all_in(GraphSelector::Named(ng(
                "http://ex/g1",
            ))))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].object, lit("named"));
    }

    #[test]
    fn any_named_graph() {
        let d = make_dataset();
        let results: Vec<_> = d
            .find_quads(&QuadPattern::all_in(GraphSelector::AnyNamed))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 2); // g1 and g2, not default
    }

    #[test]
    fn union_all_graphs() {
        let d = make_dataset();
        let results: Vec<_> = d
            .find_quads(&QuadPattern::all_in(GraphSelector::Union))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn named_graphs_enumeration() {
        let d = make_dataset();
        let mut graphs: Vec<String> = d
            .named_graphs()
            .unwrap()
            .map(|r| r.unwrap().to_string())
            .collect();
        graphs.sort();
        assert_eq!(
            graphs,
            vec!["<http://ex/g1>".to_string(), "<http://ex/g2>".to_string(),]
        );
    }

    #[test]
    fn contains_quad_default() {
        let d = make_dataset();
        assert!(
            d.contains_quad(
                &iri("http://ex/s1"),
                &iri("http://ex/p"),
                &lit("default"),
                &GraphName::DefaultGraph,
            )
            .unwrap()
        );
        assert!(
            !d.contains_quad(
                &iri("http://ex/s1"),
                &iri("http://ex/p"),
                &lit("wrong"),
                &GraphName::DefaultGraph,
            )
            .unwrap()
        );
    }
}
