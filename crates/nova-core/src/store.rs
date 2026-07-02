//! QuadStore — the pluggable storage trait.
//!
//! Every storage backend (in-memory, sled, ring, RocksDB) implements
//! this trait. The query evaluator only ever calls `quads_for_pattern` — it is
//! completely decoupled from storage internals.

use crate::{GraphName, NamedNode, Oxigraph, Quad, StoredQuad, Term};

pub trait QuadStore: Send + Sync {
    // ── LFTJ optional capability ──────────────────────────────────────────────

    /// Returns `true` if this store supports Leapfrog Triejoin acceleration.
    ///
    /// Only `RingStore` (and future Ring-backed stores) return `true`.
    /// The default is `false`; the SPARQL evaluator falls back to nested-loop
    /// when this returns false.
    fn supports_lftj(&self) -> bool {
        false
    }

    /// Intern a term to its numeric TermId for LFTJ seek operations.
    ///
    /// Returns `None` if the term is not in the dictionary (implying no matches).
    fn lftj_intern_term(&self, _term: &Term) -> Option<u64> {
        None
    }

    /// Decode a numeric TermId back to a Term (inverse of `lftj_intern_term`).
    fn lftj_decode_term(&self, _id: u64) -> Option<Term> {
        None
    }

    /// Return the internal 8-bit graph identifier for a `GraphName`.
    ///
    /// Returns `Some(0)` for the default graph, `Some(n)` for named graphs.
    /// Returns `None` if the graph is not in the dictionary.
    fn lftj_graph_id(&self, _graph: &GraphName) -> Option<u8> {
        None
    }

    /// Estimate the number of distinct values for `target_field` (0=S, 1=P, 2=O)
    /// given the other bound fields — used by the adaptive VEO predictor in LFTJ.
    ///
    /// Returns `u64::MAX` when the estimate is unavailable (non-CLTJ backends).
    /// CLTJ backends return the global vocabulary size of the target field,
    /// scaled down by `1 / (1 + n_bound_fields)` to account for selectivity.
    fn lftj_estimate_count(
        &self,
        _s: Option<u64>,
        _p: Option<u64>,
        _o: Option<u64>,
        _target_field: usize,
        _graph_id: u8,
    ) -> u64 {
        u64::MAX
    }

    /// Return a seek-capable [`TrieIterator`][crate::TrieIterator] for one
    /// variable in a Leapfrog Triejoin step.
    ///
    /// `s`, `p`, `o`: `Some(id)` = field is currently bound to this TermId
    ///   (either a pattern constant or a previously-bound join variable);
    ///   `None` = field is either the target variable or an unbound later variable.
    /// `target_field`: 0 = subject, 1 = predicate, 2 = object — identifies which
    ///   `None` slot is the *target* being iterated at this join depth.
    /// `graph_id`: the internal graph identifier (0 = default graph).
    ///
    /// Returns `None` if LFTJ is not supported (use nested-loop fallback).
    fn lftj_join_scan(
        &self,
        _s: Option<u64>,
        _p: Option<u64>,
        _o: Option<u64>,
        _target_field: usize,
        _graph_id: u8,
    ) -> Option<Box<dyn crate::trie::TrieIterator>> {
        None
    }

    /// Returns `true` if `lftj_estimate_count` provides real sub-`u64::MAX` values.
    ///
    /// Non-CLTJ stores (MemoryStore, any custom backend) return `false` (the
    /// default), which causes `lftj_step` to skip the VEO heap-allocation + sort
    /// entirely.  Only backends that implement at least a vocab-size heuristic
    /// (or the paper's §3.6 leaf-descendant predictor) should override to `true`.
    fn supports_veo_estimates(&self) -> bool {
        false
    }

    /// Returns `true` if the store has uncompacted delta writes.
    ///
    /// LFTJ operates only on the fully-sorted Ring, so it falls back to
    /// nested-loop when delta is non-empty.
    fn lftj_has_delta(&self) -> bool {
        false
    }

    /// Insert a quad. Returns `true` if the quad was newly inserted, `false` if
    /// it was already present.
    fn insert(&self, quad: &Quad) -> Result<bool, Oxigraph>;

    /// Remove a quad. Returns `true` if the quad was present and removed.
    fn remove(&self, quad: &Quad) -> Result<bool, Oxigraph>;

    /// Iterate over all quads matching the given pattern. `None` is a wildcard.
    ///
    /// Returns [`StoredQuad`] items (subject is `Term`) rather than `oxrdf::Quad`
    /// (subject is `NamedOrBlankNode`) so that quoted-triple subjects from the
    /// Ring are not silently dropped — they appear as `Term::Triple(...)`.
    fn quads_for_pattern(
        &self,
        subject: Option<&Term>,
        predicate: Option<&NamedNode>,
        object: Option<&Term>,
        graph_name: Option<&GraphName>,
    ) -> Result<Box<dyn Iterator<Item = Result<StoredQuad, Oxigraph>> + '_>, Oxigraph>;

    /// Total number of quads in the store.
    fn len(&self) -> Result<usize, Oxigraph>;

    fn is_empty(&self) -> Result<bool, Oxigraph> {
        Ok(self.len()? == 0)
    }

    fn contains(&self, quad: &Quad) -> Result<bool, Oxigraph>;

    /// Enumerate named graphs that are explicitly registered in this store,
    /// including empty named graphs that have no triples.  Backends that track
    /// named-graph membership (e.g. after loading an empty Turtle file into a
    /// named graph) should override this to return those IRIs.
    ///
    /// The default implementation returns an empty iterator; callers should
    /// merge this with the set of graphs inferred from quad-scanning.
    fn known_named_graphs(
        &self,
    ) -> Result<Box<dyn Iterator<Item = Result<GraphName, Oxigraph>> + '_>, Oxigraph> {
        Ok(Box::new(std::iter::empty()))
    }

    /// Explicitly register a named graph so it appears in `known_named_graphs()`
    /// even when it has no triples yet (e.g. an empty Turtle file loaded into a
    /// named graph context).
    ///
    /// The default implementation is a no-op; backends that track named-graph
    /// membership (e.g. `MemoryStore`, `RingStore`) should override this.
    fn register_named_graph(&self, _graph: &GraphName) -> Result<(), Oxigraph> {
        Ok(())
    }

    /// Bulk-insert from an iterator. Default impl calls `insert` in a loop;
    /// backends may override for efficiency (e.g. batch writes, skip WAL).
    fn extend(&self, quads: impl IntoIterator<Item = Quad>) -> Result<usize, Oxigraph> {
        let mut count = 0usize;
        for quad in quads {
            if self.insert(&quad)? {
                count += 1;
            }
        }
        Ok(count)
    }
}
