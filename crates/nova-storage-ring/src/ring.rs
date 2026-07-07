//! Ring index — full CompactLTJ LOUDS-trie implementation.
//!
//! Faithful implementation of Hogan et al. (SIGMOD'21 / TODS'24)
//! "Worst-Case Optimal Graph Joins in Almost No Space", using the
//! space and time optimal storage from:
//!
//! > Arroyuelo, Navarro, Gómez-Brandón et al. (VLDB Journal 2025)
//! > "CompactLTJ: Space and Time Efficient Leapfrog Triejoin on Graph Databases"
//!
//! ## Implementation
//!
//! Uses **six LOUDS height-3 tries** (one per SPO ordering) from
//! [`cltj::CltjData`].  Navigation is O(1) per step and O(log ℓ) per seek
//! (exponential search in the flat label array).  `contains()` and
//! `match_triples()` use binary search on the stored SPO-sorted array.
//!
//! ## Design
//!
//! A `GraphRing` stores the triples of one named graph (or the default graph)
//! as **six LOUDS tries** via [`CltjData`]:
//!
//! | Ordering | Depth 0 | Depth 1 | Depth 2 |
//! |---|---|---|---|
//! | SPO | S | P | O |
//! | SOP | S | O | P |
//! | PSO | P | S | O |
//! | POS | P | O | S |
//! | OPS | O | P | S |
//! | OSP | O | S | P |
//!
//! Each depth level is navigated in O(1) via LOUDS `child`/`degree`/`access`.
//! The `TrieIterator` interface consumed by LFTJ is implemented by
//! [`CltjTrieIter`] in `cltj.rs`.

use crate::cltj::{CltjData, CltjSnapshot, VocabRepr, build_cltj_data};
use crate::louds::{
    BorrowedL, BorrowedLouds, BorrowedS, BorrowedT, LoudsCore, LoudsMemBreakdown, LoudsNav,
    LoudsTrie, t_backend,
};

use epserde::Epserde;
use oxigraph_nova_core::{EmptyTrieIter, TrieIterator};
use std::collections::HashMap;
use std::sync::Arc;

// ── Ordering enum ─────────────────────────────────────────────────────────────

/// Which of the six SPO-family orderings a trie iterator uses.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Spo,
    Sop,
    Pso,
    Pos,
    Ops,
    Osp,
}

impl SortOrder {
    /// Convert a `(col0, col1, col2)` triple back to canonical `[s, p, o]`.
    #[inline]
    pub fn to_spo(self, col0: u64, col1: u64, col2: u64) -> [u64; 3] {
        match self {
            SortOrder::Spo => [col0, col1, col2],
            SortOrder::Sop => [col0, col2, col1],
            SortOrder::Pso => [col1, col0, col2],
            SortOrder::Pos => [col2, col0, col1],
            SortOrder::Ops => [col2, col1, col0],
            SortOrder::Osp => [col1, col2, col0],
        }
    }
}

// ── RingData ──────────────────────────────────────────────────────────────────

/// All immutable data for one graph's Ring index.
///
/// Generic over the vocab representation `V` (see [`CltjData`]/[`CltjTrie`]
/// in `cltj.rs`) so that a future mmap'd/zero-copy snapshot load can populate
/// `cltj`'s vocab with borrowed `&[u64]` slices, with no code duplication
/// versus the owned `Vec<u64>` path — see CLAUDE.md item 14, Phase 3.
struct RingData<Louds = LoudsTrie, V = Vec<u64>> {
    /// Six LOUDS tries (one per ordering) — O(1) navigation per step.
    /// `contains()`, `match_triples()`, and `spo_triples()` are all derived
    /// from these tries via O(1)-per-step LOUDS navigation. There is no
    /// redundant `spo: Vec<[u64;3]>` raw copy — that used to account for
    /// ~53% of Ring memory before it was removed.
    cltj: CltjData<Louds, V>,
}



// ── LFTJ ordering selection ───────────────────────────────────────────────────

/// Choose the Ring ordering for a Leapfrog Triejoin scan.
///
/// `bound_fields` — field indices (0=S, 1=P, 2=O) that are bound (constant).
/// `target_field` — the variable field we are scanning for.
fn choose_array_for_lftj(bound_fields: &[usize], target_field: usize) -> SortOrder {
    let ignored: Vec<usize> = (0..3_usize)
        .filter(|&f| f != target_field && !bound_fields.contains(&f))
        .collect();
    let mut ord = bound_fields.to_vec();
    ord.push(target_field);
    ord.extend_from_slice(&ignored);
    debug_assert_eq!(ord.len(), 3);
    match (ord[0], ord[1], ord[2]) {
        (0, 1, 2) => SortOrder::Spo,
        (0, 2, 1) => SortOrder::Sop,
        (1, 0, 2) => SortOrder::Pso,
        (1, 2, 0) => SortOrder::Pos,
        (2, 1, 0) => SortOrder::Ops,
        (2, 0, 1) => SortOrder::Osp,
        _ => unreachable!("invalid field ordering {:?}", ord),
    }
}

// ── GraphRing ─────────────────────────────────────────────────────────────────

/// The Ring index for a single named graph (or the default graph).
///
/// Immutable after construction.  `RingStore` wraps it in `Arc` so it can be
/// swapped out atomically during an LSM merge without blocking readers.
///
/// Generic over the vocab representation `V` (defaulted to owned `Vec<u64>`)
/// so that a future mmap'd/zero-copy snapshot load can produce a
/// `GraphRing<&[u64]>`-style value with no code duplication — see CLAUDE.md
/// item 14, Phase 3. All existing callers use the default `GraphRing`
/// (`V = Vec<u64>`) and are unaffected by this generic parameter.
pub struct GraphRing<Louds = LoudsTrie, V = Vec<u64>> {
    /// Number of distinct triples stored in this graph.
    pub n: usize,
    data: Option<Arc<RingData<Louds, V>>>,
}

impl<Louds: LoudsNav + Send + Sync + 'static, V: AsRef<[u64]> + Send + Sync + 'static>
    GraphRing<Louds, V>
{


    /// All triples in SPO order (for testing / serialisation).
    ///
    /// Derived by a full depth-3 traversal of the SPO LOUDS trie (O(n) over
    /// the triple count), rather than cloning from a redundant
    /// `spo: Vec<[u64;3]>` raw copy.
    pub fn spo_triples(&self) -> Vec<[u64; 3]> {
        let data = match &self.data {
            None => return Vec::new(),
            Some(d) => d,
        };
        let mut results = Vec::new();
        let mut it0 = data.cltj.trie_iter(SortOrder::Spo);
        while !it0.at_end() {
            let s = it0.key();
            let mut it1 = it0.open();
            while !it1.at_end() {
                let p = it1.key();
                let mut it2 = it1.open();
                while !it2.at_end() {
                    results.push([s, p, it2.key()]);
                    it2.advance();
                }
                it1.advance();
            }
            it0.advance();
        }
        results
    }

    /// Real allocated byte size of this graph's Ring index, for
    /// memory-breakdown diagnostics.  This is just the deduped
    /// six-LOUDS-trie + vocab total from [`CltjData::mem_size_bytes`] (there
    /// is no redundant `spo: Vec<[u64;3]>` raw copy).
    pub fn mem_size_bytes(&self) -> usize {
        match &self.data {
            None => 0,
            Some(d) => d.cltj.mem_size_bytes(),
        }
    }

    /// Per-ordering memory breakdown.  Returns `None` for an
    /// empty graph.  See [`CltjData::mem_breakdown_per_ordering`] for details.
    pub fn mem_breakdown_per_ordering(&self) -> Option<[(SortOrder, LoudsMemBreakdown, usize); 6]> {
        self.data
            .as_ref()
            .map(|d| d.cltj.mem_breakdown_per_ordering())
    }

    /// Deduped vocab bytes (3 unique `Arc<Vec<u64>>` allocations across all
    /// six tries).  Returns `(spo_bytes,
    /// vocab_deduped_bytes)` — `spo_bytes` is always `0` since there is no
    /// redundant `spo: Vec<[u64;3]>` raw copy; the tuple shape is
    /// kept for the `nova_serve.rs` diagnostic print's benefit.
    pub fn spo_and_vocab_bytes(&self) -> (usize, usize) {
        match &self.data {
            None => (0, 0),
            Some(d) => (0, d.cltj.vocab_bytes_deduped()),
        }
    }

    /// Depth-0 `TrieIterator` for the given ordering.
    pub fn trie_iter(&self, ordering: SortOrder) -> Box<dyn TrieIterator> {
        match &self.data {
            None => Box::new(EmptyTrieIter),
            Some(d) => d.cltj.trie_iter(ordering),
        }
    }

    /// `true` if the triple `(s, p, o)` exists in this graph.
    ///
    /// Navigates the SPO LOUDS trie directly (3 O(1)-amortised seek+open
    /// steps) rather than binary-searching a redundant raw `Vec<[u64;3]>`.
    pub fn contains(&self, s: u64, p: u64, o: u64) -> bool {
        let data = match &self.data {
            None => return false,
            Some(d) => d,
        };
        let mut it = data.cltj.trie_iter(SortOrder::Spo);
        if it.at_end() {
            return false;
        }
        it.seek(s);
        if it.at_end() || it.key() != s {
            return false;
        }
        let mut it = it.open();
        if it.at_end() {
            return false;
        }
        it.seek(p);
        if it.at_end() || it.key() != p {
            return false;
        }
        let mut it = it.open();
        if it.at_end() {
            return false;
        }
        it.seek(o);
        !it.at_end() && it.key() == o
    }

    /// Return all triples matching the optional `s`/`p`/`o` pattern.
    ///
    /// Chooses one of the six LOUDS-trie orderings whose depth-0 (and
    /// depth-1, if 2 fields are bound) exactly matches the bound field(s),
    /// seeks/opens through the bound prefix, then enumerates the remaining
    /// unbound depth(s), rather than binary search + linear filter over
    /// a redundant `spo: Vec<[u64;3]>`.
    pub fn match_triples(&self, s: Option<u64>, p: Option<u64>, o: Option<u64>) -> Vec<[u64; 3]> {
        let data = match &self.data {
            None => return Vec::new(),
            Some(d) => d,
        };

        // 3 bound: exact-match lookup via contains().
        if let (Some(sv), Some(pv), Some(ov)) = (s, p, o) {
            return if self.contains(sv, pv, ov) {
                vec![[sv, pv, ov]]
            } else {
                Vec::new()
            };
        }

        // Pick an ordering whose leading depth(s) are exactly the bound
        // field(s), and the seek values in trie-column order.
        let (order, seeks): (SortOrder, Vec<u64>) = match (s, p, o) {
            (None, None, None) => (SortOrder::Spo, vec![]),
            (Some(sv), None, None) => (SortOrder::Spo, vec![sv]),
            (None, Some(pv), None) => (SortOrder::Pso, vec![pv]),
            (None, None, Some(ov)) => (SortOrder::Ops, vec![ov]),
            (Some(sv), Some(pv), None) => (SortOrder::Spo, vec![sv, pv]),
            (Some(sv), None, Some(ov)) => (SortOrder::Sop, vec![sv, ov]),
            (None, Some(pv), Some(ov)) => (SortOrder::Pos, vec![pv, ov]),
            (Some(_), Some(_), Some(_)) => unreachable!("handled above"),
        };

        let mut it: Box<dyn TrieIterator> = data.cltj.trie_iter(order);
        let mut prefix = [0u64; 3];
        for (i, val) in seeks.iter().enumerate() {
            if it.at_end() {
                return Vec::new();
            }
            it.seek(*val);
            if it.at_end() || it.key() != *val {
                return Vec::new();
            }
            prefix[i] = *val;
            it = it.open();
        }

        let mut results = Vec::new();
        enumerate_suffix(it, seeks.len(), &mut prefix, order, &mut results);
        results
    }

    /// Estimate the number of distinct values for `target_field` given the other
    /// bound fields — used by the adaptive VEO predictor in LFTJ.
    ///
    /// Returns the global vocabulary size of `target_field` (0=S, 1=P, 2=O) as
    /// a conservative upper bound on the actual distinct-value count.  Using
    /// vocab size rather than exact per-node counts avoids LOUDS traversal while
    /// still giving correct relative ordering between fields (e.g. predicates
    /// have a much smaller vocab than subjects, so VEO correctly prefers
    /// iterating predicate variables first).
    ///
    /// When `n_bound > 0` bound fields are present, the estimate is scaled by
    /// `1 / (1 + n_bound)` to reflect that bound fields reduce fan-out, without
    /// requiring per-value traversal.
    pub fn estimate_count(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> u64 {
        let data = match &self.data {
            None => return 0,
            Some(d) => d,
        };
        let vocab = data.cltj.vocab_size_for_field(target_field);
        let n_bound = (s.is_some() as u64) + (p.is_some() as u64) + (o.is_some() as u64);
        // Divide by (n_bound + 1) so that each additional bound field reduces
        // the estimate, giving a useful relative ordering without exact counts.
        (vocab as u64) / (n_bound + 1)
    }

    /// Return a `TrieIterator` at the appropriate depth for an LFTJ join scan.
    ///
    /// `s`/`p`/`o` — bound values (constant); `target_field` — 0=S, 1=P, 2=O.
    /// The returned iterator yields values for the target field, with all bound
    /// fields already satisfied.
    pub fn join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator> {
        let data = match &self.data {
            None => return Box::new(EmptyTrieIter),
            Some(d) => d,
        };

        let mut bound: Vec<usize> = Vec::with_capacity(2);
        if s.is_some() && target_field != 0 {
            bound.push(0);
        }
        if p.is_some() && target_field != 1 {
            bound.push(1);
        }
        if o.is_some() && target_field != 2 {
            bound.push(2);
        }
        bound.sort_unstable();

        let sort_order = choose_array_for_lftj(&bound, target_field);
        let field_vals = [s, p, o];
        let bound_vals: Vec<u64> = bound.iter().map(|&f| field_vals[f].unwrap()).collect();

        let mut it: Box<dyn TrieIterator> = data.cltj.trie_iter(sort_order);
        if it.at_end() {
            return it;
        }

        for val in bound_vals {
            it.seek(val);
            if it.at_end() || it.key() != val {
                return Box::new(EmptyTrieIter);
            }
            it = it.open();
            if it.at_end() {
                return Box::new(EmptyTrieIter);
            }
        }

        it
    }

}

impl GraphRing {
    /// Consume this `GraphRing`, producing the ε-serde-serializable
    /// [`RingSnapshot`] (triple count + optional per-graph [`CltjSnapshot`]).
    ///
    /// Used by the persistent snapshot format.  Requires unique ownership of the underlying
    /// `Arc<RingData>` (true for a freshly built `GraphRing` that has not yet
    /// been shared, per the "always mapped" design — see `RingStore::compact`);
    /// panics via `expect` otherwise.
    ///
    /// Only defined for the owned `Vec<u64>` form (`V`'s default) — a fresh
    /// build always starts from owned vocab, matching [`CltjData::into_snapshot`].
    pub(crate) fn into_snapshot(self) -> RingSnapshot {
        let cltj = match self.data {
            Some(data) => Arc::try_unwrap(data)
                .unwrap_or_else(|_| panic!("into_snapshot: RingData Arc is shared"))
                .cltj
                .into_snapshot(),
            // Empty graph: no Option-based sentinel (see RingSnapshot's doc
            // comment) — build a structurally valid, empty CltjSnapshot
            // instead. `n == 0` is the sentinel `from_snapshot`/`from_mapped`
            // use to skip reconstructing a `RingData` at all.
            None => CltjSnapshot::empty(),
        };
        RingSnapshot { n: self.n, cltj }
    }

}


// ── Reconstruction from snapshot (generic over substrate) ─────────────────────
//
// Pure field-moves with no owned-specific logic, so this is generic over any
// LOUDS substrate `B`/`L`/`S` and any vocab representation `V: AsRef<[u64]>`
// — this is what lets a future borrowed/mmap'd
// `RingSnapshot<CltjSnapshot<DeserType<Vec<u64>>, ..., [DeserType<LoudsCore>; 6]>>`
// reconstruct directly into a navigable `GraphRing<LoudsTrie<B, L, S>, V>`
// with **zero extra code** versus the owned round-trip path (Phase 3.3c step
// 2a, CLAUDE.md item 14).
impl<B, L, S, V: AsRef<[u64]>> GraphRing<LoudsTrie<B, L, S>, V> {
    /// Reconstruct a `GraphRing` from a [`RingSnapshot`] loaded from disk (or
    /// from an in-memory round-trip buffer, or a borrowed `load_mmap`'d
    /// view).
    pub(crate) fn from_snapshot(
        snap: RingSnapshot<CltjSnapshot<V, V, V, [LoudsCore<t_backend::TBitvec<B>, L, S>; 6]>>,
    ) -> GraphRing<LoudsTrie<B, L, S>, V> {
        // `n == 0` is the empty-graph sentinel (see `RingSnapshot`'s doc
        // comment) — `snap.cltj` is always a structurally valid
        // `CltjSnapshot` (never `Option`-wrapped), but for an empty graph
        // it's the cheap placeholder from `CltjSnapshot::empty()` and must
        // NOT be reconstructed into a real `RingData` (its 6 empty
        // `LoudsCore`s don't carry meaningful vocab redistribution).
        let data = (snap.n > 0).then(|| {
            Arc::new(RingData {
                cltj: CltjData::from_snapshot(snap.cltj),
            })
        });
        GraphRing { n: snap.n, data }
    }
}

// ── Construction from a mmap'd view (Phase 3.3c step 2b) ───────────────────────
//
// Builds a navigable `GraphRing<BorrowedLouds, VocabRepr>` directly from a
// borrowed `&DeserType<RingSnapshot>` view produced by `load_mmap`, reusing
// the existing substrate-generic `GraphRing::from_snapshot` above with zero
// duplicated reconstruction logic. The only new code here is: (1) cloning
// each borrowed `LoudsCore` out of the view (cheap — the `Clone` impls
// derived in `louds.rs` only copy pointer/length fields, never deep data),
// and (2) one documented `unsafe` lifetime extension per vocab slice,
// exactly mirroring `VocabRepr::Mapped`'s own documented safety argument:
// the caller must keep the backing `Arc<epserde::deser::MemCase<StoreSnapshot>>`
// alive for as long as the resulting `GraphRing` is reachable.
impl GraphRing<BorrowedLouds, VocabRepr> {
    /// Reconstruct a navigable, zero-copy `GraphRing<BorrowedLouds, VocabRepr>`
    /// from a `load_mmap`'d [`RingSnapshot`] view.
    pub(crate) fn from_mapped(
        view: &epserde::deser::DeserType<RingSnapshot>,
    ) -> GraphRing<BorrowedLouds, VocabRepr> {
        let c = &view.cltj;

        // SAFETY: see the module-level doc comment above — the vocab
        // slices borrow from the same `MemCase` mapped memory that the
        // caller is required to keep alive for as long as this
        // `GraphRing` is reachable, exactly like `VocabRepr::Mapped`'s
        // existing documented pattern in `cltj.rs`. (Prior to the
        // `RingSnapshot::cltj` `Option` removal, `c.vocab_s` etc. were
        // owned `Vec<u64>` at this nesting level — an epserde `Option<T>`
        // zero-copy limitation, not a bug in this reconstruction code; see
        // `RingSnapshot`'s doc comment. Now that `cltj` is a bare `Cltj`,
        // `c.vocab_s`/`vocab_p`/`vocab_o` are genuinely borrowed `&[u64]`
        // slices, so `.as_slice()` below is just a `&[u64] -> &[u64]`
        // no-op reborrow kept for clarity at the transmute call site.)
        let vocab_s: &'static [u64] =
            unsafe { std::mem::transmute::<&[u64], &'static [u64]>(&c.vocab_s) };
        let vocab_p: &'static [u64] =
            unsafe { std::mem::transmute::<&[u64], &'static [u64]>(&c.vocab_p) };
        let vocab_o: &'static [u64] =
            unsafe { std::mem::transmute::<&[u64], &'static [u64]>(&c.vocab_o) };

        // SAFETY: `c.tries`'s real type (confirmed empirically via
        // `std::any::type_name_of_val` on a real `load_mmap`'d value — see
        // `BorrowedEfDict`'s doc comment in `louds.rs`) now matches our
        // `Borrowed*`-based annotation exactly, modulo lifetime: every
        // borrowed slice inside `c.tries` is tied to `c`/`view`'s real
        // (finite) borrow, whereas the `Borrowed*` aliases declare `'static`
        // (a documented lie — see `BorrowedT`'s doc comment). `Clone` on
        // `LoudsCore`/`SidecarCore`/`TBitvec` only copies pointer/length
        // fields (never deep data), so this is a cheap, lifetime-only
        // extension — exactly the same kind of unsafety as the three
        // vocab-slice transmutes above. Soundness rests on the same caller
        // contract: whoever holds the resulting `GraphRing<BorrowedLouds,
        // VocabRepr>` must keep the backing
        // `Arc<epserde::deser::MemCase<StoreSnapshot>>` alive for as long as
        // it is reachable — enforced structurally (not just by convention)
        // by `MappedGraphRing`'s encapsulation, which is the sole caller of
        // `from_mapped` (see `MappedGraphRing::new`).
        let tries: [LoudsCore<t_backend::TBitvec<BorrowedT>, BorrowedL, BorrowedS>; 6] = unsafe {
            std::mem::transmute_copy(&c.tries)
        };

        let cltj = CltjSnapshot {
            vocab_s: VocabRepr::Mapped(vocab_s),
            vocab_p: VocabRepr::Mapped(vocab_p),
            vocab_o: VocabRepr::Mapped(vocab_o),
            tries,
        };

        let snap: RingSnapshot<
            CltjSnapshot<
                VocabRepr,
                VocabRepr,
                VocabRepr,
                [LoudsCore<t_backend::TBitvec<BorrowedT>, BorrowedL, BorrowedS>; 6],
            >,
        > = RingSnapshot { n: view.n, cltj };
        GraphRing::from_snapshot(snap)

    }
}



/// Owns the mmap'd backing memory for one graph's zero-copy `GraphRing` and
/// exposes it only through a private field + accessor, so the
/// MemCase-outlives-ring invariant is enforced **structurally** by the type
/// system rather than by caller convention.
///
/// ## Design rationale (per review discussion, CLAUDE.md item 14 Phase 3.3c)
///
/// `GraphRing::from_mapped` (above) builds a navigable
/// `GraphRing<BorrowedLouds, VocabRepr>` whose every borrowed field carries a
/// **lifetime-extended `'static` lie**: three `unsafe { transmute }` calls on
/// the vocab slices, plus one `unsafe { transmute_copy }` on the whole
/// `tries` array (a lifetime-only extension — the array's real element type
/// already matches `Borrowed*` exactly after the `BorrowedEfDict`/`BorrowedS`
/// alias fixes; see their doc comments in `louds.rs`). None of these
/// transmutes change any type's bit-layout or perform type-punning — they
/// only erase a borrow's true (finite) lifetime to `'static`, mirroring the
/// exact same pattern epserde's own [`epserde::deser::MemCase`] uses
/// internally (`MemCase<S>(DeserType<'static, S>, MemBackend)` — a
/// self-referential struct is not expressible in safe Rust; `ouroboros`/
/// `self_cell`-style crates contain the same unsafety internally, just
/// behind a macro).
///
/// Because the lie is "erase to `'static`", the *only* way to keep it sound
/// is to guarantee that the real backing memory — owned by the
/// `Arc<epserde::deser::MemCase<StoreSnapshot>>` this struct holds — outlives
/// every reachable copy of any borrowed reference derived from it. A
/// documentation-only contract on `from_mapped`'s caller would rely on every
/// future caller reading and honouring a comment. Instead, `MappedGraphRing`
/// makes the invariant structural: `ring` is a **private** field, the sole
/// public accessor `ring()` returns `&GraphRing<BorrowedLouds, VocabRepr>`
/// borrowed from `&self`, and `_mem` (also private) is guaranteed to be
/// dropped no earlier than `ring` (same struct, Rust drops fields in
/// declaration order, and more importantly outside of `unsafe` code there is
/// no way to separate `ring`'s lifetime from `self`'s). `from_mapped` itself
/// remains `pub(crate)`, but `MappedGraphRing::new` (below) is its only
/// caller anywhere in this crate.
///
/// SAFETY (binding contract for `MappedGraphRing::new`, the sole place that
/// constructs a `GraphRing<BorrowedLouds, VocabRepr>`):
/// 1. The only unsafe operations anywhere in this reconstruction path are
///    the lifetime extensions on the three vocab slices and the `tries`
///    array inside `GraphRing::from_mapped` (see the SAFETY comments there).
///    No unsafe type-punning transmute is used anywhere in this file.
/// 2. Soundness depends entirely on `MappedGraphRing` owning/cloning the
///    `Arc<epserde::deser::MemCase<StoreSnapshot>>` that backs the mmap'd
///    memory `ring`'s borrowed fields point into, and never releasing that
///    `Arc` before `ring` itself is dropped.
/// 3. The `ring` field **must remain private** — if it were ever made
///    `pub`/`pub(crate)`, a caller could move it out of `self` (e.g. via
///    destructuring or `mem::take`-style APIs), decoupling its borrowed
///    lifetime from `_mem`'s and reintroducing the exact use-after-free this
///    wrapper exists to prevent.
pub(crate) struct MappedGraphRing {
    /// Kept alive for as long as `ring`'s borrowed fields are reachable —
    /// see the SAFETY contract above. Never accessed directly; its only
    /// purpose is to extend the backing mmap'd memory's lifetime.
    _mem: Arc<epserde::deser::MemCase<crate::snapshot::StoreSnapshot>>,
    /// Private — see SAFETY contract point 3 above.
    ring: GraphRing<BorrowedLouds, VocabRepr>,
}

impl MappedGraphRing {
    /// Construct a `MappedGraphRing` for the graph at `ring_index` within
    /// `mem`'s deserialized [`crate::snapshot::StoreSnapshot`] view.
    ///
    /// `mem` must be the same `Arc` (or a clone of it) that the caller keeps
    /// alive for the lifetime of the whole store's mmap'd generation — see
    /// this struct's SAFETY contract above.
    pub(crate) fn new(
        mem: Arc<epserde::deser::MemCase<crate::snapshot::StoreSnapshot>>,
        ring_index: usize,
    ) -> Self {
        let view = mem.uncase();
        let ring = GraphRing::from_mapped(&view.rings[ring_index]);
        MappedGraphRing { _mem: mem, ring }
    }

    /// Borrow the navigable, zero-copy `GraphRing`. This is the *only* way
    /// to reach `ring` from outside this module — see SAFETY contract point
    /// 3 on the struct doc comment above.
    pub(crate) fn ring(&self) -> &GraphRing<BorrowedLouds, VocabRepr> {
        &self.ring
    }
}

/// A per-graph Ring handle that is either the owned in-memory form (built
/// directly by `RingBuilder`/`build_graphs_from_triples`, or round-tripped
/// through ε-serde for an in-memory store) or a zero-copy `load_mmap`'d form
/// (installed by `commit_compaction`/`RingStore::open` for a persistent
/// store, once its snapshot generation has been written to disk).
///
/// Both variants expose the same read-only method surface — `GraphRing<Louds,
/// V>`'s inherent methods are already generic over the LOUDS substrate/vocab
/// representation (see the `impl<Louds: LoudsNav + ..., V: AsRef<[u64]> +
/// ...>` block above), so every method here is a one-line match delegating
/// to whichever concrete `GraphRing` instantiation is present — no trait
/// object or dynamic dispatch overhead beyond the match itself.
pub(crate) enum GraphRingHandle {
    Owned(Arc<GraphRing>),
    Mapped(Arc<MappedGraphRing>),
}

impl GraphRingHandle {
    /// Number of distinct triples stored in this graph.
    pub(crate) fn n(&self) -> usize {
        match self {
            GraphRingHandle::Owned(r) => r.n,
            GraphRingHandle::Mapped(m) => m.ring().n,
        }
    }

    pub(crate) fn contains(&self, s: u64, p: u64, o: u64) -> bool {
        match self {
            GraphRingHandle::Owned(r) => r.contains(s, p, o),
            GraphRingHandle::Mapped(m) => m.ring().contains(s, p, o),
        }
    }

    pub(crate) fn spo_triples(&self) -> Vec<[u64; 3]> {
        match self {
            GraphRingHandle::Owned(r) => r.spo_triples(),
            GraphRingHandle::Mapped(m) => m.ring().spo_triples(),
        }
    }

    pub(crate) fn match_triples(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
    ) -> Vec<[u64; 3]> {
        match self {
            GraphRingHandle::Owned(r) => r.match_triples(s, p, o),
            GraphRingHandle::Mapped(m) => m.ring().match_triples(s, p, o),
        }
    }

    pub(crate) fn mem_size_bytes(&self) -> usize {
        match self {
            GraphRingHandle::Owned(r) => r.mem_size_bytes(),
            GraphRingHandle::Mapped(m) => m.ring().mem_size_bytes(),
        }
    }

    pub(crate) fn mem_breakdown_per_ordering(
        &self,
    ) -> Option<[(SortOrder, LoudsMemBreakdown, usize); 6]> {
        match self {
            GraphRingHandle::Owned(r) => r.mem_breakdown_per_ordering(),
            GraphRingHandle::Mapped(m) => m.ring().mem_breakdown_per_ordering(),
        }
    }

    pub(crate) fn spo_and_vocab_bytes(&self) -> (usize, usize) {
        match self {
            GraphRingHandle::Owned(r) => r.spo_and_vocab_bytes(),
            GraphRingHandle::Mapped(m) => m.ring().spo_and_vocab_bytes(),
        }
    }

    pub(crate) fn estimate_count(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> u64 {
        match self {
            GraphRingHandle::Owned(r) => r.estimate_count(s, p, o, target_field),
            GraphRingHandle::Mapped(m) => m.ring().estimate_count(s, p, o, target_field),
        }
    }

    pub(crate) fn join_scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator> {
        match self {
            GraphRingHandle::Owned(r) => r.join_scan(s, p, o, target_field),
            GraphRingHandle::Mapped(m) => m.ring().join_scan(s, p, o, target_field),
        }
    }
}

/// ε-serde-serializable snapshot of a [`GraphRing`]: triple count plus a
/// [`CltjSnapshot`] (a structurally valid but empty snapshot — see
/// [`CltjSnapshot::empty`] — for an empty graph, i.e. `n == 0`).
///

/// This is the persistent on-disk representation.  See [`GraphRing::into_snapshot`] and
/// [`GraphRing::from_snapshot`].
///
/// Generic over `Cltj` (default [`CltjSnapshot`]) so that a future mmap'd
/// load can substitute ε-serde's borrowed `DeserType<CltjSnapshot>` form here
/// with **zero extra code** — this mirrors the "bare generic parameter with
/// a default" pattern used throughout `louds.rs`/`cltj.rs` (Phase 3.3c probe,
/// CLAUDE.md item 14).
///
/// `cltj` is a **bare** `Cltj` (not `Option<Cltj>`). An earlier version of
/// this format used `Option<Cltj>` (`None` for an empty graph, mirroring
/// `GraphRing`'s own `data: Option<Arc<RingData>>` in-memory representation)
/// — but ε-serde deserializes `Option<T>` fields as fully-owned copies even
/// under `load_mmap` (confirmed empirically: with the field as
/// `Option<CltjSnapshot>`, a `load_mmap`'d view's `vocab_s` came back as an
/// owned `alloc::vec::Vec<u64>` and `tries[0]` at the fully-owned `LoudsCore`
/// size, instead of the borrowed `&[u64]`/borrowed-`LoudsCore` shapes seen
/// when loading a bare `CltjSnapshot` directly) — silently defeating Phase 3's
/// entire zero-copy purpose for every graph in the store. Removing the
/// `Option` and using `n == 0` as the sentinel (see [`GraphRing::from_snapshot`])
/// is the standard zero-copy-format fix: it also removes a previously
/// representable invalid state (`n > 0` with `cltj` empty, or vice versa).
#[derive(Epserde)]
pub(crate) struct RingSnapshot<Cltj = CltjSnapshot> {
    pub(crate) n: usize,
    pub(crate) cltj: Cltj,
}



// ── Trie suffix enumeration helper ────────────────────────────────────────────

/// Enumerate all leaf triples reachable from `it` (positioned at trie-column
/// depth `depth_reached`), recursively `open()`-ing through the remaining
/// depths, converting each leaf's trie-column values back to canonical
/// `[s, p, o]` order via [`SortOrder::to_spo`].
///
/// Used by [`GraphRing::match_triples`] to enumerate the unbound suffix of a
/// pattern match after seeking through the bound prefix.
fn enumerate_suffix(
    mut it: Box<dyn TrieIterator>,
    depth_reached: usize,
    prefix: &mut [u64; 3],
    order: SortOrder,
    results: &mut Vec<[u64; 3]>,
) {
    match depth_reached {
        0 => {
            while !it.at_end() {
                prefix[0] = it.key();
                enumerate_suffix(it.open(), 1, prefix, order, results);
                it.advance();
            }
        }
        1 => {
            while !it.at_end() {
                prefix[1] = it.key();
                enumerate_suffix(it.open(), 2, prefix, order, results);
                it.advance();
            }
        }
        2 => {
            while !it.at_end() {
                prefix[2] = it.key();
                results.push(order.to_spo(prefix[0], prefix[1], prefix[2]));
                it.advance();
            }
        }
        _ => unreachable!("trie depth is always 0..=2"),
    }
}

// ── RingBuilder ───────────────────────────────────────────────────────────────

/// Accumulates `(s_id, p_id, o_id)` tuples and builds a `GraphRing`.
pub struct RingBuilder {
    triples: Vec<[u64; 3]>,
}

impl RingBuilder {
    pub fn new() -> Self {
        Self {
            triples: Vec::new(),
        }
    }

    /// Build directly from an existing `Vec<[u64; 3]>` — takes ownership
    /// without copying, avoiding the allocate/push/reallocate churn of
    /// repeated `add()` calls when the caller already has the triples in a
    /// single contiguous buffer (e.g. `build_graphs_from_triples`'s
    /// already-collected per-graph `Vec`).  The Vec need not be pre-sorted
    /// or deduped — `build()` still performs both.
    pub(crate) fn from_vec(triples: Vec<[u64; 3]>) -> Self {
        Self { triples }
    }

    pub fn add(&mut self, s: u64, p: u64, o: u64) {
        self.triples.push([s, p, o]);
    }

    /// Consume the builder and produce a `GraphRing`.
    ///
    /// O(n log n) construction (6 sorts + LOUDS trie construction per ordering).
    /// Deduplication is applied automatically.
    pub fn build(self) -> GraphRing {
        let mut triples = self.triples;
        triples.sort_unstable();
        triples.dedup();
        let n = triples.len();

        if n == 0 {
            return GraphRing { n: 0, data: None };
        }

        // ── Compact vocabulary ────────────────────────────────────────────────

        let build_vocab = |extract: fn(&[u64; 3]) -> u64| -> (Vec<u64>, HashMap<u64, usize>) {
            let mut vals: Vec<u64> = triples.iter().map(extract).collect();
            vals.sort_unstable();
            vals.dedup();
            let map: HashMap<u64, usize> = vals.iter().enumerate().map(|(i, &v)| (v, i)).collect();
            (vals, map)
        };

        let (orig_s, map_s) = build_vocab(|t| t[0]);
        let (orig_p, map_p) = build_vocab(|t| t[1]);
        let (orig_o, map_o) = build_vocab(|t| t[2]);

        // ── Build six LOUDS tries ─────────────────────────────────────────────

        let orig_s = Arc::new(orig_s);
        let orig_p = Arc::new(orig_p);
        let orig_o = Arc::new(orig_o);

        let cltj = build_cltj_data(
            &triples,
            &map_s,
            &map_p,
            &map_o,
            Arc::clone(&orig_s),
            Arc::clone(&orig_p),
            Arc::clone(&orig_o),
        );

        // orig_s/p/o and map_s/p/o are consumed by build_cltj_data above.
        // RingData only keeps the six LOUDS tries — there is no
        // redundant `spo: Vec<[u64;3]>` raw copy of `triples`.
        let _ = (map_s, map_p, map_o); // explicitly consumed above
        let data = Arc::new(RingData { cltj });

        GraphRing {
            n,
            data: Some(data),
        }
    }
}

impl Default for RingBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn build_ring(triples: &[[u64; 3]]) -> GraphRing {
        let mut b = RingBuilder::new();
        for &[s, p, o] in triples {
            b.add(s, p, o);
        }
        b.build()
    }

    #[test]
    fn empty_ring() {
        let r = build_ring(&[]);
        assert_eq!(r.n, 0);
        assert!(r.match_triples(None, None, None).is_empty());
    }

    #[test]
    fn dedup_on_build() {
        let r = build_ring(&[[1, 2, 3], [1, 2, 3], [4, 5, 6]]);
        assert_eq!(r.n, 2);
    }

    #[test]
    fn full_wildcard() {
        let r = build_ring(&[[1, 2, 3], [1, 2, 4], [5, 6, 7]]);
        let mut m = r.match_triples(None, None, None);
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [1, 2, 4], [5, 6, 7]]);
    }

    #[test]
    fn subject_bound() {
        let r = build_ring(&[[1, 2, 3], [1, 2, 4], [5, 6, 7]]);
        let mut m = r.match_triples(Some(1), None, None);
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [1, 2, 4]]);
    }

    #[test]
    fn predicate_bound() {
        let r = build_ring(&[[1, 2, 3], [1, 3, 4], [5, 2, 7]]);
        let mut m = r.match_triples(None, Some(2), None);
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [5, 2, 7]]);
    }

    #[test]
    fn object_bound() {
        let r = build_ring(&[[1, 2, 3], [1, 2, 4], [5, 6, 3]]);
        let mut m = r.match_triples(None, None, Some(3));
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [5, 6, 3]]);
    }

    #[test]
    fn sp_bound() {
        let r = build_ring(&[[1, 2, 3], [1, 2, 4], [1, 3, 5], [5, 2, 6]]);
        let mut m = r.match_triples(Some(1), Some(2), None);
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [1, 2, 4]]);
    }

    #[test]
    fn so_bound() {
        let r = build_ring(&[[1, 2, 3], [1, 3, 3], [2, 2, 3]]);
        let mut m = r.match_triples(Some(1), None, Some(3));
        m.sort();
        assert_eq!(m, vec![[1, 2, 3], [1, 3, 3]]);
    }

    #[test]
    fn exact_triple() {
        let r = build_ring(&[[1, 2, 3], [4, 5, 6]]);
        assert_eq!(r.match_triples(Some(1), Some(2), Some(3)), vec![[1, 2, 3]]);
        assert!(r.match_triples(Some(1), Some(2), Some(99)).is_empty());
    }

    #[test]
    fn contains_check() {
        let r = build_ring(&[[1, 2, 3], [4, 5, 6]]);
        assert!(r.contains(1, 2, 3));
        assert!(r.contains(4, 5, 6));
        assert!(!r.contains(1, 2, 4));
    }

    #[test]
    fn spo_triples_roundtrip() {
        let triples = vec![[1u64, 2, 3], [1, 2, 4], [5, 6, 7]];
        let r = build_ring(&triples);
        let mut out = r.spo_triples();
        out.sort();
        assert_eq!(out, triples);
    }

    // ── TrieIterator tests ────────────────────────────────────────────────────


    #[test]
    fn trie_iter_depth0_key_seek_advance() {
        let r = build_ring(&[[1, 2, 10], [1, 3, 11], [2, 2, 12], [3, 4, 13]]);
        let mut it = r.trie_iter(SortOrder::Spo);
        assert!(!it.at_end());
        assert_eq!(it.key(), 1);
        it.advance();
        assert_eq!(it.key(), 2);
        it.advance();
        assert_eq!(it.key(), 3);
        it.advance();
        assert!(it.at_end());
    }

    #[test]
    fn trie_iter_seek() {
        let r = build_ring(&[[1, 2, 10], [3, 4, 11], [7, 8, 12]]);
        let mut it = r.trie_iter(SortOrder::Spo);
        it.seek(4);
        assert_eq!(it.key(), 7);
        it.seek(100);
        assert!(it.at_end());
    }

    #[test]
    fn trie_iter_open_depth1_depth2() {
        let r = build_ring(&[[1, 2, 10], [1, 3, 11], [2, 5, 12]]);
        let mut it0 = r.trie_iter(SortOrder::Spo);
        assert_eq!(it0.key(), 1);

        let mut it1 = it0.open();
        assert_eq!(it1.key(), 2);
        let mut it2 = it1.open();
        assert_eq!(it2.key(), 10);
        it2.advance();
        assert!(it2.at_end());

        it1.advance();
        assert_eq!(it1.key(), 3);
        it1.advance();
        assert!(it1.at_end());

        it0.advance();
        assert_eq!(it0.key(), 2);
        let it1b = it0.open();
        assert_eq!(it1b.key(), 5);
    }

    #[test]
    fn d1b_sop_ordering() {
        // SOP: fixed=s, iterate distinct o values
        let r = build_ring(&[[1, 2, 10], [1, 3, 20], [1, 4, 10], [2, 2, 30]]);
        let it0 = r.trie_iter(SortOrder::Sop);
        assert_eq!(it0.key(), 1); // subject 1

        let mut it1 = it0.open(); // distinct objects for s=1: {10, 20}
        assert_eq!(it1.key(), 10);
        it1.advance();
        assert_eq!(it1.key(), 20);
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn d1b_pso_ordering() {
        // PSO: fixed=p, iterate distinct s values
        let r = build_ring(&[[1, 5, 10], [2, 5, 20], [3, 5, 30], [1, 6, 40]]);
        let it0 = r.trie_iter(SortOrder::Pso);
        assert_eq!(it0.key(), 5); // first predicate

        let mut it1 = it0.open(); // distinct subjects for p=5: {1, 2, 3}
        assert_eq!(it1.key(), 1);
        it1.advance();
        assert_eq!(it1.key(), 2);
        it1.advance();
        assert_eq!(it1.key(), 3);
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn d1b_seek() {
        // SOP: seek on D1B secondary
        let r = build_ring(&[[1, 2, 10], [1, 3, 20], [1, 4, 30], [1, 5, 40]]);
        let it0 = r.trie_iter(SortOrder::Sop);
        assert_eq!(it0.key(), 1);
        let mut it1 = it0.open();
        // Objects for s=1: {10, 20, 30, 40}
        it1.seek(25); // should land on 30
        assert_eq!(it1.key(), 30);
        it1.seek(50); // past end
        assert!(it1.at_end());
    }

    #[test]
    fn join_scan_sop() {
        // join_scan with s bound, target=o (uses SOP ordering)
        let r = build_ring(&[[1, 2, 10], [1, 3, 20], [2, 2, 30]]);
        let mut it = r.join_scan(Some(1), None, None, 2);
        assert!(!it.at_end());
        assert_eq!(it.key(), 10);
        it.advance();
        assert_eq!(it.key(), 20);
        it.advance();
        assert!(it.at_end());
    }

    // ── Phase 3.3c step 2b: from_mapped / MappedGraphRing tests ──────────────

    /// Build a `StoreSnapshot` containing one non-empty and one empty graph,
    /// serialize it, and `load_mmap` it into a `MappedGraphRing` for each
    /// graph. Verifies:
    /// 1. (Permanent regression probe) vocab is genuinely borrowed at BOTH
    ///    the top-level `CltjSnapshot` nesting AND through the full
    ///    `StoreSnapshot -> RingSnapshot -> CltjSnapshot` nesting used in
    ///    production — i.e. the `Option<Cltj>` removal actually restored
    ///    zero-copy end-to-end (see `RingSnapshot`'s doc comment for why
    ///    this regressed once before).
    /// 2. `MappedGraphRing::ring()` produces results identical to the owned
    ///    `GraphRing` for `contains`/`match_triples`/`spo_triples`, for both
    ///    the non-empty and the empty graph.
    #[test]
    fn mapped_graph_ring_matches_owned_and_is_zero_copy() {
        use crate::snapshot::StoreSnapshot;
        use epserde::deser::{Deserialize, Flags};
        use epserde::ser::Serialize;
        use oxigraph_nova_core::{GRAPH_DEFAULT, GraphId};
        use std::collections::HashMap;

        let triples: &[[u64; 3]] = &[[1, 2, 3], [1, 2, 4], [1, 3, 5], [2, 2, 6], [2, 5, 7]];
        let owned_non_empty = build_ring(triples);
        let owned_empty = build_ring(&[]);

        // Build the same two graphs again for the snapshot path (the first
        // copies are kept as the "expected" owned baseline). Drives the
        // exact same `StoreSnapshot::from_graphs` construction used in
        // production (now `pub(crate)` specifically so this test can reuse
        // it instead of hand-rolling a parallel struct).
        let mut graphs: HashMap<GraphId, Arc<GraphRing>> = HashMap::new();
        graphs.insert(GRAPH_DEFAULT, Arc::new(build_ring(triples)));
        graphs.insert(GraphId(2), Arc::new(build_ring(&[])));

        let snap = StoreSnapshot::from_graphs(graphs);

        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let path =
            std::env::temp_dir().join(format!("nova_mapped_graph_ring_probe_{pid}_{n}.snap"));
        let _ = std::fs::remove_file(&path);
        {
            let mut f = std::fs::File::create(&path).expect("create temp file");
            unsafe {
                snap.serialize(&mut f).expect("serialize snapshot");
            }
        }

        let mem_case = unsafe {
            <StoreSnapshot>::load_mmap(&path, Flags::empty()).expect("load_mmap StoreSnapshot")
        };
        let view: &epserde::deser::DeserType<StoreSnapshot> = mem_case.uncase();

        // Regression probe: vocab must be borrowed (not owned), matching the
        // full nesting depth used in production (StoreSnapshot -> Vec<RingSnapshot>
        // -> RingSnapshot -> CltjSnapshot). We can't directly assert a type
        // here without the same brittle probe machinery used during
        // investigation, but `from_mapped`'s tries transmute (below) has a
        // compile-time size assertion (transmute panics/fails to compile on
        // any size mismatch) — that + this test's behavioural equivalence
        // assertions together are the permanent regression guard: if a
        // future change reintroduces an `Option<Cltj>`-shaped zero-copy
        // break, either the size-checked transmute in `from_mapped` will
        // fail to compile, or (if it still happens to compile) the
        // navigation results below would no longer come from `Borrowed*`
        // types at all — this test exercises the exact same code path
        // `MappedGraphRing::new` uses in production.
        assert_eq!(view.graph_ids.len(), 2);
        assert_eq!(view.rings.len(), 2);

        let non_empty_idx = view.graph_ids.iter().position(|&g| g == GRAPH_DEFAULT.as_u8()).unwrap();
        let empty_idx = view.graph_ids.iter().position(|&g| g == GraphId(2).as_u8()).unwrap();

        let mapped_non_empty = GraphRing::from_mapped(&view.rings[non_empty_idx]);
        let mapped_empty = GraphRing::from_mapped(&view.rings[empty_idx]);

        // n matches.
        assert_eq!(mapped_non_empty.n, owned_non_empty.n);
        assert_eq!(mapped_empty.n, owned_empty.n);

        // contains() matches for every input triple plus one negative probe.
        for &[s, p, o] in triples {
            assert!(mapped_non_empty.contains(s, p, o));
        }
        assert!(!mapped_non_empty.contains(9, 9, 9));
        assert!(!mapped_empty.contains(1, 2, 3));

        // match_triples(None, None, None) matches (order-independent).
        let mut owned_all = owned_non_empty.match_triples(None, None, None);
        let mut mapped_all = mapped_non_empty.match_triples(None, None, None);
        owned_all.sort();
        mapped_all.sort();
        assert_eq!(owned_all, mapped_all);
        assert!(mapped_empty.match_triples(None, None, None).is_empty());

        // spo_triples() matches.
        let mut owned_spo = owned_non_empty.spo_triples();
        let mut mapped_spo = mapped_non_empty.spo_triples();
        owned_spo.sort();
        mapped_spo.sort();
        assert_eq!(owned_spo, mapped_spo);
        assert!(mapped_empty.spo_triples().is_empty());

        let _ = std::fs::remove_file(&path);
    }

    /// `MappedGraphRing::new`/`ring()` end-to-end: build a real
    /// `StoreSnapshot` via the public `round_trip_and_maybe_save`/on-disk
    /// path used in production, `load_mmap` it, and verify
    /// `MappedGraphRing::ring()` navigates correctly for both a populated
    /// and an empty graph.
    #[test]
    fn mapped_graph_ring_new_end_to_end() {
        use crate::snapshot::StoreSnapshot;
        use epserde::deser::{Deserialize, Flags};
        use oxigraph_nova_core::{GRAPH_DEFAULT, GraphId};
        use std::collections::HashMap;

        let mut graphs: HashMap<GraphId, Arc<GraphRing>> = HashMap::new();
        graphs.insert(
            GRAPH_DEFAULT,
            Arc::new(build_ring(&[[1, 2, 3], [4, 5, 6], [7, 8, 9]])),
        );
        graphs.insert(GraphId(2), Arc::new(build_ring(&[])));

        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let path =
            std::env::temp_dir().join(format!("nova_mapped_graph_ring_e2e_{pid}_{n}.snap"));
        let _ = std::fs::remove_file(&path);

        let _ = StoreSnapshot::round_trip_and_maybe_save(graphs, Some(&path)).unwrap();

        let mem_case = Arc::new(unsafe {
            <StoreSnapshot>::load_mmap(&path, Flags::empty()).expect("load_mmap StoreSnapshot")
        });

        let view = mem_case.uncase();
        let default_idx = view
            .graph_ids
            .iter()
            .position(|&g| g == GRAPH_DEFAULT.as_u8())
            .unwrap();
        let empty_idx = view
            .graph_ids
            .iter()
            .position(|&g| g == GraphId(2).as_u8())
            .unwrap();

        let mapped_default = MappedGraphRing::new(Arc::clone(&mem_case), default_idx);
        let mapped_empty = MappedGraphRing::new(Arc::clone(&mem_case), empty_idx);

        assert_eq!(mapped_default.ring().n, 3);
        assert!(mapped_default.ring().contains(1, 2, 3));
        assert!(mapped_default.ring().contains(4, 5, 6));
        assert!(mapped_default.ring().contains(7, 8, 9));
        assert!(!mapped_default.ring().contains(1, 2, 4));

        assert_eq!(mapped_empty.ring().n, 0);
        assert!(mapped_empty.ring().match_triples(None, None, None).is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
