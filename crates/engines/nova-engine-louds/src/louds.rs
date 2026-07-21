//! LOUDS (Level-Order Unary Degree Sequence) height-3 trie.
//!
//! Reference: Arroyuelo, Navarro, Gómez-Brandón et al.
//! "CompactLTJ: Space and Time Efficient Leapfrog Triejoin on Graph Databases"
//! VLDB Journal, 2025.  §2.2 "Compact Tries".
//!
//! ## Encoding
//!
//! Each internal node with *d* children is encoded as `0^(d-1) 1` (*d* bits).
//! Leaf nodes (depth 3) are **omitted** from T entirely.
//! The label array L stores one entry per trie edge: for node v, L[v+i] is the
//! i-th child's label (1-indexed i in `1..=degree(v)`).
//!
//! ## Index offset convention
//!
//! The paper uses 1-indexed positions.  This implementation prepends a dummy
//! `false` bit at T\[0\] and a dummy `0` at L\[0\], so the virtual root
//! has identifier v = 0.  Paper formulas translate verbatim:
//!
//! ```text
//! child(v, i)  = select1(v + i − 1)     [select1 is 0-indexed]
//! degree(v)    = selectnext1(v + 1) − v  [selectnext1(k) = first 1 at pos ≥ k]
//! access(v, i) = L[v + i]
//! leap([lo,hi], c) = first k in [lo,hi] with L[k] ≥ c  (exponential search)
//! ```
//!
//! ## L label array: per-depth `BitFieldVec` (sux)
//!
//! Labels are stored in three `sux::bits::BitFieldVec` regions (depth 0 / 1 / 2),
//! each packed at ⌈log₂(max_label_at_depth + 1)⌉.  Global LOUDS positions are
//! unchanged: dummy at 0 lives in the depth-0 region; depth-1 and depth-2 labels
//! follow contiguously.  `label_at` / `leap` keep the same `[lo, hi]` contracts
//! and stay O(1) per access.  Independent per-depth widths avoid paying the
//! largest column alphabet on shallower depths (e.g. a small predicate alphabet
//! at depth 2 when objects force a wide bit-width).
//!
//! ## T bitvector substrate
//!
//! The LOUDS `T` bitvector is **select1-dominant** — every `child()` and
//! `degree()` call issues one or two `select1` operations.
//!
//! Backend: `sux 0.14` Rank9 + SelectAdapt (benchmarked winner, −10% vs a
//! `sucds::Rank9Sel` fallback that was removed once benchmarking confirmed
//! sux was faster; a single T backend is also a prerequisite for an
//! unambiguous ε-serde/mmap snapshot format).
//!
//! The `T` substrate is fully encapsulated in the `t_backend` module below.
//! All paper navigation formulas and all unit tests are substrate-independent.
//!
//! ## Leap on bit-packed L
//!
//! `leap()` is a lower-bound / successor search over a node's monotone child
//! labels in bit-packed L: exponential search then binary search with
//! `label_at` (BitFieldVec `index_value`).
//!
//! ## Construction
//!
//! [`build_louds_from_sorted`] accepts triples sorted by `(col0, col1, col2)`
//! using compact 0-indexed local IDs and builds T and L in BFS level-order.
//! Each level is processed in one pass; no intermediate data structures beyond
//! the output vectors are required.

// sux::prelude::* brings in BitFieldVec plus all rank/select traits used by t_backend.
use epserde::Epserde;
use mem_dbg::{MemSize, SizeFlags};
use sux::prelude::*;
// SliceByValue::index_value — safe aligned accessor for BitFieldVec L reads.
use value_traits::slices::SliceByValue;

// ── T bitvector backend ───────────────────────────────────────────────────────

// The `t_backend` module exposes:
//   pub(super) struct TBitvec
//   pub(super) fn build(bits: impl Iterator<Item = bool>) -> TBitvec
//   pub(super) fn rank1(&self, k: usize) -> usize    // count of 1s in [0, k)
//   pub(super) fn select1(&self, k: usize) -> Option<usize>  // 0-indexed k-th 1
//
// sux Rank9+SelectAdapt is the sole T backend (the formerly-available
// `sucds::Rank9Sel` `--no-default-features` fallback was removed — a single
// sux-only T backend is a prerequisite for an unambiguous ε-serde/mmap
// snapshot format).
pub(crate) mod t_backend {
    //! T bitvector backend: `sux 0.14` Rank9 + SelectAdapt (Vigna).
    //!
    //! sux is the authoritative Rust implementation by Sebastiano Vigna (the
    //! original Rank9 author).  SelectAdapt avoids a binary-search-over-
    //! superblocks approach for `select` — it instead uses an anchored linear
    //! scan over a smaller inventory, which is O(1) and typically requires
    //! fewer cache misses.
    //!
    //! ## API (sux 0.14 — verified against 0.14.0)
    //!
    //! sux's Backend trait system allows **zero-cost nesting**: rank/select
    //! structures implement the same traits as their backends via `Deref`, so
    //! `SelectAdapt<AddNumBits<Rank9<B>>>` is a single structure that supports
    //! both `rank()` (via the inner `Rank9`) and `select()` (via `SelectAdapt`)
    //! from ONE bitvector backing store — no clone required.
    //!
    //! ```text
    //! use sux::prelude::*;
    //! let bv: BitVec<Vec<u64>> = iter_of_bool.collect();
    //! bv.count_ones() -> usize                           (via NumBits trait)
    //! // Nested (single backing store, zero-cost delegation via Deref):
    //! let rs: SelectAdapt<AddNumBits<Rank9<BitVec<Vec<u64>>>>> =
    //!     SelectAdapt::new(Rank9::new(bv).into());
    //!     //                              ^^^^ Rank9<B> → AddNumBits<Rank9<B>>
    //! rs.rank(pos)   // -> usize  via Deref: SelectAdapt→AddNumBits→Rank9→rank()
    //! rs.select(k)   // -> Option<usize>  0-indexed k-th 1; None if OOB
    //! ```
    //!
    //! Note: `Rank9::new(bv).into()` converts `Rank9<SuxBV>` →
    //! `AddNumBits<Rank9<SuxBV>>` (which satisfies `SelectAdapt`'s `NumBits`
    //! bound).  `SelectAdapt` then Derefs to `AddNumBits` → `Rank9`, so
    //! `rs.rank(k)` forwards to `Rank9::rank` at zero overhead.
    use epserde::Epserde;
    use mem_dbg::{MemSize, SizeFlags};
    use sux::prelude::*;

    type SuxBV = BitVec<Vec<u64>>;
    /// Single nested structure: select on top of rank9 on top of bitvec.
    /// Deref chain: SelectAdapt → AddNumBits → Rank9 → BitVec<Vec<u64>>.
    /// One backing store; no clone; both rank() and select() work on `rs`.
    pub(crate) type SuxRS = SelectAdapt<AddNumBits<Rank9<SuxBV>>>;

    // `TBitvec` is generic over its backing rank/select structure `B`,
    // defaulting to the owned `SuxRS`.  This "bare generic parameter with
    // default" pattern is what epserde's `#[derive(Epserde)]` needs in order
    // to treat the `rs` field as zero-copy-eligible: because `rs`'s declared
    // type is the bare identifier `B` (a generic parameter of this struct),
    // epserde recurses into `B`'s own `DeserType` when deserializing via
    // `load_mmap`/`deserialize_eps`, producing a borrowed (zero-copy) form
    // instead of re-allocating an owned copy.  All existing call sites are
    // unaffected because `TBitvec == TBitvec<SuxRS>` by default.
    // `Clone` is derived (bounded generically on `B: Clone` by the derive
    // macro) so that a *borrowed* `TBitvec<BorrowedT>` view produced by
    // `load_mmap` can be cheaply copied out of a `&DeserType<...>` reference
    // (copying only the slice pointer/length/`num_ones` fields — no deep data
    // copy) into an owned-by-value `TBitvec<BorrowedT>` that can then be fed
    // into the existing (owned-only-signature) `from_core`/`from_snapshot`
    // reconstruction chain. See `GraphRing::from_mapped`.
    #[derive(Epserde, Clone)]
    pub(crate) struct TBitvec<B = SuxRS> {
        rs: B,
        num_ones: usize,
    }

    impl TBitvec<SuxRS> {
        /// Construction only ever produces the owned form.
        pub(super) fn build(bits: impl Iterator<Item = bool>) -> Self {
            let bv: SuxBV = bits.collect();

            let num_ones = bv.count_ones();
            // No clone: Rank9 consumes bv; AddNumBits wraps the Rank9;
            // SelectAdapt builds its inventory on top — single allocation chain.
            let rs: SuxRS = SelectAdapt::new(Rank9::new(bv).into());
            TBitvec { rs, num_ones }
        }
    }

    impl<B> TBitvec<B> {
        /// Count of 1-bits in `[0, k)`.
        /// Deref chain: SelectAdapt → AddNumBits → Rank9::rank().
        ///
        /// Generic over `B` so this works identically whether `B` is the
        /// owned `SuxRS` or its mmap'd/borrowed `DeserType` form.
        #[inline(always)]
        pub(super) fn rank1(&self, k: usize) -> usize
        where
            B: Rank,
        {
            self.rs.rank(k)
        }

        /// Position of the `k`-th 1-bit (0-indexed).  Returns `None` if OOB.
        ///
        /// Uses `unsafe { self.rs.select_unchecked(k) }` after the bounds check —
        /// sux exposes unchecked variants of all methods; since we have already
        /// verified `k < num_ones`, the unchecked call is safe and eliminates the
        /// redundant `Option` wrap/unwrap in the LOUDS select-dominant hot path.
        #[inline(always)]
        pub(super) fn select1(&self, k: usize) -> Option<usize>
        where
            B: SelectUnchecked,
        {
            if k < self.num_ones {
                // SAFETY: k < num_ones guarantees the k-th 1-bit exists.
                // sux::SelectAdapt::select_unchecked is UB only when k >= num_ones.
                Some(unsafe { self.rs.select_unchecked(k) })
            } else {
                None
            }
        }

        /// Real allocated byte size of the T bitvector structure (bitvec +
        /// Rank9 counters + SelectAdapt inventory), for memory-breakdown
        /// diagnostics.  Uses `sux`'s derived `MemSize` (accounts for every
        /// heap allocation in the nested Deref chain).
        ///
        /// `FOLLOW_REFS` is required so that a `Borrowed*`-substrate `B`
        /// (mmap'd, holding `&'static [_]` fields internally) reports the
        /// real size of the referenced data rather than just the reference's
        /// own pointer width; it is a no-op for the owned `SuxRS` substrate.
        pub(super) fn mem_size_bytes(&self) -> usize
        where
            B: MemSize,
        {
            self.rs.mem_size(SizeFlags::FOLLOW_REFS)
        }
    }
}

// ── LoudsTrie ─────────────────────────────────────────────────────────────────

/// Per-component memory breakdown of a single [`LoudsTrie`].
/// Does not include the shared vocab arrays, which live one level up in
/// [`crate::cltj::CltjData`] (Arc-deduped across all six tries).
#[derive(Copy, Clone, Debug, Default)]
pub struct LoudsMemBreakdown {
    /// T bitvector: raw bits + Rank9 counters + SelectAdapt inventory.
    pub t_bytes: usize,
    /// L label array: bit-packed `BitFieldVec`.
    pub l_bytes: usize,
}

impl LoudsMemBreakdown {
    /// Sum of T + L.
    pub fn total(&self) -> usize {
        self.t_bytes + self.l_bytes
    }
}

/// ⌈log₂(max+1)⌉ bit-width for packing labels (minimum 1).
#[inline]
fn bit_width_for_max(max_label: u32) -> usize {
    ((u64::BITS - (max_label as u64).leading_zeros()) as usize).max(1)
}

/// Pack `labels` into a `BitFieldVec` at width derived from the slice max.
fn pack_labels(labels: &[u32]) -> BitFieldVec {
    let max = labels.iter().copied().max().unwrap_or(0);
    let w = bit_width_for_max(max);
    let mut v = BitFieldVec::new(w, 0);
    for &lbl in labels {
        v.push(lbl as usize);
    }
    v
}

/// Empty 1-bit BitFieldVec (used for vacant depth regions).
fn empty_labels() -> BitFieldVec {
    BitFieldVec::new(1, 0)
}

/// A LOUDS-encoded height-3 trie.
///
/// Both T (bitvector) and L (label array) include a dummy entry at index 0 so
/// that the virtual root has identifier v = 0.  All paper formulas hold as-is.
///
/// L is three per-depth `BitFieldVec`s. Global positions `[0, l_len)` still
/// address labels: dummy + depth-0 in `l0`, then depth-1 in `l1` from
/// `d1_start`, then depth-2 in `l2` from `d2_start`. `t` is Rank9+SelectAdapt.
/// Leap is exp+binary on L.
pub struct LoudsTrie<B = t_backend::SuxRS, L = BitFieldVec> {
    /// LOUDS bitvector with dummy `false` at index 0.
    t: t_backend::TBitvec<B>,
    /// Depth-0 labels including dummy `0` at local index 0 (global pos 0).
    l0: L,
    /// Depth-1 labels (global pos `d1_start..d2_start`).
    l1: L,
    /// Depth-2 labels (global pos `d2_start..l_len`).
    l2: L,
    /// First global L position of depth-1 labels (`1 + n_depth0`).
    d1_start: usize,
    /// First global L position of depth-2 labels.
    d2_start: usize,
    /// Logical length of L (= n_edges + 1).
    l_len: usize,
}

/// Fully ε-serde-serializable core of a [`LoudsTrie`] — T + per-depth L.
#[derive(Epserde, Clone)]
pub(crate) struct LoudsCore<T = t_backend::TBitvec, L = BitFieldVec> {
    t: T,
    l0: L,
    l1: L,
    l2: L,
    d1_start: usize,
    d2_start: usize,
    l_len: usize,
}

// ── Reconstruction from core (generic over substrate) ─────────────────────────
//
// Pure field-moves with no owned-specific logic, so this is generic over any
// substrate `B`/`L` — this is what lets a future borrowed/mmap'd
// `LoudsCore<DeserType<TBitvec<B>>, DeserType<L>>` reconstruct
// directly into a navigable `LoudsTrie<B, L>` with **zero extra code**
// versus the owned round-trip path.
impl<B, L> LoudsTrie<B, L> {
    /// Reconstruct a full `LoudsTrie` from a [`LoudsCore`].
    pub(crate) fn from_core(core: LoudsCore<t_backend::TBitvec<B>, L>) -> Self {
        LoudsTrie {
            t: core.t,
            l0: core.l0,
            l1: core.l1,
            l2: core.l2,
            d1_start: core.d1_start,
            d2_start: core.d2_start,
            l_len: core.l_len,
        }
    }
}

// ── Construction (owned-only) ─────────────────────────────────────────────────
//
// These methods only ever operate on the default, fully-owned instantiation
// (`B = SuxRS`, `L = BitFieldVec`) — building a trie from
// scratch, or converting to/from the ε-serde core, always produces/consumes
// owned data.  Read-only navigation (below) is generic over any substrate.
impl LoudsTrie {
    /// Consume this trie into its fully ε-serde-serializable [`LoudsCore`].
    pub(crate) fn into_core(self) -> LoudsCore {
        LoudsCore {
            t: self.t,
            l0: self.l0,
            l1: self.l1,
            l2: self.l2,
            d1_start: self.d1_start,
            d2_start: self.d2_start,
            l_len: self.l_len,
        }
    }

    /// Build from T bits and L labels in paper format (no dummy entries).
    ///
    /// Prepends dummy `false` / `0` entries automatically.
    ///
    /// Labels are split into three depth regions with independent bit-widths.
    /// Depth boundaries are derived from the LOUDS T topology (BFS level-order):
    /// depth-0 = root children, depth-1 = their children, depth-2 = the rest.
    ///
    /// Panics if `t_bits.len() != l_labels.len()`.
    pub fn from_raw(t_bits: &[bool], l_labels: &[u32]) -> Self {
        assert_eq!(
            t_bits.len(),
            l_labels.len(),
            "LoudsTrie: T and L must have the same length"
        );

        let bits = std::iter::once(false).chain(t_bits.iter().copied());
        let t = t_backend::TBitvec::build(bits);
        let l_len = l_labels.len() + 1;

        // Derive depth boundaries from T (height-3 LOUDS, dummy at 0).
        // Root degree = selectnext1(1) - 0; without a full trie yet, scan T bits
        // with dummy prepended: t_bits is paper form (no dummy).
        // With dummy: T_rust[0]=false, then t_bits.
        // degree(0) = selectnext1(1) - 0 = first 1 at/after pos 1, minus 0.
        let (d1_start, d2_start) = depth_boundaries_from_t_bits(t_bits, l_len);

        // Slice paper labels (no dummy) into depths:
        // global pos 1..d1_start → depth 0 (len n0 = d1_start - 1)
        // global pos d1_start..d2_start → depth 1
        // global pos d2_start..l_len → depth 2
        let n0 = d1_start.saturating_sub(1);
        let n1 = d2_start.saturating_sub(d1_start);
        debug_assert_eq!(n0 + n1 + l_len.saturating_sub(d2_start), l_labels.len());

        let d0_slice = &l_labels[..n0.min(l_labels.len())];
        let d1_slice = if n0 < l_labels.len() {
            &l_labels[n0..(n0 + n1).min(l_labels.len())]
        } else {
            &[]
        };
        let d2_slice = if n0 + n1 < l_labels.len() {
            &l_labels[n0 + n1..]
        } else {
            &[]
        };

        // l0: dummy + depth-0 labels
        let max0 = d0_slice.iter().copied().max().unwrap_or(0);
        let w0 = bit_width_for_max(max0);
        let mut l0 = BitFieldVec::new(w0, 0);
        l0.push(0usize); // dummy
        for &lbl in d0_slice {
            l0.push(lbl as usize);
        }

        let l1 = if d1_slice.is_empty() {
            empty_labels()
        } else {
            pack_labels(d1_slice)
        };
        let l2 = if d2_slice.is_empty() {
            empty_labels()
        } else {
            pack_labels(d2_slice)
        };

        LoudsTrie {
            t,
            l0,
            l1,
            l2,
            d1_start,
            d2_start,
            l_len,
        }
    }
}

/// Compute `(d1_start, d2_start)` from paper-format T bits (no dummy).
///
/// Height-3 LOUDS level-order: after the root unary code, all depth-1 node
/// unary codes, then all depth-2 node unary codes. Root degree = number of
/// children of the root = number of depth-0 labels = index of first `true` in
/// the root's unary block (which is `0^(d-1) 1`).
fn depth_boundaries_from_t_bits(t_bits: &[bool], l_len: usize) -> (usize, usize) {
    if t_bits.is_empty() {
        // Empty trie: only dummy label at 0.
        return (1, 1);
    }
    // Root unary: first true ends the root block; degree = index+1.
    let root_end = t_bits
        .iter()
        .position(|&b| b)
        .expect("LOUDS root unary must end with 1");
    let n0 = root_end + 1; // degree(root)
    let d1_start = 1 + n0;

    // Walk remaining T to sum degrees of depth-1 nodes (next n0 unary blocks).
    let mut i = root_end + 1;
    let mut n1 = 0usize;
    for _ in 0..n0 {
        if i >= t_bits.len() {
            break;
        }
        let block_end = t_bits[i..]
            .iter()
            .position(|&b| b)
            .map(|p| i + p)
            .unwrap_or(t_bits.len().saturating_sub(1));
        let degree = block_end - i + 1;
        n1 += degree;
        i = block_end + 1;
    }
    let d2_start = d1_start + n1;
    // Clamp to l_len for safety on malformed inputs.
    (d1_start.min(l_len), d2_start.min(l_len))
}

// ── Read-only navigation (generic over substrate) ─────────────────────────────
//
// Generic over `B` (T bitvector's inner rank/select backend), `L` (label
// array substrate) so that these methods work
// identically whether the trie's fields are owned (`Vec`-backed, as built by
// `from_raw`) or borrowed/mmap'd (as produced by ε-serde's `load_mmap` on a
// `LoudsCore` — see `from_core`, which itself is owned-only, but whose result
// can subsequently be treated generically here).
impl<B, L> LoudsTrie<B, L>
where
    B: Rank + SelectUnchecked + MemSize,
    L: SliceByValue<Value = usize> + MemSize,
{
    /// Number of trie edges (|T| = |L|, excluding the dummy at index 0).
    #[inline]
    pub fn n_edges(&self) -> usize {
        self.l_len.saturating_sub(1)
    }

    /// `true` when the trie contains no edges (empty dataset).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.n_edges() == 0
    }

    /// Real allocated byte size of this trie's T + L (no shared vocab).
    pub fn mem_size_bytes(&self) -> usize {
        let b = self.mem_breakdown();
        b.t_bytes + b.l_bytes
    }

    /// Per-component breakdown (T / L). Vocab is Arc-deduped at `CltjData`.
    pub fn mem_breakdown(&self) -> LoudsMemBreakdown {
        LoudsMemBreakdown {
            t_bytes: self.t.mem_size_bytes(),
            l_bytes: self.l0.mem_size(SizeFlags::FOLLOW_REFS)
                + self.l1.mem_size(SizeFlags::FOLLOW_REFS)
                + self.l2.mem_size(SizeFlags::FOLLOW_REFS),
        }
    }

    /// Position of the first 1-bit at T-index ≥ `k`.
    ///
    /// Returns `self.l.len()` (sentinel ∞) when no such bit exists.
    #[inline]
    pub fn selectnext1(&self, k: usize) -> usize {
        // selectnext1(k) = select1(rank1(k))
        let rank = self.t.rank1(k);
        self.t.select1(rank).unwrap_or(self.l_len)
    }

    /// T-position of the i-th child of node v (1-indexed i).
    ///
    /// Paper: `select1(v + i)`.  Rust (0-indexed select1): `select1(v + i − 1)`.
    ///
    /// Only exercised by `#[cfg(test)]` code (paper-formula correctness
    /// pinning) — production hot paths use `leap`/`selectnext1`/
    /// `child_from_label_pos`/`degree` instead.
    #[allow(dead_code)]
    #[inline]
    pub fn child(&self, v: usize, i: usize) -> usize {
        self.t
            .select1(v + i - 1)
            .expect("LoudsTrie::child: index out of bounds")
    }

    /// T-position of the child whose label is stored at `L[label_pos]`.
    ///
    /// The parent node v has label `L[v + i]` at `label_pos = v + i`, so the
    /// child T-position is `select1(label_pos − 1)`.
    #[inline]
    pub fn child_from_label_pos(&self, label_pos: usize) -> usize {
        self.t
            .select1(label_pos - 1)
            .expect("LoudsTrie::child_from_label_pos: index out of bounds")
    }

    /// Number of children of node v.
    ///
    /// Paper: `selectnext1(v + 1) − v`.
    #[inline]
    pub fn degree(&self, v: usize) -> usize {
        self.selectnext1(v + 1).saturating_sub(v)
    }

    /// Label (local ID) of the i-th child of node v (1-indexed i).
    ///
    /// Paper: `L[v + i]`.
    ///
    /// Only exercised by `#[cfg(test)]` code (paper-formula correctness
    /// pinning) — production hot paths use `label_at` directly.
    #[allow(dead_code)]
    #[inline]
    pub fn access(&self, v: usize, i: usize) -> u32 {
        self.label_at(v + i)
    }

    /// First index `k` in `[lo, hi]` where `L[k] >= c`.
    ///
    /// Returns `hi + 1` when no such index exists.
    ///
    /// Exponential search (doubling stride) then binary search over bit-packed
    /// L via [`Self::label_at`] — O(log ℓ) amortised where ℓ is the leap
    /// distance.
    pub fn leap(&self, lo: usize, hi: usize, c: u32) -> usize {
        if lo > hi {
            return hi.wrapping_add(1);
        }

        // Exponential search: find a bracket [left+1, right] containing target.
        if self.label_at(lo) >= c {
            return lo;
        }
        if self.label_at(hi) < c {
            return hi + 1;
        }
        let mut step = 1usize;
        let mut left = lo;
        loop {
            let right = left.saturating_add(step).min(hi);
            if self.label_at(right) >= c {
                // Binary search in (left, right].
                let mut lo_b = left + 1;
                let mut hi_b = right;
                while lo_b < hi_b {
                    let mid = lo_b + (hi_b - lo_b) / 2;
                    if self.label_at(mid) < c {
                        lo_b = mid + 1;
                    } else {
                        hi_b = mid;
                    }
                }
                return lo_b;
            }
            if right == hi {
                return hi + 1;
            }
            left = right;
            step = step.saturating_mul(2);
        }
    }

    /// Degree of root (node v=0).  Returns 0 for an empty trie.
    #[inline]
    pub fn root_degree(&self) -> usize {
        if self.is_empty() { 0 } else { self.degree(0) }
    }

    /// Label at global position `pos` in L (includes dummy at 0).
    ///
    /// Routes to the depth-specific `BitFieldVec`. Uses
    /// [`value_traits::slices::SliceByValue::index_value`] — the safe, aligned
    /// accessor for bit-packed reads.
    #[inline]
    pub fn label_at(&self, pos: usize) -> u32 {
        if pos >= self.l_len {
            return 0;
        }
        if pos < self.d1_start {
            // Depth 0 region (includes dummy at local 0).
            self.l0.index_value(pos) as u32
        } else if pos < self.d2_start {
            self.l1.index_value(pos - self.d1_start) as u32
        } else {
            self.l2.index_value(pos - self.d2_start) as u32
        }
    }
}

// ── LoudsNav ──────────────────────────────────────────────────────────────────

/// Object-safety-free trait bundling every [`LoudsTrie<B, L, S>`] navigation
/// method that [`crate::cltj::CltjTrie`] needs, collapsing the three separate
/// `B`/`L`/`S` generic parameters (and their three separate trait bounds)
/// behind a single parameter — bundling T/L substrate bounds behind one parameter.
///
/// Blanket-implemented for every `LoudsTrie<B, L, S>` instantiation whose
/// `B`/`L`/`S` satisfy the same bounds as the generic navigation `impl` block
/// above — this includes both the owned/default instantiation
/// (`LoudsTrie<t_backend::SuxRS, BitFieldVec>`, produced by
/// `from_raw`/`from_core`) and a future borrowed/mmap'd instantiation whose
/// `B`/`L`/`S` are ε-serde's `DeserType` forms, with **zero code
/// duplication**: `CltjTrie`/`CltjData`/`GraphRing` only need to be generic
/// over one `Louds: LoudsNav` parameter instead of three.
pub(crate) trait LoudsNav {
    fn root_degree(&self) -> usize;
    fn degree(&self, v: usize) -> usize;
    fn leap(&self, lo: usize, hi: usize, c: u32) -> usize;
    fn label_at(&self, pos: usize) -> u32;
    fn child_from_label_pos(&self, label_pos: usize) -> usize;
    fn mem_size_bytes(&self) -> usize;
    fn mem_breakdown(&self) -> LoudsMemBreakdown;
}

impl<B, L> LoudsNav for LoudsTrie<B, L>
where
    B: Rank + SelectUnchecked + MemSize,
    L: SliceByValue<Value = usize> + MemSize,
{
    #[inline]
    fn root_degree(&self) -> usize {
        LoudsTrie::root_degree(self)
    }
    #[inline]
    fn degree(&self, v: usize) -> usize {
        LoudsTrie::degree(self, v)
    }
    #[inline]
    fn leap(&self, lo: usize, hi: usize, c: u32) -> usize {
        LoudsTrie::leap(self, lo, hi, c)
    }
    #[inline]
    fn label_at(&self, pos: usize) -> u32 {
        LoudsTrie::label_at(self, pos)
    }
    #[inline]
    fn child_from_label_pos(&self, label_pos: usize) -> usize {
        LoudsTrie::child_from_label_pos(self, label_pos)
    }
    fn mem_size_bytes(&self) -> usize {
        LoudsTrie::mem_size_bytes(self)
    }
    fn mem_breakdown(&self) -> LoudsMemBreakdown {
        LoudsTrie::mem_breakdown(self)
    }
}

// ── Construction helpers ──────────────────────────────────────────────────────

/// Append `d − 1` `false` bits then one `true` bit to `t`.
#[inline]
fn emit_node(d: usize, t: &mut Vec<bool>) {
    for _ in 0..d.saturating_sub(1) {
        t.push(false);
    }
    t.push(true);
}

/// Build a [`LoudsTrie`] from triples sorted by `(col0, col1, col2)`.
///
/// Input triples must use compact **0-indexed local IDs** for every column and
/// must be sorted lexicographically by `(col0, col1, col2)` with duplicates
/// already removed.
///
/// Returns `None` for an empty input slice.
pub fn build_louds_from_sorted(sorted: &[[u32; 3]]) -> Option<LoudsTrie> {
    if sorted.is_empty() {
        return None;
    }

    let n = sorted.len();
    // Each triple contributes at most 3 edges (one per level).
    let mut t_bits: Vec<bool> = Vec::with_capacity(3 * n);
    let mut l_labels: Vec<u32> = Vec::with_capacity(3 * n);

    // ── Level 0: root → distinct col0 values ──────────────────────────────────
    //
    // Scan sorted triples to collect (c0_start, c0_end) groups and emit root.
    let mut groups_c0: Vec<(usize, usize)> = Vec::new();
    {
        let mut i = 0;
        while i < sorted.len() {
            let c0 = sorted[i][0];
            let end = i + sorted[i..].partition_point(|t| t[0] == c0);
            groups_c0.push((i, end));
            l_labels.push(c0); // label of this col0 child
            i = end;
        }
        emit_node(groups_c0.len(), &mut t_bits);
    }

    // ── Level 1: each col0 node → distinct col1 values within that group ──────
    //
    // Also collect (c1_start, c1_end) for level-2 processing.
    let mut groups_c1: Vec<(usize, usize)> = Vec::with_capacity(n);
    for &(c0_start, c0_end) in &groups_c0 {
        let sub = &sorted[c0_start..c0_end];
        let mut c1_count = 0usize;
        let mut j = 0;
        while j < sub.len() {
            let c1 = sub[j][1];
            let end = j + sub[j..].partition_point(|t| t[1] == c1);
            l_labels.push(c1);
            groups_c1.push((c0_start + j, c0_start + end));
            c1_count += 1;
            j = end;
        }
        emit_node(c1_count, &mut t_bits);
    }

    // ── Level 2: each (col0,col1) node → col2 values (leaves) ─────────────────
    for &(c1_start, c1_end) in &groups_c1 {
        let sub = &sorted[c1_start..c1_end];
        emit_node(sub.len(), &mut t_bits);
        for triple in sub {
            l_labels.push(triple[2]);
        }
    }

    Some(LoudsTrie::from_raw(&t_bits, &l_labels))
}

// ── Borrowed substrate ─────────────────────────────────────────────────────────
//
// Types that ε-serde's `DeserType` resolves to under `load_mmap`.
// `'static` is a soundness lie tied to the caller's `MemCase` lifetime —
// same pattern as `VocabRepr::Mapped`.
pub(crate) type BorrowedT = SelectAdapt<
    AddNumBits<Rank9<BitVec<&'static [u64]>, &'static [BlockCounters]>>,
    &'static [usize],
>;
pub(crate) type BorrowedL = BitFieldVec<&'static [usize]>;

/// Borrowed/mmap'd [`LoudsTrie`] (zero-copy slices from a `MemCase`).
pub(crate) type BorrowedLouds = LoudsTrie<BorrowedT, BorrowedL>;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct the exact trie from Example 10 of the CompactLTJ paper
    ///
    /// and verify every formula from the paper.
    ///
    /// T = 00001 111101 1111000010001  (24 bits)
    /// L = 13456 777789 3251123451234  (24 entries, 1-indexed in paper)
    ///
    /// This test is substrate-independent: the same paper formulas hold
    /// regardless of which T bitvector backend is active.
    #[test]
    fn paper_example_navigation() {
        // T bits from the paper (1-indexed; no dummy — from_raw adds it)
        #[rustfmt::skip]
        let t_bits: Vec<bool> = vec![
            false, false, false, false, true,    // root: 0^4 1  (5 bits)
            true,  true,  true,  true,  false, true,  // depth-1 nodes
            true,  true,  true,  true,  false, false, false, false, true,  // depth-2
            false, false, false, true,
        ];
        // L labels from the paper (1-indexed; no dummy)
        let l_labels: Vec<u32> = vec![
            1, 3, 4, 5, 6, // root's 5 children (S local IDs)
            7, 7, 7, 7, 8, 9, // depth-1 nodes' children (P local IDs)
            3, 2, 5, 1, 1, 2, 3, 4, 5, 1, 2, 3, 4, // depth-2 (O local IDs)
        ];

        assert_eq!(t_bits.len(), l_labels.len(), "T and L must be same length");
        let trie = LoudsTrie::from_raw(&t_bits, &l_labels);

        // ── degree and child checks ───────────────────────────────────────────

        // degree(root=0) should be 5
        assert_eq!(trie.degree(0), 5, "root degree");

        // child(0, 5) = select1(4) = 9 (0-indexed T_rust position)
        assert_eq!(trie.child(0, 5), 9, "child(root, 5)");

        // Node u=9 has 2 children: L[10..11] = {8, 9}
        assert_eq!(trie.degree(9), 2, "degree(9)");
        assert_eq!(trie.access(9, 1), 8, "L[10]");
        assert_eq!(trie.access(9, 2), 9, "L[11]");

        // 1st child of u=9: w = child(9,1) = select1(9) = 15
        assert_eq!(trie.child(9, 1), 15, "child(9,1)");

        // degree(15) = selectnext1(16) - 15 = 20 - 15 = 5
        assert_eq!(trie.degree(15), 5, "degree(15)");

        // L[16..20] = {1, 2, 3, 4, 5}
        for (i, expected) in (1u32..=5).enumerate() {
            assert_eq!(trie.access(15, i + 1), expected, "L[{}]", 15 + i + 1);
        }

        // leap([16, 20], 4) = 19
        assert_eq!(trie.leap(16, 20, 4), 19, "leap([16,20], 4)");

        // ── Additional leap checks ─────────────────────────────────────────────

        // leap([16,20], 1) = 16  (already satisfies at lo)
        assert_eq!(trie.leap(16, 20, 1), 16);

        // leap([16,20], 6) = 21  (past hi — no element >= 6 in {1,2,3,4,5})
        assert_eq!(trie.leap(16, 20, 6), 21);

        // leap([16,20], 5) = 20
        assert_eq!(trie.leap(16, 20, 5), 20);
    }

    /// Verify round-trip for a small hand-crafted trie (3 triples, depth 3).
    #[test]
    fn build_from_sorted_single_sp_multiple_o() {
        // 3 triples sharing the same (S=0, P=0): O values {0, 1, 2}
        let triples: Vec<[u32; 3]> = vec![[0, 0, 0], [0, 0, 1], [0, 0, 2]];
        let trie = build_louds_from_sorted(&triples).expect("non-empty");

        // Root: 1 S value, degree=1
        assert_eq!(trie.degree(0), 1);
        assert_eq!(trie.access(0, 1), 0); // S=0

        // S-node: 1 P value, degree=1
        let s_node = trie.child(0, 1);
        assert_eq!(trie.degree(s_node), 1);
        assert_eq!(trie.access(s_node, 1), 0); // P=0

        // SP-node: 3 O values {0,1,2}
        let sp_node = trie.child(s_node, 1);
        assert_eq!(trie.degree(sp_node), 3);
        assert_eq!(trie.access(sp_node, 1), 0);
        assert_eq!(trie.access(sp_node, 2), 1);
        assert_eq!(trie.access(sp_node, 3), 2);

        // leap within the O range
        let lo = sp_node + 1;
        let hi = sp_node + 3;
        assert_eq!(trie.leap(lo, hi, 0), lo);
        assert_eq!(trie.leap(lo, hi, 1), lo + 1);
        assert_eq!(trie.leap(lo, hi, 2), lo + 2);
        assert_eq!(trie.leap(lo, hi, 3), hi + 1); // exhausted
    }

    /// Verify the single-triple edge case.
    #[test]
    fn build_from_sorted_single_triple() {
        let triples: Vec<[u32; 3]> = vec![[7, 3, 5]];
        let trie = build_louds_from_sorted(&triples).expect("non-empty");

        // Root degree = 1 (one S value)
        assert_eq!(trie.degree(0), 1);
        assert_eq!(trie.access(0, 1), 7); // S=7

        // S-node degree = 1
        let s_node = trie.child(0, 1);
        assert_eq!(trie.degree(s_node), 1);
        assert_eq!(trie.access(s_node, 1), 3); // P=3

        // SP-node degree = 1
        let sp_node = trie.child(s_node, 1);
        assert_eq!(trie.degree(sp_node), 1);
        assert_eq!(trie.access(sp_node, 1), 5); // O=5
    }

    /// Multiple subjects each with different predicates and objects.
    #[test]
    fn build_from_sorted_multi_subject() {
        // Triples: (0,0,0), (0,1,2), (1,0,1), (1,2,3)
        let triples: Vec<[u32; 3]> = vec![[0, 0, 0], [0, 1, 2], [1, 0, 1], [1, 2, 3]];
        let trie = build_louds_from_sorted(&triples).expect("non-empty");

        // Root degree = 2 (two S values: 0 and 1)
        assert_eq!(trie.degree(0), 2);
        assert_eq!(trie.access(0, 1), 0); // first S = 0
        assert_eq!(trie.access(0, 2), 1); // second S = 1

        // S=0 node: 2 P values {0, 1}
        let s0 = trie.child(0, 1);
        assert_eq!(trie.degree(s0), 2);
        assert_eq!(trie.access(s0, 1), 0); // P=0
        assert_eq!(trie.access(s0, 2), 1); // P=1

        // S=1 node: 2 P values {0, 2}
        let s1 = trie.child(0, 2);
        assert_eq!(trie.degree(s1), 2);
        assert_eq!(trie.access(s1, 1), 0); // P=0
        assert_eq!(trie.access(s1, 2), 2); // P=2

        // SP=(0,0): O={0}
        let sp00 = trie.child(s0, 1);
        assert_eq!(trie.degree(sp00), 1);
        assert_eq!(trie.access(sp00, 1), 0);

        // SP=(0,1): O={2}
        let sp01 = trie.child(s0, 2);
        assert_eq!(trie.degree(sp01), 1);
        assert_eq!(trie.access(sp01, 1), 2);
    }

    /// Verify leap on an empty range and a range of size 1.
    #[test]
    fn leap_edge_cases() {
        let trie = build_louds_from_sorted(&[[0u32, 0, 5], [0, 0, 10]]).expect("non-empty");
        // SP-node children lo..hi cover O labels {5, 10}
        let s_node = trie.child(0, 1);
        let sp_node = trie.child(s_node, 1);
        let lo = sp_node + 1;
        let hi = sp_node + 2;

        // lo > hi
        assert_eq!(trie.leap(hi + 1, hi, 0), hi.wrapping_add(1));
        // Exact match at lo
        assert_eq!(trie.leap(lo, hi, 5), lo);
        // Exact match at hi
        assert_eq!(trie.leap(lo, hi, 10), hi);
        // Beyond all values
        assert_eq!(trie.leap(lo, hi, 11), hi + 1);
    }

    /// High-degree node: leap correctness on bit-packed L.
    #[test]
    fn high_degree_leap() {
        // One subject (0), one predicate (0), 20 objects (0..19).
        let triples: Vec<[u32; 3]> = (0u32..20).map(|o| [0, 0, o]).collect();
        let trie = build_louds_from_sorted(&triples).expect("non-empty");

        let s_node = trie.child(0, 1);
        let sp_node = trie.child(s_node, 1);
        assert_eq!(trie.degree(sp_node), 20, "SP-node degree must be 20");
        let lo = sp_node + 1;
        let hi = sp_node + 20;

        for target in 0u32..20 {
            let got = trie.leap(lo, hi, target);
            assert_eq!(got, lo + target as usize, "leap(lo, hi, {target}) wrong");
        }
        assert_eq!(
            trie.leap(lo, hi, 20),
            hi + 1,
            "leap past max should exhaust"
        );
        assert_eq!(trie.leap(lo, hi, 0), lo);
        let mid_lo = lo + 10;
        assert_eq!(
            trie.leap(mid_lo, hi, 15),
            lo + 15,
            "leap from mid-range to 15"
        );
    }

    /// Verify that CompactVector bit-packing round-trips correctly for large label
    /// values (tests the bit_width calculation).
    #[test]
    fn compact_vector_large_labels() {
        // Labels spanning a wide range to exercise bit_width computation.
        // max_label = 255 → bit_width = 8; max_label = 1000 → bit_width = 10
        let triples: Vec<[u32; 3]> = vec![[0, 0, 255], [0, 0, 1000], [1, 2, 500]];
        let trie = build_louds_from_sorted(&triples).expect("non-empty");

        // Root: S values {0, 1}
        assert_eq!(trie.degree(0), 2);
        assert_eq!(trie.access(0, 1), 0);
        assert_eq!(trie.access(0, 2), 1);

        // S=0, P=0 → O values {255, 1000}
        let s0 = trie.child(0, 1);
        let sp00 = trie.child(s0, 1);
        assert_eq!(trie.access(sp00, 1), 255);
        assert_eq!(trie.access(sp00, 2), 1000);

        // leap to 500 in {255, 1000} → lands on 1000
        let lo = sp00 + 1;
        let hi = sp00 + 2;
        assert_eq!(trie.leap(lo, hi, 500), hi);
    }
}
