pub mod dict;
mod dict_block_cache;
mod dict_compact;
pub mod engine;
pub mod error;

pub mod store;
pub mod text_search;
pub mod trie;

pub use dict::{Dictionary, GRAPH_DEFAULT, GraphId, MAX_TERM_ID, TermId};
pub use dict_block_cache::{
    BlockCachedCompactedTier, DEFAULT_BUF_BLOCK_CACHE, DEFAULT_BUF_LZ4_BLOCK, DictIndexSnapshot,
    dict_block_cache_types,
};
pub use dict_compact::DictSnapshot;
pub use engine::{
    BackendFactory, StorageEngine, StorageEngineExt, SyncPolicy, available_backends,
    backend_names_csv, default_backend_name, lookup_backend, new_backend, open_backend,
    require_backend,
};
pub use error::Oxigraph;
// Re-export the Oxigraph RDF type system directly — no custom wrappers.

// These are battle-tested, W3C-correct, and used by oxttl/spargebra already.
// Variable is included here so downstream crates share the exact same type
// as spargebra (spargebra::term re-exports oxrdf::Variable).
pub use oxrdf::{
    BlankNode, GraphName, Literal, NamedNode, NamedOrBlankNode, Quad, Term, Triple, Variable,
};
use std::sync::Arc;
pub use store::{
    LftjSource, PhysicalShape, PreparedDirectedTriangle, PreparedKChain, PreparedLeftIntersect,
    PreparedPhysicalOperator, PreparedPredObjectIntersect, PreparedSpExpansion,
    PreparedSpObjectScan, PreparedStar, PreparedTwoHop, PreparedWedge, QuadOp, QuadStore,
    QuadStoreExt,
};
pub use text_search::{TextMatch, TextSearch};
pub use trie::{EmptyTrieIter, TrieIterator};

// oxrdf 0.3 exports `Subject = NamedOrBlankNode` but marks it deprecated
// (slated for removal in oxrdf 0.5).  Define our own alias here so that
// oxigraph-nova crates never import the deprecated oxrdf name and gain an explicit
// migration point when oxrdf 0.5 arrives.
pub type Subject = NamedOrBlankNode;

/// A quad returned by [`QuadStore::quads_for_pattern`] that fully supports
/// RDF 1.2 / RDF-star subjects.
///
/// Unlike `oxrdf::Quad` (where `subject` is `NamedOrBlankNode` = no `Triple`
/// variant), this type uses `Term` for the subject field so that a stored
/// quoted-triple subject (`Term::Triple`) can be decoded and returned from
/// the Ring without being silently dropped.
///
/// Used only on the **read** path.  Insertion still goes through
/// `QuadStore::insert(&oxrdf::Quad)` — inserting a quoted-triple subject
/// requires the caller to pack the data via the dictionary directly (a
/// limitation of oxrdf 0.3 that will be lifted in 0.5).
///
/// ## Memory footprint
///
/// All three term positions are `Arc<Term>` rather than owned values. The
/// dictionary already stores each interned term as a shared `Arc<Term>`
/// (see `Dictionary::get_term_arc`), so decoding a matched row can clone
/// the `Arc` (a cheap refcount bump) instead of deep-cloning the term's
/// heap-allocated string content. This matters most for join-heavy or
/// large-result queries, where deep-cloning every subject/predicate/object
/// for every matched row would otherwise be expensive.
///
/// `predicate` is always a `Term::NamedNode` for well-formed RDF quads;
/// use [`StoredQuad::predicate_named_node`] when a `&NamedNode` is required
/// (e.g. RDF serializers that take `TripleRef`/`QuadRef`).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredQuad {
    pub subject: Arc<Term>,
    pub predicate: Arc<Term>,
    pub object: Arc<Term>,
    pub graph_name: GraphName,
}

impl StoredQuad {
    /// Borrow the predicate IRI. Returns `None` only if the quad is
    /// ill-formed (predicate is not a named node) — production decoders
    /// never emit such rows.
    #[inline]
    pub fn predicate_named_node(&self) -> Option<&NamedNode> {
        match self.predicate.as_ref() {
            Term::NamedNode(n) => Some(n),
            _ => None,
        }
    }
}

impl From<Quad> for StoredQuad {
    fn from(q: Quad) -> Self {
        Self {
            subject: Arc::new(Term::from(q.subject)),
            predicate: Arc::new(Term::NamedNode(q.predicate)),
            object: Arc::new(q.object),
            graph_name: q.graph_name,
        }
    }
}

pub type Result<T> = std::result::Result<T, Oxigraph>;
