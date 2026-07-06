//! CompactLTJ LOUDS-trie iterator and data structures.
//!
//! Reference: Arroyuelo, Navarro, Gómez-Brandón et al.
//! "CompactLTJ: Space and Time Efficient Leapfrog Triejoin on Graph Databases"
//! VLDB Journal, 2025.
//!
//! ## Design
//!
//! [`CltjData`] holds **six LOUDS height-3 tries**, one per SPO ordering
//! (SPO, SOP, PSO, POS, OPS, OSP).  Each trie stores the corresponding sort of
//! the dataset as a compact trie and exposes a [`CltjTrieIter`] that implements
//! the [`TrieIterator`] trait consumed by the LFTJ evaluator.
//!
//! Every trie navigation operation is O(1) (child, degree, access via LOUDS
//! rank/select), while seek uses exponential search O(log ℓ).  This replaces
//! the WaveletMatrix Ring's O(log σ) backward-search steps with O(1) per step.
//!
//! ## Paired build to reduce sort work
//!
//! The six orderings form **three natural pairs** by depth-0 field:
//!
//! | Pair   | Depth-0 | Primary | Secondary |
//! |--------|---------|---------|-----------|
//! | S-pair | Subject | SPO     | SOP       |
//! | P-pair | Pred.   | PSO     | POS       |
//! | O-pair | Object  | OPS     | OSP       |
//!
//! Within each pair, the depth-0 grouping is identical.  [`build_pair_shared_c0`]
//! exploits this: the primary ordering re-uses the already-sorted input, and the
//! secondary ordering performs only per-group (within-c0) re-sorts of c1↔c2.
//! Total sort work falls from 6 full O(N log N) sorts to 2 full sorts +
//! 3 sets of within-group sorts ≈ 4.5 equivalent sorts — roughly a 25% saving.
//!
//! Additionally, **SPO comes in pre-sorted** from the caller
//! (`build_cltj_data` receives `spo_sorted`), so the S-pair needs zero full
//! sorts: one free primary + one set of within-group secondary sorts.
//!
//! ## Vocabulary storage
//!
//! Each trie stores three vocabulary arrays (`vocab[0..2]`) as `Arc<Vec<u64>>`.
//! Using direct `Vec<u64>` indexing keeps `key()` at O(1) with no bit-unpacking
//! overhead (critical since it's called on every leapfrog step), and allows
//! `seek()` to use `partition_point` which the compiler can SIMD-vectorise.
//!
//! ## Structure per trie
//!
//! ```text
//! depth 0 → primary field  (e.g. S for SPO)
//! depth 1 → secondary field (e.g. P for SPO)
//! depth 2 → leaf field      (e.g. O for SPO)
//! ```
//!
//! Labels stored in L are **0-indexed compact local IDs**.  The three
//! vocabulary arrays (`vocab[0]`, `vocab[1]`, `vocab[2]`) map local → global
//! (u64 term IDs) for each depth.

use crate::louds::{LoudsCore, LoudsMemBreakdown, LoudsTrie, build_louds_from_sorted};

use crate::ring::SortOrder;
use epserde::Epserde;
use oxigraph_nova_core::{EmptyTrieIter, TrieIterator};
use std::sync::Arc;

// ── VocabRepr ─────────────────────────────────────────────────────────────────

/// Vocab representation for [`CltjTrie`]/[`CltjData`]: either owned
/// (heap-allocated, as produced by a fresh build or a heap-copy
/// `deserialize_full` round-trip) or **borrowed** from a `load_mmap`'d
/// snapshot file (Phase 3.3 of the mmap'd ε-serde snapshot plan — CLAUDE.md
/// item 14).
///
/// `Mapped`'s `&'static [u64]` is not truly `'static` data — it is a
/// lifetime-extended borrow into the backing memory owned by an
/// `Arc<epserde::deser::MemCase<StoreSnapshot>>` that the store keeps alive
/// for as long as any `Mapped` vocab derived from it is reachable (the
/// standard self-referential mmap pattern: the borrow is sound as long as
/// the owning `MemCase`'s mapped memory outlives every `VocabRepr::Mapped`
/// constructed from it, which the store's ownership structure guarantees).
///
/// This is the substrate that lets [`CltjTrie<V>`]/[`CltjData<V>`] (generic
/// since Phase 3.2a) hold zero-copy vocab slices with **no code
/// duplication** versus the owned path — every navigation method already
/// works generically over any `V: AsRef<[u64]>`.
pub(crate) enum VocabRepr {
    /// Heap-allocated owned vocab (fresh build, or in-memory round-trip).
    Owned(Vec<u64>),
    /// Borrowed vocab slice, zero-copy from a `load_mmap`'d snapshot file.
    /// See the type-level doc comment above for the lifetime-extension
    /// safety argument.
    Mapped(&'static [u64]),
}

impl AsRef<[u64]> for VocabRepr {
    #[inline]
    fn as_ref(&self) -> &[u64] {
        match self {
            VocabRepr::Owned(v) => v.as_slice(),
            VocabRepr::Mapped(s) => s,
        }
    }
}

// ── CltjTrie ──────────────────────────────────────────────────────────────────


/// A single LOUDS trie for one sort ordering, with vocabulary for global-ID
/// translation.
///
/// `vocab[d]` holds a sorted array of global term IDs for depth `d`. Generic
/// over the vocab representation `V` (bounded by `AsRef<[u64]>`) so that a
/// future mmap'd/zero-copy snapshot load can populate `vocab` with borrowed
/// `&[u64]` slices directly, with **no code duplication** versus the owned
/// `Vec<u64>` path — this mirrors the `LoudsTrie<B, L, S>` generic-substrate
/// pattern established in Phase 2b (CLAUDE.md item 14).
///
/// Direct indexing keeps `key()` at O(1) with no bit-unpacking, and `seek()`
/// can use `partition_point` which the compiler can SIMD-vectorise, for both
/// the owned and borrowed representations.
pub struct CltjTrie<V = Vec<u64>> {
    /// LOUDS bitvector + label array.
    louds: LoudsTrie,
    /// `vocab[d]` = sorted global IDs for depth `d` (0, 1, or 2).
    /// `vocab[d][local_id]` → global TermId as `u64`.
    vocab: [Arc<V>; 3],
}

impl CltjTrie {


    /// Consume this trie, discarding the vocab `Arc`s (vocab is hoisted to
    /// the top-level [`CltjSnapshot`], deduped across all six tries) and
    /// keeping only the ε-serde-serializable [`LoudsCore`].
    ///
    /// Used by the persistent snapshot format.  Requires unique ownership
    /// of the trie (i.e. the
    /// enclosing `Arc<CltjTrie>` must have refcount 1) — true for a freshly
    /// built [`CltjData`] that has not yet been shared, per the "always
    /// mapped" design (see `RingStore::compact`).
    pub(crate) fn into_core(self) -> LoudsCore {
        self.louds.into_core()
    }
}

impl<V: AsRef<[u64]>> CltjTrie<V> {
    /// Borrow depth `d`'s vocab array as a plain `&[u64]` slice, going
    /// through `V`'s `AsRef<[u64]>` impl.  `Arc<V>` itself only implements
    /// `AsRef<V>` (not a conditional `AsRef<[u64]>` passthrough), so we must
    /// deref to `V` first before calling `AsRef<[u64]>::as_ref` — the
    /// explicit `&[u64]` return type disambiguates the (potentially
    /// multi-impl) `as_ref()` call.
    #[inline]
    pub(crate) fn vocab_slice(&self, d: usize) -> &[u64] {
        (*self.vocab[d]).as_ref()
    }


    /// Number of distinct values (vocabulary size) at depth `d` (0, 1, or 2).
    ///
    /// Used by [`CltjData::vocab_size_for_field`] to produce cardinality
    /// estimates for adaptive VEO in LFTJ without opening a TrieIterator.
    pub fn vocab_len(&self, d: usize) -> usize {
        if d < 3 { self.vocab_slice(d).len() } else { 0 }
    }


    /// Real allocated byte size of this trie's LOUDS structure (T + L +
    /// sidecar), **excluding** vocab (vocab is `Arc`-shared across multiple
    /// tries within a [`CltjData`]; see [`CltjData::mem_size_bytes`] for the
    /// correctly deduped total).
    pub fn louds_mem_size_bytes(&self) -> usize {
        self.louds.mem_size_bytes()
    }

    /// Per-component (T/L/sidecar) memory breakdown of this trie's LOUDS
    /// structure.  Excludes vocab — see
    /// [`CltjData::mem_breakdown_per_ordering`] for per-field vocab bytes.
    pub fn mem_breakdown(&self) -> LoudsMemBreakdown {
        self.louds.mem_breakdown()
    }

    /// Byte size of this trie's three vocab arrays, **without** Arc-dedup
    /// (i.e. counted once per trie, even if shared with other tries).  Used
    /// by [`CltjData::mem_breakdown_per_ordering`] to report a per-ordering
    /// "as if not shared" vocab figure alongside the deduped grand total.
    ///
    /// Counts only the raw `u64` payload (`len() * 8` bytes) plus one
    /// `size_of::<V>()` header — accurate for the owned `Vec<u64>` case and a
    /// reasonable approximation for a borrowed slice reference `V` (whose
    /// backing bytes live in the mmap, not the heap, once Phase 3.3 wires in
    /// real mmap loading).
    pub fn vocab_bytes_undeduped(&self) -> usize {
        self.vocab
            .iter()
            .map(|v| {
                std::mem::size_of::<V>()
                    + AsRef::<[u64]>::as_ref(&**v).len() * std::mem::size_of::<u64>()
            })
            .sum()
    }


    /// Pointer identities of this trie's three vocab `Arc` allocations, for
    /// dedup accounting at the `CltjData` level.
    pub(crate) fn vocab_arc_ptrs(&self) -> [*const V; 3] {
        [
            Arc::as_ptr(&self.vocab[0]),
            Arc::as_ptr(&self.vocab[1]),
            Arc::as_ptr(&self.vocab[2]),
        ]
    }

    /// Create a depth-0 `CltjTrieIter` positioned at the first root child.
    ///
    /// Returns [`EmptyTrieIter`] if the trie is empty.
    pub fn iter_d0(self: &Arc<Self>) -> Box<dyn TrieIterator>
    where
        V: Send + Sync + 'static,
    {
        let degree = self.louds.root_degree();
        if degree == 0 {
            return Box::new(EmptyTrieIter);
        }
        Box::new(CltjTrieIter {
            trie: Arc::clone(self),
            hi: degree,
            pos: 1,
            depth: 0,
        })
    }

    /// Zero-allocation equivalent of `iter_d0()` followed by a `seek`/`open`
    /// sequence for each value in `bound_vals` (in trie-column order), ending
    /// with a `remaining_count()` read at the resulting depth.
    ///
    /// Unlike navigating via `Box<dyn TrieIterator>` (which allocates one
    /// heap `CltjTrieIter` per depth), this walks the raw `(hi, pos, depth)`
    /// LOUDS-navigation state entirely on the stack — used by adaptive VEO's
    /// cardinality probe (`GraphRing::real_count_no_alloc`), which needs to
    /// evaluate this for *every* candidate variable at *every* recursion
    /// depth, most of which are discarded immediately (only the winning
    /// candidate's scan is actually retained and used for leapfrogging).
    ///
    /// Returns `0` if the trie is empty, any bound value is not found, or
    /// `bound_vals.len() >= 3` (deeper than this height-3 trie supports).
    pub fn real_count_no_alloc(&self, bound_vals: &[u64]) -> u64 {
        let degree = self.louds.root_degree();
        if degree == 0 {
            return 0;
        }
        let mut hi = degree;
        let mut pos = 1usize;

        for (depth, &val) in bound_vals.iter().enumerate() {
            if depth >= 3 || pos > hi {
                return 0;
            }
            let vocab = self.vocab_slice(depth);
            // seek: no-op if val <= current key, else exponential-search leap.
            let cur_local = self.louds.label_at(pos) as usize;
            if val > vocab[cur_local] {

                let local_target = vocab.partition_point(|&v| v < val);
                if local_target >= vocab.len() {
                    return 0;
                }
                pos = self.louds.leap(pos, hi, local_target as u32);
                if pos > hi {
                    return 0;
                }
            }
            let local_id = self.louds.label_at(pos) as usize;
            if vocab[local_id] != val {
                return 0;
            }
            // open: descend into the child range.
            if depth >= 2 {
                return 0; // leaf level has no children
            }
            let child_v = self.louds.child_from_label_pos(pos);
            let child_degree = self.louds.degree(child_v);
            if child_degree == 0 {
                return 0;
            }
            hi = child_v + child_degree;
            pos = child_v + 1;
        }

        (hi - pos + 1) as u64
    }
}


// ── CltjData ──────────────────────────────────────────────────────────────────

/// Six LOUDS tries (one per ordering) for a single named graph.
///
/// Generic over the vocab representation `V` (see [`CltjTrie`]) so that the
/// borrowed (mmap'd) form can share this same implementation.
pub struct CltjData<V = Vec<u64>> {
    tries: [Arc<CltjTrie<V>>; 6],
}

impl<V: AsRef<[u64]> + Send + Sync + 'static> CltjData<V> {

    /// Index into `tries` for a given ordering.
    #[inline]
    fn idx(ord: SortOrder) -> usize {
        match ord {
            SortOrder::Spo => 0,
            SortOrder::Sop => 1,
            SortOrder::Pso => 2,
            SortOrder::Pos => 3,
            SortOrder::Ops => 4,
            SortOrder::Osp => 5,
        }
    }

    /// Depth-0 `CltjTrieIter` for the given ordering.
    pub fn trie_iter(&self, ord: SortOrder) -> Box<dyn TrieIterator> {
        self.tries[Self::idx(ord)].iter_d0()
    }

    /// Zero-allocation cardinality probe: navigate `ord`'s trie through
    /// `bound_vals` (in trie-column order) and return the number of distinct
    /// values remaining at the resulting depth — the same value
    /// `join_scan(...).remaining_count()` would report, but without
    /// constructing any `Box<dyn TrieIterator>`.
    ///
    /// See [`CltjTrie::real_count_no_alloc`] and adaptive VEO's usage in
    /// `crates/nova-query/src/lftj.rs`'s `veo_sort`.
    pub fn real_count_no_alloc(&self, ord: SortOrder, bound_vals: &[u64]) -> u64 {
        self.tries[Self::idx(ord)].real_count_no_alloc(bound_vals)
    }

    /// Number of distinct global values for the given SPO field (0=S, 1=P, 2=O).
    ///
    /// Uses the SPO trie's vocabulary arrays, which contain all distinct values
    /// at each depth.  Returns a conservative upper bound on the cardinality of
    /// any scan targeting that field — used by adaptive VEO estimation in LFTJ.
    pub fn vocab_size_for_field(&self, field: usize) -> usize {
        // SPO trie: vocab[0]=subjects, vocab[1]=predicates, vocab[2]=objects.
        self.tries[Self::idx(SortOrder::Spo)].vocab_len(field)
    }

    /// Real allocated byte size of all six LOUDS tries plus the vocab arrays,
    /// for memory-breakdown diagnostics.
    ///
    /// Vocab arrays (`orig_s`/`orig_p`/`orig_o`) are `Arc`-shared across all
    /// six tries (18 references but only 3 unique backing allocations) — this
    /// method dedupes by `Arc` pointer identity so the returned total reflects
    /// real heap usage, not an 18x-inflated naive sum.
    pub fn mem_size_bytes(&self) -> usize {
        use std::collections::HashSet;

        // Sum each trie's LOUDS-only bytes (never shared — unique per trie).
        let louds_total: usize = self.tries.iter().map(|t| t.louds_mem_size_bytes()).sum();

        // Dedup vocab Arcs by pointer identity, then sum each unique vocab's
        // real byte size once.
        let mut seen: HashSet<*const V> = HashSet::new();
        let mut vocab_total = 0usize;
        for trie in &self.tries {
            for (ptr, len) in trie
                .vocab_arc_ptrs()
                .into_iter()
                .zip(trie.vocab.iter().map(|v| AsRef::<[u64]>::as_ref(&**v).len()))
            {

                if seen.insert(ptr) {
                    vocab_total += std::mem::size_of::<V>() + len * std::mem::size_of::<u64>();
                }
            }
        }

        louds_total + vocab_total
    }



    /// Per-ordering memory breakdown.
    ///
    /// Returns one `(SortOrder, LoudsMemBreakdown, vocab_bytes_undeduped)` tuple
    /// per of the six tries, in the fixed order SPO/SOP/PSO/POS/OPS/OSP.  The
    /// vocab figure is **not** deduped (each trie's three vocab arrays are
    /// counted in full even though they are `Arc`-shared with other tries) —
    /// this is intentional, so callers can see each ordering's "full" memory
    /// footprint as well as the deduped grand total from [`Self::mem_size_bytes`].
    pub fn mem_breakdown_per_ordering(&self) -> [(SortOrder, LoudsMemBreakdown, usize); 6] {
        let orders = [
            SortOrder::Spo,
            SortOrder::Sop,
            SortOrder::Pso,
            SortOrder::Pos,
            SortOrder::Ops,
            SortOrder::Osp,
        ];
        std::array::from_fn(|i| {
            let trie = &self.tries[i];
            (
                orders[i],
                trie.mem_breakdown(),
                trie.vocab_bytes_undeduped(),
            )
        })
    }

    /// Deduped total vocab bytes across all six tries' shared `Arc<V>`
    /// arrays (3 unique allocations, regardless of the 18 references).
    pub fn vocab_bytes_deduped(&self) -> usize {
        use std::collections::HashSet;
        let mut seen: HashSet<*const V> = HashSet::new();
        let mut total = 0usize;
        for trie in &self.tries {
            for (ptr, arc) in trie.vocab_arc_ptrs().into_iter().zip(trie.vocab.iter()) {
                if seen.insert(ptr) {
                    total += std::mem::size_of::<V>()
                        + AsRef::<[u64]>::as_ref(&**arc).len() * std::mem::size_of::<u64>();
                }
            }

        }
        total
    }


}

impl CltjData {
    /// Consume this `CltjData`, producing the ε-serde-serializable
    /// [`CltjSnapshot`] (3 deduped vocab arrays + 6 [`LoudsCore`]s).
    ///
    /// Used by the persistent snapshot format.  Requires that no other
    /// `Arc<CltjTrie>` clones exist
    /// (true for a freshly built `CltjData` per the "always mapped" design —
    /// see `RingStore::compact`); panics otherwise via `expect`.
    ///
    /// The three vocab arrays are read from the SPO trie (index 0, whose
    /// `vocab` is `[orig_s, orig_p, orig_o]` — see [`build_cltj_data`]) before
    /// any trie is consumed, since all six tries share the same three `Arc`
    /// allocations.
    ///
    /// Only defined for the owned `Vec<u64>` form (`V`'s default) — a fresh
    /// build always starts from owned vocab.
    pub(crate) fn into_snapshot(self) -> CltjSnapshot {
        // Extract the three shared vocab arrays via the SPO trie (index 0)
        // before consuming any trie (all six share the same three Arcs).
        let vocab_s = (*self.tries[0].vocab[0]).clone();
        let vocab_p = (*self.tries[0].vocab[1]).clone();
        let vocab_o = (*self.tries[0].vocab[2]).clone();

        let tries = self.tries.map(|trie| {
            Arc::try_unwrap(trie)
                .unwrap_or_else(|_| panic!("into_snapshot: CltjTrie Arc is shared"))
                .into_core()
        });

        CltjSnapshot {
            vocab_s,
            vocab_p,
            vocab_o,
            tries,
        }
    }

    /// Reconstruct a `CltjData` from a [`CltjSnapshot`] loaded from disk (or
    /// from an in-memory round-trip buffer using the "always mapped" design).
    ///
    /// Rebuilds each `CltjTrie`'s sidecar via [`LoudsTrie::from_core`] and
    /// redistributes the three vocab arrays across the six tries per the
    /// static ordering permutation documented in [`build_cltj_data`] (18
    /// references, 3 unique `Arc` allocations — matching the runtime
    /// hot-path layout exactly).
    pub(crate) fn from_snapshot(snap: CltjSnapshot) -> CltjData {

        let orig_s = Arc::new(snap.vocab_s);
        let orig_p = Arc::new(snap.vocab_p);
        let orig_o = Arc::new(snap.vocab_o);

        // Static per-ordering vocab assignment — must exactly match
        // `build_cltj_data`'s vocab_primary/vocab_secondary arguments.
        let vocabs: [[Arc<Vec<u64>>; 3]; 6] = [
            [
                Arc::clone(&orig_s),
                Arc::clone(&orig_p),
                Arc::clone(&orig_o),
            ], // SPO
            [
                Arc::clone(&orig_s),
                Arc::clone(&orig_o),
                Arc::clone(&orig_p),
            ], // SOP
            [
                Arc::clone(&orig_p),
                Arc::clone(&orig_s),
                Arc::clone(&orig_o),
            ], // PSO
            [
                Arc::clone(&orig_p),
                Arc::clone(&orig_o),
                Arc::clone(&orig_s),
            ], // POS
            [
                Arc::clone(&orig_o),
                Arc::clone(&orig_p),
                Arc::clone(&orig_s),
            ], // OPS
            [
                Arc::clone(&orig_o),
                Arc::clone(&orig_s),
                Arc::clone(&orig_p),
            ], // OSP
        ];

        let mut cores = snap.tries.into_iter();
        let tries: [Arc<CltjTrie>; 6] = vocabs.map(|vocab| {
            let core = cores.next().expect("CltjSnapshot always has 6 tries");
            Arc::new(CltjTrie {
                louds: LoudsTrie::from_core(core),
                vocab,
            })
        });

        CltjData { tries }
    }
}

/// ε-serde-serializable snapshot of a [`CltjData`]: three deduped vocab
/// arrays (plain `Vec<u64>`, not `Arc`-wrapped — `Arc` is not directly
/// epserde-serializable) plus six [`LoudsCore`]s (T+L only, sidecar
/// excluded), one per [`SortOrder`] in the fixed order
/// SPO/SOP/PSO/POS/OPS/OSP.
///
/// This is the persistent on-disk representation.  Loading redistributes
/// the three vocab
/// arrays across the six tries per the static ordering permutation in
/// [`build_cltj_data`], and rebuilds each trie's sidecar via
/// [`LoudsTrie::from_core`] — see [`CltjData::from_snapshot`].
// `tries` uses the "whole array as bare generic parameter" pattern: epserde's
// zero-copy eligibility check only recognizes a field as zero-copy when its
// declared type is a bare generic parameter of the struct, and arrays are not
// themselves recognized as zero-copy-eligible unless the *entire array type*
// is substituted in as the generic parameter's default.  This mirrors the
// pattern used for `TBitvec`/`SidecarCore`/`LoudsCore` in `louds.rs`.
#[derive(Epserde)]
pub(crate) struct CltjSnapshot<
    VocabS = Vec<u64>,
    VocabP = Vec<u64>,
    VocabO = Vec<u64>,
    Tries = [LoudsCore; 6],
> {
    vocab_s: VocabS,
    vocab_p: VocabP,
    vocab_o: VocabO,
    tries: Tries,
}

// ── Pair builder ──────────────────────────────────────────────────────────────

/// Build two LOUDS tries that share the same depth-0 (c0) grouping.
///
/// ## Arguments
///
/// * `primary_sorted` — triples `[c0, c1, c2]` already sorted by `(c0, c1, c2)`.
///   Used directly for the *primary* trie (no re-sort).
/// * `vocab_primary` — `[d0, d1, d2]` global-ID vocab for the primary ordering.
/// * `vocab_secondary` — `[d0, d1, d2]` vocab for the secondary ordering,
///   where d1 and d2 are swapped relative to the primary (e.g. SPO→SOP means
///   d1=O, d2=P).
///
/// ## Secondary sort strategy
///
/// The secondary trie indexes `[c0, c2, c1]` (c1 and c2 swapped).  Rather than
/// re-sorting the entire N-triple array, we reuse the depth-0 group boundaries
/// already implied by `primary_sorted` and sort only within each c0 group.
/// Each group sort is `O(g · log g)` where `g` is the group size; summed over
/// all groups this is `O(N log(N/G))` — cheaper than a full `O(N log N)` sort
/// by a constant factor proportional to the depth-0 branching factor.
fn build_pair_shared_c0(
    primary_sorted: &[[u32; 3]],
    vocab_primary: [Arc<Vec<u64>>; 3],
    vocab_secondary: [Arc<Vec<u64>>; 3],
) -> (Arc<CltjTrie>, Arc<CltjTrie>) {
    let empty_louds = || LoudsTrie::from_raw(&[], &[]);

    // ── Primary trie: input is already sorted by (c0, c1, c2) ────────────────
    let primary_louds = build_louds_from_sorted(primary_sorted).unwrap_or_else(empty_louds);

    // ── Secondary trie: [c0, c2, c1] sorted by (c0, c2, c1) ─────────────────
    //
    // Process each c0 group from the primary, swap c1↔c2, and sort within the
    // group.  The overall output vector is sorted by (c0, c2, c1) because c0
    // values are non-decreasing and within-group entries are sorted after swap.
    let mut secondary: Vec<[u32; 3]> = Vec::with_capacity(primary_sorted.len());
    let mut i = 0;
    while i < primary_sorted.len() {
        let c0 = primary_sorted[i][0];
        let end = i + primary_sorted[i..].partition_point(|t| t[0] == c0);
        let group_start = secondary.len();
        // Emit [c0, c2, c1] for every triple in this c0 group.
        for triple in &primary_sorted[i..end] {
            secondary.push([triple[0], triple[2], triple[1]]);
        }
        // Sort within the group by (c2, c1); c0 is constant, so sort_unstable
        // compares (c0, c2, c1) lexicographically — effectively (c2, c1).
        secondary[group_start..].sort_unstable();
        i = end;
    }
    // Dedup is safe here: secondary is sorted by (c0, c2, c1) overall because
    // c0 groups are emitted in non-decreasing c0 order, and each group is sorted.
    // Duplicates can only arise if c1 == c2 for some input triple (rare in RDF).
    secondary.dedup();
    let secondary_louds = build_louds_from_sorted(&secondary).unwrap_or_else(empty_louds);

    (
        Arc::new(CltjTrie {
            louds: primary_louds,
            vocab: vocab_primary,
        }),
        Arc::new(CltjTrie {
            louds: secondary_louds,
            vocab: vocab_secondary,
        }),
    )
}

// ── build_cltj_data ───────────────────────────────────────────────────────────

/// Build a [`CltjData`] from SPO-sorted global-ID triples.
///
/// ## Build strategy
///
/// Uses paired construction to minimise sort work:
///
/// | Pair   | Full sorts | Within-group sorts |
/// |--------|------------|--------------------|
/// | S-pair | 0 (SPO input is pre-sorted) | 1 (SOP: per-S sort of O↔P) |
/// | P-pair | 1 (PSO)    | 1 (POS: per-P sort of O↔S) |
/// | O-pair | 1 (OPS)    | 1 (OSP: per-O sort of S↔P) |
///
/// Total: **2 full sorts** instead of 6 — approximately a 25% reduction in build time.
pub fn build_cltj_data(
    spo_sorted: &[[u64; 3]],
    map_s: &std::collections::HashMap<u64, usize>,
    map_p: &std::collections::HashMap<u64, usize>,
    map_o: &std::collections::HashMap<u64, usize>,
    orig_s: Arc<Vec<u64>>,
    orig_p: Arc<Vec<u64>>,
    orig_o: Arc<Vec<u64>>,
) -> CltjData {
    // ── Map global IDs → compact local IDs in SPO order ───────────────────────
    //
    // `spo_sorted` is already sorted by (S, P, O) and deduplicated.
    // The resulting `ls_lp_lo` is therefore sorted by (ls, lp, lo) — i.e. in
    // SPO local-ID order — with no extra sort required for the S-pair primary.
    let ls_lp_lo: Vec<[u32; 3]> = spo_sorted
        .iter()
        .map(|&[s, p, o]| [map_s[&s] as u32, map_p[&p] as u32, map_o[&o] as u32])
        .collect();

    // ── S-pair: SPO (primary, free) + SOP (secondary, within-group) ──────────
    //
    // `ls_lp_lo` is the SPO-sorted local-ID array.  The SOP secondary swaps
    // depth-1 and depth-2 (P↔O) and re-sorts within each S group.
    let (spo_trie, sop_trie) = build_pair_shared_c0(
        &ls_lp_lo,
        [
            Arc::clone(&orig_s),
            Arc::clone(&orig_p),
            Arc::clone(&orig_o),
        ], // SPO: d0=S, d1=P, d2=O
        [
            Arc::clone(&orig_s),
            Arc::clone(&orig_o),
            Arc::clone(&orig_p),
        ], // SOP: d0=S, d1=O, d2=P
    );

    // ── P-pair: PSO (primary, 1 full sort) + POS (secondary, within-group) ───
    //
    // Remap columns to (lp, ls, lo) and sort to get PSO order.
    // POS is then built with within-P-group re-sorts of O↔S.
    let mut pso_sorted: Vec<[u32; 3]> = ls_lp_lo.iter().map(|&[ls, lp, lo]| [lp, ls, lo]).collect();
    pso_sorted.sort_unstable();
    pso_sorted.dedup();

    let (pso_trie, pos_trie) = build_pair_shared_c0(
        &pso_sorted,
        [
            Arc::clone(&orig_p),
            Arc::clone(&orig_s),
            Arc::clone(&orig_o),
        ], // PSO: d0=P, d1=S, d2=O
        [
            Arc::clone(&orig_p),
            Arc::clone(&orig_o),
            Arc::clone(&orig_s),
        ], // POS: d0=P, d1=O, d2=S
    );

    // ── O-pair: OPS (primary, 1 full sort) + OSP (secondary, within-group) ───
    //
    // Remap columns to (lo, lp, ls) and sort to get OPS order.
    // OSP is built with within-O-group re-sorts of S↔P.
    let mut ops_sorted: Vec<[u32; 3]> = ls_lp_lo.iter().map(|&[ls, lp, lo]| [lo, lp, ls]).collect();
    ops_sorted.sort_unstable();
    ops_sorted.dedup();

    let (ops_trie, osp_trie) = build_pair_shared_c0(
        &ops_sorted,
        [
            Arc::clone(&orig_o),
            Arc::clone(&orig_p),
            Arc::clone(&orig_s),
        ], // OPS: d0=O, d1=P, d2=S
        [
            Arc::clone(&orig_o),
            Arc::clone(&orig_s),
            Arc::clone(&orig_p),
        ], // OSP: d0=O, d1=S, d2=P
    );

    CltjData {
        tries: [spo_trie, sop_trie, pso_trie, pos_trie, ops_trie, osp_trie],
    }
}

// ── CltjTrieIter ──────────────────────────────────────────────────────────────

/// A [`TrieIterator`] backed by one LOUDS trie in [`CltjData`].
///
/// State: the current level is an inclusive label-position range `[lo, hi]`
/// within L.  The current position is `pos ∈ [lo, hi]`.  Exhausted when
/// `pos > hi`.
///
/// Navigation is O(1) per step (LOUDS rank/select), with O(log ℓ) seek via
/// exponential search — replacing the WaveletMatrix Ring's O(log σ) per step.
pub struct CltjTrieIter<V = Vec<u64>> {
    /// The parent trie (shared between parent and child iterators).
    trie: Arc<CltjTrie<V>>,
    /// Inclusive upper bound of the current node's children label-range in L.
    hi: usize,
    /// Current position within [lo, hi].  `pos > hi` means exhausted.
    pos: usize,
    /// Depth: 0 = primary field, 1 = secondary field, 2 = leaf field.
    depth: u8,
}

impl<V: AsRef<[u64]> + Send + Sync + 'static> TrieIterator for CltjTrieIter<V> {
    /// Return the global term ID at the current position.
    ///
    /// O(1): direct Vec index (local_id from LOUDS L, then vocab lookup).
    #[inline]
    fn key(&self) -> u64 {
        let local_id = self.trie.louds.label_at(self.pos) as usize;
        self.trie.vocab_slice(self.depth as usize)[local_id]
    }


    /// Advance to the first position where `key() >= target`.
    ///
    /// Uses `partition_point` on the vocab `&[u64]` slice for the local-ID
    /// binary search — the compiler can SIMD-vectorise this over contiguous
    /// memory.  Then `leap()` jumps to the matching L position.
    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        if target <= self.key() {
            return;
        }

        let vocab = self.trie.vocab_slice(self.depth as usize);
        // Binary search for first local_id where vocab[local_id] >= target.

        let local_target = vocab.partition_point(|&v| v < target);
        if local_target >= vocab.len() {
            // All vocab values are below target — exhaust the iterator.
            self.pos = self.hi + 1;
            return;
        }
        self.pos = self.trie.louds.leap(self.pos, self.hi, local_target as u32);
    }

    fn advance(&mut self) {
        if !self.at_end() {
            self.pos += 1;
        }
    }

    fn open(&self) -> Box<dyn TrieIterator> {
        if self.depth >= 2 {
            // Depth 2 = leaf level; no further descent.
            return Box::new(EmptyTrieIter);
        }
        // child T-position = select1(pos − 1)
        let child_v = self.trie.louds.child_from_label_pos(self.pos);
        let child_degree = self.trie.louds.degree(child_v);
        if child_degree == 0 {
            return Box::new(EmptyTrieIter);
        }
        let new_lo = child_v + 1;
        let new_hi = child_v + child_degree;
        Box::new(CltjTrieIter {
            trie: Arc::clone(&self.trie),
            hi: new_hi,
            pos: new_lo,
            depth: self.depth + 1,
        })
    }

    fn at_end(&self) -> bool {
        self.pos > self.hi
    }
}


// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a CltjData from a small set of global-ID triples and verify
    /// basic TrieIterator behaviour on the SPO ordering.
    fn make_cltj(triples: &[[u64; 3]]) -> CltjData {
        let mut all = triples.to_vec();
        all.sort_unstable();
        all.dedup();

        let mut orig_s: Vec<u64> = all.iter().map(|t| t[0]).collect();
        orig_s.sort_unstable();
        orig_s.dedup();
        let mut orig_p: Vec<u64> = all.iter().map(|t| t[1]).collect();
        orig_p.sort_unstable();
        orig_p.dedup();
        let mut orig_o: Vec<u64> = all.iter().map(|t| t[2]).collect();
        orig_o.sort_unstable();
        orig_o.dedup();

        let map_s: HashMap<u64, usize> = orig_s.iter().enumerate().map(|(i, &v)| (v, i)).collect();
        let map_p: HashMap<u64, usize> = orig_p.iter().enumerate().map(|(i, &v)| (v, i)).collect();
        let map_o: HashMap<u64, usize> = orig_o.iter().enumerate().map(|(i, &v)| (v, i)).collect();

        build_cltj_data(
            &all,
            &map_s,
            &map_p,
            &map_o,
            Arc::new(orig_s),
            Arc::new(orig_p),
            Arc::new(orig_o),
        )
    }

    #[test]
    fn cltj_spo_depth0_advance() {
        let cltj = make_cltj(&[[10, 20, 30], [10, 20, 40], [20, 30, 50]]);
        let mut it = cltj.trie_iter(SortOrder::Spo);
        assert!(!it.at_end());
        assert_eq!(it.key(), 10);
        it.advance();
        assert_eq!(it.key(), 20);
        it.advance();
        assert!(it.at_end());
    }

    #[test]
    fn cltj_spo_depth0_seek() {
        let cltj = make_cltj(&[[1, 2, 3], [3, 4, 5], [7, 8, 9]]);
        let mut it = cltj.trie_iter(SortOrder::Spo);
        it.seek(4);
        assert_eq!(it.key(), 7);
        it.seek(100);
        assert!(it.at_end());
    }

    #[test]
    fn cltj_spo_open_depth1() {
        let cltj = make_cltj(&[[1, 2, 10], [1, 3, 11], [2, 5, 12]]);
        let it0 = cltj.trie_iter(SortOrder::Spo);
        assert_eq!(it0.key(), 1);

        let mut it1 = it0.open();
        assert_eq!(it1.key(), 2); // first P for S=1
        it1.advance();
        assert_eq!(it1.key(), 3); // second P for S=1
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn cltj_spo_open_depth2() {
        let cltj = make_cltj(&[[1, 2, 10], [1, 2, 20], [1, 3, 30]]);
        let it0 = cltj.trie_iter(SortOrder::Spo);
        assert_eq!(it0.key(), 1);
        let it1 = it0.open();
        assert_eq!(it1.key(), 2); // P=2
        let mut it2 = it1.open();
        assert_eq!(it2.key(), 10);
        it2.advance();
        assert_eq!(it2.key(), 20);
        it2.advance();
        assert!(it2.at_end());
    }

    #[test]
    fn cltj_sop_ordering() {
        // SOP: primary=S, secondary=O, leaf=P
        let cltj = make_cltj(&[[1, 2, 10], [1, 3, 20], [1, 4, 10], [2, 2, 30]]);
        let it0 = cltj.trie_iter(SortOrder::Sop);
        assert_eq!(it0.key(), 1); // subject 1

        let mut it1 = it0.open(); // distinct O values for S=1: {10, 20}
        assert_eq!(it1.key(), 10);
        it1.advance();
        assert_eq!(it1.key(), 20);
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn cltj_pso_ordering() {
        // PSO: primary=P, secondary=S
        let cltj = make_cltj(&[[1, 5, 10], [2, 5, 20], [3, 5, 30], [1, 6, 40]]);
        let it0 = cltj.trie_iter(SortOrder::Pso);
        assert_eq!(it0.key(), 5); // first predicate

        let mut it1 = it0.open(); // distinct S values for P=5: {1, 2, 3}
        assert_eq!(it1.key(), 1);
        it1.advance();
        assert_eq!(it1.key(), 2);
        it1.advance();
        assert_eq!(it1.key(), 3);
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn cltj_seek_on_secondary() {
        // SOP: seek on secondary (O values)
        let cltj = make_cltj(&[[1, 2, 10], [1, 3, 20], [1, 4, 30], [1, 5, 40]]);
        let it0 = cltj.trie_iter(SortOrder::Sop);
        assert_eq!(it0.key(), 1);
        let mut it1 = it0.open(); // O values for S=1: {10, 20, 30, 40}
        it1.seek(25); // should land on 30
        assert_eq!(it1.key(), 30);
        it1.seek(50);
        assert!(it1.at_end());
    }

    #[test]
    fn cltj_join_scan_sop() {
        // Simulate join_scan: S=1 bound, target=O
        // Using CltjData::trie_iter(SOP) and descending to depth 1
        let cltj = make_cltj(&[[1, 2, 10], [1, 3, 20], [2, 2, 30]]);
        let mut it = cltj.trie_iter(SortOrder::Sop);
        // S=1 is the first (and only) S → seek to S=1
        it.seek(1);
        assert_eq!(it.key(), 1);
        let mut it1 = it.open(); // O values for S=1: {10, 20}
        assert_eq!(it1.key(), 10);
        it1.advance();
        assert_eq!(it1.key(), 20);
        it1.advance();
        assert!(it1.at_end());
    }

    #[test]
    fn cltj_empty_trie() {
        let cltj = make_cltj(&[]);
        let it = cltj.trie_iter(SortOrder::Spo);
        assert!(it.at_end());
    }

    #[test]
    fn cltj_all_six_orderings_non_empty() {
        let cltj = make_cltj(&[[1, 2, 3], [1, 2, 4], [1, 3, 5], [2, 2, 6]]);
        for ord in [
            SortOrder::Spo,
            SortOrder::Sop,
            SortOrder::Pso,
            SortOrder::Pos,
            SortOrder::Ops,
            SortOrder::Osp,
        ] {
            let it = cltj.trie_iter(ord);
            assert!(!it.at_end(), "ordering {:?} should not be empty", ord);
        }
    }

    /// Verify PSO and POS orderings via the shared-pair builder.
    #[test]
    fn cltj_pos_ordering() {
        // POS: primary=P, secondary=O, leaf=S
        let cltj = make_cltj(&[[1, 5, 10], [2, 5, 20], [3, 5, 30], [4, 6, 10]]);
        let it0 = cltj.trie_iter(SortOrder::Pos);
        assert_eq!(it0.key(), 5); // first predicate

        let mut it1 = it0.open(); // distinct O values for P=5: {10, 20, 30}
        assert_eq!(it1.key(), 10);
        it1.advance();
        assert_eq!(it1.key(), 20);
        it1.advance();
        assert_eq!(it1.key(), 30);
        it1.advance();
        assert!(it1.at_end());
    }

    /// Verify OPS and OSP orderings via the shared-pair builder.
    #[test]
    fn cltj_osp_ordering() {
        // OSP: primary=O, secondary=S, leaf=P
        let cltj = make_cltj(&[[1, 2, 100], [3, 4, 100], [5, 6, 200]]);
        let it0 = cltj.trie_iter(SortOrder::Osp);
        assert_eq!(it0.key(), 100); // first object

        let mut it1 = it0.open(); // distinct S values for O=100: {1, 3}
        assert_eq!(it1.key(), 1);
        it1.advance();
        assert_eq!(it1.key(), 3);
        it1.advance();
        assert!(it1.at_end());
    }

    /// Phase 3.1b probe: real `load_mmap` round-trip of a `CltjSnapshot` to a
    /// temp file, confirming that epserde's zero-copy `DeserType` form
    /// actually materializes (i.e. `load_mmap` succeeds and the resulting
    /// `MemCase`'s `.uncase()` produces a navigable borrowed structure).
    ///
    /// This does NOT yet wire the borrowed form into `CltjTrie`/`CltjData` —
    /// it is a standalone feasibility probe informing the Phase 3.1 design
    /// (see CLAUDE.md item 14, Phase 3).
    #[test]
    fn cltj_snapshot_load_mmap_probe() {
        use epserde::deser::{Deserialize, Flags};
        use epserde::ser::Serialize;

        let triples: &[[u64; 3]] = &[[1, 2, 3], [1, 2, 4], [1, 3, 5], [2, 2, 6]];
        let original = make_cltj(triples);
        let snap = original.into_snapshot();

        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let path =
            std::env::temp_dir().join(format!("nova_cltj_mmap_probe_{pid}_{n}.snap"));
        let _ = std::fs::remove_file(&path);

        {
            let mut f = std::fs::File::create(&path).expect("create temp file");
            unsafe {
                snap.serialize(&mut f).expect("serialize CltjSnapshot");
            }
        }

        // Zero-copy mmap load: `mem_case` owns the mmap'd backing memory;
        // `.uncase()` yields a borrowed `DeserType<CltjSnapshot>` tied to
        // `mem_case`'s lifetime.
        let mem_case = unsafe {
            <CltjSnapshot>::load_mmap(&path, Flags::empty()).expect("load_mmap CltjSnapshot")
        };
        let view: &epserde::deser::DeserType<CltjSnapshot> = mem_case.uncase();



        // vocab_s/vocab_p/vocab_o should be borrowed `&[u64]` slices (zero-copy),
        // not freshly-allocated owned `Vec<u64>` copies.
        assert!(!view.vocab_s.is_empty());
        assert!(!view.vocab_p.is_empty());
        assert!(!view.vocab_o.is_empty());
        // 6 tries, one per SortOrder.
        assert_eq!(view.tries.len(), 6);

        let _ = std::fs::remove_file(&path);
    }

    /// End-to-end round-trip of the persistent snapshot format:
    /// build a `CltjData`, convert to `CltjSnapshot`, serialize via ε-serde
    /// into an in-memory buffer, deserialize back, reconstruct a `CltjData`
    /// via `from_snapshot`, and verify identical `TrieIterator` behaviour
    /// across all six orderings compared to the original.
    #[test]
    fn cltj_snapshot_round_trip() {

        use epserde::deser::Deserialize;
        use epserde::ser::Serialize;

        let triples: &[[u64; 3]] = &[
            [1, 2, 3],
            [1, 2, 4],
            [1, 3, 5],
            [2, 2, 6],
            [2, 5, 7],
            [3, 5, 7],
        ];
        let original = make_cltj(triples);

        // Collect full depth-3 traversal results (as (a,b,c) triples) for
        // every ordering from the original, before consuming it.
        fn collect_all(data: &CltjData, ord: SortOrder) -> Vec<(u64, u64, u64)> {
            let mut out = Vec::new();
            let mut it0 = data.trie_iter(ord);
            while !it0.at_end() {
                let a = it0.key();
                let mut it1 = it0.open();
                while !it1.at_end() {
                    let b = it1.key();
                    let mut it2 = it1.open();
                    while !it2.at_end() {
                        let c = it2.key();
                        out.push((a, b, c));
                        it2.advance();
                    }
                    it1.advance();
                }
                it0.advance();
            }
            out
        }

        let orders = [
            SortOrder::Spo,
            SortOrder::Sop,
            SortOrder::Pso,
            SortOrder::Pos,
            SortOrder::Ops,
            SortOrder::Osp,
        ];

        let expected: Vec<Vec<(u64, u64, u64)>> = orders
            .iter()
            .map(|&ord| collect_all(&original, ord))
            .collect();

        // Snapshot + ε-serde round-trip via an in-memory buffer.
        let snap = original.into_snapshot();
        let mut buf: Vec<u8> = Vec::new();
        unsafe {
            snap.serialize(&mut buf).expect("serialize CltjSnapshot");
        }
        let mut cursor = std::io::Cursor::new(&buf[..]);
        let snap2 = unsafe {
            CltjSnapshot::deserialize_full(&mut cursor).expect("deserialize CltjSnapshot")
        };
        let reconstructed = CltjData::from_snapshot(snap2);

        for (i, &ord) in orders.iter().enumerate() {
            let got = collect_all(&reconstructed, ord);
            assert_eq!(
                got, expected[i],
                "ordering {:?} mismatch after snapshot round-trip",
                ord
            );
        }
    }

    /// `VocabRepr::Owned` behaves exactly like a plain `Vec<u64>` through
    /// `AsRef<[u64]>`.
    #[test]
    fn vocab_repr_owned_as_ref() {
        let v = VocabRepr::Owned(vec![1, 2, 3]);
        assert_eq!(v.as_ref(), &[1u64, 2, 3]);
    }

    /// `VocabRepr::Mapped` (the zero-copy/mmap'd variant) returns the same
    /// slice contents via `AsRef<[u64]>` as the owned variant — proving the
    /// two variants are interchangeable for every navigation method that is
    /// generic over `V: AsRef<[u64]>` (all of `CltjTrie`/`CltjData`'s
    /// read-only methods). Uses `Box::leak` to simulate the lifetime
    /// extension that will, in Phase 3.3's full wiring, come from an
    /// `Arc<epserde::deser::MemCase<StoreSnapshot>>` kept alive by the store.
    #[test]
    fn vocab_repr_mapped_as_ref() {
        let boxed: Box<[u64]> = vec![10u64, 20, 30].into_boxed_slice();
        let leaked: &'static [u64] = Box::leak(boxed);
        let v = VocabRepr::Mapped(leaked);
        assert_eq!(v.as_ref(), &[10u64, 20, 30]);
    }

    /// `VocabRepr` must be `Send + Sync + 'static` since it plugs directly
    /// into `CltjTrie<V>`/`CltjData<V>`'s generic bound
    /// (`V: AsRef<[u64]> + Send + Sync + 'static`) used throughout
    /// `iter_d0`/`TrieIterator` — this is a compile-time-only assertion.
    #[test]
    fn vocab_repr_is_send_sync_static() {
        fn assert_bounds<T: AsRef<[u64]> + Send + Sync + 'static>() {}
        assert_bounds::<VocabRepr>();
    }

    /// Cross-check: SPO and SOP each enumerate the same number of leaf triples.
    ///
    /// Uses only the public `TrieIterator` API (no internal field access).
    #[test]
    fn cltj_spo_sop_triple_count_matches() {

        let triples = &[[1u64, 2, 3], [1, 3, 4], [2, 2, 5], [2, 4, 3]];
        let cltj = make_cltj(triples);

        // Full depth-3 traversal via SPO.
        let mut spo_count = 0usize;
        let mut it0 = cltj.trie_iter(SortOrder::Spo);
        while !it0.at_end() {
            let mut it1 = it0.open();
            while !it1.at_end() {
                let mut it2 = it1.open();
                while !it2.at_end() {
                    spo_count += 1;
                    it2.advance();
                }
                it1.advance();
            }
            it0.advance();
        }
        assert_eq!(
            spo_count,
            triples.len(),
            "SPO traversal should see all triples"
        );

        // Full depth-3 traversal via SOP — must yield same count.
        let mut sop_count = 0usize;
        let mut it0 = cltj.trie_iter(SortOrder::Sop);
        while !it0.at_end() {
            let mut it1 = it0.open();
            while !it1.at_end() {
                let mut it2 = it1.open();
                while !it2.at_end() {
                    sop_count += 1;
                    it2.advance();
                }
                it1.advance();
            }
            it0.advance();
        }
        assert_eq!(
            sop_count,
            triples.len(),
            "SOP traversal should see all triples"
        );
    }
}
