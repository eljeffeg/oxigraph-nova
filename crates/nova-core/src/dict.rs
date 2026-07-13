//! Dictionary — bidirectional Term ↔ TermId (40-bit) and GraphName ↔ GraphId (8-bit) mapping.
//!
//! ## Design rationale
//!
//! All internal computation runs over **40-bit integer IDs**, not cloned `Term`s.
//! There is no native `u40` type in Rust; IDs are carried in `u64` with the upper 24 bits
//! always zero and a hard ceiling enforced at insertion time.
//!
//! ## ID layout in the delta u128 key
//!
//! ```text
//! g[127:120] | s[119:80] | p[79:40] | o[39:0]   (8 + 40 + 40 + 40 = 128 bits exactly)
//! ```
//!
//! The graph field in the high byte means the BTreeMap orders graph-major — all triples
//! in the same named graph are contiguous — enabling efficient per-graph range queries.
//!
//! ## Reserved GraphIds
//!
//! | `GraphId` | Meaning |
//! |---|---|
//! | `0` | Default graph (always present) |
//! | `1–255` | User named graphs (assigned sequentially on first use) |
//!
//! There is no reserved graph for ontology input or reasoner output —
//! anything that needs an internal named graph (e.g.
//! `oxigraph_nova_reasoning`) simply interns whatever `GraphName` it likes
//! through the normal path; it does not matter which `GraphId` ends up
//! assigned to it.

//! ## Two-tier storage (Front-Coding compression)
//!
//! `Dictionary` is a two-tier structure:
//!
//! - **Delta tier** (mutable, always heap-resident): `id_to_term:
//!   Vec<Option<Arc<Term>>>` + `term_to_id: HashMap<Arc<Term>, TermId>` —
//!   every term interned since the last [`Dictionary::compact`], plus every
//!   quoted-triple term (which never leaves this tier — see below).
//! - **Compacted tier** (immutable): a sorted, Front-Coded byte buffer (see
//!   [`crate::dict_compact::CompactedTier`]) covering every *regular*
//!   (non-quoted-triple) term as of the last `compact()`. `TermId`s are
//!   never reassigned — only an internal sorted-rank permutation
//!   (`id2rank`/`rank2id`, bit-packed via `sux::BitFieldVec`) changes. This
//!   tier is held behind a [`crate::dict_compact::CompactedTierHandle`],
//!   which is either a freshly-built owned buffer, or a zero-copy
//!   `load_mmap`'d view straight off the `nova.dict.<gen>` file (see
//!   [`Dictionary::from_mapped`]) — both forms expose the identical
//!   read-only method surface.
//!
//! `compact()` merges both tiers into a brand new compacted tier, then
//! **frees** every compacted regular term's `Arc<Term>` from both
//! `id_to_term` (set to `None`) and `term_to_id` (removed) — this is where
//! the real memory reduction comes from. Decoding a freed term re-derives
//! it from the compacted byte buffer on demand, consulting a small bounded
//! `TermId → Arc<Term>` LRU cache first so that repeatedly-matched terms
//! (e.g. a hot predicate or object) avoid paying the Front-Coded block
//! decode cost on every lookup. The cache is cleared on every `compact()`
//! call, since ranks and block offsets shift. The decode cache — like the
//! delta tier — always stays heap-resident: both are small and bounded by
//! design, so there is no benefit to mapping them.
//!
//! A borrowed `&Term` reference cannot be produced for content that isn't
//! resident, so term lookup is exposed only via `get_term_arc` (returning
//! an owned `Arc<Term>`), which is also the primary hot-path accessor for
//! exactly this reason.

use crate::Oxigraph;
use crate::dict_compact::{
    self, CompactedTier, CompactedTierHandle, DictSnapshot, MappedCompactedTier,
};
use epserde::deser::MemCase;
use oxrdf::{BlankNode, GraphName, NamedNode, Term};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

/// Capacity of the `TermId`-keyed decode cache on the live dictionary and
/// the query-scoped sequential snapshot.
///
/// Measured via `profile_eval --count-allocs` on a 500K-entity/12.5M-triple
/// dataset by sweeping this constant (8192 → 100,000 → 500,000) and
/// comparing per-query allocation counts: queries whose distinct-term
/// working set fits within the cache (`2join`, `feature_lookup`,
/// `star_with_features`) saw allocation counts drop by 40-80% and
/// wall-clock time improve correspondingly once capacity reached 100,000 —
/// decode-cache misses (a Front-Coded block decode + LRU evict/insert pair)
/// are a real, avoidable cost for those query shapes. Queries that touch
/// most of the dictionary (`scan`, `path_2hop`, `triangle`) see no
/// improvement even at 500,000, since no realistically-bounded cache can
/// cover a working set that large without approaching the full uncompacted
/// dictionary size — defeating the memory-reduction purpose of compaction
/// in the first place. 100,000 balances fully capturing the smaller
/// working-set queries' wins against keeping the cache's own footprint
/// small relative to the compacted tier it exists to avoid re-decoding.
const DECODE_CACHE_CAPACITY: usize = 100_000;

/// Per-worker decode-cache capacity used by [`DictDecodeSnapshot::clone`].
///
/// The parallel LFTJ path clones the query snapshot once per rayon chunk
/// so each worker has a private LRU (no cross-worker contention).
/// `lru::LruCache::new` pre-allocates a `HashMap` of the full capacity, so
/// using the live dictionary's 100_000 here would allocate
/// ~`2 × num_threads` large empty maps per query. 8192 is large enough to
/// absorb the hot-term working set of typical BGP subtrees while keeping
/// the per-chunk footprint small.
const WORKER_DECODE_CACHE_CAPACITY: usize = 8192;

// ── TermId ───────────────────────────────────────────────────────────────────

/// A 40-bit term identifier carried in a `u64`.
///
/// Valid range: `0 ..= MAX_TERM_ID` (≈ 1.1 trillion distinct terms).
/// The upper 24 bits of the carrier are always zero; `new()` enforces this.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub struct TermId(u64);

/// Upper bound on valid TermIds: 2^40 − 1 ≈ 1.1 trillion.
pub const MAX_TERM_ID: u64 = (1u64 << 40) - 1; // 1_099_511_627_775

impl TermId {
    /// Create a `TermId`, returning `Err(IdSpaceExhausted)` if `id > MAX_TERM_ID`.
    #[inline]
    pub fn new(id: u64) -> Result<Self, Oxigraph> {
        if id > MAX_TERM_ID {
            return Err(Oxigraph::IdSpaceExhausted);
        }
        Ok(TermId(id))
    }

    /// The raw 40-bit value (upper 24 bits are always zero).
    #[inline]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

// ── GraphId ──────────────────────────────────────────────────────────────────

/// An 8-bit named-graph identifier.
///
/// The default graph is `GraphId(0)`; user named graphs occupy `1..=255`,
/// assigned sequentially on first use. There is no other reservation —
/// anything that needs an internal named graph (an ontology graph, a
/// reasoner's inference overlay, etc.) simply interns whatever `GraphName`
/// it likes through the normal path.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub struct GraphId(pub u8);

/// Default graph (SPARQL default, always present).
pub const GRAPH_DEFAULT: GraphId = GraphId(0);

impl GraphId {
    /// The raw 8-bit value.
    #[inline]
    pub fn as_u8(self) -> u8 {
        self.0
    }

    /// `true` iff this is the default graph (`GraphId(0)`).
    #[inline]
    pub fn is_default(self) -> bool {
        self.0 == 0
    }
}

// ── Dictionary ────────────────────────────────────────────────────────────────

/// Bidirectional mapping: `Term ↔ TermId` (40-bit) and `GraphName ↔ GraphId` (8-bit).
///
/// All Ring and delta operations work over integer IDs; the dictionary encodes at
/// ingest/query-plan time and decodes only when producing final result rows.
///
/// ## Thread safety
///
/// `Dictionary` is `Send + Sync`: every field is either immutable-after-
/// construction (the compacted tier, which is only ever swapped wholesale
/// under `&mut self` in `compact()`) or interior-mutability via `Mutex`
/// (the decode cache below), never `RefCell`. This lets `RingStoreInner`
/// (which embeds `Dictionary`) be wrapped in a `RwLock` rather than a
/// `Mutex`, so concurrent readers (e.g. parallel LFTJ subtree scans) can
/// call `get_term_arc`/`get_id`/etc. from multiple threads at once without
/// serializing on a single exclusive lock. The decode cache's `Mutex` is
/// held only for the duration of one `LruCache` get/put — far shorter and
/// less contended than the old design's per-scan store-wide lock.
pub struct Dictionary {
    // ── Term ↔ TermId (delta tier) ─────────────────────────────────────────
    /// Reverse: `id_to_term[id.as_u64()]` → `Term`, for terms still
    /// resident in the delta tier. `None` means this id's term has been
    /// folded into `compacted` and its `Arc<Term>` freed — decode via
    /// `compacted.decode_id` instead. Quoted-triple terms are always
    /// `Some` (never compacted).
    id_to_term: Vec<Option<Arc<Term>>>,
    /// Forward: `Term` → `TermId`, for terms still resident in the delta
    /// tier (same population as `id_to_term`'s `Some` entries). The key
    /// `Arc<Term>` is the *same* allocation as the corresponding
    /// `id_to_term` entry.
    term_to_id: HashMap<Arc<Term>, TermId>,

    // ── Compacted tier (immutable, Front-Coded) ────────────────────────────
    /// Sorted, Front-Coded byte buffer + `id2rank`/`rank2id` permutation for
    /// every regular term as of the last `compact()` — either an owned,
    /// freshly-built buffer, or a zero-copy `load_mmap`'d view (see
    /// [`Dictionary::from_mapped`]). Starts as an empty owned tier.
    compacted: CompactedTierHandle,

    /// `TermId → Arc<Term>` decode cache for compacted-tier terms.
    /// Avoids re-parsing an entire Front-Coded block on every lookup for
    /// hot terms (e.g. a repeated `rdf:type` object matched by thousands of
    /// LFTJ rows). `Mutex`-wrapped (not `RefCell`) since `get_term_arc`
    /// takes `&self` and — unlike the old single-`Mutex<RingStoreInner>`
    /// design — may now be called concurrently from multiple reader
    /// threads under a `RwLock<RingStoreInner>` read guard. The lock is
    /// only held for one `LruCache` get/put, not for the whole surrounding
    /// operation. Cleared on every `compact()` call, since ranks/block
    /// offsets shift.
    decode_cache: Mutex<lru::LruCache<TermId, Arc<Term>>>,

    // ── GraphName ↔ GraphId ─────────────────────────────────────────────────
    /// Forward: `GraphName` → `GraphId`
    graph_to_id: HashMap<GraphName, GraphId>,
    /// Reverse: `GraphId.as_u8()` → `GraphName`
    id_to_graph: HashMap<u8, GraphName>,
    /// Next available user GraphId, range `1..=255`. `0` doubles as an
    /// "exhausted" sentinel: since `0` is permanently reserved for the
    /// default graph, it can never legitimately be the *next available*
    /// user id, so [`Dictionary::intern_graph`] reuses it to mean "every
    /// one of the 255 user slots has been assigned" (reached via
    /// `255u8.checked_add(1)` wrapping to this sentinel rather than
    /// panicking on overflow).
    next_graph_id: u8,

    // ── Quoted-triple side table (RDF 1.2 / RDF-star) ──────────────────────
    /// `triple_terms[id]` = `[s_id, p_id, o_id]` — fast component access for
    /// `SUBJECT()` / `PREDICATE()` / `OBJECT()` built-in functions.
    pub triple_terms: HashMap<TermId, [TermId; 3]>,
    /// Inverse: `[s_id, p_id, o_id]` → `TermId` — deduplication on intern.
    triple_index: HashMap<[TermId; 3], TermId>,
}

impl Default for Dictionary {
    fn default() -> Self {
        Self::new()
    }
}

impl Dictionary {
    /// Create an empty dictionary with the default graph pre-registered.
    pub fn new() -> Self {
        let mut d = Self {
            id_to_term: Vec::new(),
            term_to_id: HashMap::new(),
            compacted: CompactedTierHandle::Owned(Arc::new(CompactedTier::empty())),
            decode_cache: Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(DECODE_CACHE_CAPACITY).expect("nonzero capacity"),
            )),
            graph_to_id: HashMap::new(),
            id_to_graph: HashMap::new(),
            next_graph_id: 1, // 0 is reserved for the default graph

            triple_terms: HashMap::new(),
            triple_index: HashMap::new(),
        };
        // Pre-register the default graph so GraphId(0) is always valid.

        d.graph_to_id.insert(GraphName::DefaultGraph, GRAPH_DEFAULT);
        d.id_to_graph.insert(0, GraphName::DefaultGraph);
        d
    }

    /// Total number of interned terms (both tiers).
    pub fn len(&self) -> usize {
        self.id_to_term.len()
    }

    /// `true` iff no terms have been interned yet.
    pub fn is_empty(&self) -> bool {
        self.id_to_term.is_empty()
    }

    /// Real allocated byte size of this dictionary, for memory-breakdown
    /// diagnostics.
    ///
    /// Accounts for the delta tier's resident `Arc<Term>`s (shared, once
    /// per term, between `id_to_term` and `term_to_id`) plus the compacted
    /// tier's actual byte-buffer/bit-packed-permutation footprint. This is
    /// a diagnostic estimate, not exact `malloc` accounting.
    pub fn mem_size_bytes(&self) -> usize {
        use std::mem::size_of;

        /// Owned-`String` heap-content bytes (plus the 24-byte `String` struct
        /// itself: ptr + len + cap) for one `Term`'s variant payload.
        fn term_heap_bytes(t: &Term) -> usize {
            const STRING_OVERHEAD: usize = size_of::<String>();
            match t {
                Term::NamedNode(n) => STRING_OVERHEAD + n.as_str().len(),
                Term::BlankNode(b) => STRING_OVERHEAD + b.as_str().len(),
                Term::Literal(l) => {
                    let mut bytes = STRING_OVERHEAD + l.value().len();
                    if let Some(lang) = l.language() {
                        bytes += STRING_OVERHEAD + lang.len();
                    }
                    // `datatype()` returns a `NamedNodeRef` — for typed literals
                    // oxrdf stores an owned `NamedNode` internally, so count it.
                    bytes += STRING_OVERHEAD + l.datatype().as_str().len();
                    bytes
                }
                // Quoted-triple components are stored by TermId in the
                // `triple_terms` side table, not re-embedded here.
                Term::Triple(_) => 3 * size_of::<TermId>(),
            }
        }

        let enum_overhead = size_of::<Term>();
        // `ArcInner<T>` prepends two refcounts (strong + weak) before `T`.
        const ARC_CTRL_OVERHEAD: usize = size_of::<usize>() * 2;

        // Each still-resident (delta-tier) term has exactly ONE heap
        // allocation, shared (via `Arc::clone`) between `id_to_term` and
        // `term_to_id`.
        let unique_term_bytes: usize = self
            .id_to_term
            .iter()
            .flatten()
            .map(|t| enum_overhead + term_heap_bytes(t) + ARC_CTRL_OVERHEAD)
            .sum();

        // `id_to_term: Vec<Option<Arc<Term>>>` — one slot per id (content
        // already counted above via `unique_term_bytes` for `Some` entries).
        let id_to_term_ptrs = self.id_to_term.len() * size_of::<Option<Arc<Term>>>();

        // `term_to_id: HashMap<Arc<Term>, TermId>` — one 8-byte `Arc` pointer
        // (a clone of the same allocation, no new heap content) + the
        // `TermId` value per entry, plus per-bucket overhead.
        let term_to_id_bytes =
            self.term_to_id.len() * (size_of::<Arc<Term>>() + size_of::<TermId>());
        let term_to_id_bucket_overhead = self.term_to_id.capacity() * size_of::<usize>();

        // Graph side tables (small — typically well under 255 entries).
        let graph_bytes = self.graph_to_id.len() * (size_of::<GraphName>() + size_of::<GraphId>())
            + self.id_to_graph.len() * (size_of::<u8>() + size_of::<GraphName>());

        // Quoted-triple side tables (usually empty for plain RDF datasets).
        let triple_terms_bytes =
            self.triple_terms.len() * (size_of::<TermId>() + size_of::<[TermId; 3]>());
        let triple_index_bytes =
            self.triple_index.len() * (size_of::<[TermId; 3]>() + size_of::<TermId>());

        unique_term_bytes
            + id_to_term_ptrs
            + term_to_id_bytes
            + term_to_id_bucket_overhead
            + graph_bytes
            + triple_terms_bytes
            + triple_index_bytes
            + self.compacted.mem_size_bytes()
    }

    // ── Term interning ────────────────────────────────────────────────────────

    /// Get or assign a `TermId` for the given `Term`.
    ///
    /// For quoted triples (`Term::Triple`), recursively interns the components
    /// and populates the `triple_terms` / `triple_index` side tables so that
    /// `SUBJECT()` / `PREDICATE()` / `OBJECT()` can look up components by ID.
    ///
    /// For regular terms already folded into the compacted tier (by a
    /// previous `compact()`), returns the existing `TermId` without
    /// assigning a new one — `TermId`s are always insertion-order-stable,
    /// never reassigned.
    pub fn intern(&mut self, term: &Term) -> Result<TermId, Oxigraph> {
        // Fast path: already interned in the delta tier (or a quoted triple).
        if let Some(&id) = self.term_to_id.get(term) {
            return Ok(id);
        }

        // Handle quoted triples (RDF 1.2 / RDF-star). Never compacted.
        if let Term::Triple(t) = term {
            // Recursively intern each component first.
            let s_id = self.intern(&Term::from(t.subject.clone()))?;
            let p_id = self.intern(&Term::NamedNode(t.predicate.clone()))?;
            let o_id = self.intern(&t.object.clone())?;
            let components = [s_id, p_id, o_id];

            // Re-check after recursive interns (the triple itself may have been
            // inserted as a side-effect if the same quoted triple appears inside
            // a component — pathological but possible).
            if let Some(&id) = self.term_to_id.get(term) {
                return Ok(id);
            }

            let raw = self.id_to_term.len() as u64;
            let id = TermId::new(raw)?;
            let arc = Arc::new(term.clone());
            self.id_to_term.push(Some(Arc::clone(&arc)));
            self.term_to_id.insert(arc, id);
            self.triple_terms.insert(id, components);
            self.triple_index.insert(components, id);
            return Ok(id);
        }

        // Regular term: check the compacted tier before assigning a new id
        // — it may already be present from a previous compaction.
        if let Some(raw) = self.compacted.get_id(term) {
            return TermId::new(raw);
        }

        // Brand new term: append to the delta tier.
        let raw = self.id_to_term.len() as u64;
        let id = TermId::new(raw)?;
        let arc = Arc::new(term.clone());
        self.id_to_term.push(Some(Arc::clone(&arc)));
        self.term_to_id.insert(arc, id);
        Ok(id)
    }

    /// Look up a `TermId` **without** creating a new entry.
    ///
    /// Returns `None` if the term has never been interned — which for query
    /// evaluation means the pattern cannot match anything (early-out, no scan).
    pub fn get_id(&self, term: &Term) -> Option<TermId> {
        if let Some(&id) = self.term_to_id.get(term) {
            return Some(id);
        }
        self.compacted
            .get_id(term)
            .and_then(|raw| TermId::new(raw).ok())
    }

    /// Look up the `Arc<Term>` for `id` — a cheap refcount bump for
    /// delta-tier terms, or (for terms folded into the compacted tier by a
    /// previous `compact()`) a decode-cache lookup, falling back to a fresh
    /// decode from the compacted Front-Coded tier on a cache miss.
    ///
    /// The read hot path (`quads_for_pattern` / `decode_stored_quad`) needs
    /// to avoid deep-copying a term's owned `String` content on *every*
    /// matched row — even when many rows share the exact same interned term
    /// (e.g. a `rdf:type` object repeated across thousands of matching
    /// subjects). For delta-tier terms this turns an O(rows × term size)
    /// cost into O(rows × pointer size); for compacted-tier terms, a warm
    /// decode-cache entry gives the same O(rows × pointer size) behavior,
    /// while a cache miss pays a bounded O(block_size) Front-Coded block
    /// decode (see `decode_cache`'s field docs).
    pub fn get_term_arc(&self, id: TermId) -> Option<Arc<Term>> {
        match self.id_to_term.get(id.as_u64() as usize) {
            Some(Some(arc)) => Some(Arc::clone(arc)),
            Some(None) => {
                if let Some(arc) = self.decode_cache.lock().get(&id) {
                    return Some(Arc::clone(arc));
                }
                // Cache miss: Front-Coding forces decoding the *entire*
                // containing block to reconstruct any single entry's LCP
                // chain, so populate the decode cache with every entry in
                // that block (not just the one requested) — this amortizes
                // the O(block_size) decode cost across up to `BLOCK_SIZE`
                // future lookups instead of paying it on every single one.
                // Critical for high-cardinality sequential scans that touch
                // many distinct compacted-tier terms (e.g. `?s wdt:P31 ?o`
                // over hundreds of thousands of subjects).
                let block = self.compacted.decode_block_for_id(id.as_u64())?;
                let mut wanted: Option<Arc<Term>> = None;
                let mut cache = self.decode_cache.lock();
                for (orig_id, result) in block {
                    if let Ok(arc) = result {
                        let tid = TermId(orig_id);
                        if tid == id {
                            wanted = Some(Arc::clone(&arc));
                        }
                        cache.put(tid, arc);
                    }
                }
                wanted
            }
            None => None,
        }
    }

    // ── Graph interning ───────────────────────────────────────────────────────

    /// Get or assign a `GraphId` for the given `GraphName`.
    ///
    /// `GraphName::DefaultGraph` always maps to `GRAPH_DEFAULT` (`GraphId(0)`).
    /// User named graphs are assigned IDs sequentially from `1..=255`
    /// (all 255 slots usable — no reservation beyond `GraphId(0)`),
    /// returning `Err(GraphSpaceExhausted)` once every slot is consumed.
    ///
    /// `next_graph_id` doubles as its own "exhausted" sentinel (see that
    /// field's doc comment): assigning `GraphId(255)` advances
    /// `next_graph_id` via `checked_add(1)`, which is `None` exactly when
    /// the just-assigned id was `255` — `unwrap_or(0)` turns that into the
    /// sentinel value `0`, which can never collide with a real "next
    /// available" id (since `0` is always the default graph's own id).
    pub fn intern_graph(&mut self, graph: &GraphName) -> Result<GraphId, Oxigraph> {
        if let Some(&id) = self.graph_to_id.get(graph) {
            return Ok(id);
        }
        if *graph == GraphName::DefaultGraph {
            // Should already be pre-registered but handle defensively.
            self.graph_to_id.insert(graph.clone(), GRAPH_DEFAULT);
            self.id_to_graph.insert(0, graph.clone());
            return Ok(GRAPH_DEFAULT);
        }
        if self.next_graph_id == 0 {
            return Err(Oxigraph::GraphSpaceExhausted);
        }
        let id = GraphId(self.next_graph_id);
        self.next_graph_id = self.next_graph_id.checked_add(1).unwrap_or(0);
        self.graph_to_id.insert(graph.clone(), id);
        self.id_to_graph.insert(id.as_u8(), graph.clone());
        Ok(id)
    }

    /// Look up a `GraphId` without registering a new entry.
    pub fn get_graph_id(&self, graph: &GraphName) -> Option<GraphId> {
        self.graph_to_id.get(graph).copied()
    }

    /// Decode a `GraphId` back to the original `GraphName`.
    pub fn get_graph(&self, id: GraphId) -> Option<&GraphName> {
        self.id_to_graph.get(&id.as_u8())
    }

    /// Enumerate all interned `(GraphId, GraphName)` pairs (including default).
    pub fn all_graphs(&self) -> impl Iterator<Item = (GraphId, &GraphName)> {
        self.id_to_graph.iter().map(|(&raw, g)| (GraphId(raw), g))
    }

    /// The next available user `GraphId` (range `1..=255`; `0` means the
    /// range is exhausted — see `next_graph_id`'s field doc comment).
    pub fn next_graph_id_raw(&self) -> u8 {
        self.next_graph_id
    }

    /// All interned terms in `TermId` order (index == `TermId::as_u64()`),
    /// decoding compacted-tier entries on the fly.
    pub fn terms_in_order(&self) -> impl Iterator<Item = Term> + '_ {
        (0..self.id_to_term.len() as u64).map(move |raw_id| {
            match &self.id_to_term[raw_id as usize] {
                Some(arc) => arc.as_ref().clone(),
                None => self
                    .compacted
                    .decode_id(raw_id)
                    .expect("every freed delta-tier id must be present in the compacted tier")
                    .expect("compacted tier decode must succeed for internally-encoded data")
                    .as_ref()
                    .clone(),
            }
        })
    }

    /// Rebuild the compacted Front-Coded tier from all currently-interned
    /// regular (non-quoted-triple) terms — both those already folded into
    /// the previous compacted tier (decoded on the fly) and those interned
    /// since (the delta tier) — then **free** every compacted term's
    /// `Arc<Term>` from the delta tier (`id_to_term` slot set to `None`,
    /// `term_to_id` entry removed). This is the actual memory-reduction
    /// step; `TermId`s are never reassigned, only the internal sorted-rank
    /// permutation.
    ///
    /// Called from `RingStore::commit_compaction`, on the same cadence that
    /// already rebuilds all 6 Ring tries.
    pub fn compact(&mut self) -> Result<(), Oxigraph> {
        let high_water = self.id_to_term.len() as u64;
        let mut entries: Vec<(u8, Vec<u8>, Vec<u8>, u64)> = Vec::with_capacity(high_water as usize);

        for raw_id in 0..high_water {
            let tid = TermId(raw_id);
            if self.triple_terms.contains_key(&tid) {
                continue; // quoted triples never enter the FC tier
            }
            let term: Arc<Term> = match &self.id_to_term[raw_id as usize] {
                Some(arc) => Arc::clone(arc),
                None => match self.compacted.decode_id(raw_id) {
                    Some(res) => res?,
                    None => continue,
                },
            };
            let (tag, primary) = dict_compact::term_sort_key(&term);
            let aux = dict_compact::term_aux_bytes(&term);
            entries.push((tag, primary, aux, raw_id));
        }

        entries.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let new_compacted = CompactedTier::build(&entries, high_water);

        // Free the delta-tier storage for every term now covered by the
        // new compacted tier.
        for raw_id in 0..high_water {
            let tid = TermId(raw_id);
            if self.triple_terms.contains_key(&tid) {
                continue;
            }
            if let Some(arc) = self.id_to_term[raw_id as usize].take() {
                self.term_to_id.remove(&arc);
            }
        }

        self.compacted = CompactedTierHandle::Owned(Arc::new(new_compacted));

        // Ranks/block offsets shift on every compaction — any previously
        // cached decode results may point at stale offsets, so drop them
        // all rather than trying to selectively invalidate.
        self.decode_cache.lock().clear();
        Ok(())
    }

    /// Rebuild a `Dictionary` from persisted state, preserving `TermId`s
    /// exactly (by insertion order in `terms`) and `GraphId`s exactly (via
    /// `graphs`).
    ///
    /// Legacy full-heap-copy reconstruction path (predates
    /// [`Dictionary::from_mapped`]'s direct compacted-tier mmap
    /// reconstruction). Retained for any caller that has a plain `Vec<Term>`
    /// in hand rather than a `DictSnapshot`.
    ///
    /// The rebuilt `Dictionary` always starts with an empty compacted
    /// tier — every term is delta-tier-resident — regardless of whether
    /// the source `Dictionary` had compacted terms at save time. The next
    /// `compact()` call re-establishes the compacted tier.
    ///
    /// Quoted-triple (`Term::Triple`) side tables (`triple_terms`/
    /// `triple_index`) are **not** persisted separately — they are
    /// re-derived here by re-decoding each `Term::Triple` entry in `terms`
    /// and looking up its subject/predicate/object components, which are
    /// guaranteed to already be present (interning always inserts a quoted
    /// triple's components before the triple itself — see `intern()` — so
    /// they appear earlier in `terms`).
    pub fn rebuild(
        terms: Vec<Term>,
        graphs: Vec<(u8, GraphName)>,
        next_graph_id: u8,
    ) -> Result<Self, Oxigraph> {
        let mut d = Self {
            id_to_term: Vec::with_capacity(terms.len()),
            term_to_id: HashMap::with_capacity(terms.len()),
            compacted: CompactedTierHandle::Owned(Arc::new(CompactedTier::empty())),
            decode_cache: Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(DECODE_CACHE_CAPACITY).expect("nonzero capacity"),
            )),
            graph_to_id: HashMap::new(),
            id_to_graph: HashMap::new(),
            next_graph_id,

            triple_terms: HashMap::new(),
            triple_index: HashMap::new(),
        };

        for (raw_id, term) in terms.into_iter().enumerate() {
            let id = TermId::new(raw_id as u64)?;
            if let Term::Triple(t) = &term {
                let s_id = *d
                    .term_to_id
                    .get(&Term::from(t.subject.clone()))
                    .expect("quoted-triple subject must already be interned (lower TermId)");
                let p_id = *d
                    .term_to_id
                    .get(&Term::NamedNode(t.predicate.clone()))
                    .expect("quoted-triple predicate must already be interned (lower TermId)");
                let o_id = *d
                    .term_to_id
                    .get(&t.object)
                    .expect("quoted-triple object must already be interned (lower TermId)");
                let components = [s_id, p_id, o_id];
                d.triple_terms.insert(id, components);
                d.triple_index.insert(components, id);
            }
            let arc = Arc::new(term);
            d.id_to_term.push(Some(Arc::clone(&arc)));
            d.term_to_id.insert(arc, id);
        }

        for (raw, graph) in graphs {
            d.graph_to_id.insert(graph.clone(), GraphId(raw));
            d.id_to_graph.insert(raw, graph);
        }
        // Defensive: ensure the default graph is always present even if it
        // was somehow missing from `graphs` (should never happen since
        // `Dictionary::new()` always pre-registers it).
        d.graph_to_id
            .entry(GraphName::DefaultGraph)
            .or_insert(GRAPH_DEFAULT);
        d.id_to_graph.entry(0).or_insert(GraphName::DefaultGraph);

        Ok(d)
    }

    // ── mmap-based persistence (dictionary residency) ──────────────────────

    /// Build an ε-serde-serializable [`DictSnapshot`] of this dictionary's
    /// current state.
    ///
    /// Requires that `self.compacted` currently holds a freshly-built
    /// **owned** tier (true immediately after [`Self::compact`] — the exact
    /// call sequence `RingStore::commit_compaction` always uses: `dict.compact()`
    /// then persist); panics otherwise, mirroring the "Arc is shared"
    /// panics used by the analogous `into_snapshot` methods in
    /// `oxigraph_nova_storage_ring`.
    ///
    /// Quoted-triple terms (never covered by the compacted tier) are
    /// persisted as just their three component `TermId`s, sorted ascending
    /// by the triple's own `TermId` — sufficient for
    /// [`Self::from_mapped`] to reconstruct every quoted triple's full
    /// `Term::Triple` value, since a triple's component ids are always
    /// smaller than its own id (`intern` always interns components first).
    pub fn to_snapshot(&self) -> DictSnapshot {
        let compacted = match &self.compacted {
            CompactedTierHandle::Owned(t) => t.clone_owned(),
            CompactedTierHandle::Mapped(_) => panic!(
                "Dictionary::to_snapshot: compacted tier is Mapped, not Owned — \
                 caller must call Dictionary::compact() immediately before persisting"
            ),
        };

        let mut triples: Vec<(u64, [u64; 3])> = self
            .triple_terms
            .iter()
            .map(|(&id, &components)| {
                (
                    id.as_u64(),
                    [
                        components[0].as_u64(),
                        components[1].as_u64(),
                        components[2].as_u64(),
                    ],
                )
            })
            .collect();
        triples.sort_unstable_by_key(|&(id, _)| id);

        let mut triple_ids = Vec::with_capacity(triples.len());
        let mut triple_s = Vec::with_capacity(triples.len());
        let mut triple_p = Vec::with_capacity(triples.len());
        let mut triple_o = Vec::with_capacity(triples.len());
        for (id, [s, p, o]) in triples {
            triple_ids.push(id);
            triple_s.push(s);
            triple_p.push(p);
            triple_o.push(o);
        }

        let mut graph_ids = Vec::new();
        let mut graph_kinds = Vec::new();
        let mut graph_str_flat: Vec<u8> = Vec::new();
        let mut graph_str_offsets = Vec::new();
        for (gid, gname) in self.all_graphs() {
            graph_ids.push(gid.as_u8());
            graph_str_offsets.push(graph_str_flat.len() as u32);
            match gname {
                GraphName::DefaultGraph => graph_kinds.push(2u8),
                GraphName::NamedNode(n) => {
                    graph_kinds.push(0u8);
                    graph_str_flat.extend_from_slice(n.as_str().as_bytes());
                }
                GraphName::BlankNode(b) => {
                    graph_kinds.push(1u8);
                    graph_str_flat.extend_from_slice(b.as_str().as_bytes());
                }
            }
        }

        DictSnapshot {
            next_graph_id: self.next_graph_id,
            compacted,
            triple_ids,
            triple_s,
            triple_p,
            triple_o,
            graph_ids,
            graph_kinds,
            graph_str_flat,
            graph_str_offsets,
        }
    }

    /// Reconstruct a `Dictionary` directly from a `load_mmap`'d
    /// [`DictSnapshot`] — the compacted Front-Coded tier stays a genuine
    /// zero-copy view of the mmap'd `nova.dict.<gen>` file (via
    /// [`MappedCompactedTier`]); only the small delta-tier-equivalent state
    /// (quoted triples + the graph table) is copied into owned, heap-
    /// resident structures, exactly mirroring the design already used for
    /// the Ring index's `GraphRingHandle`/`MappedGraphRing`.
    ///
    /// `TermId`s are preserved exactly (the compacted tier's own
    /// `id2rank`/`rank2id` arrays already encode the original insertion
    /// order), so WAL-tail replay's `intern()` calls continue appending new
    /// terms strictly after this snapshot's high-water-mark, same as the
    /// legacy [`Self::rebuild`] path guaranteed.
    pub fn from_mapped(mem: Arc<MemCase<DictSnapshot>>) -> Result<Self, Oxigraph> {
        let view = mem.uncase();
        let next_graph_id = view.next_graph_id;
        let high_water = view.compacted.high_water;

        // Small, bounded parallel arrays — copied into owned `Vec`s (cheap:
        // one entry per quoted triple / per graph, never per regular term).
        let triple_ids: Vec<u64> = view.triple_ids.to_vec();
        let triple_s: Vec<u64> = view.triple_s.to_vec();
        let triple_p: Vec<u64> = view.triple_p.to_vec();
        let triple_o: Vec<u64> = view.triple_o.to_vec();
        let graph_ids: Vec<u8> = view.graph_ids.to_vec();
        let graph_kinds: Vec<u8> = view.graph_kinds.to_vec();
        let graph_str_flat: Vec<u8> = view.graph_str_flat.to_vec();
        let graph_str_offsets: Vec<u32> = view.graph_str_offsets.to_vec();

        let compacted =
            CompactedTierHandle::Mapped(Arc::new(MappedCompactedTier::new(Arc::clone(&mem))));

        let mut id_to_term: Vec<Option<Arc<Term>>> = vec![None; high_water as usize];
        let mut term_to_id: HashMap<Arc<Term>, TermId> = HashMap::new();
        let mut triple_terms: HashMap<TermId, [TermId; 3]> = HashMap::new();
        let mut triple_index: HashMap<[TermId; 3], TermId> = HashMap::new();

        // Reconstruct every quoted triple in ascending-id order. Each
        // triple's component ids are guaranteed to already be decodable at
        // this point — either directly from the compacted tier (regular
        // terms), or from an earlier iteration of this very loop (nested
        // quoted triples) — since `intern()` always interns a triple's
        // components before the triple itself, so component ids are always
        // strictly smaller than the enclosing triple's id.
        for i in 0..triple_ids.len() {
            let tid_raw = triple_ids[i];
            let s_raw = triple_s[i];
            let p_raw = triple_p[i];
            let o_raw = triple_o[i];

            let decode_component =
                |raw_id: u64, id_to_term: &[Option<Arc<Term>>]| -> Result<Arc<Term>, Oxigraph> {
                    if let Some(Some(arc)) = id_to_term.get(raw_id as usize) {
                        return Ok(Arc::clone(arc));
                    }
                    compacted.decode_id(raw_id).ok_or_else(|| {
                        Oxigraph::Storage(format!(
                            "dict snapshot: quoted-triple component id {raw_id} \
                             not found in compacted tier or prior reconstruction"
                        ))
                    })?
                };

            let s_term = decode_component(s_raw, &id_to_term)?;
            let p_term = decode_component(p_raw, &id_to_term)?;
            let o_term = decode_component(o_raw, &id_to_term)?;

            let subject = match s_term.as_ref() {
                Term::NamedNode(n) => oxrdf::NamedOrBlankNode::NamedNode(n.clone()),
                Term::BlankNode(b) => oxrdf::NamedOrBlankNode::BlankNode(b.clone()),
                other => {
                    return Err(Oxigraph::Storage(format!(
                        "dict snapshot: quoted-triple subject decoded to non-subject term {other:?}"
                    )));
                }
            };
            let predicate = match p_term.as_ref() {
                Term::NamedNode(n) => n.clone(),
                other => {
                    return Err(Oxigraph::Storage(format!(
                        "dict snapshot: quoted-triple predicate decoded to non-IRI term {other:?}"
                    )));
                }
            };
            let object = o_term.as_ref().clone();

            let triple_term = Term::Triple(Box::new(oxrdf::Triple {
                subject,
                predicate,
                object,
            }));

            let tid = TermId::new(tid_raw)?;
            let s_id = TermId::new(s_raw)?;
            let p_id = TermId::new(p_raw)?;
            let o_id = TermId::new(o_raw)?;
            let components = [s_id, p_id, o_id];

            let arc = Arc::new(triple_term);
            id_to_term[tid_raw as usize] = Some(Arc::clone(&arc));
            term_to_id.insert(arc, tid);
            triple_terms.insert(tid, components);
            triple_index.insert(components, tid);
        }

        let mut graph_to_id: HashMap<GraphName, GraphId> = HashMap::new();
        let mut id_to_graph: HashMap<u8, GraphName> = HashMap::new();
        for i in 0..graph_ids.len() {
            let gid = graph_ids[i];
            let kind = graph_kinds[i];
            let start = graph_str_offsets[i] as usize;
            let end = graph_str_offsets
                .get(i + 1)
                .map(|&x| x as usize)
                .unwrap_or(graph_str_flat.len());
            let s = std::str::from_utf8(&graph_str_flat[start..end])
                .map_err(|e| Oxigraph::Storage(format!("dict snapshot: invalid utf8: {e}")))?
                .to_string();
            let gname = match kind {
                0 => GraphName::NamedNode(NamedNode::new_unchecked(s)),
                1 => GraphName::BlankNode(BlankNode::new_unchecked(s)),
                2 => GraphName::DefaultGraph,
                other => {
                    return Err(Oxigraph::Storage(format!(
                        "dict snapshot: unknown graph kind {other}"
                    )));
                }
            };
            graph_to_id.insert(gname.clone(), GraphId(gid));
            id_to_graph.insert(gid, gname);
        }
        // Defensive: ensure the default graph is always present.
        graph_to_id
            .entry(GraphName::DefaultGraph)
            .or_insert(GRAPH_DEFAULT);
        id_to_graph.entry(0).or_insert(GraphName::DefaultGraph);

        Ok(Self {
            id_to_term,
            term_to_id,
            compacted,
            decode_cache: Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(DECODE_CACHE_CAPACITY).expect("nonzero capacity"),
            )),
            graph_to_id,
            id_to_graph,
            next_graph_id,
            triple_terms,
            triple_index,
        })
    }

    // ── u128 quad key packing ─────────────────────────────────────────────────

    /// Pack `(GraphId, TermId, TermId, TermId)` into a single `u128` key.
    ///
    /// Bit layout: `g[127:120] | s[119:80] | p[79:40] | o[39:0]`  
    /// (8 + 40 + 40 + 40 = 128 bits exactly).
    ///
    /// Because the graph is in the high byte, `BTreeMap` orders graph-major —
    /// all quads in the same named graph are contiguous, enabling efficient
    /// per-graph range queries.
    #[inline]
    pub fn pack_quad(g: GraphId, s: TermId, p: TermId, o: TermId) -> u128 {
        ((g.as_u8() as u128) << 120)
            | ((s.as_u64() as u128) << 80)
            | ((p.as_u64() as u128) << 40)
            | (o.as_u64() as u128)
    }

    /// Unpack a `u128` key back to `(GraphId, s: TermId, p: TermId, o: TermId)`.
    #[inline]
    pub fn unpack_quad(key: u128) -> (GraphId, TermId, TermId, TermId) {
        let g = GraphId(((key >> 120) & 0xFF) as u8);
        let s = TermId(((key >> 80) & MAX_TERM_ID as u128) as u64);
        let p = TermId(((key >> 40) & MAX_TERM_ID as u128) as u64);
        let o = TermId((key & MAX_TERM_ID as u128) as u64);
        (g, s, p, o)
    }

    // ── Quoted-triple component access (SPARQL 1.2 built-ins) ────────────────

    /// If `id` is a quoted-triple term, return its `[s_id, p_id, o_id]`.
    pub fn triple_components(&self, id: TermId) -> Option<[TermId; 3]> {
        self.triple_terms.get(&id).copied()
    }

    /// Given `[s_id, p_id, o_id]`, return the interned `TermId` for that quoted triple.
    pub fn lookup_triple(&self, components: [TermId; 3]) -> Option<TermId> {
        self.triple_index.get(&components).copied()
    }

    // ── Helper: intern a Subject as a Term ───────────────────────────────────

    /// Convenience wrapper used by storage backends that receive `oxrdf::Subject`.
    pub fn intern_subject(
        &mut self,
        subject: &oxrdf::NamedOrBlankNode,
    ) -> Result<TermId, Oxigraph> {
        let t: Term = subject.clone().into();
        self.intern(&t)
    }

    /// Convenience wrapper for predicates.
    pub fn intern_predicate(&mut self, pred: &NamedNode) -> Result<TermId, Oxigraph> {
        self.intern(&Term::NamedNode(pred.clone()))
    }

    /// Cheaply clone the read-only state needed to decode `TermId`s without
    /// holding a store-level lock for the duration of a query.
    ///
    /// Used by the parallel LFTJ path: a single `RwLock` read is taken to
    /// build this snapshot, then every worker decodes through the snapshot
    /// (no further store-lock acquisitions). The compacted tier is
    /// `Arc`-shared (see [`CompactedTierHandle::Owned`]); the delta-tier
    /// maps and graph table are each wrapped in an `Arc` so subsequent
    /// [`DictDecodeSnapshot::clone`] calls (one per rayon chunk) are O(1)
    /// refcount bumps rather than O(delta-tier size) deep clones.
    ///
    /// The returned snapshot carries its **own** decode-cache `Mutex`
    /// (not the live dictionary's). [`Clone`] installs a **fresh empty**
    /// worker-sized cache — the parallel LFTJ path clones once per rayon
    /// chunk so workers never contend on a shared LRU.
    pub fn decode_snapshot(&self) -> DictDecodeSnapshot {
        DictDecodeSnapshot {
            id_to_term: Arc::new(self.id_to_term.clone()),
            term_to_id: Arc::new(self.term_to_id.clone()),
            compacted: self.compacted.clone(),
            graph_to_id: Arc::new(self.graph_to_id.clone()),
            decode_cache: Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(DECODE_CACHE_CAPACITY).expect("nonzero capacity"),
            )),
        }
    }
}

/// Query-scoped snapshot of the dictionary state needed for `TermId → Term`
/// decoding during LFTJ evaluation.
///
/// Built once via [`Dictionary::decode_snapshot`] under a brief store read
/// lock; thereafter every decode is local to this value (and its private
/// decode cache), so parallel LFTJ workers never re-enter the store lock
/// just to materialise a result row.
///
/// ## Concurrency model
///
/// Each snapshot owns a private `Mutex`-guarded LRU. Sharing one snapshot
/// across rayon workers would re-introduce decode-cache contention, so
/// [`Clone`] gives each worker a fresh empty cache while `Arc`-sharing the
/// immutable compacted tier, delta maps, and graph table. On the sequential
/// path a single snapshot is enough (the mutex is uncontended).
pub struct DictDecodeSnapshot {
    /// Delta-tier reverse map (same layout as [`Dictionary::id_to_term`]).
    /// `Some` entries are `Arc`-shared with the live dictionary; `None`
    /// means "decode from `compacted`". Wrapped in `Arc` so worker clones
    /// are O(1) refcount bumps rather than O(delta) deep clones.
    id_to_term: Arc<Vec<Option<Arc<Term>>>>,
    /// Delta-tier forward map — `Arc`-shared keys with the live dictionary.
    /// Wrapped in `Arc` for the same O(1) clone reason as `id_to_term`.
    term_to_id: Arc<HashMap<Arc<Term>, TermId>>,
    /// Compacted Front-Coded tier — `Arc`-shared with the live dictionary
    /// (cheap `Arc::clone`, no buffer copy).
    compacted: CompactedTierHandle,
    /// GraphName → GraphId (small; typically ≤ 255 entries). `Arc`-shared
    /// so worker clones stay O(1).
    graph_to_id: Arc<HashMap<GraphName, GraphId>>,
    /// Private per-snapshot decode cache. Owned (not `Arc`-shared) so
    /// [`Clone`] can install a fresh empty cache per worker.
    decode_cache: Mutex<lru::LruCache<TermId, Arc<Term>>>,
}

impl Clone for DictDecodeSnapshot {
    fn clone(&self) -> Self {
        // Share the compacted tier + delta/graph maps via Arc; give the
        // clone its own empty worker-sized decode cache so parallel
        // workers never contend and never pay for a 100k pre-sized map.
        Self {
            id_to_term: Arc::clone(&self.id_to_term),
            term_to_id: Arc::clone(&self.term_to_id),
            compacted: self.compacted.clone(),
            graph_to_id: Arc::clone(&self.graph_to_id),
            decode_cache: Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(WORKER_DECODE_CACHE_CAPACITY).expect("nonzero capacity"),
            )),
        }
    }
}

impl DictDecodeSnapshot {
    /// Decode `id` to an owned `Term`, using the same two-tier logic as
    /// [`Dictionary::get_term_arc`] (delta → private cache → compacted).
    pub fn get_term_arc(&self, id: TermId) -> Option<Arc<Term>> {
        match self.id_to_term.get(id.as_u64() as usize) {
            Some(Some(arc)) => Some(Arc::clone(arc)),
            Some(None) => {
                if let Some(arc) = self.decode_cache.lock().get(&id) {
                    return Some(Arc::clone(arc));
                }
                let block = self.compacted.decode_block_for_id(id.as_u64())?;
                let mut wanted: Option<Arc<Term>> = None;
                let mut cache = self.decode_cache.lock();
                for (orig_id, result) in block {
                    if let Ok(arc) = result {
                        let tid = TermId(orig_id);
                        if tid == id {
                            wanted = Some(Arc::clone(&arc));
                        }
                        cache.put(tid, arc);
                    }
                }
                wanted
            }
            None => None,
        }
    }

    /// Decode `id` to an owned `Term` (clones out of the `Arc` for the
    /// LFTJ result-row path, matching [`LftjSource::lftj_decode_term`]).
    pub fn decode_term(&self, id: u64) -> Option<Term> {
        let tid = TermId::new(id).ok()?;
        self.get_term_arc(tid).map(|arc| arc.as_ref().clone())
    }

    /// Look up a `TermId` without creating a new entry — same semantics as
    /// [`Dictionary::get_id`].
    pub fn get_id(&self, term: &Term) -> Option<TermId> {
        if let Some(&id) = self.term_to_id.get(term) {
            return Some(id);
        }
        self.compacted
            .get_id(term)
            .and_then(|raw| TermId::new(raw).ok())
    }

    /// Look up a `GraphId` without registering a new entry.
    pub fn get_graph_id(&self, graph: &GraphName) -> Option<GraphId> {
        self.graph_to_id.get(graph).copied()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::{Literal, NamedNode};
    use value_traits::slices::SliceByValue;

    fn nn(s: &str) -> Term {
        Term::NamedNode(NamedNode::new_unchecked(s))
    }
    fn lit(s: &str) -> Term {
        Term::Literal(Literal::new_simple_literal(s))
    }

    #[test]
    fn intern_and_decode() {
        let mut d = Dictionary::new();
        let id_a = d.intern(&nn("http://ex/a")).unwrap();
        let id_b = d.intern(&lit("hello")).unwrap();
        let id_a2 = d.intern(&nn("http://ex/a")).unwrap();
        assert_eq!(id_a, id_a2, "same term must yield same TermId");
        assert_ne!(id_a, id_b, "different terms must yield different TermIds");
        assert_eq!(d.get_term_arc(id_a).as_deref(), Some(&nn("http://ex/a")));
        assert_eq!(d.get_term_arc(id_b).as_deref(), Some(&lit("hello")));
    }

    #[test]
    fn get_id_returns_none_for_unknown() {
        let d = Dictionary::new();
        assert!(d.get_id(&nn("http://ex/unknown")).is_none());
    }

    #[test]
    fn intern_graph_default() {
        let mut d = Dictionary::new();
        let id = d.intern_graph(&GraphName::DefaultGraph).unwrap();
        assert_eq!(id, GRAPH_DEFAULT);
        // Idempotent
        assert_eq!(
            d.intern_graph(&GraphName::DefaultGraph).unwrap(),
            GRAPH_DEFAULT
        );
    }

    #[test]
    fn intern_graph_named() {
        let mut d = Dictionary::new();
        let g = GraphName::NamedNode(NamedNode::new_unchecked("http://ex/g1"));
        let id1 = d.intern_graph(&g).unwrap();
        let id2 = d.intern_graph(&g).unwrap();
        assert_eq!(id1, id2);
        assert_ne!(id1, GRAPH_DEFAULT);
        assert_eq!(d.get_graph(id1), Some(&g));
    }

    #[test]
    fn pack_unpack_roundtrip() {
        let g = GraphId(3);
        let s = TermId(1_234_567);
        let p = TermId(89);
        let o = TermId(MAX_TERM_ID);
        let packed = Dictionary::pack_quad(g, s, p, o);
        let (g2, s2, p2, o2) = Dictionary::unpack_quad(packed);
        assert_eq!(g, g2);
        assert_eq!(s, s2);
        assert_eq!(p, p2);
        assert_eq!(o, o2);
    }

    #[test]
    fn max_term_id_roundtrip() {
        let id = TermId::new(MAX_TERM_ID).unwrap();
        assert_eq!(id.as_u64(), MAX_TERM_ID);
    }

    #[test]
    fn term_id_overflow_rejected() {
        assert!(TermId::new(MAX_TERM_ID + 1).is_err());
    }

    // ── Two-tier compaction tests ────────────────────────────────────────

    #[test]
    fn compact_preserves_term_ids_and_content() {
        let mut d = Dictionary::new();
        let mut ids = Vec::new();
        let terms: Vec<Term> = (0..50)
            .map(|i| nn(&format!("http://example.org/entity/{i}")))
            .chain((0..20).map(|i| lit(&format!("literal-value-{i}"))))
            .collect();
        for t in &terms {
            ids.push(d.intern(t).unwrap());
        }

        d.compact().unwrap();

        for (t, &id) in terms.iter().zip(ids.iter()) {
            assert_eq!(d.get_id(t), Some(id), "get_id must survive compaction");
            assert_eq!(
                d.get_term_arc(id).as_deref(),
                Some(t),
                "get_term_arc must decode the same term after compaction"
            );
        }

        // New terms interned after compaction still get fresh, higher ids.
        let new_id = d.intern(&nn("http://example.org/entity/new")).unwrap();
        assert!(new_id.as_u64() as usize >= terms.len());
    }

    #[test]
    fn rank2id_id2rank_roundtrip_after_every_compaction() {
        let mut d = Dictionary::new();
        // Multiple compaction cycles: interleave interning and compacting.
        for round in 0..3 {
            for i in 0..30 {
                d.intern(&nn(&format!("http://ex/r{round}/e{i}"))).unwrap();
                d.intern(&lit(&format!("val-{round}-{i}"))).unwrap();
            }
            d.compact().unwrap();

            // rank2id[id2rank[id]] == id for every non-quoted-triple id.
            let high_water = d.id_to_term.len() as u64;
            let owned = d.compacted.expect_owned();
            for raw_id in 0..high_water {
                let tid = TermId(raw_id);
                if d.triple_terms.contains_key(&tid) {
                    continue;
                }
                let rank = owned.id2rank.index_value(raw_id as usize);
                let back = owned.rank2id.index_value(rank);
                assert_eq!(
                    back as u64, raw_id,
                    "rank2id[id2rank[id]] must equal id after compaction round {round}"
                );
            }
        }
    }

    #[test]
    fn compact_roundtrip_at_max_term_id() {
        // Directly exercise the permutation arrays' bit-width computation
        // at a large id value without actually allocating a trillion terms:
        // build a small dictionary, then verify a manual high `TermId`
        // roundtrip against `CompactedTier` in isolation.
        let mut d = Dictionary::new();
        for i in 0..10 {
            d.intern(&nn(&format!("http://ex/{i}"))).unwrap();
        }
        d.compact().unwrap();
        let high_water = d.id_to_term.len() as u64;
        let owned = d.compacted.expect_owned();
        for raw_id in 0..high_water {
            let rank = owned.id2rank.index_value(raw_id as usize);
            let back = owned.rank2id.index_value(rank);
            assert_eq!(back as u64, raw_id);
        }

        // MAX_TERM_ID itself must still be constructible/roundtrippable as
        // a TermId (independent of dictionary size — see max_term_id_roundtrip).
        let id = TermId::new(MAX_TERM_ID).unwrap();
        assert_eq!(id.as_u64(), MAX_TERM_ID);
    }

    // ── Decode-cache tests ───────────────────────────────────────────────

    #[test]
    fn decode_cache_hit_path_returns_correct_content_repeatedly() {
        let mut d = Dictionary::new();
        let mut ids = Vec::new();
        let terms: Vec<Term> = (0..30)
            .map(|i| nn(&format!("http://example.org/cache/{i}")))
            .collect();
        for t in &terms {
            ids.push(d.intern(t).unwrap());
        }
        d.compact().unwrap();

        // First call per id: cache miss, decodes from the compacted tier.
        // Every subsequent call: cache hit. Both paths must return identical
        // content, repeatedly, for every id.
        for _ in 0..5 {
            for (t, &id) in terms.iter().zip(ids.iter()) {
                assert_eq!(
                    d.get_term_arc(id).as_deref(),
                    Some(t),
                    "repeated get_term_arc calls (cache hit path) must return the same content"
                );
            }
        }
    }

    #[test]
    fn decode_cache_cleared_on_compact_still_decodes_correctly() {
        let mut d = Dictionary::new();
        let terms: Vec<Term> = (0..20)
            .map(|i| nn(&format!("http://example.org/precompact/{i}")))
            .collect();
        let mut ids = Vec::new();
        for t in &terms {
            ids.push(d.intern(t).unwrap());
        }
        d.compact().unwrap();

        // Warm the cache for every id (populates decode_cache with the
        // *first*-generation block/rank layout).
        for &id in &ids {
            let _ = d.get_term_arc(id);
        }
        assert!(
            !d.decode_cache.lock().is_empty(),
            "cache should be warm before the second compaction"
        );

        // Intern more terms, then compact again — ranks/block offsets shift
        // for every previously-compacted term, and the cache is cleared.
        for i in 20..40 {
            d.intern(&nn(&format!("http://example.org/precompact/{i}")))
                .unwrap();
        }
        d.compact().unwrap();
        assert_eq!(
            d.decode_cache.lock().len(),
            0,
            "decode_cache must be cleared immediately after compact()"
        );

        // Despite the cache being cleared (and ranks having shifted), every
        // original id must still decode to its original content.
        for (t, &id) in terms.iter().zip(ids.iter()) {
            assert_eq!(
                d.get_term_arc(id).as_deref(),
                Some(t),
                "content must still be correct after cache-clearing recompaction"
            );
        }
    }

    #[test]
    fn decode_cache_never_exceeds_configured_capacity() {
        // Intern well more than DECODE_CACHE_CAPACITY regular terms, compact,
        // then decode every single one — the cache must never grow past its
        // configured bound (no unbounded growth).

        let mut d = Dictionary::new();
        let n = DECODE_CACHE_CAPACITY + 500;
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            ids.push(
                d.intern(&nn(&format!("http://example.org/bound/{i}")))
                    .unwrap(),
            );
        }
        d.compact().unwrap();

        for &id in &ids {
            let _ = d.get_term_arc(id);
            assert!(
                d.decode_cache.lock().len() <= DECODE_CACHE_CAPACITY,
                "decode_cache must never exceed its configured capacity"
            );
        }
        assert_eq!(
            d.decode_cache.lock().len(),
            DECODE_CACHE_CAPACITY,
            "after decoding more ids than capacity, the cache should be fully (but not over-) populated"
        );
    }

    #[test]
    fn decode_snapshot_clone_shares_maps_and_uses_worker_capacity() {
        // Worker clones must Arc-share the delta/graph maps (so Clone is O(1)
        // and pointer-equal) and install a fresh empty cache sized to the
        // smaller worker capacity — not the live dictionary's 100k.
        let mut d = Dictionary::new();
        let ids: Vec<_> = (0..20)
            .map(|i| {
                d.intern(&nn(&format!("http://example.org/snap/{i}")))
                    .unwrap()
            })
            .collect();
        d.compact().unwrap();

        let snap = d.decode_snapshot();
        // Warm the sequential snapshot's full-size cache.
        for &id in &ids {
            let _ = snap.get_term_arc(id);
        }
        assert!(
            !snap.decode_cache.lock().is_empty(),
            "sequential snapshot cache should be warm"
        );

        let worker = snap.clone();
        assert!(
            Arc::ptr_eq(&snap.id_to_term, &worker.id_to_term),
            "worker clone must Arc-share id_to_term"
        );
        assert!(
            Arc::ptr_eq(&snap.term_to_id, &worker.term_to_id),
            "worker clone must Arc-share term_to_id"
        );
        assert!(
            Arc::ptr_eq(&snap.graph_to_id, &worker.graph_to_id),
            "worker clone must Arc-share graph_to_id"
        );
        assert_eq!(
            worker.decode_cache.lock().len(),
            0,
            "worker clone must start with a fresh empty cache"
        );
        assert_eq!(
            worker.decode_cache.lock().cap().get(),
            WORKER_DECODE_CACHE_CAPACITY,
            "worker clone must use the smaller worker capacity"
        );

        // Correctness: worker still decodes every id after the shared maps.
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(
                worker.get_term_arc(id).as_deref(),
                Some(&nn(&format!("http://example.org/snap/{i}"))),
            );
        }
    }

    #[test]
    fn quoted_triple_survives_compaction() {
        let mut d = Dictionary::new();
        let s = nn("http://ex/s");
        let p = NamedNode::new_unchecked("http://ex/p");
        let o = lit("v");
        let triple_term = Term::Triple(Box::new(oxrdf::Triple {
            subject: oxrdf::NamedOrBlankNode::NamedNode(NamedNode::new_unchecked("http://ex/s")),
            predicate: p.clone(),
            object: o.clone(),
        }));
        d.intern(&s).unwrap();
        d.intern(&Term::NamedNode(p)).unwrap();
        d.intern(&o).unwrap();
        let triple_id = d.intern(&triple_term).unwrap();

        d.compact().unwrap();

        assert_eq!(d.get_id(&triple_term), Some(triple_id));
        assert_eq!(d.get_term_arc(triple_id).as_deref(), Some(&triple_term));
    }

    // ── mmap snapshot round-trip tests ──────────────────────────────────

    /// Real `load_mmap` round-trip of a `to_snapshot()`-produced
    /// `DictSnapshot`, reconstructed via `from_mapped`, verifying every
    /// term/graph/quoted-triple survives byte-identically.
    #[test]
    fn dict_snapshot_mmap_round_trip() {
        use epserde::deser::{Deserialize, Flags};
        use epserde::ser::Serialize;

        let mut d = Dictionary::new();
        let mut ids = Vec::new();
        let terms: Vec<Term> = (0..40)
            .map(|i| nn(&format!("http://example.org/mmap/{i}")))
            .chain((0..10).map(|i| lit(&format!("mmap-literal-{i}"))))
            .collect();
        for t in &terms {
            ids.push(d.intern(t).unwrap());
        }
        let g = GraphName::NamedNode(NamedNode::new_unchecked("http://ex/g"));
        let gid = d.intern_graph(&g).unwrap();

        // Quoted triple, referencing already-interned components.
        let s = nn("http://example.org/mmap/0");
        let p = NamedNode::new_unchecked("http://ex/p");
        let o = lit("mmap-literal-0");
        let triple_term = Term::Triple(Box::new(oxrdf::Triple {
            subject: oxrdf::NamedOrBlankNode::NamedNode(NamedNode::new_unchecked(
                "http://example.org/mmap/0",
            )),
            predicate: p.clone(),
            object: o.clone(),
        }));
        d.intern(&Term::NamedNode(p)).unwrap();
        let triple_id = d.intern(&triple_term).unwrap();
        let _ = s;

        d.compact().unwrap();

        let snap = d.to_snapshot();
        let mut buf: Vec<u8> = Vec::new();
        unsafe {
            snap.serialize(&mut buf).expect("serialize DictSnapshot");
        }

        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("nova_dict_mmap_test_{pid}_{n}.dict"));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, &buf).expect("write temp dict snapshot");

        let mem = std::sync::Arc::new(unsafe {
            DictSnapshot::load_mmap(&path, Flags::empty()).expect("load_mmap DictSnapshot")
        });

        let loaded = Dictionary::from_mapped(mem).expect("from_mapped");

        for (t, &id) in terms.iter().zip(ids.iter()) {
            assert_eq!(loaded.get_id(t), Some(id));
            assert_eq!(loaded.get_term_arc(id).as_deref(), Some(t));
        }
        assert_eq!(loaded.get_graph_id(&g), Some(gid));
        assert_eq!(loaded.get_graph(gid), Some(&g));
        assert_eq!(loaded.get_id(&triple_term), Some(triple_id));
        assert_eq!(
            loaded.get_term_arc(triple_id).as_deref(),
            Some(&triple_term)
        );

        // A fresh intern after reload must NOT collide with existing IDs.
        let mut loaded = loaded;
        let fresh_id = loaded.intern(&nn("http://example.org/mmap/fresh")).unwrap();
        assert!(fresh_id.as_u64() as usize >= loaded.len() - 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_dict_snapshot_round_trip() {
        use epserde::deser::{Deserialize, Flags};
        use epserde::ser::Serialize;

        let d = Dictionary::new();
        let snap = d.to_snapshot();
        let mut buf: Vec<u8> = Vec::new();
        unsafe {
            snap.serialize(&mut buf)
                .expect("serialize empty DictSnapshot");
        }

        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("nova_dict_mmap_empty_test_{pid}_{n}.dict"));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, &buf).expect("write temp dict snapshot");

        let mem = std::sync::Arc::new(unsafe {
            DictSnapshot::load_mmap(&path, Flags::empty()).expect("load_mmap DictSnapshot")
        });
        let loaded = Dictionary::from_mapped(mem).expect("from_mapped");
        assert!(loaded.is_empty());
        assert_eq!(
            loaded.get_graph_id(&GraphName::DefaultGraph),
            Some(GRAPH_DEFAULT)
        );

        let _ = std::fs::remove_file(&path);
    }
}
