//! E5.9B Phase 2 (research/notes/e5.9b-qwt-substrate-matrix.md §7/§8) —
//! column-local Huffman substrate prototype for **C_p only**.
//!
//! Feature-gated: `ring-huffman-cp` (implies `cyclic-ring-pilot`). Product
//! `ring-backend` enables this by default (Phase 1D). Wired into
//! [`crate::cyclic::CyclicRing`] as `PredicateColumn::Huff` and NOVARNG1 HQWA.
//!
//! # Why C_p only
//!
//! §6/§6.1 of the substrate matrix note structurally **excludes** HQWT for
//! C_o/C_s: the vendored `HuffOccsRangeIter` has undefined symbol order
//! (breaks Nova's numeric-ordered RDI) and Huffman code ranges are not
//! contiguous wavelet subtrees (breaks log-time numeric RNV), forcing an
//! O(σ) probe fallback — prohibitive when σ≈U≈10⁵.
//!
//! C_p is different: its alphabet is **schema-sized** (`np` distinct
//! predicates, typically tens), so an O(σ_P) linear-scan RDI/RNV fallback
//! is cheap. §7.2 additionally showed `Col::P` *is* reachable as an
//! RDI/RNV target (predicate-unbound `SELECT DISTINCT ?p` query shapes via
//! `dense_scan_kind`), so this fallback is required for correctness, not
//! just theoretical completeness.
//!
//! # Local densification (refinement over the §6.1 harness)
//!
//! The archived `hqwt_vs_qwt_fair` harness built `HQWT256` directly over
//! **shared-alphabet** P values (offset into `[ns, ns+np)`), which inflates
//! the crate's internal `codes_encode` lookup table to size `O(ns+np)`
//! (indexed by raw symbol value, not by rank) even though only `np` symbols
//! ever occur. [`HuffColP`] instead **locally re-densifies** P values to
//! `[0, np)` before building the `HQWT256`, translating to/from shared
//! coordinates at the API boundary. This makes both the lookup-table
//! overhead and the O(σ_P) RDI/RNV scan genuinely bounded by `np`, not by
//! the shared universe size `U`.
//!
//! # Hard requirements carried over from `CyclicRing`
//!
//! RDI/RNV here deliberately do **not** use the crate's `occs_range` /
//! `range_next_value` (undefined order / non-contiguous ranges for Huffman
//! codes — see module docs above). They use a plain linear scan over the
//! local alphabet `[0, np)` with `rank` at the range endpoints, always
//! yielding symbols in **numeric ascending order** as Nova's star/leap
//! join primitives require.

use qwt::mem_dbg::{MemSize, SizeFlags};
use qwt::{AccessUnsigned, HQWT256, RankUnsigned, SelectUnsigned};
use std::sync::atomic::{AtomicU64, Ordering};

// ── Counters ─────────────────────────────────────────────────────────────────

/// Process-wide counters for the Huffman C_p O(σ_P) RDI/RNV fallback.
/// Thread-safe; attach via [`HuffColP::with_counters`].
#[derive(Default)]
pub struct HuffCpCounters {
    pub access: AtomicU64,
    pub rank: AtomicU64,
    pub select: AtomicU64,
    pub rdi_calls: AtomicU64,
    /// One probe per symbol scanned in `[0, np)` (two `rank` calls each).
    pub rdi_symbol_probes: AtomicU64,
    pub rdi_symbols_found: AtomicU64,
    pub rnv_calls: AtomicU64,
    pub rnv_symbol_probes: AtomicU64,
}

impl HuffCpCounters {
    pub fn reset(&self) {
        for a in [
            &self.access,
            &self.rank,
            &self.select,
            &self.rdi_calls,
            &self.rdi_symbol_probes,
            &self.rdi_symbols_found,
            &self.rnv_calls,
            &self.rnv_symbol_probes,
        ] {
            a.store(0, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> HuffCpCounterSnapshot {
        HuffCpCounterSnapshot {
            access: self.access.load(Ordering::Relaxed),
            rank: self.rank.load(Ordering::Relaxed),
            select: self.select.load(Ordering::Relaxed),
            rdi_calls: self.rdi_calls.load(Ordering::Relaxed),
            rdi_symbol_probes: self.rdi_symbol_probes.load(Ordering::Relaxed),
            rdi_symbols_found: self.rdi_symbols_found.load(Ordering::Relaxed),
            rnv_calls: self.rnv_calls.load(Ordering::Relaxed),
            rnv_symbol_probes: self.rnv_symbol_probes.load(Ordering::Relaxed),
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct HuffCpCounterSnapshot {
    pub access: u64,
    pub rank: u64,
    pub select: u64,
    pub rdi_calls: u64,
    pub rdi_symbol_probes: u64,
    pub rdi_symbols_found: u64,
    pub rnv_calls: u64,
    pub rnv_symbol_probes: u64,
}

// ── HuffColP ─────────────────────────────────────────────────────────────────

/// Column-local Huffman substrate for C_p, addressed in **shared-alphabet**
/// coordinates (matching [`crate::cyclic::CyclicRing`]'s `Col::P` API) but
/// internally storing a **locally densified** `HQWT256<u32>` over `[0, np)`.
pub struct HuffColP {
    wt: HQWT256<u32>,
    /// Sorted unique shared-alphabet P symbols; index = local id.
    /// Contiguous role-local case: `local_to_shared[i] == p_base + i`.
    local_to_shared: Vec<u32>,
    /// Legacy contiguous base (min shared P, or first local_to_shared[0]).
    /// Kept for HQWA header compatibility / diagnostics.
    p_base: u32,
    /// σ_P: number of distinct predicate ids (local alphabet size).
    np: u32,
    counters: Option<&'static HuffCpCounters>,
}

impl HuffColP {
    /// Build from a column of **shared-alphabet** P values with a contiguous
    /// role-local range `[p_base, p_base+np)`. Prefer [`Self::build_from_column`]
    /// for the product compact shared alphabet (non-contiguous P subset).
    pub fn build_from_shared(col_p_vals: &[u32], p_base: u32, np: u32) -> Self {
        let local_to_shared: Vec<u32> = (0..np).map(|i| p_base + i).collect();
        let local: Vec<u32> = col_p_vals
            .iter()
            .map(|&v| {
                debug_assert!(
                    v >= p_base && v < p_base + np,
                    "P value {v} out of range [{p_base}, {})",
                    p_base + np
                );
                v - p_base
            })
            .collect();
        Self {
            wt: HQWT256::from(local),
            local_to_shared,
            p_base,
            np,
            counters: None,
        }
    }

    /// Build from arbitrary shared-alphabet P values (product path).
    ///
    /// Densifies the **unique** P symbols present in the column to `[0, σ_P)`,
    /// preserving numeric order of shared ids. Works when S/P/O share a compact
    /// alphabet and P is a non-contiguous subset of `[0, U)`.
    pub fn build_from_column(col_p_vals: &[u32]) -> Self {
        let mut uniq: Vec<u32> = col_p_vals.to_vec();
        uniq.sort_unstable();
        uniq.dedup();
        let np = uniq.len() as u32;
        let p_base = uniq.first().copied().unwrap_or(0);
        let local: Vec<u32> = col_p_vals
            .iter()
            .map(|&v| {
                uniq
                    .binary_search(&v)
                    .expect("P value must appear in unique set") as u32
            })
            .collect();
        Self {
            wt: HQWT256::from(local),
            local_to_shared: uniq,
            p_base,
            np,
            counters: None,
        }
    }

    /// Sorted unique shared symbols (local index → shared id).
    #[inline]
    pub fn local_to_shared(&self) -> &[u32] {
        &self.local_to_shared
    }

    #[inline]
    fn shared_to_local(&self, symbol: u32) -> Option<u32> {
        self.local_to_shared
            .binary_search(&symbol)
            .ok()
            .map(|i| i as u32)
    }

    /// Attach process-global counters (typically a `static`).
    pub fn with_counters(mut self, c: &'static HuffCpCounters) -> Self {
        self.counters = Some(c);
        self
    }

    #[inline]
    fn bump(&self, f: impl Fn(&HuffCpCounters)) {
        if let Some(c) = self.counters {
            f(c);
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.wt.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.wt.is_empty()
    }

    #[inline]
    pub fn np(&self) -> u32 {
        self.np
    }

    #[inline]
    pub fn p_base(&self) -> u32 {
        self.p_base
    }

    /// Borrow the underlying densified `HQWT256` (local alphabet `[0, np)`).
    /// Used by the NOVARNG1 HQWA flatten path.
    pub fn wt(&self) -> &HQWT256<u32> {
        &self.wt
    }

    /// Bytes: HQWT256 internal size only (lookup tables + level qvectors).
    /// Locally densified, so this table scales with `np`, not `U`.
    pub fn mem_bytes(&self) -> usize {
        self.wt.mem_size(SizeFlags::default())
    }

    /// `access(position)` — `C_p[position]` in **shared-alphabet** coordinates.
    #[inline]
    pub fn access(&self, pos: u32) -> u32 {
        self.bump(|c| {
            c.access.fetch_add(1, Ordering::Relaxed);
        });
        let local = self.wt.get(pos as usize).expect("access in bounds");
        self.local_to_shared[local as usize]
    }

    /// `rank(symbol, position)` — `symbol` given in **shared-alphabet**
    /// coordinates. Returns 0 for any symbol outside `[p_base, p_base+np)`
    /// (cannot occur in this column by construction).
    #[inline]
    pub fn rank(&self, symbol: u32, position: u32) -> u32 {
        self.bump(|c| {
            c.rank.fetch_add(1, Ordering::Relaxed);
        });
        let Some(local) = self.shared_to_local(symbol) else {
            return 0;
        };
        self.wt.rank(local, position as usize).unwrap_or(0) as u32
    }

    /// `select(symbol, occurrence)` — `symbol` in shared-alphabet coordinates;
    /// returns a **row position** (not translated; positions are not symbols).
    #[inline]
    pub fn select(&self, symbol: u32, occurrence: u32) -> Option<u32> {
        self.bump(|c| {
            c.select.fetch_add(1, Ordering::Relaxed);
        });
        let local = self.shared_to_local(symbol)?;
        self.wt.select(local, occurrence as usize).map(|p| p as u32)
    }

    /// O(σ_P) linear-scan distinct-symbol enumeration over row range
    /// `[start, end)`. Returns `(shared_symbol, count)` pairs in **numeric
    /// ascending order** — required for Nova's star/leap join primitives.
    ///
    /// Does **not** use the crate's `occs_range` (undefined symbol order for
    /// Huffman trees; see module docs). Cost: `2 * np` rank probes.
    pub fn range_distinct_scan(&self, start: u32, end: u32) -> Vec<(u32, u32)> {
        self.bump(|c| {
            c.rdi_calls.fetch_add(1, Ordering::Relaxed);
        });
        let mut out = Vec::new();
        if start >= end {
            return out;
        }
        for local in 0..self.np {
            self.bump(|c| {
                c.rdi_symbol_probes.fetch_add(1, Ordering::Relaxed);
            });
            let lo = self.wt.rank(local, start as usize).unwrap_or(0);
            let hi = self.wt.rank(local, end as usize).unwrap_or(0);
            if hi > lo {
                out.push((self.local_to_shared[local as usize], (hi - lo) as u32));
            }
        }
        self.bump(|c| {
            c.rdi_symbols_found
                .fetch_add(out.len() as u64, Ordering::Relaxed);
        });
        out
    }

    /// O(σ_P) linear-scan RNV: smallest **shared-alphabet** symbol `≥ target`
    /// present at least once in row range `[start, end)`. `None` if no such
    /// symbol exists (including `target` beyond the P range or an empty
    /// input range).
    ///
    /// `target` values below `p_base` are clamped to the first local symbol
    /// (matching `CyclicRing::range_next_value_scan`'s below-min semantics).
    pub fn range_next_value_scan(&self, start: u32, end: u32, target: u32) -> Option<u32> {
        self.bump(|c| {
            c.rnv_calls.fetch_add(1, Ordering::Relaxed);
        });
        if start >= end || self.np == 0 {
            return None;
        }
        // First local whose shared id ≥ target.
        let local_start = self
            .local_to_shared
            .partition_point(|&s| s < target) as u32;
        for local in local_start..self.np {
            self.bump(|c| {
                c.rnv_symbol_probes.fetch_add(1, Ordering::Relaxed);
            });
            let lo = self.wt.rank(local, start as usize).unwrap_or(0);
            let hi = self.wt.rank(local, end as usize).unwrap_or(0);
            if hi > lo {
                return Some(self.local_to_shared[local as usize]);
            }
        }
        None
    }
}

// ── unit tests: differential vs QWT256 oracle ────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use qwt::QWT256;

    /// Deterministic LCG (no external rand dependency, matches bench harness style).
    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
            self.0
        }
        fn gen_range(&mut self, lo: u32, hi: u32) -> u32 {
            lo + (self.next_u64() as u32 % (hi - lo))
        }
    }

    /// Synthetic skewed P column: `np` distinct predicates, Zipf-ish skew,
    /// offset into shared-alphabet coordinates starting at `p_base`.
    /// Mirrors realistic-corpus predicate skew (few hot predicates dominate).
    fn synth_p_column(n: u32, np: u32, p_base: u32, seed: u64) -> Vec<u32> {
        let mut rng = Lcg::new(seed);
        (0..n)
            .map(|_| {
                // Skew: square a uniform draw to bias toward symbol 0 (hot pred).
                let u = rng.gen_range(0, np * np);
                let local = (u as f64).sqrt() as u32;
                p_base + local.min(np - 1)
            })
            .collect()
    }

    #[test]
    fn access_rank_select_parity_vs_qwt256() {
        let np = 7u32;
        let p_base = 20_000u32; // mimic ns offset in the shared universe
        let n = 5_000u32;
        let col = synth_p_column(n, np, p_base, 0xC0FFEE);

        let qwt = QWT256::<u32>::from(col.clone());
        let huff = HuffColP::build_from_shared(&col, p_base, np);

        assert_eq!(huff.len(), qwt.len());

        for i in 0..n {
            assert_eq!(
                huff.access(i),
                qwt.get(i as usize).unwrap(),
                "access mismatch at {i}"
            );
        }

        for sym in p_base..(p_base + np) {
            for pos in (0..=n).step_by(97) {
                let h = huff.rank(sym, pos);
                let q = qwt.rank(sym, pos as usize).unwrap_or(0) as u32;
                assert_eq!(h, q, "rank mismatch sym={sym} pos={pos}");
            }
        }

        // symbols outside the P range must rank 0 (never occur in this column)
        assert_eq!(huff.rank(0, n), 0);
        assert_eq!(huff.rank(p_base + np, n), 0);

        for sym in p_base..(p_base + np) {
            let occs = qwt.rank(sym, n as usize).unwrap_or(0) as u32;
            for occ in 0..occs {
                let h = huff.select(sym, occ);
                let q = qwt.select(sym, occ as usize).map(|p| p as u32);
                assert_eq!(h, q, "select mismatch sym={sym} occ={occ}");
            }
            // one past the last occurrence must be None on both
            assert_eq!(huff.select(sym, occs), None);
            assert_eq!(qwt.select(sym, occs as usize), None);
        }
    }

    #[test]
    fn rdi_scan_matches_qwt256_native_rdi_ascending() {
        let np = 11u32;
        let p_base = 102_000u32;
        let n = 8_000u32;
        let col = synth_p_column(n, np, p_base, 0xFACADE);

        let qwt = QWT256::<u32>::from(col.clone());
        let huff = HuffColP::build_from_shared(&col, p_base, np);

        let mut rng = Lcg::new(0xBEEF);
        for _ in 0..500 {
            let start = rng.gen_range(0, n);
            let len = 1 + rng.gen_range(0, (n / 8).max(1));
            let end = (start + len).min(n);
            if start >= end {
                continue;
            }

            let via_huff = huff.range_distinct_scan(start, end);

            // Oracle: QWT256's native range_distinct_iter (already differentially
            // tested against native RNV in cyclic.rs) restricted/offset the same way.
            let mut via_qwt = Vec::new();
            let it = qwt.range_distinct_iter(start as usize..end as usize);
            for (sym, cnt) in it {
                via_qwt.push((sym, cnt as u32));
            }

            via_qwt.sort_unstable_by_key(|&(s, _)| s); // QWT order is numeric-ascending already

            assert_eq!(via_huff, via_qwt, "RDI mismatch [{start},{end})");
            // ascending-order invariant required by star/leap joins
            for w in via_huff.windows(2) {
                assert!(w[0].0 < w[1].0, "RDI must be numeric-ascending");
            }
        }
    }

    #[test]
    fn rnv_scan_matches_qwt256_native_rnv() {
        let np = 9u32;
        let p_base = 55_000u32;
        let n = 6_000u32;
        let col = synth_p_column(n, np, p_base, 0x1337);

        let qwt = QWT256::<u32>::from(col.clone());
        let huff = HuffColP::build_from_shared(&col, p_base, np);

        let mut rng = Lcg::new(0x5EED);
        for _ in 0..1000 {
            let start = rng.gen_range(0, n);
            let len = 1 + rng.gen_range(0, (n / 4).max(1));
            let end = (start + len).min(n);
            if start >= end {
                continue;
            }
            let target = p_base.saturating_sub(5) + rng.gen_range(0, np + 10);

            let h = huff.range_next_value_scan(start, end, target);
            let q = qwt.range_next_value(start as usize..end as usize, target);
            assert_eq!(h, q, "RNV mismatch [{start},{end}) target={target}");
        }

        // empty range
        assert_eq!(huff.range_next_value_scan(3, 3, p_base), None);
        // target beyond P range
        assert_eq!(huff.range_next_value_scan(0, n, p_base + np), None);
        assert_eq!(huff.range_next_value_scan(0, n, p_base + np + 50), None);
    }

    #[test]
    fn locally_densified_bytes_smaller_than_undensified_baseline() {
        // Demonstrates the §7 refinement: local densification keeps the
        // HQWT256 lookup-table overhead bounded by `np`, not by `p_base+np`.
        let np = 7u32;
        let p_base = 200_000u32; // large shared-universe offset
        let n = 20_000u32;
        let col = synth_p_column(n, np, p_base, 0xD00D);

        let densified = HuffColP::build_from_shared(&col, p_base, np);
        let undensified = HQWT256::<u32>::from(col.clone());

        let densified_bytes = densified.mem_bytes();
        let undensified_bytes = undensified.mem_size(SizeFlags::default());

        assert!(
            densified_bytes < undensified_bytes,
            "densified={densified_bytes} should be < undensified={undensified_bytes}"
        );
    }

    #[test]
    fn lf_cycle_style_composition_matches_across_substrates() {
        // Not a full CyclicRing integration (Phase 2 is standalone), but
        // verifies the F(i) = A[c] + rank(c, i+1) - 1 formula produces
        // identical results whether rank/access come from HuffColP or
        // QWT256, given the same external A_p cumulative array.
        let np = 6u32;
        let p_base = 40_000u32;
        let n = 4_000u32;
        let col = synth_p_column(n, np, p_base, 0xABCD);

        let qwt = QWT256::<u32>::from(col.clone());
        let huff = HuffColP::build_from_shared(&col, p_base, np);

        // Build A array over the *shared* alphabet up to p_base+np (only the
        // P-range slice is meaningful; CyclicRing builds one shared A per
        // column over the full universe — we only need the P slice here).
        let universe = (p_base + np) as usize;
        let mut a = vec![0u32; universe + 1];
        for &s in &col {
            a[s as usize + 1] += 1;
        }
        for i in 0..universe {
            a[i + 1] += a[i];
        }

        for i in 0..n {
            let c_qwt = qwt.get(i as usize).unwrap();
            let c_huff = huff.access(i);
            assert_eq!(c_qwt, c_huff);

            let ac = a[c_qwt as usize];
            let r_qwt = qwt.rank(c_qwt, (i + 1) as usize).unwrap() as u32;
            let r_huff = huff.rank(c_huff, i + 1);
            assert_eq!(r_qwt, r_huff, "rank mismatch at i={i}");

            let f_qwt = ac + r_qwt - 1;
            let f_huff = ac + r_huff - 1;
            assert_eq!(f_qwt, f_huff, "F(i) mismatch at i={i}");
        }
    }
}
