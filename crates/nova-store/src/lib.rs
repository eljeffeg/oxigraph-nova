//! `Store` — a monolithic, `oxigraph::store::Store`-shaped embedding facade.
//!
//! This crate exists for two audiences:
//!
//! - A Rust caller who wants to embed Nova directly and would rather call
//!   `store.query(...)`/`store.insert(...)` than hand-wire `RingStore` +
//!   `StoreDataset` + `Evaluator` + `execute_update` itself.
//! - A language binding (Python/JS/etc.) whose `store.rs` glue code is
//!   written against `oxigraph::store::Store`'s method surface: `Store`
//!   here mirrors that surface closely enough that porting one over is a
//!   small diff, not a rewrite.
//!
//! Internally, every method here is a thin wrapper: `query`/`update`
//! construct a `StoreDataset`/`Evaluator` (or call `execute_update`)
//! per-call exactly the way `oxigraph`'s own CLI tooling already does
//! inline; `insert`/`remove`/`contains`/`quads_for_pattern`/`len` are
//! direct passthroughs to the wrapped `RingStore`'s `QuadStore` impl;
//! `load`/`dump` stream through `oxrdfio`.
//!
//! ```
//! use oxigraph_nova_core::{GraphName, Literal, NamedNode, Quad, Term};
//! use oxigraph_nova_query::QueryOptions;
//! use oxigraph_nova_store::{QueryResults, Store};
//!
//! let store = Store::new();
//! store
//!     .insert(&Quad::new(
//!         NamedNode::new("http://example.com/s").unwrap(),
//!         NamedNode::new("http://example.com/p").unwrap(),
//!         Term::Literal(Literal::new_simple_literal("o")),
//!         GraphName::DefaultGraph,
//!     ))
//!     .unwrap();
//!
//! match store
//!     .query("SELECT ?s WHERE { ?s ?p ?o }", QueryOptions::default())
//!     .unwrap()
//! {
//!     QueryResults::Solutions(solutions) => assert_eq!(solutions.len(), 1),
//!     _ => unreachable!(),
//! }
//! ```

use oxigraph_nova_core::{
    GraphName, NamedNode, NamedOrBlankNode, Quad, QuadOp, QuadStore, StoredQuad, Term,
};
use oxigraph_nova_query::{
    Evaluator, QueryOptions, QueryResult, Solutions, StoreDataset, execute_update,
    projected_variables,
};
use oxigraph_nova_reasoning::{ReasoningEngine, ReasoningState};
use oxigraph_nova_storage_ring::RingStore;
use oxrdf::{QuadRef, TripleRef, Variable};
use oxrdfio::{RdfFormat, RdfParser, RdfSerializer};
use spargebra::SparqlParser;
use std::path::Path;
use std::sync::Arc;

/// Convert a storage-layer [`oxigraph_nova_core::Oxigraph`] error into an
/// `anyhow::Error` — the error type used throughout this facade (matching
/// `oxigraph_nova_query`'s own convention, so `?` composes cleanly between
/// `Store`'s passthroughs and its `query`/`update` methods).
fn storage_err(e: oxigraph_nova_core::Oxigraph) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// The result of [`Store::query`] — a Nova-facade counterpart to
/// `oxigraph::sparql::QueryResults`, normalizing SELECT/ASK/CONSTRUCT/
/// DESCRIBE into one enum so binding code can branch on results the same
/// way existing `oxigraph`-based binding code already does.
#[derive(Debug)]
pub enum QueryResults {
    /// SELECT results: a sequence of solution mappings.
    Solutions(Solutions),
    /// ASK results: a single boolean.
    Boolean(bool),
    /// CONSTRUCT/DESCRIBE results: a sequence of triples.
    Graph(Vec<oxrdf::Triple>),
}

/// Fallibly convert a [`QueryResult`] (whose `Solutions`/`Triples` variants
/// now carry a lazy, `Send` iterator rather than a materialized `Vec` — see
/// `nova-query`'s `evaluator` module doc comment) into this facade's
/// eager [`QueryResults`], collecting the underlying stream. `QueryResult`
/// is a foreign type (defined in `oxigraph_nova_query`), so this is a free
/// function rather than a `From`/`TryFrom` impl's trait method — either
/// works equally well here, but a plain function avoids any ambiguity with
/// a hypothetical future `TryFrom<QueryResult>` impl.
pub fn collect_query_result(result: QueryResult) -> anyhow::Result<QueryResults> {
    match result {
        s @ QueryResult::Solutions { .. } => {
            let (_, solutions) = s.into_solutions_vec()?;
            Ok(QueryResults::Solutions(solutions))
        }
        QueryResult::Boolean(b) => Ok(QueryResults::Boolean(b)),
        t @ QueryResult::Triples(_) => Ok(QueryResults::Graph(t.into_triples_vec()?)),
    }
}

/// A monolithic, embeddable RDF store: `RingStore` (storage) + the SPARQL
/// evaluator + `execute_update`, wrapped behind one type with an
/// `oxigraph::store::Store`-shaped method surface.
///
/// Cheaply cloneable (an `Arc<RingStore>` handle internally) — clones share
/// the same underlying store, exactly like `oxigraph::store::Store`.
#[derive(Clone)]
pub struct Store {
    store: Arc<RingStore>,
    /// Opt-in OWL 2 RL reasoning overlay cache, configured via
    /// [`Store::with_reasoning`]. When present, [`Store::query`] evaluates
    /// every query over the cached
    /// `oxigraph_nova_reasoning::ReasoningDataset` overlay (rebuilt lazily
    /// when the store's compaction generation advances — see
    /// `oxigraph_nova_reasoning::ReasoningState`'s module doc comment)
    /// instead of the raw store.
    reasoning: Option<Arc<ReasoningState<RingStore>>>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    /// Create a new, purely in-memory store with no on-disk persistence.
    pub fn new() -> Self {
        Self {
            store: Arc::new(RingStore::new()),
            reasoning: None,
        }
    }

    /// Open (or create) a persistent store rooted at `path`, recovering any
    /// previously-committed data via the WAL + snapshot scheme (see
    /// `RingStore::open`).
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let store = RingStore::open(path.as_ref()).map_err(storage_err)?;
        Ok(Self {
            store: Arc::new(store),
            reasoning: None,
        })
    }

    /// Enable OWL 2 RL reasoning: every [`Store::query`] call is evaluated
    /// over an in-memory `oxigraph_nova_reasoning::ReasoningDataset` overlay
    /// built by `engine`, rebuilt lazily whenever the store's compaction
    /// generation advances (see
    /// `oxigraph_nova_reasoning::ReasoningState`'s module doc comment for
    /// the full staleness policy). Mirrors `nova-server`'s
    /// `Server::with_reasoning` / `--reasoning`.
    ///
    /// Typical argument: `Arc::new(oxigraph_nova_reasoning::LftjFixpointEngine::new())`.
    #[must_use]
    pub fn with_reasoning(mut self, engine: Arc<dyn ReasoningEngine>) -> Self {
        self.reasoning = Some(Arc::new(ReasoningState::new(engine)));
        self
    }

    // ── SPARQL ───────────────────────────────────────────────────────────

    /// Run a SPARQL query against this store.
    ///
    /// If [`Store::with_reasoning`] was configured, the query is evaluated
    /// over the (lazily-rebuilt) OWL 2 RL reasoning overlay instead of the
    /// raw store — see `oxigraph_nova_reasoning::ReasoningState`'s doc
    /// comment for the staleness policy.
    pub fn query(&self, query: &str, options: QueryOptions) -> anyhow::Result<QueryResults> {
        let parsed = SparqlParser::new().parse_query(query)?;
        if let Some(rs) = &self.reasoning {
            let overlay = rs.current(&self.store)?;
            let evaluator = Evaluator::with_options(&*overlay, options);
            collect_query_result(evaluator.evaluate(&parsed)?)
        } else {
            let dataset = StoreDataset::new(Arc::clone(&self.store));
            let evaluator = Evaluator::with_options(&dataset, options);
            collect_query_result(evaluator.evaluate(&parsed)?)
        }
    }

    /// Run a SPARQL query against this store, additionally returning the
    /// ordered SELECT projection variable list (empty for ASK/CONSTRUCT/
    /// DESCRIBE) alongside the results.
    ///
    /// Equivalent to calling [`Store::query`] and separately re-parsing
    /// `query` to recover its projection variables, but parses `query`
    /// exactly once — for a caller (e.g. a language binding's SPARQL
    /// results serializer) that needs both the results *and* the variable
    /// list to build a results header, this avoids a redundant second
    /// parse of the same query text.
    pub fn query_with_variables(
        &self,
        query: &str,
        options: QueryOptions,
    ) -> anyhow::Result<(QueryResults, Vec<Variable>)> {
        let parsed = SparqlParser::new().parse_query(query)?;
        let vars = projected_variables(&parsed);
        let results = if let Some(rs) = &self.reasoning {
            let overlay = rs.current(&self.store)?;
            let evaluator = Evaluator::with_options(&*overlay, options);
            collect_query_result(evaluator.evaluate(&parsed)?)?
        } else {
            let dataset = StoreDataset::new(Arc::clone(&self.store));
            let evaluator = Evaluator::with_options(&dataset, options);
            collect_query_result(evaluator.evaluate(&parsed)?)?
        };
        Ok((results, vars))
    }

    /// Run a SPARQL update against this store.
    ///
    /// See `oxigraph_nova_query::update`'s module doc comment for the
    /// non-atomicity caveats of multi-statement updates.
    pub fn update(&self, update: &str) -> anyhow::Result<()> {
        let parsed = SparqlParser::new().parse_update(update)?;
        execute_update(&self.store, &parsed)
    }

    // ── Passthroughs to the wrapped QuadStore ───────────────────────────

    /// Insert a quad. Returns `true` if it was newly inserted.
    pub fn insert(&self, quad: &Quad) -> anyhow::Result<bool> {
        self.store.insert(quad).map_err(storage_err)
    }

    /// Insert every quad in `quads` as a single logical batch.
    ///
    /// Applied via [`QuadStore::apply_batch`] rather than one `insert` call
    /// per quad: on a backend that overrides `apply_batch` (e.g.
    /// `RingStore`, which acquires its internal lock once and writes every
    /// resulting WAL record in a single `append_batch` call), this means
    /// one lock acquisition and one `fsync` for the whole batch instead of
    /// one per quad, and the batch becomes visible to concurrent readers
    /// atomically rather than quad-by-quad. See `QuadStore::apply_batch`'s
    /// doc comment for the exact durability/atomicity contract — notably,
    /// this is a durability/visibility batching guarantee, not a full ACID
    /// transaction with in-process rollback on a partial failure.
    ///
    /// Returns the number of quads newly inserted (quads already present
    /// are not double-counted, matching [`Store::insert`]'s convention).
    pub fn extend(&self, quads: impl IntoIterator<Item = Quad>) -> anyhow::Result<usize> {
        let ops: Vec<QuadOp> = quads.into_iter().map(QuadOp::Insert).collect();
        let (inserted, _removed) = self.store.apply_batch(&ops).map_err(storage_err)?;
        Ok(inserted)
    }

    /// Remove a quad. Returns `true` if it was present and removed.
    pub fn remove(&self, quad: &Quad) -> anyhow::Result<bool> {
        self.store.remove(quad).map_err(storage_err)
    }

    /// Returns `true` if `quad` is present in the store.
    pub fn contains(&self, quad: &Quad) -> anyhow::Result<bool> {
        self.store.contains(quad).map_err(storage_err)
    }

    /// Total number of quads in the store.
    pub fn len(&self) -> anyhow::Result<usize> {
        self.store.len().map_err(storage_err)
    }

    /// Returns `true` if the store has no quads.
    pub fn is_empty(&self) -> anyhow::Result<bool> {
        self.store.is_empty().map_err(storage_err)
    }

    /// Iterate over all quads matching the given pattern. `None` is a
    /// wildcard for that position.
    pub fn quads_for_pattern<'a>(
        &'a self,
        subject: Option<&Term>,
        predicate: Option<&NamedNode>,
        object: Option<&Term>,
        graph_name: Option<&GraphName>,
    ) -> anyhow::Result<impl Iterator<Item = anyhow::Result<StoredQuad>> + 'a> {
        let iter = self
            .store
            .quads_for_pattern(subject, predicate, object, graph_name)
            .map_err(storage_err)?;
        Ok(iter.map(|r| r.map_err(storage_err)))
    }

    /// Direct access to the underlying `RingStore`, for callers (e.g. a
    /// language binding's `query`/`update` glue) that need to bypass this
    /// facade's own `query`/`update` — for instance to parse with a custom
    /// base IRI/prefixes via `spargebra::SparqlParser` directly, or to
    /// reconstruct a parsed `Query`'s `dataset` field before evaluating —
    /// neither of which this facade's `query`/`update` methods expose
    /// parameters for.
    pub fn inner(&self) -> Arc<RingStore> {
        Arc::clone(&self.store)
    }

    /// Enumerate every named graph explicitly known to this store (including
    /// empty ones registered but never populated).
    pub fn named_graphs(&self) -> anyhow::Result<Vec<GraphName>> {
        self.store
            .known_named_graphs()
            .map_err(storage_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_err)
    }

    /// Returns `true` if `graph_name` is explicitly known to this store
    /// (the default graph is always considered known).
    ///
    /// Short-circuits on the first match rather than collecting the full
    /// `known_named_graphs()` iterator into a `Vec` first (as a naive
    /// `self.named_graphs()?.contains(graph_name)` would) — for a store
    /// with many named graphs, this avoids an O(n) allocation-and-scan
    /// just to answer a yes/no membership question that's usually settled
    /// within the first few entries.
    pub fn contains_named_graph(&self, graph_name: &GraphName) -> anyhow::Result<bool> {
        if matches!(graph_name, GraphName::DefaultGraph) {
            return Ok(true);
        }
        for g in self.store.known_named_graphs().map_err(storage_err)? {
            if &g.map_err(storage_err)? == graph_name {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ── I/O ──────────────────────────────────────────────────────────────

    /// Parse RDF data from `reader` and insert every resulting quad into
    /// this store, returning the number of quads newly inserted.
    ///
    /// `to_graph_name`, if given, is used as the destination graph for
    /// plain-triple formats (N-Triples/Turtle/RDF-XML); dataset formats
    /// (N-Quads/TriG/JSON-LD) use each quad's own encoded graph instead —
    /// see `oxrdfio::RdfParser::with_default_graph`'s doc comment.
    pub fn load<R: std::io::Read>(
        &self,
        reader: R,
        format: RdfFormat,
        base_iri: Option<&str>,
        to_graph_name: Option<&GraphName>,
    ) -> anyhow::Result<usize> {
        let mut parser = RdfParser::from_format(format);
        if let Some(iri) = base_iri {
            parser = parser.with_base_iri(iri)?;
        }
        if let Some(g) = to_graph_name {
            parser = parser.with_default_graph(g.clone());
        }

        let mut count = 0usize;
        for quad in parser.for_reader(reader) {
            let quad = quad?;
            if self.store.insert(&quad).map_err(storage_err)? {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Serialize this store's logical RDF content to `writer`.
    ///
    /// `from_graph_name`, if given, dumps only that one graph as plain
    /// triples; otherwise every known graph (default + named) is dumped,
    /// which requires a dataset-capable `format` (N-Quads/TriG/JSON-LD).
    ///
    /// RDF-1.2 quoted-triple subjects are silently skipped: `oxrdf`'s
    /// `Quad`/`Triple` subject type (`NamedOrBlankNode`) has no variant for
    /// them, so a stored quad whose subject is a quoted triple cannot be
    /// re-serialized as a triple/quad subject via this API.
    pub fn dump<W: std::io::Write>(
        &self,
        writer: W,
        format: RdfFormat,
        from_graph_name: Option<&GraphName>,
    ) -> anyhow::Result<()> {
        if from_graph_name.is_none() && !format.supports_datasets() {
            anyhow::bail!(
                "no graph given (dumping every graph), but {} is a plain triple format; pass a \
                 graph, or choose a dataset format (N-Quads/TriG/JSON-LD)",
                format.name()
            );
        }

        let mut writer = RdfSerializer::from_format(format).for_writer(writer);

        match from_graph_name {
            Some(g) => {
                for sq in self
                    .store
                    .quads_for_pattern(None, None, None, Some(g))
                    .map_err(storage_err)?
                {
                    let sq = sq.map_err(storage_err)?;
                    let Some(subject) = stored_subject(&sq) else {
                        continue;
                    };
                    let object = sq.object.as_ref().clone();
                    writer.serialize_triple(TripleRef::new(&subject, &sq.predicate, &object))?;
                }
            }
            None => {
                let mut graphs: Vec<GraphName> = vec![GraphName::DefaultGraph];
                graphs.extend(self.named_graphs()?);
                for g in &graphs {
                    for sq in self
                        .store
                        .quads_for_pattern(None, None, None, Some(g))
                        .map_err(storage_err)?
                    {
                        let sq = sq.map_err(storage_err)?;
                        let Some(subject) = stored_subject(&sq) else {
                            continue;
                        };
                        let object = sq.object.as_ref().clone();
                        writer.serialize_quad(QuadRef::new(&subject, &sq.predicate, &object, g))?;
                    }
                }
            }
        }

        writer.finish()?;
        Ok(())
    }

    /// A thin wrapper over `RingStore::bulk_load`, for loading large,
    /// known-fresh datasets without going through per-quad `insert` calls.
    pub fn bulk_loader(&self) -> BulkLoader {
        BulkLoader {
            store: Arc::clone(&self.store),
        }
    }

    // ── Maintenance passthroughs ─────────────────────────────────────────

    /// Merge accumulated writes into the compacted index, for better scan
    /// performance. Mirrors upstream `Store::optimize`.
    pub fn optimize(&self) -> anyhow::Result<()> {
        self.store.compact().map_err(storage_err)
    }

    /// Create a consistent, restorable copy of this store's on-disk state
    /// at `destination`. Only valid for a persistent store (opened via
    /// [`Store::open`]).
    pub fn backup(&self, destination: impl AsRef<Path>) -> anyhow::Result<()> {
        self.store.backup(destination.as_ref()).map_err(storage_err)
    }

    // ── Full-text search (opt-in via the `fulltext` cargo feature) ──────

    /// Turn on Tantivy-backed full-text indexing for this store (see
    /// `RingStore::enable_fulltext`). Once enabled, literal objects are
    /// indexed incrementally on every compaction. Call
    /// [`Store::text_search`] to obtain a handle usable with
    /// `QueryOptions::with_text_search` for `text:query`/`text:contains`
    /// extension-function dispatch.
    #[cfg(feature = "fulltext")]
    pub fn enable_fulltext(&self) -> anyhow::Result<()> {
        self.store.enable_fulltext().map_err(storage_err)
    }

    /// A handle to this store's full-text search backend, for attaching via
    /// `QueryOptions::with_text_search`. Requires [`Store::enable_fulltext`]
    /// to have been called first (otherwise `text:query`/`text:contains`
    /// will simply find nothing, since nothing has been indexed).
    #[cfg(feature = "fulltext")]
    pub fn text_search(&self) -> Arc<dyn oxigraph_nova_core::TextSearch> {
        Arc::clone(&self.store) as Arc<dyn oxigraph_nova_core::TextSearch>
    }
}

/// Convert a [`StoredQuad`]'s `Term` subject to `NamedOrBlankNode`, `None`
/// for a quoted-triple subject (see [`Store::dump`]'s doc comment).
fn stored_subject(sq: &StoredQuad) -> Option<NamedOrBlankNode> {
    match sq.subject.as_ref() {
        Term::NamedNode(n) => Some(NamedOrBlankNode::NamedNode(n.clone())),
        Term::BlankNode(b) => Some(NamedOrBlankNode::BlankNode(b.clone())),
        _ => None,
    }
}

/// A thin wrapper over [`RingStore::bulk_load`], returned by
/// [`Store::bulk_loader`].
pub struct BulkLoader {
    store: Arc<RingStore>,
}

impl BulkLoader {
    /// Bulk-insert `quads` directly into the compacted index, bypassing the
    /// per-write delta buffer. Returns the number of quads loaded. Intended
    /// for initial dataset loads where every quad is known to be fresh —
    /// see `RingStore::bulk_load`'s doc comment for the full rationale.
    pub fn load(&self, quads: impl IntoIterator<Item = Quad>) -> anyhow::Result<usize> {
        self.store.bulk_load(quads).map_err(storage_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::Literal;

    fn quad(s: &str, p: &str, o: &str) -> Quad {
        Quad::new(
            NamedNode::new_unchecked(s),
            NamedNode::new_unchecked(p),
            Term::Literal(Literal::new_simple_literal(o)),
            GraphName::DefaultGraph,
        )
    }

    #[test]
    fn insert_contains_len() {
        let store = Store::new();
        let q = quad("http://ex/s", "http://ex/p", "hello");
        assert!(store.insert(&q).unwrap());
        assert!(!store.insert(&q).unwrap()); // already present
        assert!(store.contains(&q).unwrap());
        assert_eq!(store.len().unwrap(), 1);
        assert!(!store.is_empty().unwrap());
        assert!(store.remove(&q).unwrap());
        assert!(store.is_empty().unwrap());
    }

    #[test]
    fn select_query_round_trip() {
        let store = Store::new();
        store
            .insert(&quad("http://ex/s", "http://ex/p", "hello"))
            .unwrap();

        match store
            .query("SELECT ?s ?o WHERE { ?s ?p ?o }", QueryOptions::default())
            .unwrap()
        {
            QueryResults::Solutions(solutions) => assert_eq!(solutions.len(), 1),
            other => panic!("expected Solutions, got {other:?}"),
        }
    }

    #[test]
    fn ask_query() {
        let store = Store::new();
        store
            .insert(&quad("http://ex/s", "http://ex/p", "hello"))
            .unwrap();
        match store
            .query("ASK { ?s ?p ?o }", QueryOptions::default())
            .unwrap()
        {
            QueryResults::Boolean(b) => assert!(b),
            other => panic!("expected Boolean, got {other:?}"),
        }
    }

    #[test]
    fn construct_query() {
        let store = Store::new();
        store
            .insert(&quad("http://ex/s", "http://ex/p", "hello"))
            .unwrap();
        match store
            .query(
                "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }",
                QueryOptions::default(),
            )
            .unwrap()
        {
            QueryResults::Graph(triples) => assert_eq!(triples.len(), 1),
            other => panic!("expected Graph, got {other:?}"),
        }
    }

    #[test]
    fn update_insert_and_delete_data() {
        let store = Store::new();
        store
            .update("INSERT DATA { <http://ex/s> <http://ex/p> \"hello\" }")
            .unwrap();
        assert_eq!(store.len().unwrap(), 1);

        store
            .update("DELETE DATA { <http://ex/s> <http://ex/p> \"hello\" }")
            .unwrap();
        assert!(store.is_empty().unwrap());
    }

    #[test]
    fn persistent_open_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store
                .insert(&quad("http://ex/s", "http://ex/p", "hello"))
                .unwrap();
        }
        {
            let store = Store::open(dir.path()).unwrap();
            assert_eq!(store.len().unwrap(), 1);
        }
    }

    #[test]
    fn load_dump_round_trip() {
        let store = Store::new();
        let input = "<http://ex/s> <http://ex/p> \"hello\" .\n";
        let count = store
            .load(input.as_bytes(), RdfFormat::NTriples, None, None)
            .unwrap();
        assert_eq!(count, 1);

        let mut out = Vec::new();
        store
            .dump(
                &mut out,
                RdfFormat::NTriples,
                Some(&GraphName::DefaultGraph),
            )
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("http://ex/s"));
    }

    #[test]
    fn bulk_loader_loads_quads() {
        let store = Store::new();
        let loader = store.bulk_loader();
        let count = loader
            .load(vec![quad("http://ex/s", "http://ex/p", "hello")])
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn quads_for_pattern_filters() {
        let store = Store::new();
        store
            .insert(&quad("http://ex/s", "http://ex/p", "hello"))
            .unwrap();
        let subject = Term::NamedNode(NamedNode::new_unchecked("http://ex/s"));
        let results: Vec<_> = store
            .quads_for_pattern(Some(&subject), None, None, None)
            .unwrap()
            .collect::<anyhow::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn extend_batch_inserts_all_and_counts_new_only() {
        let store = Store::new();
        // Insert one quad up front so we can verify `extend` doesn't
        // double-count a quad that was already present.
        store
            .insert(&quad("http://ex/s0", "http://ex/p", "zero"))
            .unwrap();

        let inserted = store
            .extend(vec![
                quad("http://ex/s0", "http://ex/p", "zero"), // already present
                quad("http://ex/s1", "http://ex/p", "one"),
                quad("http://ex/s2", "http://ex/p", "two"),
            ])
            .unwrap();

        assert_eq!(inserted, 2, "only the 2 new quads should be counted");
        assert_eq!(store.len().unwrap(), 3);
        assert!(
            store
                .contains(&quad("http://ex/s1", "http://ex/p", "one"))
                .unwrap()
        );
        assert!(
            store
                .contains(&quad("http://ex/s2", "http://ex/p", "two"))
                .unwrap()
        );
    }

    #[test]
    fn extend_empty_is_noop() {
        let store = Store::new();
        let inserted = store.extend(Vec::<Quad>::new()).unwrap();
        assert_eq!(inserted, 0);
        assert!(store.is_empty().unwrap());
    }

    #[test]
    fn query_with_variables_select_returns_projected_vars_and_results() {
        let store = Store::new();
        store
            .insert(&quad("http://ex/s", "http://ex/p", "hello"))
            .unwrap();

        let (results, vars) = store
            .query_with_variables("SELECT ?s ?o WHERE { ?s ?p ?o }", QueryOptions::default())
            .unwrap();

        let var_names: Vec<&str> = vars.iter().map(|v| v.as_str()).collect();
        assert_eq!(var_names, vec!["s", "o"]);
        match results {
            QueryResults::Solutions(solutions) => assert_eq!(solutions.len(), 1),
            other => panic!("expected Solutions, got {other:?}"),
        }
    }

    #[test]
    fn query_with_variables_ask_has_empty_vars() {
        let store = Store::new();
        store
            .insert(&quad("http://ex/s", "http://ex/p", "hello"))
            .unwrap();

        let (results, vars) = store
            .query_with_variables("ASK { ?s ?p ?o }", QueryOptions::default())
            .unwrap();

        assert!(vars.is_empty());
        match results {
            QueryResults::Boolean(b) => assert!(b),
            other => panic!("expected Boolean, got {other:?}"),
        }
    }

    #[test]
    fn contains_named_graph_default_graph_always_true() {
        let store = Store::new();
        assert!(
            store
                .contains_named_graph(&GraphName::DefaultGraph)
                .unwrap()
        );
    }

    #[test]
    fn contains_named_graph_true_and_false_for_named_graphs() {
        let store = Store::new();
        let g = GraphName::NamedNode(NamedNode::new_unchecked("http://ex/g1"));
        store
            .insert(&Quad::new(
                NamedNode::new_unchecked("http://ex/s"),
                NamedNode::new_unchecked("http://ex/p"),
                Term::Literal(Literal::new_simple_literal("v")),
                g.clone(),
            ))
            .unwrap();

        assert!(store.contains_named_graph(&g).unwrap());
        assert!(
            !store
                .contains_named_graph(&GraphName::NamedNode(NamedNode::new_unchecked(
                    "http://ex/nonexistent"
                )))
                .unwrap()
        );
    }

    // ── Optional: reasoning ───────────────────────────────────────────────

    #[test]
    fn reasoning_infers_subclass_transitivity() {
        use oxigraph_nova_core::NamedNode as N;
        use oxigraph_nova_reasoning::LftjFixpointEngine;

        let store = Store::new().with_reasoning(Arc::new(LftjFixpointEngine::new()));

        let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        let subclass_of = "http://www.w3.org/2000/01/rdf-schema#subClassOf";

        // A subClassOf B, B subClassOf C, x type A ⟹ x type C (by transitivity
        // + type propagation).
        store
            .insert(&Quad::new(
                N::new_unchecked("http://ex/A"),
                N::new_unchecked(subclass_of),
                Term::NamedNode(N::new_unchecked("http://ex/B")),
                GraphName::DefaultGraph,
            ))
            .unwrap();
        store
            .insert(&Quad::new(
                N::new_unchecked("http://ex/B"),
                N::new_unchecked(subclass_of),
                Term::NamedNode(N::new_unchecked("http://ex/C")),
                GraphName::DefaultGraph,
            ))
            .unwrap();
        store
            .insert(&Quad::new(
                N::new_unchecked("http://ex/x"),
                N::new_unchecked(rdf_type),
                Term::NamedNode(N::new_unchecked("http://ex/A")),
                GraphName::DefaultGraph,
            ))
            .unwrap();

        let ask = format!("ASK {{ <http://ex/x> <{rdf_type}> <http://ex/C> }}");
        match store.query(&ask, QueryOptions::default()).unwrap() {
            QueryResults::Boolean(b) => assert!(b, "expected inferred rdf:type via reasoning"),
            other => panic!("expected Boolean, got {other:?}"),
        }
    }

    // ── Optional: full-text search ─────────────────────────────────────────

    #[cfg(feature = "fulltext")]
    #[test]
    fn fulltext_search_finds_indexed_literal() {
        let store = Store::new();
        store.enable_fulltext().unwrap();
        store
            .insert(&quad("http://ex/s", "http://ex/p", "the quick brown fox"))
            .unwrap();
        // Force a compaction so the delta-buffered insert above gets folded
        // into the compacted index and indexed by the full-text backend
        // (see `RingStore`'s fulltext module doc comment: indexing happens
        // incrementally on compaction).
        store.optimize().unwrap();

        let ts = store.text_search();
        let options = QueryOptions::default().with_text_search(ts);
        let sparql = r#"
            PREFIX text: <http://oxigraph-nova.dev/fn/text#>
            SELECT ?s WHERE {
                ?s <http://ex/p> ?o .
                FILTER(text:query(?o, "quick"))
            }
        "#;
        match store.query(sparql, options).unwrap() {
            QueryResults::Solutions(solutions) => {
                assert_eq!(solutions.len(), 1, "expected 1 full-text match");
            }
            other => panic!("expected Solutions, got {other:?}"),
        }
    }

    // ── Optional: geosparql ─────────────────────────────────────────────────

    #[cfg(feature = "geosparql")]
    #[test]
    fn geosparql_distance_function_dispatches() {
        // Pure feature-forwarding sanity check: confirms `geof:distance` is
        // dispatched end-to-end through `Store::query` once the `geosparql`
        // cargo feature is compiled in — no new `Store` API is required.
        let store = Store::new();
        let sparql = r#"
            PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
            SELECT ?d WHERE {
                BIND(geof:distance(
                    "POINT(0 0)"^^<http://www.opengis.net/ont/geosparql#wktLiteral>,
                    "POINT(3 4)"^^<http://www.opengis.net/ont/geosparql#wktLiteral>,
                    <http://www.opengis.net/def/uom/OGC/1.0/metre>
                ) AS ?d)
            }
        "#;
        match store.query(sparql, QueryOptions::default()).unwrap() {
            QueryResults::Solutions(solutions) => {
                assert_eq!(solutions.len(), 1, "expected exactly 1 bound row");
            }
            other => panic!("expected Solutions, got {other:?}"),
        }
    }
}
