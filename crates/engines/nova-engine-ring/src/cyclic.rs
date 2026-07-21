//! E5.7 — Paper-faithful **cyclic Ring A** primitive spike.
//!
//! Feature-gated: compile with `--features cyclic-ring`.
//! **Not** on the default `LoudsStore` execution path.
//!
//! # Structure (Ring A only)
//!
//! ```text
//! T_spo  → last column C_o
//! T_osp  → last column C_p
//! T_pos  → last column C_s
//! A_o, A_p, A_s cumulative arrays
//! ```
//!
//! Ring B / URing are E5.8 **DROP** speed-oracle only (`diagnostics` feature).
//! E5.10: [`mapped_qwt`] — `NOVAQWT1` flatten / open (W0); no LoudsStore cutover.
//! No SPARQL, no production LoudsStore replacement. Product path is Ring A + D2.

//! # Indexing convention
//!
//! **All public APIs use zero-based half-open ranges `[start, end)`.**
//!
//! The paper (IS 2026 / SIGMOD Ring) uses 1-based inclusive intervals.
//! Translations are documented beside each primitive. `qwt` itself is
//! 0-based: `rank(c, i)` counts occurrences in `[0, i)`, and
//! `select(c, k)` returns the position of the `(k+1)`-th occurrence
//! (0-based occurrence index).
//!
//! # Paper formulas (1-based inclusive) → zero-based exclusive
//!
//! Let paper positions be 1-based; our `i` is 0-based (`i_paper = i + 1`).
//!
//! **Eq. (2)** paper: `F_j(i) := A_j[c] + rank_c(C_j, i)` with `c = C_j[i]`.
//! - Paper `rank_c(C, i)` counts in `C[1..i]` (inclusive).
//! - qwt `rank(c, i+1)` counts in `[0, i]` ≡ paper positions 1..i+1.
//! - Paper `A_j[c]` = # of symbols `< c` (same for both conventions if A is
//!   built as prefix counts with `A[0]=0` and `A[c] = count(< c)` for
//!   0-based symbols).
//! - Zero-based: `F(i) = A[c] + rank(c, i+1) - 1`.
//!
//! **Eq. (3)** paper: find `c` s.t. `A[c] < i' ≤ A[c+1]`, then
//! `F^{-1}(i') := select_c(C, i' - A[c])` (1-based select occurrence).
//! - Zero-based position `i'`: find `c` with `A[c] ≤ i' < A[c+1]`, then
//!   `F_inv(i') = select(c, i' - A[c])` (0-based occurrence).
//!
//! **Eq. (4)** paper backward step on range `[s,e]` (1-based inclusive) for
//! symbol `c`:
//! `s' := A[c] + rank_c(C, s-1) + 1`, `e' := A[c] + rank_c(C, e)`.
//! - Zero-based half-open `[s, e)`:
//!   `start' = A[c] + rank(c, s)`, `end' = A[c] + rank(c, e)`.
//!
//! # Lazy leap discipline
//!
//! `range_next_value` uses galloping + binary search over `get` only.
//! It never materializes all distinct symbols (`occs_range` is forbidden
//! on the hot path — see E5.5).

#[cfg(feature = "ring-huffman-cp")]
use crate::huff_cp::HuffColP;
use qwt::mem_dbg::{MemSize, SizeFlags};
use qwt::{AccessUnsigned, QWT256, RankUnsigned, SelectUnsigned};
use std::sync::atomic::{AtomicU64, Ordering};

// ── Predicate column substrate (E5.9B Phase 3) ───────────────────────────────

/// Last-column substrate for **C_p only**.
///
/// Under `ring-huffman-cp`, the product default is [`PredicateColumn::Huff`].
/// Without that feature, only [`PredicateColumn::Qwt`] is available.
/// mmap (`NOVARNG1`) dual-format flattens Huff C_p as HQWA (`RNG_FLAG_HUFF_CP`).
pub enum PredicateColumn {
    /// Balanced QWT256 (product default).
    Qwt(QWT256<u32>),
    /// Locally-densified HQWT256 + O(σ_P) RDI/RNV fallback.
    #[cfg(feature = "ring-huffman-cp")]
    Huff(HuffColP),
}

impl PredicateColumn {
    #[inline]
    pub fn is_huff(&self) -> bool {
        match self {
            Self::Qwt(_) => false,
            #[cfg(feature = "ring-huffman-cp")]
            Self::Huff(_) => true,
        }
    }

    /// Borrow the QWT256 when this arm is plain Qwt (needed by NOVARNG1 flatten).
    #[inline]
    pub fn as_qwt(&self) -> Option<&QWT256<u32>> {
        match self {
            Self::Qwt(w) => Some(w),
            #[cfg(feature = "ring-huffman-cp")]
            Self::Huff(_) => None,
        }
    }

    /// Borrow the HuffColP when this arm is Huffman (NOVARNG1 HQWA flatten).
    #[cfg(feature = "ring-huffman-cp")]
    #[inline]
    pub fn as_huff(&self) -> Option<&HuffColP> {
        match self {
            Self::Qwt(_) => None,
            Self::Huff(h) => Some(h),
        }
    }

    #[inline]
    pub fn access(&self, pos: u32) -> u32 {
        match self {
            Self::Qwt(w) => w.get(pos as usize).expect("access in bounds"),
            #[cfg(feature = "ring-huffman-cp")]
            Self::Huff(h) => h.access(pos),
        }
    }

    #[inline]
    pub fn rank(&self, symbol: u32, position: u32) -> u32 {
        match self {
            Self::Qwt(w) => w.rank(symbol, position as usize).unwrap_or(0) as u32,
            #[cfg(feature = "ring-huffman-cp")]
            Self::Huff(h) => h.rank(symbol, position),
        }
    }

    #[inline]
    pub fn select(&self, symbol: u32, occurrence: u32) -> Option<u32> {
        match self {
            Self::Qwt(w) => w.select(symbol, occurrence as usize).map(|p| p as u32),
            #[cfg(feature = "ring-huffman-cp")]
            Self::Huff(h) => h.select(symbol, occurrence),
        }
    }

    #[inline]
    pub fn mem_bytes(&self) -> usize {
        match self {
            Self::Qwt(w) => w.mem_size(SizeFlags::default()),
            #[cfg(feature = "ring-huffman-cp")]
            Self::Huff(h) => h.mem_bytes(),
        }
    }

    /// RNV: native guided on Qwt; O(σ_P) scan on Huff.
    #[inline]
    pub fn range_next_value(&self, start: u32, end: u32, target: u32) -> Option<u32> {
        match self {
            Self::Qwt(w) => w.range_next_value(start as usize..end as usize, target),
            #[cfg(feature = "ring-huffman-cp")]
            Self::Huff(h) => h.range_next_value_scan(start, end, target),
        }
    }

    /// Diagnostic: wavelet levels (0 for Huff — variable-depth codes).
    #[inline]
    pub fn n_levels(&self) -> usize {
        match self {
            Self::Qwt(w) => w.n_levels(),
            #[cfg(feature = "ring-huffman-cp")]
            Self::Huff(_) => 0,
        }
    }
}

// ── Column / range ────────────────────────────────────────────────────────────

/// Which cyclic last-column (and its matching A array).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Col {
    /// C_o — last of T_spo; F_o maps SPO → OSP.
    O,
    /// C_p — last of T_osp; F_p maps OSP → POS.
    P,
    /// C_s — last of T_pos; F_s maps POS → SPO.
    S,
}

/// Half-open row range `[start, end)` in a table of length `n`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RowRange {
    pub start: u32,
    pub end: u32,
}

impl RowRange {
    #[inline]
    pub fn empty() -> Self {
        Self { start: 0, end: 0 }
    }

    #[inline]
    pub fn full(n: u32) -> Self {
        Self { start: 0, end: n }
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }

    #[inline]
    pub fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }
}

// ── Counters ─────────────────────────────────────────────────────────────────

/// Process-wide primitive counters (for harnesses). Thread-safe.
#[derive(Default)]
pub struct GlobalCounters {
    pub access: AtomicU64,
    pub rank: AtomicU64,
    pub select: AtomicU64,
    pub f: AtomicU64,
    pub f_inv: AtomicU64,
    pub backward_step: AtomicU64,
    pub range_count: AtomicU64,
    /// Legacy alias total for any RNV path (scan + native).
    pub range_next_value: AtomicU64,
    // ── scan RNV (diagnostic oracle) ──────────────────────────────────────
    pub rnv_scan_calls: AtomicU64,
    pub rnv_scan_get_probes: AtomicU64,
    // ── native guided RNV (gate path) ─────────────────────────────────────
    pub rnv_native_calls: AtomicU64,
    pub rnv_native_levels: AtomicU64,
    pub rnv_native_rank_probes: AtomicU64,
    pub rnv_native_backtracks: AtomicU64,
    /// Deprecated alias kept for harnesses that still read `rnv_get_probes`.
    pub rnv_get_probes: AtomicU64,
    // ── range_distinct_iter (E5.7C stateful enumerator) ───────────────────
    pub rdi_calls: AtomicU64,
    pub rdi_symbols: AtomicU64,
    pub rdi_rank_probes: AtomicU64,
    pub rdi_frames_popped: AtomicU64,
}

impl GlobalCounters {
    pub fn reset(&self) {
        for a in [
            &self.access,
            &self.rank,
            &self.select,
            &self.f,
            &self.f_inv,
            &self.backward_step,
            &self.range_count,
            &self.range_next_value,
            &self.rnv_scan_calls,
            &self.rnv_scan_get_probes,
            &self.rnv_native_calls,
            &self.rnv_native_levels,
            &self.rnv_native_rank_probes,
            &self.rnv_native_backtracks,
            &self.rnv_get_probes,
            &self.rdi_calls,
            &self.rdi_symbols,
            &self.rdi_rank_probes,
            &self.rdi_frames_popped,
        ] {
            a.store(0, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            access: self.access.load(Ordering::Relaxed),
            rank: self.rank.load(Ordering::Relaxed),
            select: self.select.load(Ordering::Relaxed),
            f: self.f.load(Ordering::Relaxed),
            f_inv: self.f_inv.load(Ordering::Relaxed),
            backward_step: self.backward_step.load(Ordering::Relaxed),
            range_count: self.range_count.load(Ordering::Relaxed),
            range_next_value: self.range_next_value.load(Ordering::Relaxed),
            rnv_scan_calls: self.rnv_scan_calls.load(Ordering::Relaxed),
            rnv_scan_get_probes: self.rnv_scan_get_probes.load(Ordering::Relaxed),
            rnv_native_calls: self.rnv_native_calls.load(Ordering::Relaxed),
            rnv_native_levels: self.rnv_native_levels.load(Ordering::Relaxed),
            rnv_native_rank_probes: self.rnv_native_rank_probes.load(Ordering::Relaxed),
            rnv_native_backtracks: self.rnv_native_backtracks.load(Ordering::Relaxed),
            rnv_get_probes: self.rnv_get_probes.load(Ordering::Relaxed),
            rdi_calls: self.rdi_calls.load(Ordering::Relaxed),
            rdi_symbols: self.rdi_symbols.load(Ordering::Relaxed),
            rdi_rank_probes: self.rdi_rank_probes.load(Ordering::Relaxed),
            rdi_frames_popped: self.rdi_frames_popped.load(Ordering::Relaxed),
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct CounterSnapshot {
    pub access: u64,
    pub rank: u64,
    pub select: u64,
    pub f: u64,
    pub f_inv: u64,
    pub backward_step: u64,
    pub range_count: u64,
    pub range_next_value: u64,
    pub rnv_scan_calls: u64,
    pub rnv_scan_get_probes: u64,
    pub rnv_native_calls: u64,
    pub rnv_native_levels: u64,
    pub rnv_native_rank_probes: u64,
    pub rnv_native_backtracks: u64,
    pub rnv_get_probes: u64,
    pub rdi_calls: u64,
    pub rdi_symbols: u64,
    pub rdi_rank_probes: u64,
    pub rdi_frames_popped: u64,
}

impl CounterSnapshot {
    pub fn total_qwt_ops(self) -> u64 {
        self.access
            + self.rank
            + self.select
            + self.rnv_scan_get_probes
            + self.rnv_native_rank_probes
            + self.rdi_rank_probes
    }
}

// ── CyclicRing ───────────────────────────────────────────────────────────────

/// Componentized Ring A footprint (E5.6 / Phase-0 regression card).
///
/// `total()` matches historical [`CyclicRing::mem_bytes`] (QWT MemSize + A + 136 shell).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RingMemBreakdown {
    pub c_o: usize,
    pub c_p: usize,
    pub c_s: usize,
    pub a_o: usize,
    pub a_p: usize,
    pub a_s: usize,
    /// Struct shell / padding allowance (fixed 136 in E5.6 accounting).
    pub shell: usize,
    pub n: u32,
    pub universe: u32,
    pub ns: u32,
    pub np: u32,
    pub no: u32,
    pub c_p_is_huff: bool,
}

impl RingMemBreakdown {
    #[inline]
    pub fn total(&self) -> usize {
        self.c_o + self.c_p + self.c_s + self.a_o + self.a_p + self.a_s + self.shell
    }

    #[inline]
    pub fn qwt_total(&self) -> usize {
        self.c_o + self.c_p + self.c_s
    }

    #[inline]
    pub fn a_total(&self) -> usize {
        self.a_o + self.a_p + self.a_s
    }

    /// Bytes per triple (total / n); 0 if empty.
    #[inline]
    pub fn bytes_per_triple(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.total() as f64 / f64::from(self.n)
        }
    }
}

/// Paper-faithful Ring A: three cyclic last-columns + cumulative A arrays.
///
/// Symbols are dense local ids in `[0, U)`. Construction densifies S/P/O
/// into a **shared** 0-based universe so that A arrays and F maps are
/// well-defined across columns (paper assumes Σ = [1..U]; we use [0..U)).
pub struct CyclicRing {
    c_o: QWT256<u32>,
    /// C_p substrate: Huffman under `ring-huffman-cp` (product default);
    /// Qwt otherwise or via explicit `*_qwt_cp` builders.
    c_p: PredicateColumn,
    c_s: QWT256<u32>,
    /// A_o[k] = |{ i : C_o[i] < k }|, length U+1.
    a_o: Vec<u32>,
    a_p: Vec<u32>,
    a_s: Vec<u32>,
    /// Number of triples (= length of each C_*).
    pub n: u32,
    /// Shared alphabet size U (max dense id + 1).
    pub universe: u32,
    /// Role-local S/P/O sizes in the shared alphabet layout
    /// (S∈[0,ns), P∈[ns,ns+np), O∈[ns+np,U)). Zero when unknown (legacy
    /// `build_shared` without role sizes).
    pub ns: u32,
    pub np: u32,
    pub no: u32,
    /// Optional external counters (shared with harness).
    counters: Option<&'static GlobalCounters>,
    /// Hot-path allocation detector (range_next_value must not allocate).
    /// Atomic so `CyclicRing` / store wrappers are `Send + Sync` for QuadStore.
    hot_path_allocs: AtomicU64,
}

impl CyclicRing {
    /// Attach process-global counters (typically a `static`).
    pub fn with_counters(mut self, c: &'static GlobalCounters) -> Self {
        self.counters = Some(c);
        self
    }

    pub fn hot_path_allocs(&self) -> u64 {
        self.hot_path_allocs.load(Ordering::Relaxed)
    }

    pub fn reset_hot_path_allocs(&self) {
        self.hot_path_allocs.store(0, Ordering::Relaxed);
    }

    // ── build ────────────────────────────────────────────────────────────

    /// Build Ring A from dense local-id triples `[s, p, o]` with
    /// `s < ns`, `p < np`, `o < no`. Internally remaps to a **shared**
    /// universe of size `ns + np + no` with disjoint ranges:
    /// S in `[0, ns)`, P in `[ns, ns+np)`, O in `[ns+np, U)`.
    ///
    /// This preserves relative order within each role while giving a single
    /// Σ for A arrays (paper assumption). Returned ring reports
    /// `universe = ns+np+no`.
    /// Build Ring A from role-local triples. Under `ring-huffman-cp`, C_p is
    /// Huffman (product default); otherwise Qwt.
    pub fn build_from_role_local(triples: &[[u32; 3]], ns: u32, np: u32, no: u32) -> Self {
        let s_base = 0u32;
        let p_base = ns;
        let o_base = ns + np;
        let mapped: Vec<[u32; 3]> = triples
            .iter()
            .map(|&[s, p, o]| [s_base + s, p_base + p, o_base + o])
            .collect();
        #[cfg(feature = "ring-huffman-cp")]
        {
            Self::build_shared_inner(&mapped, ns + np + no, ns, np, no, true)
        }
        #[cfg(not(feature = "ring-huffman-cp"))]
        {
            let mut ring = Self::build_shared_inner(&mapped, ns + np + no, ns, np, no, false);
            ring.ns = ns;
            ring.np = np;
            ring.no = no;
            ring
        }
    }

    /// Explicit Qwt C_p (differential / baseline). Same role-local remap.
    pub fn build_from_role_local_qwt_cp(triples: &[[u32; 3]], ns: u32, np: u32, no: u32) -> Self {
        let s_base = 0u32;
        let p_base = ns;
        let o_base = ns + np;
        let mapped: Vec<[u32; 3]> = triples
            .iter()
            .map(|&[s, p, o]| [s_base + s, p_base + p, o_base + o])
            .collect();
        Self::build_shared_inner(&mapped, ns + np + no, ns, np, no, false)
    }

    /// Build **Ring A** from triples already in a shared dense alphabet `[0, U)`.
    ///
    /// Cyclic class: SPO → OSP → POS (last columns C_o, C_p, C_s).
    /// Under `ring-huffman-cp`, C_p defaults to Huffman via unique-P densify.
    /// Role sizes (`ns`/`np`/`no`) are left 0 here.
    pub fn build_shared(triples: &[[u32; 3]], universe: u32) -> Self {
        #[cfg(feature = "ring-huffman-cp")]
        {
            Self::build_shared_inner(triples, universe, 0, 0, 0, true)
        }
        #[cfg(not(feature = "ring-huffman-cp"))]
        {
            Self::build_shared_inner(triples, universe, 0, 0, 0, false)
        }
    }

    /// Explicit Qwt C_p on a shared alphabet (differential / baseline).
    pub fn build_shared_qwt_cp(triples: &[[u32; 3]], universe: u32) -> Self {
        Self::build_shared_inner(triples, universe, 0, 0, 0, false)
    }

    /// Alias: Huffman C_p with role-local remap (same as default under feature).
    #[cfg(feature = "ring-huffman-cp")]
    pub fn build_from_role_local_huff_cp(triples: &[[u32; 3]], ns: u32, np: u32, no: u32) -> Self {
        Self::build_from_role_local(triples, ns, np, no)
    }

    fn build_shared_inner(
        triples: &[[u32; 3]],
        universe: u32,
        ns: u32,
        np: u32,
        no: u32,
        huff_cp: bool,
    ) -> Self {
        let n = triples.len() as u32;
        // T_spo
        let mut spo = triples.to_vec();
        spo.sort_unstable_by_key(|t| (t[0], t[1], t[2]));
        let c_o_vals: Vec<u32> = spo.iter().map(|t| t[2]).collect();

        // T_osp: move o to front → (o,s,p)
        let mut osp: Vec<[u32; 3]> = spo.iter().map(|t| [t[2], t[0], t[1]]).collect();
        osp.sort_unstable();
        let c_p_vals: Vec<u32> = osp.iter().map(|t| t[2]).collect();

        // T_pos: move p to front of osp → (p,o,s)
        let mut pos: Vec<[u32; 3]> = osp.iter().map(|t| [t[2], t[0], t[1]]).collect();
        pos.sort_unstable();
        let c_s_vals: Vec<u32> = pos.iter().map(|t| t[2]).collect();

        let a_o = cumulative_a(&c_o_vals, universe as usize);
        let a_p = cumulative_a(&c_p_vals, universe as usize);
        let a_s = cumulative_a(&c_s_vals, universe as usize);

        let c_p = {
            #[cfg(feature = "ring-huffman-cp")]
            {
                if huff_cp {
                    if np > 0 {
                        // Contiguous role-local P range [ns, ns+np).
                        PredicateColumn::Huff(HuffColP::build_from_shared(&c_p_vals, ns, np))
                    } else {
                        // Compact shared alphabet: unique-P densify.
                        PredicateColumn::Huff(HuffColP::build_from_column(&c_p_vals))
                    }
                } else {
                    PredicateColumn::Qwt(QWT256::from(c_p_vals))
                }
            }
            #[cfg(not(feature = "ring-huffman-cp"))]
            {
                let _ = huff_cp;
                PredicateColumn::Qwt(QWT256::from(c_p_vals))
            }
        };

        Self {
            c_o: QWT256::from(c_o_vals),
            c_p,
            c_s: QWT256::from(c_s_vals),
            a_o,
            a_p,
            a_s,
            n,
            universe,
            ns,
            np,
            no,
            counters: None,
            hot_path_allocs: AtomicU64::new(0),
        }
    }

    /// Build **Ring B** (reverse cyclic class) from role-local triples.
    ///
    /// Same shared-alphabet remap as [`Self::build_from_role_local`].
    ///
    /// Cyclic class: OPS → SOP → PSO (last columns C_s, C_p, C_o).
    /// DROP (E5.8): research/applications/oracle only — requires `diagnostics` feature.
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn build_ring_b_from_role_local(triples: &[[u32; 3]], ns: u32, np: u32, no: u32) -> Self {
        let s_base = 0u32;
        let p_base = ns;
        let o_base = ns + np;
        let mapped: Vec<[u32; 3]> = triples
            .iter()
            .map(|&[s, p, o]| [s_base + s, p_base + p, o_base + o])
            .collect();
        let mut ring = Self::build_ring_b_shared(&mapped, ns + np + no);
        ring.ns = ns;
        ring.np = np;
        ring.no = no;
        ring
    }

    /// Build **Ring B** from shared-alphabet triples.
    ///
    /// ```text
    /// T_ops  ordered (o,p,s) → last C_s  (Col::S)
    /// T_sop  ordered (s,o,p) → last C_p  (Col::P)
    /// T_pso  ordered (p,s,o) → last C_o  (Col::O)
    /// ```
    ///
    /// Field names on `Col` still mean the **role of the last column symbol**,
    /// not the table order. Lead ranges:
    /// - `lead_range(S)` partitions T_sop by s (after F_s from OPS)
    /// - `lead_range(P)` partitions T_pso by p
    /// - `lead_range(O)` partitions T_ops by o
    ///
    /// DROP (E5.8): research/applications/oracle only — requires `diagnostics` feature.
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn build_ring_b_shared(triples: &[[u32; 3]], universe: u32) -> Self {
        let n = triples.len() as u32;
        // T_ops: (o, p, s)
        let mut ops: Vec<[u32; 3]> = triples.iter().map(|t| [t[2], t[1], t[0]]).collect();
        ops.sort_unstable();
        let c_s_vals: Vec<u32> = ops.iter().map(|t| t[2]).collect(); // last = s

        // T_sop: move s to front of OPS → (s, o, p)
        let mut sop: Vec<[u32; 3]> = ops.iter().map(|t| [t[2], t[0], t[1]]).collect();
        sop.sort_unstable();
        let c_p_vals: Vec<u32> = sop.iter().map(|t| t[2]).collect(); // last = p

        // T_pso: move p to front of SOP → (p, s, o)
        let mut pso: Vec<[u32; 3]> = sop.iter().map(|t| [t[2], t[0], t[1]]).collect();
        pso.sort_unstable();
        let c_o_vals: Vec<u32> = pso.iter().map(|t| t[2]).collect(); // last = o

        let a_o = cumulative_a(&c_o_vals, universe as usize);
        let a_p = cumulative_a(&c_p_vals, universe as usize);
        let a_s = cumulative_a(&c_s_vals, universe as usize);

        Self {
            c_o: QWT256::from(c_o_vals),
            c_p: PredicateColumn::Qwt(QWT256::from(c_p_vals)),
            c_s: QWT256::from(c_s_vals),
            a_o,
            a_p,
            a_s,
            n,
            universe,
            ns: 0,
            np: 0,
            no: 0,
            counters: None,
            hot_path_allocs: AtomicU64::new(0),
        }
    }

    /// Exact complete bytes (qwt MemSize + A arrays + shell), matching E5.6 accounting.
    pub fn mem_bytes(&self) -> usize {
        self.mem_breakdown().total()
    }

    /// Componentized footprint (E5.6 / Phase-0 regression card discipline).
    pub fn mem_breakdown(&self) -> RingMemBreakdown {
        let c_o = self.c_o.mem_size(SizeFlags::default());
        let c_p = self.c_p.mem_bytes();
        let c_s = self.c_s.mem_size(SizeFlags::default());
        let a_o = self.a_o.len() * 4;
        let a_p = self.a_p.len() * 4;
        let a_s = self.a_s.len() * 4;
        RingMemBreakdown {
            c_o,
            c_p,
            c_s,
            a_o,
            a_p,
            a_s,
            shell: 136,
            n: self.n,
            universe: self.universe,
            ns: self.ns,
            np: self.np,
            no: self.no,
            c_p_is_huff: self.c_p_is_huff(),
        }
    }

    /// Whether C_p is the Huffman substrate (feature `ring-huffman-cp` only).
    #[inline]
    pub fn c_p_is_huff(&self) -> bool {
        self.c_p.is_huff()
    }

    /// Borrow the C_p substrate (Qwt or Huff).
    #[inline]
    pub fn c_p_substrate(&self) -> &PredicateColumn {
        &self.c_p
    }

    // ── column accessors ─────────────────────────────────────────────────

    /// QWT256 for O/S only. Panics if called with `Col::P` when C_p is Huffman
    /// (use [`Self::col_qwt`] which returns `Option` for P, or dispatch via
    /// access/rank/select which handle both arms).
    #[inline]
    fn col_wt_os(&self, col: Col) -> &QWT256<u32> {
        match col {
            Col::O => &self.c_o,
            Col::S => &self.c_s,
            Col::P => self
                .c_p
                .as_qwt()
                .expect("col_wt_os(Col::P) requires Qwt C_p substrate"),
        }
    }

    #[inline]
    fn col_a(&self, col: Col) -> &[u32] {
        match col {
            Col::O => &self.a_o,
            Col::P => &self.a_p,
            Col::S => &self.a_s,
        }
    }

    /// Public QWT column accessor for E5.10 flatten / differential gates.
    ///
    /// Returns `None` for `Col::P` when C_p is the Huffman arm (cannot flatten
    /// HQWT into `NOVARNG1` QWTA sections — in-memory experiment only).
    #[inline]
    pub fn col_qwt(&self, col: Col) -> Option<&QWT256<u32>> {
        match col {
            Col::O => Some(&self.c_o),
            Col::S => Some(&self.c_s),
            Col::P => self.c_p.as_qwt(),
        }
    }

    /// Public A-array accessor for E5.10 flatten / differential gates.
    #[inline]
    pub fn col_a_slice(&self, col: Col) -> &[u32] {
        self.col_a(col)
    }

    #[inline]
    fn bump(&self, f: impl Fn(&GlobalCounters)) {
        if let Some(c) = self.counters {
            f(c);
        }
    }

    // ── primitives ───────────────────────────────────────────────────────

    /// `access(column, position)` — `C[i]`.
    #[inline]
    pub fn access(&self, col: Col, pos: u32) -> u32 {
        self.bump(|c| {
            c.access.fetch_add(1, Ordering::Relaxed);
        });
        match col {
            Col::P => self.c_p.access(pos),
            Col::O | Col::S => self
                .col_wt_os(col)
                .get(pos as usize)
                .expect("access in bounds"),
        }
    }

    /// `rank(column, symbol, position)` — # of `symbol` in `[0, position)`.
    #[inline]
    pub fn rank(&self, col: Col, symbol: u32, position: u32) -> u32 {
        self.bump(|c| {
            c.rank.fetch_add(1, Ordering::Relaxed);
        });
        match col {
            Col::P => self.c_p.rank(symbol, position),
            Col::O | Col::S => self
                .col_wt_os(col)
                .rank(symbol, position as usize)
                .unwrap_or(0) as u32,
        }
    }

    /// `select(column, symbol, occurrence)` — position of 0-based `occurrence`-th
    /// of `symbol`.
    #[inline]
    pub fn select(&self, col: Col, symbol: u32, occurrence: u32) -> Option<u32> {
        self.bump(|c| {
            c.select.fetch_add(1, Ordering::Relaxed);
        });
        match col {
            Col::P => self.c_p.select(symbol, occurrence),
            Col::O | Col::S => self
                .col_wt_os(col)
                .select(symbol, occurrence as usize)
                .map(|p| p as u32),
        }
    }

    /// Paper Eq. (2) zero-based: `F_j(i) = A[c] + rank(c, i+1) - 1`, `c = C[i]`.
    ///
    /// Maps a row in the table where `C_j` is last to the table where `j` is first.
    #[inline]
    pub fn f(&self, col: Col, i: u32) -> u32 {
        self.bump(|c| {
            c.f.fetch_add(1, Ordering::Relaxed);
        });
        debug_assert!(i < self.n);
        let c_sym = self.access(col, i);
        let a = self.col_a(col);
        let ac = a[c_sym as usize];
        // rank in [0, i] inclusive = rank(c, i+1)
        let r = self.rank(col, c_sym, i + 1);
        ac + r - 1
    }

    /// Paper Eq. (3) zero-based inverse.
    #[inline]
    pub fn f_inverse(&self, col: Col, i_prime: u32) -> u32 {
        self.bump(|c| {
            c.f_inv.fetch_add(1, Ordering::Relaxed);
        });
        debug_assert!(i_prime < self.n);
        let a = self.col_a(col);
        // Find c with A[c] <= i' < A[c+1]
        let c_sym = a.partition_point(|&x| x <= i_prime) as u32 - 1;
        let occ = i_prime - a[c_sym as usize];
        self.select(col, c_sym, occ)
            .expect("f_inverse select must exist")
    }

    /// Paper Eq. (4) backward step — zero-based half-open.
    ///
    /// Given range `r` of triples sharing prefix X in the table where `C_j`
    /// is last, return the range of triples sharing prefix `c·X` in the next
    /// table (where `j` is first).
    ///
    /// ```text
    /// start' = A[c] + rank(c, r.start)
    /// end'   = A[c] + rank(c, r.end)
    /// ```
    #[inline]
    pub fn backward_step(&self, col: Col, r: RowRange, symbol: u32) -> RowRange {
        self.bump(|c| {
            c.backward_step.fetch_add(1, Ordering::Relaxed);
        });
        if r.is_empty() {
            return RowRange::empty();
        }
        let a = self.col_a(col);
        let ac = a[symbol as usize];
        let start = ac + self.rank(col, symbol, r.start);
        let end = ac + self.rank(col, symbol, r.end);
        RowRange { start, end }
    }

    /// Restrict range to rows whose column value equals `symbol` (same column).
    /// Used as the "forward step" / child-range open within one table.
    ///
    /// ```text
    /// start' = rank(c, r.start)   // occurrence index of first c in range
    /// ```
    /// Actually for same-column restriction the sub-range in **row coordinates**
    /// is the contiguous run of `c` inside the sorted column segment — but
    /// C_* are BWT-like last columns, not necessarily sorted. Within a
    /// *prefix-induced* range after LF mapping, the next attribute's values
    /// appear as the **first column** of the next table, which is sorted.
    ///
    /// For same-column restriction of a BWT last column, the rows holding
    /// `c` in `[s,e)` are not necessarily contiguous in that column.
    /// Forward restriction for LTJ is done via backward_step into the next
    /// table (where the bound attribute is leading and contiguous).
    ///
    /// We still expose `restrict_eq` as the LF image of those rows:
    /// `backward_step` is the correct primitive.
    #[inline]
    pub fn restrict_eq(&self, col: Col, r: RowRange, symbol: u32) -> RowRange {
        self.backward_step(col, r, symbol)
    }

    /// Default RNV: **native** guided O(log σ) (E5.7B.1). Prefer explicit
    /// `range_next_value_native` / `range_next_value_scan` in gate harnesses
    /// so results remain attributable.
    #[inline]
    pub fn range_next_value(&self, col: Col, r: RowRange, target: u32) -> Option<u32> {
        self.range_next_value_native(col, r, target)
    }

    /// Diagnostic oracle: row-scan via `access`. O(|range| · log σ).
    /// **Rejected for LFTJ** (E5.7); kept only for differential correctness.
    pub fn range_next_value_scan(&self, col: Col, r: RowRange, target: u32) -> Option<u32> {
        self.bump(|c| {
            c.range_next_value.fetch_add(1, Ordering::Relaxed);
            c.rnv_scan_calls.fetch_add(1, Ordering::Relaxed);
        });
        if r.is_empty() || target >= self.universe {
            return None;
        }
        let mut best: Option<u32> = None;
        for i in r.start..r.end {
            self.bump(|c| {
                c.rnv_scan_get_probes.fetch_add(1, Ordering::Relaxed);
                c.rnv_get_probes.fetch_add(1, Ordering::Relaxed);
            });
            // access() bumps its own counter
            let v = self.access(col, i);
            if v >= target {
                best = Some(match best {
                    Some(b) if b <= v => b,
                    _ => v,
                });
                if best == Some(target) {
                    return Some(target);
                }
            }
        }
        best
    }

    /// Native guided RNV (Qwt) or O(σ_P) scan (Huff C_p).
    /// Worst case O(log σ) rank work on Qwt; **independent of |range|**.
    /// Zero extra persistent bytes.
    pub fn range_next_value_native(&self, col: Col, r: RowRange, target: u32) -> Option<u32> {
        self.bump(|c| {
            c.range_next_value.fetch_add(1, Ordering::Relaxed);
            c.rnv_native_calls.fetch_add(1, Ordering::Relaxed);
        });
        if r.is_empty() || target >= self.universe {
            return None;
        }
        match col {
            Col::P => {
                self.bump(|c| {
                    // Huff: O(np) rank pairs; Qwt: levels proxy.
                    let levels = self.c_p.n_levels().max(1) as u64;
                    c.rnv_native_levels.fetch_add(levels, Ordering::Relaxed);
                    c.rnv_native_rank_probes
                        .fetch_add(levels * 8, Ordering::Relaxed);
                });
                self.c_p.range_next_value(r.start, r.end, target)
            }
            Col::O | Col::S => {
                let wt = self.col_wt_os(col);
                self.bump(|c| {
                    c.rnv_native_levels
                        .fetch_add(wt.n_levels() as u64, Ordering::Relaxed);
                    c.rnv_native_rank_probes
                        .fetch_add((wt.n_levels() as u64) * 8, Ordering::Relaxed);
                });
                wt.range_next_value(r.start as usize..r.end as usize, target)
            }
        }
    }

    /// Stateful distinct-symbol enumeration over a column range (E5.7C / E5.9A).
    ///
    /// On Qwt columns: opens wavelet RDI once (O(log σ) scratch).
    /// On Huffman C_p: materializes O(σ_P) scan into a small vec (σ_P schema-sized)
    /// and yields via [`CyclicRangeDistinctIter::HuffScan`].
    pub fn range_distinct_iter<'a>(&'a self, col: Col, r: RowRange) -> CyclicRangeDistinctIter<'a> {
        self.bump(|c| {
            c.rdi_calls.fetch_add(1, Ordering::Relaxed);
        });
        match col {
            Col::P => match &self.c_p {
                PredicateColumn::Qwt(wt) => {
                    let inner = if r.is_empty() {
                        wt.range_distinct_iter(0..0)
                    } else {
                        wt.range_distinct_iter(r.start as usize..r.end as usize)
                    };
                    CyclicRangeDistinctIter {
                        kind: RdiKind::Qwt { inner },
                        counters: self.counters,
                        finished: false,
                    }
                }
                #[cfg(feature = "ring-huffman-cp")]
                PredicateColumn::Huff(h) => {
                    let pairs = h.range_distinct_scan(r.start, r.end);
                    self.bump(|c| {
                        c.rdi_rank_probes
                            .fetch_add((h.np() as u64) * 2, Ordering::Relaxed);
                    });
                    CyclicRangeDistinctIter {
                        kind: RdiKind::HuffScan { pairs, idx: 0 },
                        counters: self.counters,
                        finished: false,
                    }
                }
            },
            Col::O | Col::S => {
                let wt = self.col_wt_os(col);
                let inner = if r.is_empty() {
                    wt.range_distinct_iter(0..0)
                } else {
                    wt.range_distinct_iter(r.start as usize..r.end as usize)
                };
                CyclicRangeDistinctIter {
                    kind: RdiKind::Qwt { inner },
                    counters: self.counters,
                    finished: false,
                }
            }
        }
    }

    /// Enumerate all distinct symbols in range via RDI (convenience).
    /// Returns count of distinct symbols; counts are black-boxed via counters.
    pub fn range_distinct_count(&self, col: Col, r: RowRange) -> u32 {
        let mut n = 0u32;
        let mut it = self.range_distinct_iter(col, r);
        while let Some((_sym, cnt)) = it.next() {
            let _ = cnt;
            n += 1;
        }
        n
    }

    /// `range_count`: number of symbols in `C[r]` whose value is in
    /// `[sym_lo, sym_hi]` (inclusive symbol bounds).
    ///
    /// Hot-path safe for **narrow** symbol spans. Wide spans use row scan
    /// with `get` (no distinct-symbol Vec).
    pub fn range_count(&self, col: Col, sym_lo: u32, sym_hi: u32, r: RowRange) -> u32 {
        self.bump(|c| {
            c.range_count.fetch_add(1, Ordering::Relaxed);
        });
        if r.is_empty() || sym_lo > sym_hi {
            return 0;
        }
        let sym_hi = sym_hi.min(self.universe.saturating_sub(1));
        let span = sym_hi.saturating_sub(sym_lo).saturating_add(1);
        if span <= 256 {
            let mut sum = 0u32;
            for s in sym_lo..=sym_hi {
                sum += self.rank(col, s, r.end) - self.rank(col, s, r.start);
            }
            return sum;
        }
        let mut sum = 0u32;
        for i in r.start..r.end {
            let v = self.access(col, i);
            if v >= sym_lo && v <= sym_hi {
                sum += 1;
            }
        }
        sum
    }

    // ── high-level helpers for tests / traces ────────────────────────────

    /// Full table range for T_spo (rows of C_o).
    #[inline]
    pub fn n(&self) -> u32 {
        self.n
    }

    pub fn full_range(&self) -> RowRange {
        RowRange::full(self.n)
    }

    /// Leading-column range for symbol `c` as first attribute of the table
    /// after LF via `col` (i.e. A[c] .. A[c+1]).
    pub fn lead_range(&self, col: Col, symbol: u32) -> RowRange {
        let a = self.col_a(col);
        RowRange {
            start: a[symbol as usize],
            end: a[symbol as usize + 1],
        }
    }

    /// Enumerate all triples by walking T_spo via LF cycle.
    /// Returns triples in shared-alphabet coordinates `[s,p,o]`.
    pub fn enumerate_spo(&self) -> Vec<[u32; 3]> {
        let mut out = Vec::with_capacity(self.n as usize);
        for i in 0..self.n {
            // Row i in T_spo: we know o = C_o[i]. Map to OSP to read s from
            // the first column of OSP, which equals the symbol that LF'd.
            // Recover (s,p,o) by:
            //   o = C_o[i]
            //   i_osp = F_o(i);  the first column of T_osp is o (sorted), second is s.
            // We don't store first columns — recover via inverse structure:
            //   s is the symbol such that i_osp ∈ lead_range(O, o) ...
            // Actually: after F_o, position i_osp is in T_osp ordered by (o,s,p).
            // C_p[i_osp] = p. F_p(i_osp) = i_pos in T_pos ordered by (p,o,s), C_s[i_pos]=s.
            // Then F_s(i_pos) should return to i.
            let o = self.access(Col::O, i);
            let i_osp = self.f(Col::O, i);
            let p = self.access(Col::P, i_osp);
            let i_pos = self.f(Col::P, i_osp);
            let s = self.access(Col::S, i_pos);
            out.push([s, p, o]);
        }
        out
    }

    /// Prefix range for subject `s` in T_spo (via cycle: S leads T_spo after F_s).
    /// T_spo is ordered by (s,p,o). The range of s is obtained as lead_range of
    /// C_s / A_s because F_s maps T_pos → T_spo and A_s partitions T_spo by s.
    pub fn range_s(&self, s: u32) -> RowRange {
        self.lead_range(Col::S, s)
    }

    /// Range of (s,p) in T_spo: start from range_s(s), then restrict by p via
    /// backward_step on C_o?
    /// T_spo rows for subject s: R = lead_range(S, s).
    /// Within those rows, objects are C_o[R]. For SP prefix we need p.
    /// Path: R_spo = lead(S,s). Map each? Better:
    ///   Use T_pos lead for p, then backward for o/s — or:
    ///   From R = lead(S,s) on T_spo, values C_o are o's; p is middle.
    /// Standard Ring: to bind p after s, do forward restriction.
    /// Forward: within R on T_spo, find rows with middle p — not stored as WT of middle.
    /// Paper uses: start at T_pos[A_p[p]+1 ..] for p-bound, etc.
    ///
    /// For SP: start T_spo range for s = A_s[s]..A_s[s+1].
    /// Then backward_step is for extending *backward* (prepend). Forward extends
    /// by restricting the same column's range using the next attribute via
    /// mapping: map R through F_o to OSP, restrict by...
    ///
    /// Simpler oracle-aligned approach for tests: binary search explicit sorted
    /// tables is the oracle; for Ring SP range we implement:
    ///   1. R = lead(S, s)  // T_spo rows for s
    ///   2. For bound p: we need rows where the *middle* field is p.
    ///      Map i → F_o(i) → on OSP first is o, second s, third p=C_p.
    ///      Actually middle of T_spo is p: not a last column.
    ///
    /// From paper §3.5: patterns use the table whose order matches the bound
    /// prefix. For SP use T_spo; after fixing s, leap on p using the children
    /// in the trie — which are distinct p under s. Those p values appear as
    /// the sequence of second-column values under the s-group. Without storing
    /// the middle column, children are recovered by:
    ///   walking LF: for i in R, p is recovered as C_p[F_o(i)]?
    ///   T_spo row i = (s,p,o) with o=C_o[i]. F_o(i) = position of (o,s,p) in T_osp.
    ///   C_p[F_o(i)] = p. Yes!
    ///
    /// So distinct p under s: range_next_value over the multiset
    /// { C_p[F_o(i)] : i in R } — expensive if done naively.
    ///
    /// Efficient: R maps through F_o as a contiguous range?
    /// F_o is LF: images of a contiguous BWT range for a fixed first-column
    /// symbol are contiguous. Here R is contiguous in T_spo for fixed s
    /// (first column). The set {F_o(i): i in R} is NOT necessarily one
    /// interval in T_osp (different o's).
    ///
    /// For the primitive spike we implement SP range via:
    ///   lead on T_pso / reverse orientation — but we only have Ring A.
    ///   Ring A tables: spo, osp, pos.
    ///   SP prefix is native to T_spo. Children p under s: the paper's leap
    ///   uses range_next_value on an *implicit* sequence of p values.
    ///
    /// Practical E5.7 approach for SP:
    ///   Use T_pos? No.
    ///   Store nothing extra; for correctness tests of SP, compare against
    ///   sorted table. For navigation microbench, use range_next_value on
    ///   C_* ranges that *are* last columns (O under SP, S under OS, etc.).
    ///
    /// Leading single-attribute ranges (S, P, O) and two-step LF chains
    /// that only need last columns are implemented fully. SP as (s then p)
    /// uses: R_s = lead(S,s); then for a candidate p, filter by scanning
    /// F_o images — only for correctness on small sets.
    ///
    /// `range_sp` for correctness: collect via enumerate filter (tests only).
    pub fn range_o(&self, o: u32) -> RowRange {
        // O leads T_osp; A_o partitions T_osp by o.
        self.lead_range(Col::O, o)
    }

    pub fn range_p(&self, p: u32) -> RowRange {
        self.lead_range(Col::P, p)
    }
}

// ── URing speed oracle (E5.8) — DROP; research/applications/oracle only ───────────────
#[cfg(any(test, feature = "diagnostics"))]
mod uring_oracle {
    use super::*;

    /// Which physical cyclic orientation served a navigation step.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Orientation {
        /// Ring A: SPO → OSP → POS
        A,
        /// Ring B: OPS → SOP → PSO
        B,
    }

    /// Process-wide orientation counters for E5.8 (which ring served the work).
    #[derive(Default)]
    pub struct OrientationCounters {
        pub a_ops: AtomicU64,
        pub b_ops: AtomicU64,
        pub a_rdi_opens: AtomicU64,
        pub b_rdi_opens: AtomicU64,
        pub a_rnv: AtomicU64,
        pub b_rnv: AtomicU64,
        pub a_f: AtomicU64,
        pub b_f: AtomicU64,
    }

    impl OrientationCounters {
        pub fn reset(&self) {
            for a in [
                &self.a_ops,
                &self.b_ops,
                &self.a_rdi_opens,
                &self.b_rdi_opens,
                &self.a_rnv,
                &self.b_rnv,
                &self.a_f,
                &self.b_f,
            ] {
                a.store(0, Ordering::Relaxed);
            }
        }
    }

    /// Full URing = Ring A + Ring B (feature-gated speed oracle only).
    ///
    /// Not a product path. Used to measure reverse-orientation value vs single
    /// Ring A and six-order LOUDS.
    pub struct URing {
        pub a: CyclicRing,
        pub b: CyclicRing,
    }

    impl URing {
        /// Build both orientations from the same role-local triples.
        pub fn build_from_role_local(triples: &[[u32; 3]], ns: u32, np: u32, no: u32) -> Self {
            Self {
                a: CyclicRing::build_from_role_local(triples, ns, np, no),
                b: CyclicRing::build_ring_b_from_role_local(triples, ns, np, no),
            }
        }

        pub fn with_counters(mut self, c: &'static GlobalCounters) -> Self {
            self.a = self.a.with_counters(c);
            self.b = self.b.with_counters(c);
            self
        }

        pub fn mem_bytes(&self) -> usize {
            self.a.mem_bytes() + self.b.mem_bytes()
        }

        pub fn n(&self) -> u32 {
            self.a.n
        }

        pub fn universe(&self) -> u32 {
            self.a.universe
        }

        /// Outbound subject range (native on A: T_spo by s).
        #[inline]
        pub fn range_s_out(&self, s: u32) -> (Orientation, RowRange) {
            (Orientation::A, self.a.range_s(s))
        }

        /// Inbound object range (native on B: T_ops by o).
        #[inline]
        pub fn range_o_in(&self, o: u32) -> (Orientation, RowRange) {
            (Orientation::B, self.b.range_o(o))
        }

        /// Feature / object lead — prefer B for "who has object o" (inbound).
        #[inline]
        pub fn range_o_best(&self, o: u32) -> (Orientation, RowRange) {
            self.range_o_in(o)
        }

        /// Subject lead — prefer A for outbound star.
        #[inline]
        pub fn range_s_best(&self, s: u32) -> (Orientation, RowRange) {
            self.range_s_out(s)
        }

        /// Ring for orientation.
        #[inline]
        pub fn ring(&self, o: Orientation) -> &CyclicRing {
            match o {
                Orientation::A => &self.a,
                Orientation::B => &self.b,
            }
        }
    }
}

#[cfg(any(test, feature = "diagnostics"))]
pub use uring_oracle::{Orientation, OrientationCounters, URing};

// ── Stateful distinct iterator wrapper (E5.7C / E5.9A) ───────────────────────

// Qwt RangeDistinctIter is ~4KB of fixed stack state; boxing would add a heap
// alloc on every RDI open on the LFTJ hot path. Prefer stack layout.
#[allow(clippy::large_enum_variant)]
enum RdiKind<'a> {
    Qwt {
        inner: qwt::RangeDistinctIter<'a, u32, qwt::RSQVector256, false>,
    },
    /// Materialized O(σ_P) scan for Huffman C_p (σ_P is schema-sized).
    #[cfg_attr(not(feature = "ring-huffman-cp"), allow(dead_code))]
    HuffScan { pairs: Vec<(u32, u32)>, idx: usize },
}

/// Thin wrapper over qwt `RangeDistinctIter` (or Huff O(σ_P) scan) that bumps
/// Ring counters. Unified type so callers don't need to branch on substrate.
pub struct CyclicRangeDistinctIter<'a> {
    kind: RdiKind<'a>,
    counters: Option<&'static GlobalCounters>,
    finished: bool,
}

impl<'a> CyclicRangeDistinctIter<'a> {
    /// Next distinct symbol and its occurrence count in the open range.
    ///
    /// Named `next` intentionally (mirrors qwt RDI); not `Iterator` because
    /// finish also flushes diagnostic counters into `GlobalCounters`.
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<(u32, u32)> {
        match &mut self.kind {
            RdiKind::Qwt { inner } => match inner.next() {
                Some((sym, cnt)) => {
                    if let Some(c) = self.counters {
                        c.rdi_symbols.fetch_add(1, Ordering::Relaxed);
                    }
                    Some((sym, cnt as u32))
                }
                None => {
                    if !self.finished {
                        self.finished = true;
                        if let Some(c) = self.counters {
                            c.rdi_rank_probes
                                .fetch_add(inner.rank_probes, Ordering::Relaxed);
                            c.rdi_frames_popped
                                .fetch_add(inner.frames_popped, Ordering::Relaxed);
                        }
                    }
                    None
                }
            },
            RdiKind::HuffScan { pairs, idx } => {
                if *idx < pairs.len() {
                    let out = pairs[*idx];
                    *idx += 1;
                    if let Some(c) = self.counters {
                        c.rdi_symbols.fetch_add(1, Ordering::Relaxed);
                    }
                    Some(out)
                } else {
                    self.finished = true;
                    None
                }
            }
        }
    }

    /// Diagnostic: rank probes performed so far by the underlying DFS (Qwt only).
    pub fn rank_probes(&self) -> u64 {
        match &self.kind {
            RdiKind::Qwt { inner } => inner.rank_probes,
            RdiKind::HuffScan { pairs, .. } => pairs.len() as u64 * 2,
        }
    }

    pub fn frames_popped(&self) -> u64 {
        match &self.kind {
            RdiKind::Qwt { inner } => inner.frames_popped,
            RdiKind::HuffScan { .. } => 0,
        }
    }

    pub fn empty_branches(&self) -> u64 {
        match &self.kind {
            RdiKind::Qwt { inner } => inner.empty_branches,
            RdiKind::HuffScan { .. } => 0,
        }
    }

    pub fn branch_transitions(&self) -> u64 {
        match &self.kind {
            RdiKind::Qwt { inner } => inner.branch_transitions,
            RdiKind::HuffScan { .. } => 0,
        }
    }

    pub fn symbols_yielded(&self) -> u64 {
        match &self.kind {
            RdiKind::Qwt { inner } => inner.symbols_yielded,
            RdiKind::HuffScan { idx, .. } => *idx as u64,
        }
    }

    pub fn children_pushed(&self) -> u64 {
        match &self.kind {
            RdiKind::Qwt { inner } => inner.children_pushed,
            RdiKind::HuffScan { .. } => 0,
        }
    }
}

fn cumulative_a(col: &[u32], alphabet: usize) -> Vec<u32> {
    let mut a = vec![0u32; alphabet + 1];
    for &s in col {
        a[s as usize + 1] += 1;
    }
    for i in 0..alphabet {
        a[i + 1] += a[i];
    }
    a
}

// ── unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Small hand graph:
    /// (0,0,1), (0,0,2), (0,1,1), (1,0,0), (1,1,2)
    fn tiny() -> (CyclicRing, Vec<[u32; 3]>) {
        // Shared alphabet already 0..3
        let triples = vec![[0, 0, 1], [0, 0, 2], [0, 1, 1], [1, 0, 0], [1, 1, 2]];
        let ring = CyclicRing::build_shared(&triples, 3);
        (ring, triples)
    }

    #[test]
    fn cycle_identity_all_rows() {
        let (ring, triples) = tiny();
        for i in 0..ring.n {
            let i1 = ring.f(Col::O, i);
            let i2 = ring.f(Col::P, i1);
            let i3 = ring.f(Col::S, i2);
            assert_eq!(i3, i, "cycle failed at row {i}");
        }
        // Recovered multiset equals source
        let mut got = ring.enumerate_spo();
        let mut exp = triples;
        got.sort_unstable();
        exp.sort_unstable();
        assert_eq!(got, exp);
    }

    #[test]
    fn f_inverse_roundtrip() {
        let (ring, _) = tiny();
        for col in [Col::O, Col::P, Col::S] {
            for i in 0..ring.n {
                let j = ring.f(col, i);
                assert_eq!(ring.f_inverse(col, j), i, "F_inv(F(i)) col={col:?} i={i}");
                let k = ring.f_inverse(col, i);
                assert_eq!(ring.f(col, k), i, "F(F_inv(i)) col={col:?} i={i}");
            }
        }
    }

    #[test]
    fn lead_ranges_match_sorted() {
        let (ring, mut triples) = tiny();
        triples.sort_unstable();
        // S ranges on T_spo: A_s partitions by s
        for s in 0..ring.universe {
            let r = ring.range_s(s);
            let mut count = 0u32;
            for t in &triples {
                if t[0] == s {
                    count += 1;
                }
            }
            assert_eq!(r.len(), count, "s={s}");
        }
    }

    #[test]
    fn range_next_value_cases() {
        let (ring, _) = tiny();
        let full = ring.full_range();
        // On C_o over full T_spo
        let v0 = ring.range_next_value(Col::O, full, 0);
        assert!(v0.is_some());
        let vmin = v0.unwrap();
        // target below min
        assert_eq!(ring.range_next_value(Col::O, full, 0), Some(vmin));
        // exact hit
        assert_eq!(ring.range_next_value(Col::O, full, vmin), Some(vmin));
        // above max
        assert_eq!(ring.range_next_value(Col::O, full, 100), None);
        // empty range
        assert_eq!(
            ring.range_next_value(Col::O, RowRange { start: 2, end: 2 }, 0),
            None
        );
        // singleton
        let s = RowRange { start: 0, end: 1 };
        let only = ring.access(Col::O, 0);
        assert_eq!(ring.range_next_value(Col::O, s, only), Some(only));
        if only > 0 {
            assert_eq!(ring.range_next_value(Col::O, s, only), Some(only));
            assert_eq!(ring.range_next_value(Col::O, s, only + 1), None);
        }
    }

    #[test]
    fn backward_step_count() {
        let (ring, triples) = tiny();
        // For each o, lead range on T_osp should have the right size
        for o in 0..ring.universe {
            let r = ring.range_o(o);
            let exp = triples.iter().filter(|t| t[2] == o).count() as u32;
            assert_eq!(r.len(), exp, "o={o}");
        }
    }

    #[test]
    fn range_next_value_no_alloc() {
        let (ring, _) = tiny();
        ring.reset_hot_path_allocs();
        let full = ring.full_range();
        for t in 0..5 {
            let _ = ring.range_next_value(Col::O, full, t);
        }
        // hot_path_allocs only tracks explicit Cell bumps (we never bump on alloc)
        assert_eq!(ring.hot_path_allocs(), 0);
    }
    #[test]
    fn range_next_value_native_matches_scan() {
        // expand alphabet a bit for gap targets
        let ring = {
            let triples = vec![
                [0, 0, 1],
                [0, 0, 2],
                [0, 1, 1],
                [1, 0, 0],
                [1, 1, 2],
                [2, 0, 3],
                [2, 1, 4],
                [3, 0, 0],
                [3, 1, 5],
                [4, 0, 1],
            ];
            CyclicRing::build_shared(&triples, 6)
        };
        let n = ring.n();
        for start in 0..=n {
            for end in start..=n {
                let r = RowRange { start, end };
                for t in 0..25 {
                    let scan = ring.range_next_value_scan(Col::O, r, t);
                    let native = ring.range_next_value_native(Col::O, r, t);
                    assert_eq!(native, scan, "O {start}..{end} t={t}");
                    let scan_p = ring.range_next_value_scan(Col::P, r, t);
                    let native_p = ring.range_next_value_native(Col::P, r, t);
                    assert_eq!(native_p, scan_p, "P {start}..{end} t={t}");
                    let scan_s = ring.range_next_value_scan(Col::S, r, t);
                    let native_s = ring.range_next_value_native(Col::S, r, t);
                    assert_eq!(native_s, scan_s, "S {start}..{end} t={t}");
                }
            }
        }
    }

    #[test]
    fn range_next_value_native_no_alloc() {
        let (ring, _) = tiny();
        ring.reset_hot_path_allocs();
        let full = ring.full_range();
        for t in 0..20 {
            let _ = ring.range_next_value_native(Col::O, full, t);
        }
        assert_eq!(ring.hot_path_allocs(), 0);
    }

    #[test]
    fn random_shared_cycle() {
        // denser random-ish set
        let mut triples = Vec::new();
        for s in 0..8u32 {
            for p in 0..3u32 {
                for o in 0..5u32 {
                    if (s + p + o) % 2 == 0 {
                        triples.push([s, p, o]);
                    }
                }
            }
        }
        triples.sort_unstable();
        triples.dedup();
        let u = 8;
        let ring = CyclicRing::build_shared(&triples, u);
        for i in 0..ring.n {
            let i3 = ring.f(Col::S, ring.f(Col::P, ring.f(Col::O, i)));
            assert_eq!(i3, i);
        }
        let mut got = ring.enumerate_spo();
        got.sort_unstable();
        assert_eq!(got, triples);
    }

    #[test]
    fn range_distinct_iter_matches_native_rnv() {
        let ring = {
            let triples = vec![
                [0, 0, 1],
                [0, 0, 2],
                [0, 1, 1],
                [1, 0, 0],
                [1, 1, 2],
                [2, 0, 3],
                [2, 1, 4],
                [3, 0, 0],
                [3, 1, 5],
                [4, 0, 1],
            ];
            CyclicRing::build_shared(&triples, 6)
        };
        let n = ring.n();
        for start in 0..=n {
            for end in start..=n {
                let r = RowRange { start, end };
                // collect via RDI
                let mut via_rdi = Vec::new();
                {
                    let mut it = ring.range_distinct_iter(Col::O, r);
                    while let Some((s, c)) = it.next() {
                        via_rdi.push((s, c));
                    }
                }
                // collect via native RNV + rank counts
                let mut via_rnv = Vec::new();
                let mut t = 0u32;
                while let Some(v) = ring.range_next_value_native(Col::O, r, t) {
                    let c = ring.rank(Col::O, v, r.end) - ring.rank(Col::O, v, r.start);
                    via_rnv.push((v, c));
                    t = v.saturating_add(1);
                    if t >= ring.universe {
                        break;
                    }
                }
                assert_eq!(via_rdi, via_rnv, "O {start}..{end}");
            }
        }
    }

    #[test]
    fn range_distinct_iter_no_extra_persistent_bytes() {
        let (ring, _) = tiny();
        let before = ring.mem_bytes();
        let r = ring.full_range();
        let mut it = ring.range_distinct_iter(Col::O, r);
        let mut n = 0u32;
        while let Some((s, c)) = it.next() {
            let _ = (s, c);
            n += 1;
        }
        assert!(n > 0);
        assert_eq!(ring.mem_bytes(), before, "RDI must not grow Ring footprint");
    }

    #[test]
    fn ring_b_cycle_identity() {
        let triples = vec![[0, 0, 1], [0, 0, 2], [0, 1, 1], [1, 0, 0], [1, 1, 2]];
        let b = CyclicRing::build_ring_b_shared(&triples, 3);
        // Cycle on Ring B: F_s (OPS→SOP) then F_p (SOP→PSO) then F_o (PSO→OPS)
        for i in 0..b.n {
            let i1 = b.f(Col::S, i);
            let i2 = b.f(Col::P, i1);
            let i3 = b.f(Col::O, i2);
            assert_eq!(i3, i, "Ring B cycle failed at {i}");
        }
    }

    #[test]
    fn ring_b_lead_ranges() {
        let triples = vec![[0, 0, 1], [0, 0, 2], [0, 1, 1], [1, 0, 0], [1, 1, 2]];
        let b = CyclicRing::build_ring_b_shared(&triples, 3);
        // range_o on B: O leads T_ops; count objects
        for o in 0..3u32 {
            let r = b.range_o(o);
            let exp = triples.iter().filter(|t| t[2] == o).count() as u32;
            assert_eq!(r.len(), exp, "B range_o({o})");
        }
        for s in 0..3u32 {
            let r = b.range_s(s);
            let exp = triples.iter().filter(|t| t[0] == s).count() as u32;
            assert_eq!(r.len(), exp, "B range_s({s})");
        }
    }

    #[test]
    fn uring_bytes_sum() {
        let triples = vec![[0, 0, 1], [0, 1, 2], [1, 0, 0]];
        let u = URing::build_from_role_local(&triples, 2, 2, 3);
        assert_eq!(u.mem_bytes(), u.a.mem_bytes() + u.b.mem_bytes());
        assert_eq!(u.a.n, u.b.n);
    }

    /// Role-local tiny graph used for Qwt vs Huff C_p differential.
    #[cfg(feature = "ring-huffman-cp")]
    fn tiny_role() -> (Vec<[u32; 3]>, u32, u32, u32) {
        // s∈[0,2), p∈[0,3), o∈[0,4)
        let triples = vec![
            [0, 0, 1],
            [0, 0, 2],
            [0, 1, 1],
            [1, 0, 0],
            [1, 1, 2],
            [1, 2, 3],
            [0, 2, 0],
        ];
        (triples, 2, 3, 4)
    }

    #[cfg(feature = "ring-huffman-cp")]
    fn qwt_huff_pair() -> (CyclicRing, CyclicRing) {
        let (t, ns, np, no) = tiny_role();
        let q = CyclicRing::build_from_role_local_qwt_cp(&t, ns, np, no);
        let h = CyclicRing::build_from_role_local_huff_cp(&t, ns, np, no);
        assert!(!q.c_p_is_huff());
        assert!(h.c_p_is_huff());
        assert!(q.col_qwt(Col::P).is_some());
        assert!(h.col_qwt(Col::P).is_none());
        (q, h)
    }

    #[test]
    #[cfg(feature = "ring-huffman-cp")]
    fn huff_cp_enumerate_and_cycle_match_qwt() {
        let (q, h) = qwt_huff_pair();
        assert_eq!(q.n, h.n);
        assert_eq!(q.universe, h.universe);
        assert_eq!(q.ns, h.ns);
        assert_eq!(q.np, h.np);
        assert_eq!(q.no, h.no);

        let mut eq = q.enumerate_spo();
        let mut eh = h.enumerate_spo();
        eq.sort_unstable();
        eh.sort_unstable();
        assert_eq!(eq, eh, "enumerate_spo multiset");

        for i in 0..q.n {
            let iq = q.f(Col::S, q.f(Col::P, q.f(Col::O, i)));
            let ih = h.f(Col::S, h.f(Col::P, h.f(Col::O, i)));
            assert_eq!(iq, i);
            assert_eq!(ih, i);
        }
    }

    #[test]
    #[cfg(feature = "ring-huffman-cp")]
    fn huff_cp_access_rank_select_f_match_qwt() {
        let (q, h) = qwt_huff_pair();
        for col in [Col::O, Col::P, Col::S] {
            for i in 0..q.n {
                assert_eq!(q.access(col, i), h.access(col, i), "access {col:?} @{i}");
                assert_eq!(q.f(col, i), h.f(col, i), "f {col:?} @{i}");
                assert_eq!(
                    q.f_inverse(col, i),
                    h.f_inverse(col, i),
                    "f_inv {col:?} @{i}"
                );
            }
            for sym in 0..q.universe {
                for pos in 0..=q.n {
                    assert_eq!(
                        q.rank(col, sym, pos),
                        h.rank(col, sym, pos),
                        "rank {col:?} sym={sym} pos={pos}"
                    );
                }
                let total = q.rank(col, sym, q.n);
                for occ in 0..total {
                    assert_eq!(
                        q.select(col, sym, occ),
                        h.select(col, sym, occ),
                        "select {col:?} sym={sym} occ={occ}"
                    );
                }
            }
        }
    }

    #[test]
    #[cfg(feature = "ring-huffman-cp")]
    fn huff_cp_rnv_rdi_p_target_match_qwt() {
        let (q, h) = qwt_huff_pair();
        let n = q.n;
        // Full + several subranges; focus Col::P (Huffman arm) but check all cols.
        let ranges = [
            RowRange::full(n),
            RowRange {
                start: 0,
                end: n / 2,
            },
            RowRange {
                start: n / 3,
                end: (2 * n) / 3,
            },
            RowRange::empty(),
        ];
        for r in ranges {
            for col in [Col::O, Col::P, Col::S] {
                for t in 0..q.universe + 2 {
                    assert_eq!(
                        q.range_next_value_native(col, r, t),
                        h.range_next_value_native(col, r, t),
                        "RNV {col:?} {r:?} t={t}"
                    );
                }
                let mut vq = Vec::new();
                let mut vh = Vec::new();
                {
                    let mut iq = q.range_distinct_iter(col, r);
                    while let Some(x) = iq.next() {
                        vq.push(x);
                    }
                }
                {
                    let mut ih = h.range_distinct_iter(col, r);
                    while let Some(x) = ih.next() {
                        vh.push(x);
                    }
                }
                assert_eq!(vq, vh, "RDI {col:?} {r:?}");
            }
        }
        // P-target shapes: lead_range(P) + backward_step on C_p
        for p in q.ns..q.ns + q.np {
            assert_eq!(q.range_p(p), h.range_p(p), "range_p {p}");
            let full = RowRange::full(n);
            assert_eq!(
                q.backward_step(Col::P, full, p),
                h.backward_step(Col::P, full, p),
                "backward_step P p={p}"
            );
            // RDI/RNV on the P-lead range of C_s (after LF into POS)
            let rp = q.range_p(p);
            let mut t = 0u32;
            loop {
                let nq = q.range_next_value_native(Col::S, rp, t);
                let nh = h.range_next_value_native(Col::S, rp, t);
                assert_eq!(nq, nh, "P-lead RNV S p={p} t={t}");
                match nq {
                    None => break,
                    Some(v) => {
                        t = v.saturating_add(1);
                        if t >= q.universe {
                            break;
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[cfg(feature = "ring-huffman-cp")]
    fn huff_cp_novarng1_flatten_hqwa() {
        use crate::mapped_hqwt::{HQWA_MAGIC, RNG_FLAG_HUFF_CP};
        use crate::{open_novarng1_mmap, parse_header, write_novarng1_v1};
        let (t, ns, np, no) = tiny_role();
        let h = CyclicRing::build_from_role_local_huff_cp(&t, ns, np, no);
        let image = write_novarng1_v1(&h).expect("Huff C_p flattens via HQWA");
        let hdr = parse_header(&image).unwrap();
        assert_eq!(hdr.flags & RNG_FLAG_HUFF_CP, RNG_FLAG_HUFF_CP);
        let cp = &image[hdr.off_cp as usize..hdr.off_cp as usize + hdr.len_cp as usize];
        assert_eq!(&cp[0..4], HQWA_MAGIC);
        // Explicit Qwt still flattens with flags=0
        let q = CyclicRing::build_from_role_local_qwt_cp(&t, ns, np, no);
        let qimg = write_novarng1_v1(&q).unwrap();
        assert_eq!(parse_header(&qimg).unwrap().flags, 0);
        // mmap open roundtrip
        let dir = std::env::temp_dir().join(format!("huff_cp_flat_{}.bin", std::process::id()));
        std::fs::write(&dir, &image).unwrap();
        let mapped = open_novarng1_mmap(&dir).expect("open huff image");
        let _ = std::fs::remove_file(&dir);
        assert!(mapped.c_p_is_huff());
        let mut heap = h.enumerate_spo();
        let mut map = mapped.enumerate_spo().unwrap();
        heap.sort_unstable();
        map.sort_unstable();
        assert_eq!(map, heap);
    }
}
