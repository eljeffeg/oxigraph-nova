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
//! | `1` | Ontology graph — TBox input for reasoner |
//! | `2–254` | User named graphs |
//! | `255` | Inference graph — OWL 2 RL closure written by the reasoner |

use crate::Oxigraph;
use oxrdf::{GraphName, NamedNode, Term};
use std::collections::HashMap;
use std::sync::Arc;

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
/// The default graph is `GraphId(0)`; user named graphs occupy `2..=254`.
/// `1` is reserved for the ontology TBox; `255` is reserved for the OWL 2 RL
/// inference closure written by the reasoner.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub struct GraphId(pub u8);

/// Default graph (SPARQL default, always present).
pub const GRAPH_DEFAULT: GraphId = GraphId(0);
/// Ontology graph — loads OWL TBox axioms for reasoning.
pub const GRAPH_ONTOLOGY: GraphId = GraphId(1);
/// Inference graph — written by the OWL 2 RL materializing reasoner.
pub const GRAPH_INFERENCE: GraphId = GraphId(255);

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
/// `Dictionary` is `!Sync` by design — callers are expected to wrap it in a
/// `Mutex<RingStoreInner>` (as `RingStore` does) rather than using separate locks.
pub struct Dictionary {
    // ── Term ↔ TermId ──────────────────────────────────────────────────────
    /// Reverse: `id_to_term[id.as_u64()]` → `Term`.
    ///
    /// Stored as `Arc<Term>` (Phase A.4 — see `CLAUDE.md`'s "Memory footprint
    /// investigation" section) so the same heap-allocated term content is
    /// shared with the `term_to_id` HashMap key below, instead of being
    /// duplicated. This halves the Dictionary's real memory footprint versus
    /// storing two independent owned `Term`s per interned value.
    id_to_term: Vec<Arc<Term>>,
    /// Forward: `Term` → `TermId`. The key `Arc<Term>` is the *same*
    /// allocation as the corresponding `id_to_term` entry (Phase A.4).
    term_to_id: HashMap<Arc<Term>, TermId>,

    // ── GraphName ↔ GraphId ─────────────────────────────────────────────────
    /// Forward: `GraphName` → `GraphId`
    graph_to_id: HashMap<GraphName, GraphId>,
    /// Reverse: `GraphId.as_u8()` → `GraphName`
    id_to_graph: HashMap<u8, GraphName>,
    /// Next available user GraphId (range `2..=254`; `0/1/255` are reserved).
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
            graph_to_id: HashMap::new(),
            id_to_graph: HashMap::new(),
            next_graph_id: 2, // 0=default, 1=ontology — both reserved
            triple_terms: HashMap::new(),
            triple_index: HashMap::new(),
        };
        // Pre-register the default graph so GraphId(0) is always valid.
        d.graph_to_id.insert(GraphName::DefaultGraph, GRAPH_DEFAULT);
        d.id_to_graph.insert(0, GraphName::DefaultGraph);
        d
    }

    /// Total number of interned terms.
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
    /// **Phase A.4** (see `CLAUDE.md`'s "Memory footprint investigation"
    /// section): `id_to_term` and `term_to_id` now share the *same*
    /// `Arc<Term>` heap allocation per interned term — `oxrdf` 0.3.3's `Term`
    /// variants don't do this sharing natively (owned `String`s, no `Arc`),
    /// so we wrap each interned `Term` in our own `Arc` once and clone the
    /// `Arc` (cheap refcount bump) into both tables. This halves the prior
    /// "pays for every term's string content twice" cost down to once per
    /// term, plus a small fixed `Arc` control-block + pointer overhead.
    ///
    /// This is a diagnostic estimate, not exact `malloc` accounting: it uses
    /// `String::len()` (not allocator capacity, which is usually equal or only
    /// slightly larger for strings built once via `to_owned`/`String::from`)
    /// plus fixed struct/enum overhead per field.
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

        // Each interned term now has exactly ONE heap allocation, shared
        // (via `Arc::clone`) between `id_to_term` and `term_to_id` — Phase
        // A.4. Iterating `id_to_term` visits each unique term exactly once.
        let unique_term_bytes: usize = self
            .id_to_term
            .iter()
            .map(|t| enum_overhead + term_heap_bytes(t) + ARC_CTRL_OVERHEAD)
            .sum();

        // `id_to_term: Vec<Arc<Term>>` — one 8-byte pointer per slot (content
        // already counted above via `unique_term_bytes`).
        let id_to_term_ptrs = self.id_to_term.len() * size_of::<Arc<Term>>();

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
    }

    // ── Term interning ────────────────────────────────────────────────────────

    /// Get or assign a `TermId` for the given `Term`.
    ///
    /// For quoted triples (`Term::Triple`), recursively interns the components
    /// and populates the `triple_terms` / `triple_index` side tables so that
    /// `SUBJECT()` / `PREDICATE()` / `OBJECT()` can look up components by ID.
    pub fn intern(&mut self, term: &Term) -> Result<TermId, Oxigraph> {
        // Fast path: already interned.
        if let Some(&id) = self.term_to_id.get(term) {
            return Ok(id);
        }

        // Handle quoted triples (RDF 1.2 / RDF-star).
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
            self.id_to_term.push(Arc::clone(&arc));
            self.term_to_id.insert(arc, id);
            self.triple_terms.insert(id, components);
            self.triple_index.insert(components, id);
            return Ok(id);
        }

        // Regular terms: IRI, blank node, literal.
        // Phase A.4: wrap in one `Arc` and clone the (cheap) refcounted
        // pointer into both tables, instead of cloning the full `Term`
        // (and its heap-allocated `String` content) twice.
        let raw = self.id_to_term.len() as u64;
        let id = TermId::new(raw)?;
        let arc = Arc::new(term.clone());
        self.id_to_term.push(Arc::clone(&arc));
        self.term_to_id.insert(arc, id);
        Ok(id)
    }

    /// Look up a `TermId` **without** creating a new entry.
    ///
    /// Returns `None` if the term has never been interned — which for query
    /// evaluation means the pattern cannot match anything (early-out, no scan).
    pub fn get_id(&self, term: &Term) -> Option<TermId> {
        self.term_to_id.get(term).copied()
    }

    /// Decode a `TermId` back to the original `Term`.
    ///
    /// Returns `None` only for IDs that were never assigned by this dictionary
    /// (should never happen in correct usage).
    pub fn get_term(&self, id: TermId) -> Option<&Term> {
        self.id_to_term
            .get(id.as_u64() as usize)
            .map(|v| v.as_ref())
    }

    // ── Graph interning ───────────────────────────────────────────────────────

    /// Get or assign a `GraphId` for the given `GraphName`.
    ///
    /// `GraphName::DefaultGraph` always maps to `GRAPH_DEFAULT` (`GraphId(0)`).
    /// User named graphs are assigned IDs in `2..=254`; returning
    /// `Err(GraphSpaceExhausted)` when all 253 slots are consumed.
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
        if self.next_graph_id > 254 {
            return Err(Oxigraph::GraphSpaceExhausted);
        }
        let id = GraphId(self.next_graph_id);
        self.next_graph_id += 1;
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

    /// The next available user `GraphId` (range `2..=254`). Exposed for
    /// on-disk `Dictionary` persistence (see `oxigraph_nova_storage_ring`'s
    /// `dict_snapshot` module — item 1c in `CLAUDE.md`'s "What's Next").
    pub fn next_graph_id_raw(&self) -> u8 {
        self.next_graph_id
    }

    /// All interned terms in `TermId` order (index == `TermId::as_u64()`).
    /// Exposed for on-disk `Dictionary` persistence.
    pub fn terms_in_order(&self) -> impl Iterator<Item = &Term> {
        self.id_to_term.iter().map(|t| t.as_ref())
    }

    /// Rebuild a `Dictionary` from persisted state, preserving `TermId`s
    /// exactly (by insertion order in `terms`) and `GraphId`s exactly (via
    /// `graphs`).
    ///
    /// Used by [`crate::store`]-adjacent persistence code (in
    /// `oxigraph_nova_storage_ring`) to reconstruct the exact `Dictionary`
    /// that was in effect when a snapshot was written, so that further WAL
    /// replay (which uses `intern()`, appending new terms after whatever's
    /// already present) continues assigning IDs correctly instead of
    /// colliding with the snapshot's embedded IDs.
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
            d.id_to_term.push(Arc::clone(&arc));
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
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::{Literal, NamedNode};

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
        assert_eq!(d.get_term(id_a), Some(&nn("http://ex/a")));
        assert_eq!(d.get_term(id_b), Some(&lit("hello")));
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
}
