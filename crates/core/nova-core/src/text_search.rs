//! `TextSearch` — the storage-agnostic full-text search seam.
//!
//! Mirrors the [`crate::QuadStore`] pattern: a trait defined here (in
//! `nova-core`, which has **no** dependency on any particular search-index
//! library) that a storage backend can optionally implement. `nova-query`'s
//! evaluator depends only on this trait (via `Option<Arc<dyn TextSearch>>`
//! threaded through `QueryOptions`), never on the concrete index
//! implementation — so `nova-core` and `nova-query` stay free of any
//! full-text-search dependency, and the feature is entirely opt-in.
//!
//! `oxigraph-nova-engine-ring` implements this trait for `LoudsStore` behind
//! its `fulltext` cargo feature (see `crates/core/nova-fulltext` for the actual
//! Tantivy-backed implementation it delegates to).
//!
//! ## Consistency model
//!
//! Implementations are expected to index **incrementally per compaction**,
//! not on every write — i.e. results are "compaction-eventually-consistent",
//! exactly like `QuadStore::lftj_has_delta`'s nested-loop-fallback semantics
//! for the Ring index itself. A quad inserted since the last compaction may
//! not yet be found by `search`; this is a documented, accepted trade-off in
//! exchange for indexing living on the (infrequent) LSM merge cycle rather
//! than the (hot) write path.
//!
//! ## TermId stability
//!
//! Search results are returned as raw `u64` values corresponding to
//! `TermId::as_u64()` for the *object* term of each matching quad.
//! `TermId`s are insertion-order-stable across `Dictionary::compact()` (see
//! `dict.rs`'s `intern()` doc comment), so an implementation backed by an
//! index keyed by `TermId::as_u64()` never needs to remap documents when the
//! dictionary's compacted tier is rebuilt.

/// A single full-text match: the object `TermId` (as `u64`) of a quad whose
/// literal object matched the search, along with the quad's other
/// coordinates so callers can further filter/join without a second lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextMatch {
    /// `TermId::as_u64()` of the matching object literal.
    pub object_id: u64,
    /// `TermId::as_u64()` of the quad's subject.
    pub subject_id: u64,
    /// `TermId::as_u64()` of the quad's predicate.
    pub predicate_id: u64,
    /// `GraphId::as_u8()` of the quad's graph.
    pub graph_id: u8,
}

/// Storage-agnostic full-text search capability, implemented by backends
/// that maintain a text index alongside their primary quad storage.
///
/// All methods are read-only from the caller's perspective — indexing is
/// entirely the implementer's responsibility (typically hooked into the
/// backend's own compaction/merge cycle).
pub trait TextSearch: Send + Sync {
    /// Run a free-text query (implementation-defined query syntax — Tantivy's
    /// query-parser syntax for the reference implementation) against indexed
    /// literal objects, optionally restricted to a single predicate.
    ///
    /// `predicate_id`: `Some(id)` restricts the search to literal objects of
    /// exactly that predicate (`TermId::as_u64()`); `None` searches across
    /// all indexed predicates.
    ///
    /// `limit`: maximum number of matches to return (implementations may
    /// return fewer, e.g. if the underlying index has fewer hits, but must
    /// not return more).
    ///
    /// Returns matches ordered by relevance score (descending), best-effort.
    fn search(&self, query: &str, predicate_id: Option<u64>, limit: usize) -> Vec<TextMatch>;

    /// `true` if this store has full-text search enabled and its index is
    /// ready to be queried (i.e. built at least once, whether via compaction
    /// or a startup rebuild). `false` means `search` will always return an
    /// empty `Vec` (e.g. the feature is compiled in but the store has never
    /// compacted yet).
    fn text_search_ready(&self) -> bool {
        true
    }
}
