//! QuadStore — the pluggable storage trait.
//!
//! Every storage backend (in-memory, sled, ring, RocksDB) implements
//! this trait. The query evaluator only ever calls `quads_for_pattern` — it is
//! completely decoupled from storage internals.

use crate::{GraphName, NamedNode, Oxigraph, Quad, StoredQuad, Term};

/// Optional Leapfrog Triejoin (LFTJ) / Worst-Case-Optimal-Join acceleration
/// capability for a [`QuadStore`].
///
/// This is a **separate supertrait** rather than being folded directly into
/// [`QuadStore`] so that:
///
/// - The LFTJ-specific surface area (9 methods) is named and documented as
///   one cohesive unit, rather than being interleaved with `QuadStore`'s
///   core CRUD/observability methods.
/// - A backend author implementing a brand-new [`QuadStore`] only ever sees
///   one extra `impl LftjSource for MyStore {}` line (every method defaults
///   to "unsupported") — the acceleration surface is discoverable but never
///   in the way.
///
/// Every method here is defaulted to "not supported"/"unknown", so any
/// `QuadStore` implementor can opt out entirely with an empty
/// `impl LftjSource for MyStore {}` block. Only Ring-backed (CLTJ) stores
/// currently override these to enable the accelerated join path — see
/// `oxigraph_nova_storage_ring::LoudsStore`'s `impl LftjSource` block.

// ── K7.2 prepared fixed-P object intersection (product wedge) ────────────────

/// Opaque prepared fixed-predicate context for multi-subject object ∩.
///
/// Built once per recognized wedge query. Implementations hold graph image +
/// densified predicate so each `(a,b)` open skips predicate remap.
pub trait PreparedPredObjectIntersect: Send {
    /// Bind outer subject `a` once; reuses SP(a,P) across all right subjects.
    fn bind_left(&self, subject_a: u64) -> Option<Box<dyn PreparedLeftIntersect>>;
    /// One-shot D1 open under the prepared predicate (no left-range reuse).
    fn intersect2(
        &self,
        subject_a: u64,
        subject_b: u64,
    ) -> Option<Box<dyn crate::trie::TrieIterator>>;
}

/// Outer-subject handle under a prepared fixed-P context.
pub trait PreparedLeftIntersect: Send {
    /// Intersect objects of the bound left subject with `subject_b` under P.
    fn intersect_right(&self, subject_b: u64) -> Option<Box<dyn crate::trie::TrieIterator>>;
}

// ── K9 prepared SP→O scan + two-hop plan ─────────────────────────────────────

/// Resettable subject-predicate → object scanner (K9.2).
///
/// Built once per fixed predicate. Each logical open becomes
/// [`reset_to_subject`](Self::reset_to_subject) instead of a full
/// `lftj_join_scan` adapter path:
///
/// - predicate densify / column lookup / policy dispatch: once at prepare
/// - per reset: subject densify + SP range + cursor rebind only
pub trait PreparedSpObjectScan: Send {
    /// Rebind the cursor to objects of `subject` under the prepared predicate.
    ///
    /// Returns `false` when the subject is unmappable or the SP range is empty
    /// (caller should skip). After `true`, [`key`](Self::key) /
    /// [`advance`](Self::advance) / [`at_end`](Self::at_end) enumerate objects.
    fn reset_to_subject(&mut self, subject: u64) -> bool;

    /// Current object TermId. Panic if `at_end`.
    fn key(&self) -> u64;

    /// Advance to the next object under the current subject.
    fn advance(&mut self);

    /// `true` when the current subject has no more objects.
    fn at_end(&self) -> bool;

    /// Last SP range length after a successful reset (0 if empty / failed).
    /// Used by harness degree stats; default 0.
    fn last_range_len(&self) -> u64 {
        0
    }
}

// ── L: unified prepared physical operators ───────────────────────────────────
//
// Pipeline:
//   BGP → shape recognizer → physical operator → prepared operator → reusable exec
//
// Two-hop (K9/K10) and wedge/triangle (K11) are instances of one abstraction.
// Future stars, longer chains, diamonds extend the same trait + cache key space.

/// Prepared physical operator for a specialized BGP shape (Phase L).
///
/// Built once per recognized motif (and optionally retained across requests via
/// a store-scoped, snapshot-keyed cache). `execute` streams external TermId
/// triples; the query engine decodes or counts as needed (id_only COUNT path).
///
/// Concrete product shapes today:
/// - **two-hop** `?a P1 ?b . ?b P2 ?c` (K9/K10)
/// - **wedge**   `?a P ?b . ?b P ?c . ?a P ?c` (K11)
/// - **sp-expansion / 2join** `?s P1 O1 . ?s P2 ?o` — emit `(s, o, 0)`
pub trait PreparedPhysicalOperator: Send {
    /// Run the prepared physical plan.
    ///
    /// `emit(a, b, c)` is invoked once per result triple (TermIds).
    /// For SP-expansion / 2join, `c` is unused (`0`) and `emit(s, o, 0)`.
    /// Return `Err(())` from `emit` to abort early (cancellation).
    ///
    /// Returns the number of emitted triples, or `Err` if aborted.
    fn execute(
        &mut self,
        emit: &mut dyn FnMut(u64, u64, u64) -> Result<(), ()>,
    ) -> Result<u64, ()>;
}

/// Historical name for chain-path prepared ops (K9/K10). Same as
/// [`PreparedPhysicalOperator`].
pub use PreparedPhysicalOperator as PreparedTwoHop;

/// Historical name for wedge/triangle prepared ops (K11). Same as
/// [`PreparedPhysicalOperator`].
pub use PreparedPhysicalOperator as PreparedWedge;

/// SP-expansion / 2join prepared ops. Same as [`PreparedPhysicalOperator`]
/// (`emit(s, o, 0)`).
pub use PreparedPhysicalOperator as PreparedSpExpansion;

pub trait LftjSource: Send + Sync {
    /// Returns `true` if this store supports Leapfrog Triejoin acceleration.
    ///
    /// Only `LoudsStore` (and future Ring-backed stores) return `true`.
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

    /// Zero-allocation cardinality probe for adaptive VEO.
    ///
    /// Unlike `lftj_join_scan`, this never constructs a
    /// `Box<dyn TrieIterator>` — backends that can answer it cheaply (e.g.
    /// the LOUDS-based CLTJ/Ring implementation, which reuses its
    /// `seek`/`open` navigation logic on raw locals instead of iterator
    /// objects) should override it to return `Some(count)`. Returns `None`
    /// when unsupported, in which case the caller falls back to
    /// `lftj_estimate_count`.
    fn lftj_real_count(
        &self,
        _s: Option<u64>,
        _p: Option<u64>,
        _o: Option<u64>,
        _target_field: usize,
        _graph_id: u8,
    ) -> Option<u64> {
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

    /// Optional multi-subject object intersection (W4b / braided D1–D2).
    ///
    /// When LFTJ would open ≥2 leapfrog scans that all iterate **objects** under
    /// distinct bound subjects (triangle closing edge / product G3 shape), the
    /// engine may call this once instead of N independent `lftj_join_scan`s.
    ///
    /// `subjects`: external TermIds of the bound subjects (length ≥ 2).
    /// `predicate`: optional bound predicate TermId shared by all patterns.
    /// `graph_id`: internal graph id (0 = default).
    ///
    /// Default: `None` → engine keeps ordinary multi-scan leapfrog (LOUDS).
    /// RingStore returns a D1 (2-way) or D2 (3-way) streaming iterator.
    fn lftj_multi_subject_object_intersect(
        &self,
        _subjects: &[u64],
        _predicate: Option<u64>,
        _graph_id: u8,
    ) -> Option<Box<dyn crate::trie::TrieIterator>> {
        None
    }

    /// K7.2: prepare fixed-P D1 context once per wedge query.
    ///
    /// Default `None` — only Ring (mmap) implements this. Callers fall back to
    /// [`lftj_multi_subject_object_intersect`] per `(a,b)` pair.
    fn lftj_prepare_pred_object_intersect(
        &self,
        _predicate: u64,
        _graph_id: u8,
    ) -> Option<Box<dyn PreparedPredObjectIntersect>> {
        None
    }

    /// K9.2: prepare a resettable SP→O scanner for a fixed predicate.
    ///
    /// Default `None` — Ring implements this. Callers fall back to repeated
    /// `lftj_join_scan(Some(s), Some(p), None, 2, …)`.
    fn lftj_prepare_sp_object_scan(
        &self,
        _predicate: u64,
        _graph_id: u8,
    ) -> Option<Box<dyn PreparedSpObjectScan>> {
        None
    }

    /// K9: prepare a two-hop path plan `?a P1 ?b . ?b P2 ?c`.
    ///
    /// Default `None` — Ring implements with resettable hop scanners.
    /// The query engine falls back to generic LFTJ when unavailable.
    fn lftj_prepare_two_hop(
        &self,
        _p1: u64,
        _p2: u64,
        _graph_id: u8,
    ) -> Option<Box<dyn PreparedTwoHop>> {
        None
    }

    /// K11: prepare a fixed-P wedge plan `?a P ?b . ?b P ?c . ?a P ?c`.
    ///
    /// Default `None` — Ring implements with predicate adjacency + D1.
    /// The query engine falls back to the K7.2 bind_left / multi_subject path.
    fn lftj_prepare_wedge(
        &self,
        _predicate: u64,
        _graph_id: u8,
    ) -> Option<Box<dyn PreparedWedge>> {
        None
    }

    /// Prepare SP-expansion / 2join: `?s P_filter O_filter . ?s P_expand ?o`.
    ///
    /// Default `None` — Ring implements a dense-internal plan that:
    /// - materializes outer subjects of (P_filter, O_filter) once (cached)
    /// - expands each subject under P_expand via K9.4 adj + Singleton O access
    /// - emits external `(s, o, 0)` without per-subject external↔dense remap
    ///
    /// Query engine falls back to `lftj_prepare_sp_object_scan` + outer join_scan.
    fn lftj_prepare_sp_expansion(
        &self,
        _p_filter: u64,
        _o_filter: u64,
        _p_expand: u64,
        _graph_id: u8,
    ) -> Option<Box<dyn PreparedSpExpansion>> {
        None
    }
}

/// A single write operation for batch/transactional application via
/// [`QuadStore::apply_batch`].
///
/// This exists so that a caller with a *mix* of inserts and removes to apply
/// (e.g. a SPARQL `DELETE/INSERT` update, or a bulk-load routine that also
/// needs to retract some superseded facts) can hand the whole batch to the
/// store as a single logical unit, rather than issuing separate `insert`/
/// `remove` calls (each of which may acquire a lock and hit the WAL
/// independently on a persistent backend). See [`QuadStore::apply_batch`]'s
/// doc comment for the durability/atomicity contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum QuadOp {
    /// Insert this quad (idempotent if already present).
    Insert(Quad),
    /// Remove this quad (no-op if not present).
    Remove(Quad),
}

pub trait QuadStore: Send + Sync + LftjSource {
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
    /// membership (e.g. `MemoryStore`, `LoudsStore`) should override this.
    fn register_named_graph(&self, _graph: &GraphName) -> Result<(), Oxigraph> {
        Ok(())
    }

    /// Bulk-insert from a boxed iterator. Default impl calls `insert` in a
    /// loop; backends may override for efficiency (e.g. batch writes, skip
    /// WAL).
    ///
    /// Takes `Box<dyn Iterator<...>>` rather than a generic `impl
    /// IntoIterator` parameter so this method stays **object-safe** — a
    /// generic method on a trait prevents that trait from being used as
    /// `dyn QuadStore`/`Box<dyn QuadStore>`, which is otherwise a completely
    /// reasonable way to select a storage backend at runtime (e.g. a
    /// downstream project switching between `MemoryStore` and `LoudsStore`
    /// based on a config flag). Call sites that have a concrete iterator
    /// type can still pass it directly via `Box::new(iter)`; see
    /// [`QuadStoreExt::extend`] for an ergonomic generic wrapper that
    /// accepts any `impl IntoIterator<Item = Quad>` and boxes it for you.
    fn extend_boxed(&self, quads: Box<dyn Iterator<Item = Quad> + '_>) -> Result<usize, Oxigraph> {
        let mut count = 0usize;
        for quad in quads {
            if self.insert(&quad)? {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Apply a mixed batch of inserts/removes as a single logical unit.
    ///
    /// This is the transactional/batch seam for callers that need to apply
    /// more than one write — e.g. a SPARQL `DELETE { .. } INSERT { .. }
    /// WHERE { .. }` update, or any bulk routine that needs to retract some
    /// facts while adding others — without issuing N independent `insert`/
    /// `remove` calls.
    ///
    /// Returns `(inserted, removed)`: the number of `QuadOp::Insert` ops that
    /// were newly inserted (i.e. `insert` would have returned `true`) and the
    /// number of `QuadOp::Remove` ops that actually removed a present quad
    /// (i.e. `remove` would have returned `true`) — mirroring the existing
    /// `bool` return convention of `insert`/`remove` and the `usize` count
    /// returned by `extend_boxed`, just split two ways since a batch can
    /// contain both kinds of op.
    ///
    /// ## Default implementation
    ///
    /// The default here simply loops, calling `insert`/`remove` per-op in
    /// order. This is always *correct* (each op is applied, in order), but
    /// on a backend with its own internal lock and/or write-ahead log this
    /// means N lock acquisitions and (worst case) N fsyncs — no different
    /// from the caller doing the loop itself.
    ///
    /// ## Backends with a WAL/lock (e.g. `LoudsStore`)
    ///
    /// Should override this method to:
    /// 1. Acquire their internal lock **once** for the whole batch.
    /// 2. Write every resulting `WalRecord` (in the same order as `ops`) in
    ///    a **single** `append_batch` call — one `fsync` for the whole
    ///    batch instead of one per op — exactly mirroring the existing
    ///    `extend_boxed` bulk-insert override, just generalized to mixed
    ///    insert/remove ops.
    /// 3. Only then apply each op to in-memory state, in order.
    ///
    /// ## Durability vs. atomicity — no rollback on partial failure
    ///
    /// The "log intent durably BEFORE applying" discipline guarantees that a
    /// crash partway through step 3 is always *recoverable*: replaying the
    /// WAL from the start of the batch on the next `open()` re-applies every
    /// op, including ones that hadn't yet been applied to in-memory state at
    /// the moment of the crash. It also guarantees concurrent readers never
    /// observe a *partially-applied* batch mid-flight (the lock is held for
    /// the whole operation, so a batch's writes become visible atomically
    /// from a reader's perspective).
    ///
    /// It does **not**, however, guarantee that an in-process error raised
    /// by `apply_insert`/`apply_remove` partway through step 3 (as opposed to
    /// a process crash) can be rolled back: by the time step 3 begins, every
    /// op's `WalRecord` has already been durably written in step 2. If step
    /// 3 then fails on, say, the 5th of 10 ops, the in-memory state reflects
    /// only ops 1–4, but the WAL now claims the full batch was intended. A
    /// subsequent crash-and-replay (even one unrelated to this failure) would
    /// apply all 10 ops, which may diverge from the state the store was
    /// actually left in immediately after the failure. In practice
    /// `apply_insert`/`apply_remove` only fail on dictionary-interning I/O
    /// errors (not on ordinary content), making this a rare edge case, but it
    /// means `apply_batch` is a **durability/visibility** batching seam, not
    /// a full ACID transaction with in-process rollback.
    fn apply_batch(&self, ops: &[QuadOp]) -> Result<(usize, usize), Oxigraph> {
        let mut inserted = 0usize;
        let mut removed = 0usize;
        for op in ops {
            match op {
                QuadOp::Insert(q) => {
                    if self.insert(q)? {
                        inserted += 1;
                    }
                }
                QuadOp::Remove(q) => {
                    if self.remove(q)? {
                        removed += 1;
                    }
                }
            }
        }
        Ok((inserted, removed))
    }

    // ── Observability (optional) ──────────────────────────────────────────────

    // Storage-specific metrics for the `/metrics` endpoint (see
    // `oxigraph_nova_server`). Every method here defaults to `None`,
    // meaning "this backend doesn't track that metric" — `nova-server`
    // simply omits the corresponding line from its Prometheus-text-format
    // output rather than reporting a misleading `0`. Only `LoudsStore`
    // currently overrides these (see its `QuadStore` impl); `MemoryStore`
    // and any other backend inherit the defaults.

    /// Number of live entries (inserts + tombstones) sitting in an
    /// uncompacted write-buffer/delta, if this backend has one.
    fn delta_len(&self) -> Option<usize> {
        None
    }

    /// Total number of completed compactions (manual + automatic) since
    /// process start, if this backend supports compaction.
    fn compaction_count(&self) -> Option<u64> {
        None
    }

    /// Cumulative wall-clock time spent inside compaction since process
    /// start, in fractional seconds, if this backend supports compaction.
    fn compaction_duration_seconds_total(&self) -> Option<f64> {
        None
    }
}

/// Ergonomic, generic bulk-insert helper for any `QuadStore` (object-safe or
/// not — this extension trait, unlike `QuadStore` itself, is allowed a
/// generic method since it is never used as `dyn QuadStoreExt`).
///
/// Blanket-implemented for every `QuadStore`, so callers can write
/// `store.extend(quads)` exactly as before this dyn-safety fix, passing any
/// `impl IntoIterator<Item = Quad>` (a `Vec<Quad>`, a parser iterator, etc.)
/// without manually boxing it first.
pub trait QuadStoreExt: QuadStore {
    /// Bulk-insert from any iterable. Boxes the iterator and forwards to
    /// [`QuadStore::extend_boxed`] — see that method's doc comment for why
    /// the underlying trait method takes a boxed iterator instead of a
    /// generic parameter.
    fn extend(&self, quads: impl IntoIterator<Item = Quad>) -> Result<usize, Oxigraph> {
        self.extend_boxed(Box::new(quads.into_iter()))
    }
}

impl<T: QuadStore + ?Sized> QuadStoreExt for T {}
