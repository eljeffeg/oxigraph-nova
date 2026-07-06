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
//! ## L label array: `BitFieldVec<usize>` (sux)
//!
//! L is stored as a `sux::bits::BitFieldVec<usize>` using ⌈log₂(U)⌉ bits per
//! label, where U = alphabet size (max local ID + 1).  This provides bit-packed
//! storage with direct index access, and — since sux 0.14's `BitFieldVec`
//! derives `epserde::Epserde` — a zero-copy ε-serde + mmap persistence path.
//! Typically 14–17 bits/label for real datasets.
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
//! ## High-degree sidecar with SIMD `partition_point`

//! `leap()` benefits from SIMD-vectorised scanning on `&[u32]` slices via
//! `partition_point`, which the compiler can optimize to AVX2/NEON vector
//! compares.  Bit-packed L storage improves memory but prevents direct SIMD
//! access (requires bit-extract per comparison).  The sidecar optimization
//! restores SIMD performance for high-degree nodes where it matters most.
//!
//! L-opt B restores SIMD `partition_point` for high-degree nodes by maintaining
//! a **sidecar array** alongside the bit-packed L.  For every node whose degree
//! ≥ `SIDECAR_THRESH` (16), the sidecar stores the child labels as a contiguous
//! `Box<[u32]>`.  `leap()` branches:
//!
//! - **degree < SIDECAR_THRESH**: inline scalar exponential/binary search (cheap
//!   for 1–15 labels — most RDF nodes).
//! - **degree ≥ SIDECAR_THRESH**: `partition_point` on `&[u32]` sidecar —
//!   O(log d) SIMD, O(1) fallback to scalar if sidecar lookup misses (safety).
//!
//! The sidecar is keyed by `hi` (the exclusive-upper-bound label position of the
//! node's child range in L), which is the only information available at `leap`
//! call-time without changing the signature.  `lo = hi - degree + 1` is derived.
//!
//! Space impact: sidecar stored only for high-degree nodes; negligible overhead
//! relative to the LOUDS index itself.  The 20 B/triple compact target is
//! maintained.
//!
//! ## Construction
//!
//! [`build_louds_from_sorted`] accepts triples sorted by `(col0, col1, col2)`
//! using compact 0-indexed local IDs and builds T and L in BFS level-order.
//! Each level is processed in one pass; no intermediate data structures beyond
//! the output vectors are required.

// sux::prelude::* brings in BitFieldVec plus all rank/select traits used by t_backend.
// ── Adaptive Elias-Fano sidecar payload ──────────────────────────────────────
//
// sux is always a dependency (since Step 10c-L L-opt A), so EfDict/
// EliasFanoBuilder/Succ are always available — no conditional compilation needed
// for the imports.  The feature flag `l-opt-ef` only controls the crossover
// threshold (EF_THRESH_EFFECTIVE), enabling per-degree A/B benchmarking.
use epserde::Epserde;
use mem_dbg::{MemSize, SizeFlags};
use sux::dict::{EfDict, EliasFanoBuilder};
use sux::prelude::*;
use sux::traits::Succ;
// SliceByValue::index_value — the safe aligned accessor for BitFieldVec L reads.
// value-traits 0.2.0 is the same version sux 0.14 depends on (pinned for trait compatibility).
use value_traits::slices::SliceByValue; // not re-exported by sux::prelude — must be imported explicitly

/// Intermediate, construction-time-only sidecar payload for a single
/// high-degree node, produced during `build_sidecar`'s BFS walk before
/// being flattened into the ε-serde-serializable [`SidecarCore`]
/// representation. This type is never itself serialized (only
/// `SidecarCore`'s parallel vectors are).
enum SidecarPayload {
    /// SIMD-friendly contiguous label array: `partition_point` O(log d).
    Slice(Box<[u32]>),
    /// Elias-Fano indexed dictionary: `succ()` O(1).
    Ef(EfDict<usize>),
}

/// ε-serde-serializable sidecar (Phase 2 of the mmap'd ε-serde snapshot plan —
/// see CLAUDE.md item 14).
///
/// Previously the sidecar was an owned `HashMap<usize, (usize, SidecarPayload)>`
/// keyed by `hi` with an enum payload — neither `HashMap` nor enums with
/// heap-allocated variants are directly ε-serde-serializable, so the sidecar
/// used to be excluded from the persistent snapshot format entirely and
/// rebuilt from scratch (an O(n) walk) every time a trie was loaded from disk
/// or round-tripped in memory.
///
/// This representation instead stores **sorted parallel vectors** keyed by
/// `hi` (ascending) with **binary-search** lookup (`his.binary_search`)
/// replacing the hash lookup, and both payload kinds (SIMD slice / Elias-Fano)
/// flattened into shared backing arrays instead of a payload enum:
///
/// - `his`/`los`: sorted-by-`his` parallel arrays of (node_hi, node_lo) for
///   every high-degree node (`degree >= SIDECAR_THRESH`).
/// - `kind`: 0 = `Slice` payload, 1 = `Ef` payload, one per entry.
/// - `slice_start`: for `Slice` entries, the starting offset into
///   `slice_flat` (labels are `slice_flat[start .. start + (hi - lo + 1)]`).
///   Unused (0) for `Ef` entries.
/// - `slice_flat`: concatenated label arrays for every `Slice` entry, in
///   entry order.
/// - `ef_idx`: for `Ef` entries, the index into `ef_dicts`. Unused (0) for
///   `Slice` entries.
/// - `ef_dicts`: one `EfDict<usize>` per `Ef` entry, in entry order. `EfDict`
///   is already `epserde::Epserde`-derivable (from `sux`), and epserde
///   supports `Vec<T>` generically for any `T: Epserde`, so this composes
///   through `#[derive(Epserde)]` with zero extra work.
///
/// Every field here is a plain `Vec` of a primitive or an `Epserde` type, so
/// `SidecarCore` (and therefore all of [`LoudsCore`]) is now **fully**
/// ε-serde-serializable — the sidecar no longer needs to be excluded from the
/// snapshot or rebuilt on load.
#[derive(Epserde, Default)]
pub(crate) struct SidecarCore<
    His = Vec<usize>,
    Los = Vec<usize>,
    Kind = Vec<u8>,
    SliceStart = Vec<usize>,
    EfIdx = Vec<usize>,
    SliceFlat = Vec<u32>,
    EfDicts = Vec<EfDict<usize>>,
> {
    his: His,
    los: Los,
    kind: Kind,
    slice_start: SliceStart,
    ef_idx: EfIdx,
    slice_flat: SliceFlat,
    ef_dicts: EfDicts,
}

impl SidecarCore {
    /// Binary-search `his` (sorted ascending) for an exact match, returning
    /// the shared entry index on success.
    #[inline]
    fn find(&self, hi: usize) -> Option<usize> {
        self.his.binary_search(&hi).ok()
    }

    /// Approximate heap byte size of this sidecar's backing vectors, for
    /// memory-breakdown diagnostics.
    fn mem_size_bytes(&self) -> usize {
        std::mem::size_of_val(&self.his[..])
            + std::mem::size_of_val(&self.los[..])
            + std::mem::size_of_val(&self.kind[..])
            + std::mem::size_of_val(&self.slice_start[..])
            + std::mem::size_of_val(&self.ef_idx[..])
            + std::mem::size_of_val(&self.slice_flat[..])
            + self
                .ef_dicts
                .iter()
                .map(|ef| ef.mem_size(SizeFlags::default()))
                .sum::<usize>()
    }
}


// ── T bitvector backend ───────────────────────────────────────────────────────
//
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
    #[derive(Epserde)]
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
        pub(super) fn mem_size_bytes(&self) -> usize
        where
            B: MemSize,
        {
            self.rs.mem_size(SizeFlags::default())
        }
    }
}

// ── LoudsTrie ─────────────────────────────────────────────────────────────────

/// Minimum node degree to store a SIMD-friendly u32 sidecar (L-opt B).
///
/// Nodes with `degree >= SIDECAR_THRESH` get a contiguous `Box<[u32]>` sidecar
/// of their child labels so that `leap()` can use `partition_point` (SIMD-
/// vectorisable) instead of scalar `index_value` bit-extracts.
///
/// Threshold = 16 labels:
/// - RDF datasets: the vast majority of nodes have degree 1–3 (scalar path is
///   fastest for them — no branch overhead, no cache pressure from sidecar).
/// - High-degree nodes (e.g. hot predicates with many objects) are exactly the
///   nodes where SIMD parallelism pays off — 16+ labels span ≥ 2 SIMD lanes.
/// - 16 u32 = 64 bytes = one cache line — aligned with typical SIMD register
///   width and cache-line granularity.
pub const SIDECAR_THRESH: usize = 16;

/// Minimum node degree to prefer Elias-Fano `succ()` over SIMD `partition_point`.
///
/// ## Measured crossover (aarch64, degree-sweep bench, 2026-07-01)
///
/// Sweep: 10k entities × 4 features, POS-trie degrees 16/32/64/128/256/512.
/// Baseline = adaptive (Slice for D<128, Ef for D≥128); A/B = forced-Ef.
///
/// | Degree | Baseline path | Forced-Ef Δ | Verdict            |
/// |--------|---------------|-------------|--------------------|
/// |  16    | Slice         | +1.84%      | Within noise       |
/// |  32    | Slice         | +2.15%      | Slice wins (barely)|
/// |  64    | Slice         | +2.98%      | Slice wins         |
/// | 128    | Ef (control)  | +2.50%      | Same path — noise  |
/// | 256    | Ef (control)  | +1.55%      | Same path — noise  |
/// | 512    | Ef (control)  | +0.60%      | Same path — noise  |
///
/// Noise floor ≈ 1.5–2.5% (inferred from D=128–512 controls where both arms use Ef).
/// D=64 Slice advantage (+2.98%) is just above the noise floor → Slice wins at ≤64.
/// Prior `feature_lookup` benchmark (D≈500, 1.25M triples): Ef −3.1% → Ef wins at ≥500.
///
/// **Crossover is in the [64, 500] range.**  EF_THRESH=128 is positioned inside
/// that window (log-midpoint ≈ 179).  No clean A/B data at D=128–256 (baseline
/// already used Ef there); 128 remains the measured conservative floor.
///
/// All sidecar nodes with `degree < EF_THRESH` keep the Slice path.
pub const EF_THRESH: usize = 128;

/// Effective EF threshold — compile-time override via `l-opt-ef` feature.
///
/// With `--features l-opt-ef`: forces EF for ALL sidecar nodes (EF_THRESH_EFFECTIVE
/// = SIDECAR_THRESH=16), enabling clean A/B benchmarking against the Slice path.
///
/// Without the feature: adaptive — EF only for `degree >= EF_THRESH` (= 128).
#[cfg(feature = "l-opt-ef")]
const EF_THRESH_EFFECTIVE: usize = SIDECAR_THRESH;
#[cfg(not(feature = "l-opt-ef"))]
const EF_THRESH_EFFECTIVE: usize = EF_THRESH;

/// Per-component memory breakdown of a single [`LoudsTrie`].
/// Does not include the shared vocab arrays, which live one level up in
/// [`crate::cltj::CltjData`] (Arc-deduped across all six tries).
#[derive(Copy, Clone, Debug, Default)]
pub struct LoudsMemBreakdown {
    /// T bitvector: raw bits + Rank9 counters + SelectAdapt inventory.
    pub t_bytes: usize,
    /// L label array: bit-packed `BitFieldVec`.
    pub l_bytes: usize,
    /// High-degree sidecar (Slice or Elias-Fano payloads).
    pub sidecar_bytes: usize,
}

impl LoudsMemBreakdown {
    /// Sum of all three components.
    pub fn total(&self) -> usize {
        self.t_bytes + self.l_bytes + self.sidecar_bytes
    }
}

/// A LOUDS-encoded height-3 trie.
///
/// Both T (bitvector) and L (label array) include a dummy entry at index 0 so
/// that the virtual root has identifier v = 0.  All paper formulas hold as-is.
///
/// ## Label storage
///
/// `l` is a `sux::bits::BitFieldVec` (backend `Vec<usize>`) storing ⌈log₂(U)⌉
/// bits per label using bit-packed storage with direct index access.  Since
/// `BitFieldVec` derives `epserde::Epserde`, this is also the basis for the
/// ε-serde mmap persistence path.
/// `l_len` stores the logical length (= n_edges + 1 for the dummy) separately
/// to avoid pulling in `value_traits::SliceByValue` as a direct dependency.
///
/// `t` is a `t_backend::TBitvec` selected at compile time by Cargo features.
/// Default = sux Rank9+SelectAdapt (best performance in benchmarks).
///
/// ## High-degree sidecar
///
/// `sidecar` maps `hi` → (node_lo, sorted_labels) for nodes with
/// `degree >= SIDECAR_THRESH`.  `leap()` uses this to run `partition_point` on
/// a contiguous `&[u32]` instead of scalar `index_value` bit-extracts.
pub struct LoudsTrie {
    /// LOUDS bitvector with dummy `false` at index 0.
    /// Supports O(1) `rank1` and `select1` via the active `t_backend`.
    t: t_backend::TBitvec,
    /// Label array with dummy `0` at index 0, bit-packed at ⌈log₂(U)⌉ bits/entry.
    l: BitFieldVec,
    /// Logical length of `l` (= n_edges + 1).  Stored explicitly to avoid
    /// importing `value_traits::SliceByValue` just for `.len()`.
    l_len: usize,
    /// L-opt B sidecar: for high-degree nodes (degree >= SIDECAR_THRESH),
    /// stores (node_lo, payload) keyed by `hi` (inclusive upper bound of the
    /// node's label range in L), as sorted parallel vectors with binary
    /// search — see [`SidecarCore`].
    ///
    /// `hi` is the only context available inside `leap(lo, hi, c)` without
    /// changing the public signature.  `node_lo` is stored alongside to avoid
    /// recomputing it from `hi` and `degree`.
    sidecar: SidecarCore,
}

/// Fully ε-serde-serializable core of a [`LoudsTrie`] — T, L, **and** the
/// sidecar (Phase 2 of the mmap'd ε-serde snapshot plan; see
/// [`SidecarCore`]'s docs for why the sidecar is now included here rather
/// than excluded-and-rebuilt-on-load as it was pre-Phase-2).
///
/// `t_backend::TBitvec`, `sux::bits::BitFieldVec`, and [`SidecarCore`] all
/// derive `epserde::Epserde`, so this struct composes cleanly through
/// `#[derive(Epserde)]` with zero extra trait-bound or lifetime work.
#[derive(Epserde)]
pub(crate) struct LoudsCore<T = t_backend::TBitvec, L = BitFieldVec, S = SidecarCore> {
    t: T,
    l: L,
    l_len: usize,
    sidecar: S,
}

impl LoudsTrie {
    /// Consume this trie into its fully ε-serde-serializable [`LoudsCore`]
    /// (T + L + sidecar — no information is discarded or needs rebuilding).
    ///
    /// Used by the persistent snapshot format to serialize a freshly built
    /// trie.
    pub(crate) fn into_core(self) -> LoudsCore {
        LoudsCore {
            t: self.t,
            l: self.l,
            l_len: self.l_len,
            sidecar: self.sidecar,
        }
    }

    /// Reconstruct a full `LoudsTrie` from a [`LoudsCore`] loaded from disk
    /// (or from an in-memory round-trip buffer). The sidecar travels with
    /// the core as-is — no rebuild required (Phase 2).
    pub(crate) fn from_core(core: LoudsCore) -> Self {
        LoudsTrie {
            t: core.t,
            l: core.l,
            l_len: core.l_len,
            sidecar: core.sidecar,
        }
    }


    /// Build from T bits and L labels in paper format (no dummy entries).
    ///
    /// Prepends dummy `false` / `0` entries automatically.
    ///
    /// The `CompactVector` bit-width is computed from the maximum label value:
    /// `⌈log₂(max_label + 1)⌉` bits, minimum 1.
    ///
    /// High-degree nodes (degree ≥ `SIDECAR_THRESH`) are recorded in the L-opt B
    /// sidecar so that `leap()` can use SIMD `partition_point` on them.
    ///
    /// Panics if `t_bits.len() != l_labels.len()`.
    pub fn from_raw(t_bits: &[bool], l_labels: &[u32]) -> Self {
        assert_eq!(
            t_bits.len(),
            l_labels.len(),
            "LoudsTrie: T and L must have the same length"
        );

        // ── Build T bitvector (substrate selected by feature flag) ───────────
        let bits = std::iter::once(false).chain(t_bits.iter().copied());
        let t = t_backend::TBitvec::build(bits);

        // ── Build L label array as BitFieldVec ────────────────────────────────
        //
        // Bit-width = ⌈log₂(max_label + 1)⌉, minimum 1.
        // Build via push() — a direct inherent method on BitFieldVec<Vec<usize>>
        // (no trait import needed).  Index 0 = dummy 0; indices 1..=n = labels.
        let max_label = l_labels.iter().copied().max().unwrap_or(0) as u64;
        let bit_width = ((u64::BITS - max_label.leading_zeros()) as usize).max(1);
        let l_len = l_labels.len() + 1;
        let mut l: BitFieldVec = BitFieldVec::new(bit_width, 0);
        l.push(0usize); // dummy entry at index 0
        for &lbl in l_labels {
            l.push(lbl as usize);
        }

        // ── Build L-opt B sidecar ─────────────────────────────────────────────
        //
        // Walk the T bitvector (already built) to enumerate every node and its
        // child label range [node_lo, node_hi].  For nodes with degree ≥
        // SIDECAR_THRESH, copy their labels into a Box<[u32]> keyed by `node_hi`.
        //
        // We use the same LOUDS primitives (rank1, select1) that the built `t`
        // exposes, so this is one forward scan of the T bitvector post-build.
        let sidecar = Self::build_sidecar(&t, &l, l_len);

        LoudsTrie {
            t,
            l,
            l_len,
            sidecar,
        }
    }

    /// Walk T to find every node's child range and populate the sidecar.
    ///
    /// Separate function to avoid a partial-move issue (called after `t`, `l`,
    /// and `l_len` are computed but before they are moved into `Self`).
    ///
    /// Builds an intermediate `Vec<(hi, lo, SidecarPayload)>` during the BFS
    /// walk (order is BFS, not sorted by `hi`), then sorts by `hi` and
    /// flattens into the ε-serde-serializable [`SidecarCore`] representation
    /// (Phase 2 of the mmap'd ε-serde snapshot plan).
    fn build_sidecar(t: &t_backend::TBitvec, l: &BitFieldVec, l_len: usize) -> SidecarCore {
        let mut entries: Vec<(usize, usize, SidecarPayload)> = Vec::new();
        if l_len <= 1 {
            return SidecarCore::default(); // empty trie
        }


        // Enumerate nodes by walking the T bitvector BFS-style.
        // A node at T-position v has degree = selectnext1(v+1) - v.
        // selectnext1(k) = select1(rank1(k)).
        // Root is v=0. BFS queue: nodes are their T-positions.
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(0usize); // root

        while let Some(v) = queue.pop_front() {
            // Compute degree(v) = selectnext1(v+1) - v.
            let rank = t.rank1(v + 1);
            let next1 = t.select1(rank).unwrap_or(l_len);
            let degree = next1.saturating_sub(v);
            if degree == 0 {
                continue;
            }
            let node_lo = v + 1;
            let node_hi = v + degree;

            // Enqueue depth-1 and depth-2 internal nodes.
            // Leaf nodes (at depth 2 in a height-3 trie) have no T-encoding,
            // so we only enqueue nodes whose children are internal (not leaves).
            // In practice, we enqueue all internal nodes; leaf detection
            // is implicit because `degree()` returns 0 for positions past T.

            // Enqueue the children of this node (their T-positions = child() calls).
            // child(v, i) = select1(v + i - 1).
            for i in 1..=degree {
                if let Some(child_v) = t.select1(v + i - 1)
                    && child_v + 1 < l_len
                {
                    queue.push_back(child_v);
                }
            }

            if degree >= SIDECAR_THRESH {
                // Build the sidecar payload from the bit-packed L labels in [node_lo..=node_hi].
                // Labels are sorted ascending — guaranteed by trie construction from sorted triples.
                //
                // Adaptive dispatch (Step 12):
                //   degree >= EF_THRESH_EFFECTIVE → SidecarPayload::Ef  (O(1) succ)
                //   degree <  EF_THRESH_EFFECTIVE → SidecarPayload::Slice  (SIMD partition_point)
                //
                // EF_THRESH_EFFECTIVE = SIDECAR_THRESH when `l-opt-ef` feature is set
                // (all sidecar nodes use EF — A/B benchmark mode).
                // EF_THRESH_EFFECTIVE = EF_THRESH (128) otherwise — conservative crossover floor.
                let payload: SidecarPayload = if degree >= EF_THRESH_EFFECTIVE {
                    // EF universe = max label value + 1.  Last label = max since sorted.
                    let max_lbl = if node_hi < l_len {
                        l.index_value(node_hi)
                    } else {
                        0
                    };
                    let mut efb = EliasFanoBuilder::new(degree, max_lbl.saturating_add(1));
                    for pos in node_lo..=node_hi {
                        efb.push(if pos < l_len { l.index_value(pos) } else { 0 });
                    }
                    SidecarPayload::Ef(efb.build_with_dict())
                } else {
                    let labels: Box<[u32]> = (node_lo..=node_hi)
                        .map(|pos| {
                            if pos < l_len {
                                l.index_value(pos) as u32
                            } else {
                                0
                            }
                        })
                        .collect::<Vec<u32>>()
                        .into_boxed_slice();
                    SidecarPayload::Slice(labels)
                };

                entries.push((node_hi, node_lo, payload));
            }
        }

        // Sort by `hi` ascending (BFS order is level-order, not `hi`-sorted)
        // so that `SidecarCore::find` can binary-search.
        entries.sort_unstable_by_key(|(hi, _, _)| *hi);

        let mut core: SidecarCore = SidecarCore::default();
        for (hi, lo, payload) in entries {
            core.his.push(hi);
            core.los.push(lo);
            match payload {
                SidecarPayload::Slice(labels) => {
                    core.kind.push(0);
                    core.slice_start.push(core.slice_flat.len());
                    core.ef_idx.push(0);
                    core.slice_flat.extend_from_slice(&labels);
                }
                SidecarPayload::Ef(ef) => {
                    core.kind.push(1);
                    core.slice_start.push(0);
                    core.ef_idx.push(core.ef_dicts.len());
                    core.ef_dicts.push(ef);
                }
            }
        }
        core
    }


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

    /// Real allocated byte size of this trie's T + L + sidecar structures,
    /// for memory-breakdown diagnostics (does not include shared vocab —
    /// see `CltjTrie::mem_size_bytes` / `CltjData::mem_size_bytes` for the
    /// Arc-deduped vocab accounting).
    pub fn mem_size_bytes(&self) -> usize {
        let b = self.mem_breakdown();
        b.t_bytes + b.l_bytes + b.sidecar_bytes
    }

    /// Per-component breakdown of this trie's memory (T bitvector / L label
    /// array / sidecar). Does not include shared
    /// vocab — see `CltjData::mem_size_bytes` for the Arc-deduped vocab total.
    pub fn mem_breakdown(&self) -> LoudsMemBreakdown {
        LoudsMemBreakdown {
            t_bytes: self.t.mem_size_bytes(),
            l_bytes: self.l.mem_size(SizeFlags::default()),
            sidecar_bytes: self.sidecar.mem_size_bytes(),
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
    #[inline]
    pub fn access(&self, v: usize, i: usize) -> u32 {
        self.label_at(v + i)
    }

    /// First index `k` in `[lo, hi]` where `L[k] >= c`.
    ///
    /// Returns `hi + 1` when no such index exists.
    ///
    /// ## Sidecar dispatch
    ///
    /// For nodes with `degree >= SIDECAR_THRESH`, uses SIMD-friendly
    /// `partition_point` on a contiguous `&[u32]` sidecar.  For small nodes
    /// (most RDF trie nodes are degree 1–3), falls through to the inline
    /// **exponential search** (doubling stride) then binary search — O(log ℓ)
    /// amortised where ℓ is the leap distance.
    ///
    /// The sidecar is keyed by `hi` (inclusive upper bound of the node's label
    /// range in L), which is the sole context `leap` receives.
    pub fn leap(&self, lo: usize, hi: usize, c: u32) -> usize {
        if lo > hi {
            return hi.wrapping_add(1);
        }

        // ── L-opt B: sidecar dispatch for high-degree nodes ───────────────────
        //
        // Keyed by `hi` — populated at construction for degree >= SIDECAR_THRESH.
        // Binary search via `SidecarCore::find`, then dispatch on `kind`:
        // 0 = Slice + partition_point (SIMD, O(log d)); 1 = EfDict + succ() (O(1)).
        if let Some(idx) = self.sidecar.find(hi) {
            let node_lo = self.sidecar.los[idx];
            let offset = lo.saturating_sub(node_lo);

            return if self.sidecar.kind[idx] == 0 {
                // SIMD-friendly `partition_point` on contiguous u32 sidecar.
                let start = self.sidecar.slice_start[idx];
                let end = start + (hi - node_lo + 1);
                let labels = &self.sidecar.slice_flat[start..end];
                let slice = &labels[offset..];
                if slice.is_empty() {
                    return hi + 1;
                }
                let rel = slice.partition_point(|&v| v < c);
                if rel >= slice.len() { hi + 1 } else { lo + rel }
            } else {
                // Elias-Fano O(1) succ: find first value >= c in the full
                // label range, then clamp to [offset, end].
                //
                // rank < offset: labels[offset] >= labels[rank] >= c (sorted) → lo.
                // rank >= offset: exact position node_lo + rank (>= lo).
                // None: no value >= c in entire node range → hi + 1.
                let ef = &self.sidecar.ef_dicts[self.sidecar.ef_idx[idx]];
                match ef.succ(c as usize) {
                    None => hi + 1,
                    Some((rank, _)) => {
                        if rank < offset {
                            lo
                        } else {
                            node_lo + rank
                        }
                    }
                }
            };
        }


        // ── Scalar path for low-degree nodes (degree < SIDECAR_THRESH) ────────
        //
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

    /// Label at position `pos` in L (includes dummy at 0).
    ///
    /// Decodes one bit-packed word from the `BitFieldVec` using
    /// [`value_traits::slices::SliceByValue::index_value`] — the safe, aligned
    /// accessor that does a proper bit-packed, bounds-checked read without loading
    /// past the last allocated backing word.
    ///
    /// `get_unaligned` (the inherent method) reads a full 8-byte `Word` at an
    /// arbitrary byte offset and **panics** when `pos` is near the buffer end
    /// because the read overshoots the allocation.  `index_value` uses aligned
    /// word reads with masking and is always safe.
    #[inline]
    pub fn label_at(&self, pos: usize) -> u32 {
        if pos < self.l_len {
            self.l.index_value(pos) as u32
        } else {
            0
        }
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct the exact trie from Example 10 of the CompactLTJ paper
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

    /// Verify L-opt B sidecar: a high-degree node (degree ≥ SIDECAR_THRESH) must
    /// produce the same `leap()` results as the scalar path would.
    ///
    /// We build a trie with one S node that has 20 O-children (degree=20 ≥ 16),
    /// verify the sidecar is populated, and check all leap targets.
    #[test]
    fn sidecar_high_degree_node() {
        // One subject (0), one predicate (0), 20 objects (0..19).
        let triples: Vec<[u32; 3]> = (0u32..20).map(|o| [0, 0, o]).collect();
        let trie = build_louds_from_sorted(&triples).expect("non-empty");

        // The SP-node (depth-2) has degree 20 → sidecar should be populated.
        let s_node = trie.child(0, 1);
        let sp_node = trie.child(s_node, 1);
        assert_eq!(trie.degree(sp_node), 20, "SP-node degree must be 20");
        let lo = sp_node + 1;
        let hi = sp_node + 20;

        // The sidecar must be present for this hi.
        assert!(
            trie.sidecar.find(hi).is_some(),
            "sidecar must contain entry for hi={hi} (degree=20 >= SIDECAR_THRESH={SIDECAR_THRESH})"
        );


        // Every possible target — compare against manually expected position.
        for target in 0u32..20 {
            let got = trie.leap(lo, hi, target);
            // Labels are 0..19 at positions lo..hi; target should land at lo+target.
            assert_eq!(got, lo + target as usize, "leap(lo, hi, {target}) wrong");
        }
        // Target past max → exhausted.
        assert_eq!(
            trie.leap(lo, hi, 20),
            hi + 1,
            "leap past max should exhaust"
        );
        // Target == 0 at start.
        assert_eq!(trie.leap(lo, hi, 0), lo);
        // Seek from mid-range.
        let mid_lo = lo + 10;
        let got_mid = trie.leap(mid_lo, hi, 15);
        assert_eq!(got_mid, lo + 15, "leap from mid-range to 15");
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
