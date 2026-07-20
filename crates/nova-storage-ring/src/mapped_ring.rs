//! E5.10 W1 — mmap-backed Ring A shell (`NOVARNG1`).
//!
//! Flatten pilot [`CyclicRing`] (3× `QWT256` + A arrays) into a page-aligned
//! immutable image and open via **`memmap2::Mmap`** with borrowed section views
//! (no body copy, no full-body checksum on open).
//!
//! **W1 scope:** header + three QWTA sections + A arrays; `access` +
//! level-local `rank` / `rank_all` differential vs heap.
//! **W2 scope:** full-symbol `rank` / `select` / `F` / `F_inv` / `backward_step`
//! / `lead_range` over mmap views.
//! **W3 scope:** native `range_next_value` + fixed-stack `range_distinct_iter`
//! (E5.9A algorithms: rank-all-4, unary collapse, endpoint prefetch).
//! **W4 scope:** `enumerate_spo` / `range_s|o|p`; full product-format gate
//! (tuple differential, bytes, residency, star/triangle vs heap + LOUDS).

//! **Not** on the LoudsStore path. Feature `cyclic-ring-pilot` only.
//!
//! Platform: little-endian only (v1). Substrate: **QWT256** (E5.9B locked).
//!
//! # File layout (`NOVARNG1` v1)
//!
//! ```text
//! [0..256)     header (parse by offset; zero-padded)
//! [256..4096)  pad to page
//! [page-aligned]
//!   C_o QWTA section
//!   C_p QWTA section  (or HQWA when flags & RNG_FLAG_HUFF_CP)
//!   C_s QWTA section
//!   A_o[U+1] u32 LE  (8-byte aligned)
//!   A_p[U+1] u32 LE
//!   A_s[U+1] u32 LE
//! ```
//!
//! ## Header fields (LE)
//!
//! | off | field |
//! |-----|--------|
//! | 0   | magic `NOVARNG1` |
//! | 8   | version u32 (=1) |
//! | 12  | flags u32 (bit0 = [`RNG_FLAG_HUFF_CP`] when C_p is HQWA) |
//! | 16  | n u64 (#triples) |
//! | 24  | universe u32 |
//! | 28  | n_levels u16 (max of three columns; diagnostic) |
//! | 30  | page_align_log2 u8 (=12) |
//! | 31  | reserved u8 |
//! | 32  | checksum_hdr u64 (FNV-1a of header w/ this field 0) |
//! | 40  | checksum_body u64 (0 in v1 open path; offline only) |
//! | 48  | off_co, off_cp, off_cs : u64 |
//! | 72  | off_ao, off_ap, off_as : u64 |
//! | 96  | len_ao, len_ap, len_as : u64  (element counts = U+1) |
//! | 120 | len_co, len_cp, len_cs : u64  (section byte lengths) |
//! | 144 | reserved → 256 |
//!
//! Dual-format (E5.9B Phase 4, feature `ring-huffman-cp`): product default still
//! writes QWTA C_p with `flags=0`. Huffman C_p writes an HQWA section and sets
//! `flags |= RNG_FLAG_HUFF_CP`. Open branches on the flag (and C_p section magic).

use crate::mapped_qwt::{
    HotQwtColumn, MappedQwtError, MappedQwtSection, MappedRangeDistinctIter, PAGE_ALIGN,
    PAGE_ALIGN_LOG2, align_up, build_qwta_section, fnv1a64,
};
#[cfg(any(test, feature = "diagnostics"))]
use crate::mapped_qwt::IntersectionIter3;
#[cfg(feature = "ring-huffman-cp")]
use crate::mapped_hqwt::{
    HQWA_MAGIC, HotHuffColumn, MappedHqwtSection, RNG_FLAG_HUFF_CP, build_hqwa_section,
};
// Re-export flag constant for callers without the hqwt module path when feature is on.
#[cfg(feature = "ring-huffman-cp")]
pub use crate::mapped_hqwt::RNG_FLAG_HUFF_CP as NOVARNG1_FLAG_HUFF_CP;
use crate::{Col, CyclicRing, RowRange};
use memmap2::Mmap;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// Magic for full Ring A image.
pub const NOVARNG1_MAGIC: &[u8; 8] = b"NOVARNG1";
pub const RNG_FORMAT_VERSION: u32 = 1;
pub const RNG_HEADER_SIZE: usize = 256;


/// Errors opening or writing a `NOVARNG1` image.
#[derive(Debug, thiserror::Error)]
pub enum MappedRingError {
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("QWT section: {0}")]
    Qwt(#[from] MappedQwtError),
    #[error("bad magic (expected NOVARNG1)")]
    BadMagic,
    #[error("unsupported version {0}")]
    BadVersion(u32),
    #[error("host is not little-endian (NOVARNG1 v1 requires LE)")]
    NotLittleEndian,
    #[error("header checksum mismatch")]
    HeaderChecksum,
    #[error("bounds/alignment: {0}")]
    Layout(&'static str),
}

/// Parsed header directory (offsets into the image).
#[derive(Clone, Copy, Debug)]
pub struct Novarng1Header {
    /// Header flags (bit0 = Huffman C_p / HQWA when feature enabled).
    pub flags: u32,
    pub n: u64,
    pub universe: u32,
    pub n_levels: u16,
    pub page_align_log2: u8,
    pub off_co: u64,
    pub off_cp: u64,
    pub off_cs: u64,
    pub off_ao: u64,
    pub off_ap: u64,
    pub off_as: u64,
    pub len_ao: u64,
    pub len_ap: u64,
    pub len_as: u64,
    pub len_co: u64,
    pub len_cp: u64,
    pub len_cs: u64,
}

/// mmap-backed Ring A shell. Owns the mapping; sections are borrowed views.
///
/// Open path validates header + directory bounds only — **no** full-body
/// checksum and **no** QWT payload materialization into `Vec`.
///
/// E5.11 B1: open-time [`HotQwtColumn`] for O/P/S (validated raw pointers into
/// the mmap). Hot primitives (access/rank/RNV/RDI) route through these; select
/// and cold section views stay on the re-open path.
pub struct MappedRingA {
    mmap: Mmap,
    /// Open-time hot views (pointers alias `mmap`); built once at open.
    hot_o: HotQwtColumn,
    /// Qwt C_p hot view. Present when C_p is QWTA (product default).
    hot_p: HotQwtColumn,
    /// Huffman C_p hot view. Present when header flag RNG_FLAG_HUFF_CP is set.
    #[cfg(feature = "ring-huffman-cp")]
    hot_p_huff: Option<HotHuffColumn>,
    hot_s: HotQwtColumn,
    /// Header flags (mirrors NOVARNG1 header).
    pub flags: u32,
    pub n: u32,
    pub universe: u32,
    /// Diagnostic: max n_levels among columns (from header).
    pub n_levels: u16,
    off_co: usize,
    off_cp: usize,
    off_cs: usize,
    len_co: usize,
    len_cp: usize,
    len_cs: usize,
    off_ao: usize,
    off_ap: usize,
    off_as: usize,
    len_a: usize, // elements per A array (= universe + 1)
}

impl MappedRingA {
    /// Image byte length (mapped file size).
    pub fn image_bytes(&self) -> usize {
        self.mmap.len()
    }

    /// Raw mapped bytes (for diagnostics only).
    pub fn bytes(&self) -> &[u8] {
        &self.mmap
    }

    /// Whether open used a real OS mapping (always true for this type).
    pub fn is_mmap_backed(&self) -> bool {
        true
    }

    #[inline]
    fn col_section_bytes(&self, col: Col) -> Result<&[u8], MappedRingError> {
        let (off, len) = match col {
            Col::O => (self.off_co, self.len_co),
            Col::P => (self.off_cp, self.len_cp),
            Col::S => (self.off_cs, self.len_cs),
        };
        let end = off
            .checked_add(len)
            .ok_or(MappedRingError::Layout("col OOB"))?;
        if end > self.mmap.len() {
            return Err(MappedRingError::Layout("col OOB"));
        }
        Ok(&self.mmap[off..end])
    }

    /// Open a QWTA section view. For `Col::P` fails when the image stores HQWA.
    #[inline]
    pub fn col_qwt(&self, col: Col) -> Result<MappedQwtSection<'_>, MappedRingError> {
        if col == Col::P && self.c_p_is_huff() {
            return Err(MappedRingError::Layout("C_p is HQWA (Huffman), not QWTA"));
        }
        Ok(MappedQwtSection::open(self.col_section_bytes(col)?)?)
    }

    /// Open HQWA C_p section when the image is Huffman-encoded.
    #[cfg(feature = "ring-huffman-cp")]
    #[inline]
    pub fn col_hqwt_p(&self) -> Result<MappedHqwtSection<'_>, MappedRingError> {
        if !self.c_p_is_huff() {
            return Err(MappedRingError::Layout("C_p is QWTA, not HQWA"));
        }
        Ok(MappedHqwtSection::open(self.col_section_bytes(Col::P)?)?)
    }

    /// Whether this image stores Huffman C_p (HQWA section).
    #[inline]
    pub fn c_p_is_huff(&self) -> bool {
        #[cfg(feature = "ring-huffman-cp")]
        {
            self.hot_p_huff.is_some() || (self.flags & RNG_FLAG_HUFF_CP) != 0
        }
        #[cfg(not(feature = "ring-huffman-cp"))]
        {
            false
        }
    }

    /// Open-time Qwt hot column for `col` (E5.11 B1).
    ///
    /// For `Col::P` on a Huffman image this returns a dummy empty hot column;
    /// use [`Self::access`] / [`Self::rank`] / [`Self::range_next_value`] which
    /// dispatch correctly, or [`Self::col_hot_p_huff`].
    #[inline(always)]
    pub fn col_hot(&self, col: Col) -> &HotQwtColumn {
        match col {
            Col::O => &self.hot_o,
            Col::P => &self.hot_p,
            Col::S => &self.hot_s,
        }
    }

    /// Huffman C_p hot view when present.
    #[cfg(feature = "ring-huffman-cp")]
    #[inline(always)]
    pub fn col_hot_p_huff(&self) -> Option<&HotHuffColumn> {
        self.hot_p_huff.as_ref()
    }

    #[inline]
    pub fn col_a(&self, col: Col) -> Result<&[u32], MappedRingError> {
        let off = match col {
            Col::O => self.off_ao,
            Col::P => self.off_ap,
            Col::S => self.off_as,
        };
        cast_u32_slice(&self.mmap, off, self.len_a)
    }

    /// `access(column, position)` — `C[i]` via open-time hot column.
    #[inline]
    pub fn access(&self, col: Col, pos: u32) -> Option<u32> {
        #[cfg(feature = "ring-huffman-cp")]
        if col == Col::P {
            if let Some(h) = self.hot_p_huff.as_ref() {
                return h.get(pos as usize);
            }
        }
        self.col_hot(col).get(pos as usize)
    }

    /// Level-local rank of 2-bit symbol `b` on `col` at wavelet `level`.
    /// Not meaningful for Huffman C_p (variable-depth codes); returns `None`.
    #[inline]
    pub fn rank_level(&self, col: Col, level: usize, b: u8, i: usize) -> Option<usize> {
        if col == Col::P && self.c_p_is_huff() {
            return None;
        }
        self.col_hot(col).rank_level(level, b, i)
    }

    /// Level-local rank-all-4 on `col` at wavelet `level`.
    /// Not meaningful for Huffman C_p; returns `None`.
    #[inline]
    pub fn rank_all_level(&self, col: Col, level: usize, i: usize) -> Option<[usize; 4]> {
        if col == Col::P && self.c_p_is_huff() {
            return None;
        }
        self.col_hot(col).rank_all_level(level, i)
    }

    /// A[c] for lead-range / F.
    #[inline]
    pub fn a_at(&self, col: Col, symbol: u32) -> Option<u32> {
        let a = self.col_a(col).ok()?;
        a.get(symbol as usize).copied()
    }

    // ── W2: full-symbol rank / select / F maps ────────────────────────────

    /// Full-symbol `rank(col, symbol, position)` — # of `symbol` in `[0, position)`.
    #[inline]
    pub fn rank(&self, col: Col, symbol: u32, position: u32) -> Option<u32> {
        #[cfg(feature = "ring-huffman-cp")]
        if col == Col::P {
            if let Some(h) = self.hot_p_huff.as_ref() {
                return h.rank(symbol, position as usize).map(|r| r as u32);
            }
        }
        self.col_hot(col)
            .rank(symbol, position as usize)
            .map(|r| r as u32)
    }

    /// Full-symbol `select(col, symbol, occurrence)` — 0-based occurrence position.
    #[inline]
    pub fn select(&self, col: Col, symbol: u32, occurrence: u32) -> Option<u32> {
        #[cfg(feature = "ring-huffman-cp")]
        if col == Col::P {
            if self.c_p_is_huff() {
                return self
                    .col_hqwt_p()
                    .ok()?
                    .select_shared(symbol, occurrence as usize)
                    .map(|p| p as u32);
            }
        }
        self.col_qwt(col)
            .ok()?
            .select(symbol, occurrence as usize)
            .map(|p| p as u32)
    }

    /// Paper Eq. (2) zero-based: `F_j(i) = A[c] + rank(c, i+1) - 1`, `c = C[i]`.
    #[inline]
    pub fn f(&self, col: Col, i: u32) -> Option<u32> {
        if i >= self.n {
            return None;
        }
        let c_sym = self.access(col, i)?;
        let ac = self.a_at(col, c_sym)?;
        let r = self.rank(col, c_sym, i + 1)?;
        Some(ac + r - 1)
    }

    /// Paper Eq. (3) zero-based inverse: find `c` with `A[c] ≤ i' < A[c+1]`,
    /// then `select(c, i' - A[c])`.
    #[inline]
    pub fn f_inverse(&self, col: Col, i_prime: u32) -> Option<u32> {
        if i_prime >= self.n {
            return None;
        }
        let a = self.col_a(col).ok()?;
        // partition_point: first index where a[idx] > i_prime → c = idx - 1
        let c_sym = a.partition_point(|&x| x <= i_prime) as u32 - 1;
        let occ = i_prime - a[c_sym as usize];
        self.select(col, c_sym, occ)
    }

    /// Paper Eq. (4) backward step — zero-based half-open.
    ///
    /// ```text
    /// start' = A[c] + rank(c, r.start)
    /// end'   = A[c] + rank(c, r.end)
    /// ```
    #[inline]
    pub fn backward_step(&self, col: Col, r: RowRange, symbol: u32) -> Option<RowRange> {
        if r.is_empty() {
            return Some(RowRange::empty());
        }
        let ac = self.a_at(col, symbol)?;
        let start = ac + self.rank(col, symbol, r.start)?;
        let end = ac + self.rank(col, symbol, r.end)?;
        Some(RowRange { start, end })
    }

    /// Lead range for `symbol` after LF via `col` (i.e. `A[c] .. A[c+1]`).
    #[inline]
    pub fn lead_range(&self, col: Col, symbol: u32) -> Option<RowRange> {
        let a = self.col_a(col).ok()?;
        let start = *a.get(symbol as usize)?;
        let end = *a.get(symbol as usize + 1)?;
        Some(RowRange { start, end })
    }

    // ── W3: native RNV + fixed-stack RDI ──────────────────────────────────

    /// Native guided `range_next_value` on column `col` (open-time hot path).
    /// Huffman C_p uses O(σ_P) numeric scan (same as heap HuffColP).
    #[inline]
    pub fn range_next_value(&self, col: Col, r: RowRange, target: u32) -> Option<u32> {
        if r.is_empty() {
            return None;
        }
        #[cfg(feature = "ring-huffman-cp")]
        if col == Col::P {
            if let Some(h) = self.hot_p_huff.as_ref() {
                return h.range_next_value_scan(r.start as usize..r.end as usize, target);
            }
        }
        self.col_hot(col)
            .range_next_value(r.start as usize..r.end as usize, target)
    }

    /// E5.11 D1: braided two-range intersection successor on column `col`.
    ///
    /// Smallest symbol `v ≥ target` present in **both** row ranges. Uses
    /// synchronized 4-ary descent (mask AND) rather than alternating dual RNV.
    #[inline]
    pub fn intersection_next_value2(
        &self,
        col: Col,
        left: RowRange,
        right: RowRange,
        target: u32,
    ) -> Option<u32> {
        if left.is_empty() || right.is_empty() {
            return None;
        }
        // Huffman C_p has no balanced 4-ary braid; dual-RNV leapfrog is correct.
        if col == Col::P && self.c_p_is_huff() {
            return self.intersection_next_value2_dual_rnv(col, left, right, target);
        }
        self.col_hot(col).intersection_next_value2(
            left.start as usize..left.end as usize,
            right.start as usize..right.end as usize,
            target,
        )
    }

    /// E5.11 D1: braided intersection with software counters.
    #[inline]
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value2_counted(
        &self,
        col: Col,
        left: RowRange,
        right: RowRange,
        target: u32,
    ) -> (Option<u32>, super::mapped_qwt::Intersection2Stats) {
        if left.is_empty() || right.is_empty() {
            return (None, super::mapped_qwt::Intersection2Stats::default());
        }
        self.col_hot(col).intersection_next_value2_counted(
            left.start as usize..left.end as usize,
            right.start as usize..right.end as usize,
            target,
        )
    }

    /// Dual-RNV leapfrog oracle for D1 (correctness / baseline).
    #[inline]
    pub fn intersection_next_value2_dual_rnv(
        &self,
        col: Col,
        left: RowRange,
        right: RowRange,
        target: u32,
    ) -> Option<u32> {
        if left.is_empty() || right.is_empty() {
            return None;
        }
        // Implement via MappedRingA::range_next_value so Huffman C_p dispatches.
        let mut t = target;
        loop {
            let a = self.range_next_value(col, left, t)?;
            let b = self.range_next_value(col, right, a)?;
            if b == a {
                return Some(a);
            }
            t = b;
        }
    }

    /// E5.11 D2: braided three-range intersection successor on `col`.
    ///
    /// All three row ranges are descended together and only symbol-prefixes
    /// present in every range are visited. This is the zero-allocation product
    /// triangle primitive.
    #[inline]
    pub fn intersection_next_value3(
        &self,
        col: Col,
        first: RowRange,
        second: RowRange,
        third: RowRange,
        target: u32,
    ) -> Option<u32> {
        if first.is_empty() || second.is_empty() || third.is_empty() {
            return None;
        }
        if col == Col::P && self.c_p_is_huff() {
            return self.intersection_next_value3_dual_rnv(col, first, second, third, target);
        }
        self.col_hot(col).intersection_next_value3(
            first.start as usize..first.end as usize,
            second.start as usize..second.end as usize,
            third.start as usize..third.end as usize,
            target,
        )
    }

    /// D3-A diagnostic form of [`Self::intersection_next_value3`].
    #[inline]
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_counted(
        &self,
        col: Col,
        first: RowRange,
        second: RowRange,
        third: RowRange,
        target: u32,
    ) -> (Option<u32>, super::mapped_qwt::Intersection3Stats) {
        if first.is_empty() || second.is_empty() || third.is_empty() {
            return (None, super::mapped_qwt::Intersection3Stats::default());
        }
        self.col_hot(col).intersection_next_value3_counted(
            first.start as usize..first.end as usize,
            second.start as usize..second.end as usize,
            third.start as usize..third.end as usize,
            target,
        )
    }

    /// D4-A expand locality counters (diagnostic; not product path).
    #[inline]
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_overlap(
        &self,
        col: Col,
        first: RowRange,
        second: RowRange,
        third: RowRange,
        target: u32,
    ) -> (Option<u32>, super::mapped_qwt::Expand3OverlapStats) {
        if first.is_empty() || second.is_empty() || third.is_empty() {
            return (None, super::mapped_qwt::Expand3OverlapStats::default());
        }
        self.col_hot(col).intersection_next_value3_overlap(
            first.start as usize..first.end as usize,
            second.start as usize..second.end as usize,
            third.start as usize..third.end as usize,
            target,
        )
    }

    /// D4-B mask-first fused expand3 prototype (diagnostic; not product path).
    #[inline]
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_fused(
        &self,
        col: Col,
        first: RowRange,
        second: RowRange,
        third: RowRange,
        target: u32,
    ) -> (Option<u32>, super::mapped_qwt::Expand3FusedStats) {
        if first.is_empty() || second.is_empty() || third.is_empty() {
            return (None, super::mapped_qwt::Expand3FusedStats::default());
        }
        self.col_hot(col).intersection_next_value3_fused(
            first.start as usize..first.end as usize,
            second.start as usize..second.end as usize,
            third.start as usize..third.end as usize,
            target,
        )
    }

    /// D4-C true unique-load expand3 successor (diagnostic; not product path).
    #[inline]
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_shared(
        &self,
        col: Col,
        first: RowRange,
        second: RowRange,
        third: RowRange,
        target: u32,
    ) -> (Option<u32>, super::mapped_qwt::Expand3SharedStats) {
        if first.is_empty() || second.is_empty() || third.is_empty() {
            return (None, super::mapped_qwt::Expand3SharedStats::default());
        }
        self.col_hot(col).intersection_next_value3_shared(
            first.start as usize..first.end as usize,
            second.start as usize..second.end as usize,
            third.start as usize..third.end as usize,
            target,
        )
    }

    /// E1.0 summary-gated three-range successor (diagnostic; not product path).
    #[inline]
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_summary(
        &self,
        col: Col,
        table: &super::mapped_qwt::PresenceSummaryTable,
        first: RowRange,
        second: RowRange,
        third: RowRange,
        target: u32,
    ) -> (Option<u32>, super::mapped_qwt::PresenceSummaryStats) {
        if first.is_empty() || second.is_empty() || third.is_empty() {
            return (
                None,
                super::mapped_qwt::PresenceSummaryStats {
                    summary_bytes: table.bytes as u64,
                    ..Default::default()
                },
            );
        }
        self.col_hot(col).intersection_next_value3_summary(
            table,
            first.start as usize..first.end as usize,
            second.start as usize..second.end as usize,
            third.start as usize..third.end as usize,
            target,
        )
    }

    /// Persistent fixed-stack three-range intersection iterator (D3-B).

    /// The iterator borrows this mmap-backed image and stores only its bounded
    /// traversal stack; no persistent image bytes or heap allocation are added.
    #[inline]
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_iter3(
        &self,
        col: Col,
        first: RowRange,
        second: RowRange,
        third: RowRange,
        target: u32,
    ) -> IntersectionIter3<'_> {
        self.col_hot(col).intersection_iter3(
            first.start as usize..first.end as usize,
            second.start as usize..second.end as usize,
            third.start as usize..third.end as usize,
            target,
        )
    }

    /// Three independent RNV leapfrog oracle for D2 differential tests.
    #[inline]
    pub fn intersection_next_value3_dual_rnv(
        &self,
        col: Col,
        first: RowRange,
        second: RowRange,
        third: RowRange,
        target: u32,
    ) -> Option<u32> {
        if first.is_empty() || second.is_empty() || third.is_empty() {
            return None;
        }
        let mut t = target;
        loop {
            let a = self.range_next_value(col, first, t)?;
            let b = self.range_next_value(col, second, a)?;
            let c = self.range_next_value(col, third, b)?;
            if c == a && b == a {
                return Some(a);
            }
            t = c;
        }
    }

    /// Ordered distinct-symbol iterator on column `col`.
    ///
    /// Qwt columns use the fixed-stack mapped RDI (E5.11 B1). Huffman C_p uses
    /// an O(σ_P) numeric scan materialised once (schema-sized).
    #[inline]
    pub fn range_distinct_iter(
        &self,
        col: Col,
        r: RowRange,
    ) -> Option<MappedColDistinctIter<'_>> {
        #[cfg(feature = "ring-huffman-cp")]
        if col == Col::P {
            if let Some(h) = self.hot_p_huff.as_ref() {
                let pairs = if r.is_empty() {
                    Vec::new()
                } else {
                    h.range_distinct_scan(r.start as usize..r.end as usize)
                };
                return Some(MappedColDistinctIter::HuffScan { pairs, idx: 0 });
            }
        }
        let hot = self.col_hot(col);
        let it = if r.is_empty() {
            hot.range_distinct_iter(0..0)
        } else {
            hot.range_distinct_iter(r.start as usize..r.end as usize)
        };
        Some(MappedColDistinctIter::Qwt(it))
    }

    /// Count distinct symbols in range via RDI (diagnostic / gate).
    #[inline]
    pub fn range_distinct_count(&self, col: Col, r: RowRange) -> Option<u32> {
        let mut it = self.range_distinct_iter(col, r)?;
        let mut n = 0u32;
        while it.next_symbol().is_some() {
            n += 1;
        }
        Some(n)
    }

    // ── W4: high-level helpers (match CyclicRing navigation surface) ──────

    /// Prefix range for subject `s` in T_spo (`lead_range(S, s)`).
    #[inline]
    pub fn range_s(&self, s: u32) -> Option<RowRange> {
        self.lead_range(Col::S, s)
    }

    /// Prefix range for object `o` in T_osp (`lead_range(O, o)`).
    #[inline]
    pub fn range_o(&self, o: u32) -> Option<RowRange> {
        self.lead_range(Col::O, o)
    }

    /// Prefix range for predicate `p` in T_pos (`lead_range(P, p)`).
    #[inline]
    pub fn range_p(&self, p: u32) -> Option<RowRange> {
        self.lead_range(Col::P, p)
    }

    /// Enumerate all triples by walking T_spo via the LF cycle.
    ///
    /// Same recovery as [`CyclicRing::enumerate_spo`]:
    /// `o = C_o[i]`, `i_osp = F_o(i)`, `p = C_p[i_osp]`, `i_pos = F_p(i_osp)`,
    /// `s = C_s[i_pos]`. Returns shared-alphabet coordinates `[s,p,o]`.
    pub fn enumerate_spo(&self) -> Option<Vec<[u32; 3]>> {
        let mut out = Vec::with_capacity(self.n as usize);
        for i in 0..self.n {
            let o = self.access(Col::O, i)?;
            let i_osp = self.f(Col::O, i)?;
            let p = self.access(Col::P, i_osp)?;
            let i_pos = self.f(Col::P, i_osp)?;
            let s = self.access(Col::S, i_pos)?;
            out.push([s, p, o]);
        }
        Some(out)
    }
}

/// Column-agnostic mapped distinct iterator (Qwt RDI or Huffman O(σ_P) scan).
pub enum MappedColDistinctIter<'a> {
    Qwt(MappedRangeDistinctIter<'a>),
    #[cfg(feature = "ring-huffman-cp")]
    HuffScan {
        pairs: Vec<(u32, u32)>,
        idx: usize,
    },
}

impl MappedColDistinctIter<'_> {
    /// Next distinct symbol and its occurrence count in the range.
    #[inline]
    pub fn next_symbol(&mut self) -> Option<(u32, usize)> {
        match self {
            Self::Qwt(it) => it.next_symbol(),
            #[cfg(feature = "ring-huffman-cp")]
            Self::HuffScan { pairs, idx } => {
                if *idx >= pairs.len() {
                    return None;
                }
                let (s, c) = pairs[*idx];
                *idx += 1;
                Some((s, c as usize))
            }
        }
    }
}

// ── write ───────────────────────────────────────────────────────────────────

/// Flatten a pilot [`CyclicRing`] into a `NOVARNG1` image (owned bytes).
///
/// Product default: three QWTA sections, `flags = 0`.
/// With `ring-huffman-cp`, a Huffman C_p ring writes an HQWA C_p section and
/// sets `flags |= RNG_FLAG_HUFF_CP`.
pub fn write_novarng1_v1(ring: &CyclicRing) -> Result<Vec<u8>, MappedRingError> {
    if !cfg!(target_endian = "little") {
        return Err(MappedRingError::NotLittleEndian);
    }

    let sec_o = build_qwta_section(
        ring.col_qwt(Col::O)
            .ok_or(MappedRingError::Layout("C_o QWT missing"))?,
    )?;
    let sec_s = build_qwta_section(
        ring.col_qwt(Col::S)
            .ok_or(MappedRingError::Layout("C_s QWT missing"))?,
    )?;

    #[cfg(feature = "ring-huffman-cp")]
    let (sec_p, flags): (Vec<u8>, u32) = if ring.c_p_is_huff() {
        let h = ring
            .c_p_substrate()
            .as_huff()
            .ok_or(MappedRingError::Layout("C_p Huff arm missing"))?;
        let sec = build_hqwa_section(h.wt(), h.local_to_shared())?;
        (sec, RNG_FLAG_HUFF_CP)
    } else {
        let sec = build_qwta_section(
            ring.col_qwt(Col::P)
                .ok_or(MappedRingError::Layout("C_p QWT missing"))?,
        )?;
        (sec, 0)
    };
    #[cfg(not(feature = "ring-huffman-cp"))]
    let (sec_p, flags): (Vec<u8>, u32) = {
        let sec = build_qwta_section(
            ring.col_qwt(Col::P).ok_or(MappedRingError::Layout(
                "C_p is Huffman substrate; rebuild with ring-huffman-cp for HQWA, or use Qwt C_p",
            ))?,
        )?;
        (sec, 0)
    };

    let a_o = ring.col_a_slice(Col::O);
    let a_p = ring.col_a_slice(Col::P);
    let a_s = ring.col_a_slice(Col::S);
    debug_assert_eq!(a_o.len(), ring.universe as usize + 1);
    debug_assert_eq!(a_p.len(), a_o.len());
    debug_assert_eq!(a_s.len(), a_o.len());

    // Diagnostic n_levels: max among Qwt columns (Huff C_p contributes 0).
    let n_levels = {
        let mut m = 0usize;
        if let Some(w) = ring.col_qwt(Col::O) {
            m = m.max(w.n_levels());
        }
        if let Some(w) = ring.col_qwt(Col::P) {
            m = m.max(w.n_levels());
        } else {
            #[cfg(feature = "ring-huffman-cp")]
            if let Some(h) = ring.c_p_substrate().as_huff() {
                m = m.max(h.wt().n_levels());
            }
        }
        if let Some(w) = ring.col_qwt(Col::S) {
            m = m.max(w.n_levels());
        }
        m as u16
    };

    // Layout after 4 KiB header pad: three page-aligned sections, then A.
    let mut cur = PAGE_ALIGN;
    let off_co = cur;
    cur += sec_o.len();
    cur = align_up(cur, PAGE_ALIGN);
    let off_cp = cur;
    cur += sec_p.len();
    cur = align_up(cur, PAGE_ALIGN);
    let off_cs = cur;
    cur += sec_s.len();
    cur = align_up(cur, 8);

    let off_ao = cur;
    let a_bytes = a_o.len() * 4;
    cur += a_bytes;
    cur = align_up(cur, 8);
    let off_ap = cur;
    cur += a_bytes;
    cur = align_up(cur, 8);
    let off_as = cur;
    cur += a_bytes;

    let mut buf = vec![0u8; cur];

    // Header (256 B)
    buf[0..8].copy_from_slice(NOVARNG1_MAGIC);
    buf[8..12].copy_from_slice(&RNG_FORMAT_VERSION.to_le_bytes());
    buf[12..16].copy_from_slice(&flags.to_le_bytes());
    buf[16..24].copy_from_slice(&(ring.n as u64).to_le_bytes());
    buf[24..28].copy_from_slice(&ring.universe.to_le_bytes());
    buf[28..30].copy_from_slice(&n_levels.to_le_bytes());
    buf[30] = PAGE_ALIGN_LOG2;
    // checksum_hdr at 32 filled below
    // checksum_body at 40 = 0
    buf[48..56].copy_from_slice(&(off_co as u64).to_le_bytes());
    buf[56..64].copy_from_slice(&(off_cp as u64).to_le_bytes());
    buf[64..72].copy_from_slice(&(off_cs as u64).to_le_bytes());
    buf[72..80].copy_from_slice(&(off_ao as u64).to_le_bytes());
    buf[80..88].copy_from_slice(&(off_ap as u64).to_le_bytes());
    buf[88..96].copy_from_slice(&(off_as as u64).to_le_bytes());
    let len_a = a_o.len() as u64;
    buf[96..104].copy_from_slice(&len_a.to_le_bytes());
    buf[104..112].copy_from_slice(&len_a.to_le_bytes());
    buf[112..120].copy_from_slice(&len_a.to_le_bytes());
    buf[120..128].copy_from_slice(&(sec_o.len() as u64).to_le_bytes());
    buf[128..136].copy_from_slice(&(sec_p.len() as u64).to_le_bytes());
    buf[136..144].copy_from_slice(&(sec_s.len() as u64).to_le_bytes());

    let mut hdr = [0u8; RNG_HEADER_SIZE];
    hdr.copy_from_slice(&buf[..RNG_HEADER_SIZE]);
    hdr[32..40].fill(0);
    let ck = fnv1a64(&hdr);
    buf[32..40].copy_from_slice(&ck.to_le_bytes());

    // Sections
    buf[off_co..off_co + sec_o.len()].copy_from_slice(&sec_o);
    buf[off_cp..off_cp + sec_p.len()].copy_from_slice(&sec_p);
    buf[off_cs..off_cs + sec_s.len()].copy_from_slice(&sec_s);

    write_u32_slice(&mut buf, off_ao, a_o);
    write_u32_slice(&mut buf, off_ap, a_p);
    write_u32_slice(&mut buf, off_as, a_s);

    Ok(buf)
}

/// Write `NOVARNG1` image to path; returns byte length.
pub fn write_novarng1_file(path: &Path, ring: &CyclicRing) -> Result<usize, MappedRingError> {
    let bytes = write_novarng1_v1(ring)?;
    let mut f = File::create(path)?;
    f.write_all(&bytes)?;
    Ok(bytes.len())
}

// ── open (mmap) ─────────────────────────────────────────────────────────────

/// Open a `NOVARNG1` file via `memmap2` (read-only). Header/dir only — no body scan.
pub fn open_novarng1_mmap(path: &Path) -> Result<MappedRingA, MappedRingError> {
    if !cfg!(target_endian = "little") {
        return Err(MappedRingError::NotLittleEndian);
    }
    let file = File::open(path)?;
    // SAFETY: file is read-only mapped; we never mutate through the mapping.
    let mmap = unsafe { Mmap::map(&file)? };
    open_novarng1_from_mmap(mmap)
}

/// Open from an already-mapped region (tests may use temp files).
pub fn open_novarng1_from_mmap(mmap: Mmap) -> Result<MappedRingA, MappedRingError> {
    let h = parse_header(&mmap)?;
    validate_directory(&mmap, &h)?;

    // Open O/S QWT sections once: validate magic/dir + build E5.11 B1 hot columns.
    let sec_o = MappedQwtSection::open(slice_range(&mmap, h.off_co, h.len_co)?)?;
    let sec_s = MappedQwtSection::open(slice_range(&mmap, h.off_cs, h.len_cs)?)?;
    let hot_o = sec_o.build_hot()?;
    let hot_s = sec_s.build_hot()?;

    // C_p: QWTA (default) or HQWA when flags bit0 set (feature ring-huffman-cp).
    let cp_bytes = slice_range(&mmap, h.off_cp, h.len_cp)?;
    #[cfg(feature = "ring-huffman-cp")]
    let (hot_p, hot_p_huff) = {
        let want_huff = (h.flags & RNG_FLAG_HUFF_CP) != 0;
        if want_huff {
            if cp_bytes.len() < 4 || &cp_bytes[0..4] != HQWA_MAGIC {
                return Err(MappedRingError::Layout(
                    "flags request HQWA C_p but section magic is not HQWA",
                ));
            }
            let sec_p = MappedHqwtSection::open(cp_bytes)?;
            let hot_h = sec_p.build_hot()?;
            // Keep a dummy empty Qwt hot_p so col_hot(Col::P) stays safe.
            (HotQwtColumn::empty_for_huff_placeholder(), Some(hot_h))
        } else {
            if cp_bytes.len() >= 4 && &cp_bytes[0..4] == HQWA_MAGIC {
                return Err(MappedRingError::Layout(
                    "C_p section is HQWA but RNG_FLAG_HUFF_CP not set",
                ));
            }
            let sec_p = MappedQwtSection::open(cp_bytes)?;
            (sec_p.build_hot()?, None)
        }
    };
    #[cfg(not(feature = "ring-huffman-cp"))]
    let hot_p = {
        if (h.flags & 1) != 0 {
            return Err(MappedRingError::Layout(
                "image has Huffman C_p (RNG_FLAG_HUFF_CP); rebuild with ring-huffman-cp",
            ));
        }
        let sec_p = MappedQwtSection::open(cp_bytes)?;
        sec_p.build_hot()?
    };

    // Bounds-check A arrays only (align + length).
    let _ = cast_u32_slice(&mmap, h.off_ao as usize, h.len_ao as usize)?;
    let _ = cast_u32_slice(&mmap, h.off_ap as usize, h.len_ap as usize)?;
    let _ = cast_u32_slice(&mmap, h.off_as as usize, h.len_as as usize)?;

    if h.len_ao != h.len_ap || h.len_ao != h.len_as {
        return Err(MappedRingError::Layout("A length mismatch"));
    }
    if h.len_ao != h.universe as u64 + 1 {
        return Err(MappedRingError::Layout("A length != U+1"));
    }

    Ok(MappedRingA {
        mmap,
        hot_o,
        hot_p,
        #[cfg(feature = "ring-huffman-cp")]
        hot_p_huff,
        hot_s,
        flags: h.flags,
        n: h.n as u32,
        universe: h.universe,
        n_levels: h.n_levels,
        off_co: h.off_co as usize,
        off_cp: h.off_cp as usize,
        off_cs: h.off_cs as usize,
        len_co: h.len_co as usize,
        len_cp: h.len_cp as usize,
        len_cs: h.len_cs as usize,
        off_ao: h.off_ao as usize,
        off_ap: h.off_ap as usize,
        off_as: h.off_as as usize,
        len_a: h.len_ao as usize,
    })
}

/// Parse header from bytes without mapping (harness / offline tools).
pub fn parse_header(bytes: &[u8]) -> Result<Novarng1Header, MappedRingError> {
    if bytes.len() < RNG_HEADER_SIZE {
        return Err(MappedRingError::Layout("short file"));
    }
    if &bytes[0..8] != NOVARNG1_MAGIC {
        return Err(MappedRingError::BadMagic);
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != RNG_FORMAT_VERSION {
        return Err(MappedRingError::BadVersion(version));
    }
    if bytes[30] != PAGE_ALIGN_LOG2 {
        return Err(MappedRingError::Layout("page_align_log2"));
    }
    let checksum_stored = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
    let mut hdr = [0u8; RNG_HEADER_SIZE];
    hdr.copy_from_slice(&bytes[..RNG_HEADER_SIZE]);
    hdr[32..40].fill(0);
    if fnv1a64(&hdr) != checksum_stored {
        return Err(MappedRingError::HeaderChecksum);
    }
    Ok(Novarng1Header {
        flags: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        n: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        universe: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
        n_levels: u16::from_le_bytes(bytes[28..30].try_into().unwrap()),
        page_align_log2: bytes[30],
        off_co: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
        off_cp: u64::from_le_bytes(bytes[56..64].try_into().unwrap()),
        off_cs: u64::from_le_bytes(bytes[64..72].try_into().unwrap()),
        off_ao: u64::from_le_bytes(bytes[72..80].try_into().unwrap()),
        off_ap: u64::from_le_bytes(bytes[80..88].try_into().unwrap()),
        off_as: u64::from_le_bytes(bytes[88..96].try_into().unwrap()),
        len_ao: u64::from_le_bytes(bytes[96..104].try_into().unwrap()),
        len_ap: u64::from_le_bytes(bytes[104..112].try_into().unwrap()),
        len_as: u64::from_le_bytes(bytes[112..120].try_into().unwrap()),
        len_co: u64::from_le_bytes(bytes[120..128].try_into().unwrap()),
        len_cp: u64::from_le_bytes(bytes[128..136].try_into().unwrap()),
        len_cs: u64::from_le_bytes(bytes[136..144].try_into().unwrap()),
    })
}

fn validate_directory(bytes: &[u8], h: &Novarng1Header) -> Result<(), MappedRingError> {
    let page = 1usize << h.page_align_log2;
    for (off, len, name) in [
        (h.off_co, h.len_co, "co"),
        (h.off_cp, h.len_cp, "cp"),
        (h.off_cs, h.len_cs, "cs"),
    ] {
        let _ = name;
        if (off as usize) % page != 0 {
            return Err(MappedRingError::Layout("QWT section not page-aligned"));
        }
        let end = (off as usize)
            .checked_add(len as usize)
            .ok_or(MappedRingError::Layout("section overflow"))?;
        if end > bytes.len() {
            return Err(MappedRingError::Layout("section OOB"));
        }
    }
    for (off, len) in [
        (h.off_ao, h.len_ao),
        (h.off_ap, h.len_ap),
        (h.off_as, h.len_as),
    ] {
        if (off as usize) % 4 != 0 {
            return Err(MappedRingError::Layout("A not 4-aligned"));
        }
        let need = (len as usize)
            .checked_mul(4)
            .ok_or(MappedRingError::Layout("A overflow"))?;
        let end = (off as usize)
            .checked_add(need)
            .ok_or(MappedRingError::Layout("A overflow"))?;
        if end > bytes.len() {
            return Err(MappedRingError::Layout("A OOB"));
        }
    }
    Ok(())
}

fn slice_range(bytes: &[u8], off: u64, len: u64) -> Result<&[u8], MappedRingError> {
    let off = off as usize;
    let len = len as usize;
    let end = off
        .checked_add(len)
        .ok_or(MappedRingError::Layout("slice overflow"))?;
    if end > bytes.len() {
        return Err(MappedRingError::Layout("slice OOB"));
    }
    Ok(&bytes[off..end])
}

fn write_u32_slice(buf: &mut [u8], off: usize, vals: &[u32]) {
    for (i, &v) in vals.iter().enumerate() {
        let o = off + i * 4;
        buf[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
}

fn cast_u32_slice(bytes: &[u8], off: usize, n: usize) -> Result<&[u32], MappedRingError> {
    let need = n
        .checked_mul(4)
        .ok_or(MappedRingError::Layout("overflow"))?;
    if off % 4 != 0 {
        return Err(MappedRingError::Layout("align"));
    }
    if off
        .checked_add(need)
        .map(|e| e > bytes.len())
        .unwrap_or(true)
    {
        return Err(MappedRingError::Layout("slice OOB"));
    }
    // SAFETY: alignment checked; u32 POD; LE host; bounds checked.
    Ok(unsafe { std::slice::from_raw_parts(bytes.as_ptr().add(off) as *const u32, n) })
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CyclicRing;
    use qwt::{AccessUnsigned, RankQuad};
    use std::io::Write;

    fn tiny_ring() -> CyclicRing {
        let triples = vec![[0, 0, 1], [0, 0, 2], [0, 1, 1], [1, 0, 0], [1, 1, 2]];
        // W1–W4 tests cover QWTA path; force Qwt C_p.
        CyclicRing::build_shared_qwt_cp(&triples, 3)
    }

    #[test]
    fn w1_roundtrip_mmap_access_and_a() {
        let ring = tiny_ring();
        let image = write_novarng1_v1(&ring).expect("write");
        assert_eq!(&image[0..8], NOVARNG1_MAGIC);
        assert!(image.len() >= PAGE_ALIGN);

        let dir = std::env::temp_dir().join(format!("novarng1_w1_{}.bin", std::process::id()));
        {
            let mut f = File::create(&dir).unwrap();
            f.write_all(&image).unwrap();
        }

        let mapped = open_novarng1_mmap(&dir).expect("mmap open");
        let _ = std::fs::remove_file(&dir);

        assert!(mapped.is_mmap_backed());
        assert_eq!(mapped.n, ring.n);
        assert_eq!(mapped.universe, ring.universe);
        assert_eq!(mapped.image_bytes(), image.len());

        // Access differential all columns
        for col in [Col::O, Col::P, Col::S] {
            let heap = ring.col_qwt(col).expect("Qwt C_p for NOVARNG1 tests");
            for i in 0..ring.n {
                assert_eq!(
                    mapped.access(col, i),
                    heap.get(i as usize),
                    "access {col:?} @{i}"
                );
            }
        }

        // A arrays
        for col in [Col::O, Col::P, Col::S] {
            let heap_a = ring.col_a_slice(col);
            let map_a = mapped.col_a(col).unwrap();
            assert_eq!(map_a, heap_a, "A {col:?}");
        }

        // Level-local rank_all
        for col in [Col::O, Col::P, Col::S] {
            let heap = ring.col_qwt(col).expect("Qwt C_p for NOVARNG1 tests");
            let levels = heap.levels();
            for level in 0..heap.n_levels() {
                let h = &levels[level];
                let nsym = h.len();
                for &i in &[0usize, 1, nsym / 2, nsym] {
                    if i > nsym {
                        continue;
                    }
                    let got = mapped.rank_all_level(col, level, i).unwrap();
                    let exp = unsafe { h.rank_all_unchecked(i) };
                    assert_eq!(got, exp, "rank_all {col:?} L{level} i={i}");
                }
            }
        }
    }

    #[test]
    fn w1_rejects_bad_magic() {
        let ring = tiny_ring();
        let mut image = write_novarng1_v1(&ring).unwrap();
        image[0] = b'X';
        let dir = std::env::temp_dir().join(format!("novarng1_bad_{}.bin", std::process::id()));
        std::fs::write(&dir, &image).unwrap();
        let err = open_novarng1_mmap(&dir);
        let _ = std::fs::remove_file(&dir);
        assert!(matches!(err, Err(MappedRingError::BadMagic)));
    }

    #[test]
    fn w1_rejects_bad_checksum() {
        let ring = tiny_ring();
        let mut image = write_novarng1_v1(&ring).unwrap();
        image[32] ^= 0xff;
        let dir = std::env::temp_dir().join(format!("novarng1_ck_{}.bin", std::process::id()));
        std::fs::write(&dir, &image).unwrap();
        let err = open_novarng1_mmap(&dir);
        let _ = std::fs::remove_file(&dir);
        assert!(matches!(err, Err(MappedRingError::HeaderChecksum)));
    }

    #[test]
    fn w1_page_aligned_sections() {
        let ring = tiny_ring();
        let image = write_novarng1_v1(&ring).unwrap();
        let h = parse_header(&image).unwrap();
        assert_eq!(h.off_co % PAGE_ALIGN as u64, 0);
        assert_eq!(h.off_cp % PAGE_ALIGN as u64, 0);
        assert_eq!(h.off_cs % PAGE_ALIGN as u64, 0);
        assert!(h.off_co >= PAGE_ALIGN as u64);
    }

    #[test]
    fn w2_rank_select_f_backward_vs_pilot() {
        let ring = tiny_ring();
        let image = write_novarng1_v1(&ring).unwrap();
        let dir = std::env::temp_dir().join(format!("novarng1_w2_{}.bin", std::process::id()));
        {
            let mut f = File::create(&dir).unwrap();
            f.write_all(&image).unwrap();
        }
        let mapped = open_novarng1_mmap(&dir).expect("mmap open");
        let _ = std::fs::remove_file(&dir);

        for col in [Col::O, Col::P, Col::S] {
            // Full-symbol rank
            for i in 0..=ring.n {
                for sym in 0..ring.universe.min(8) {
                    assert_eq!(
                        mapped.rank(col, sym, i),
                        Some(ring.rank(col, sym, i)),
                        "rank {col:?} sym={sym} i={i}"
                    );
                }
            }

            // Select first occurrences of present symbols
            for sym in 0..ring.universe {
                let total = ring.rank(col, sym, ring.n);
                for occ in 0..total.min(3) {
                    assert_eq!(
                        mapped.select(col, sym, occ),
                        ring.select(col, sym, occ),
                        "select {col:?} sym={sym} occ={occ}"
                    );
                }
            }

            // F and F_inv roundtrip
            for i in 0..ring.n {
                let f_m = mapped.f(col, i).expect("f");
                let f_h = ring.f(col, i);
                assert_eq!(f_m, f_h, "f {col:?} i={i}");
                assert_eq!(
                    mapped.f_inverse(col, f_m),
                    Some(ring.f_inverse(col, f_h)),
                    "f_inv {col:?} i'={f_m}"
                );
            }

            // Lead ranges
            for sym in 0..ring.universe {
                assert_eq!(
                    mapped.lead_range(col, sym),
                    Some(ring.lead_range(col, sym)),
                    "lead_range {col:?} sym={sym}"
                );
            }

            // Backward step on full range for a few symbols
            let full = RowRange::full(ring.n);
            for sym in 0..ring.universe.min(8) {
                assert_eq!(
                    mapped.backward_step(col, full, sym),
                    Some(ring.backward_step(col, full, sym)),
                    "backward_step {col:?} sym={sym}"
                );
            }
        }
    }

    #[test]
    fn w3_rnv_rdi_vs_pilot() {
        let ring = tiny_ring();
        let image = write_novarng1_v1(&ring).unwrap();
        let dir = std::env::temp_dir().join(format!("novarng1_w3_{}.bin", std::process::id()));
        {
            let mut f = File::create(&dir).unwrap();
            f.write_all(&image).unwrap();
        }
        let mapped = open_novarng1_mmap(&dir).expect("mmap open");
        let _ = std::fs::remove_file(&dir);

        let full = RowRange::full(ring.n);
        for col in [Col::O, Col::P, Col::S] {
            // RNV differential
            for t in 0..ring.universe + 2 {
                assert_eq!(
                    mapped.range_next_value(col, full, t),
                    ring.range_next_value_native(col, full, t),
                    "RNV {col:?} t={t}"
                );
            }
            // empty
            assert_eq!(mapped.range_next_value(col, RowRange::empty(), 0), None);

            // RDI ordered + counts
            let mut map_it = mapped.range_distinct_iter(col, full).unwrap();
            let mut heap_it = ring.range_distinct_iter(col, full);
            let mut prev = None;
            loop {
                let m = map_it.next_symbol().map(|(s, c)| (s, c as u32));
                let h = heap_it.next();
                assert_eq!(m, h, "RDI {col:?}");
                match m {
                    None => break,
                    Some((sym, _)) => {
                        if let Some(p) = prev {
                            assert!(sym > p);
                        }
                        prev = Some(sym);
                    }
                }
            }

            assert_eq!(
                mapped.range_distinct_count(col, full),
                Some(ring.range_distinct_count(col, full))
            );
        }
    }

    #[test]
    fn w4_enumerate_spo_matches_pilot() {
        let ring = tiny_ring();
        let image = write_novarng1_v1(&ring).unwrap();
        let dir = std::env::temp_dir().join(format!("novarng1_w4_{}.bin", std::process::id()));
        {
            let mut f = File::create(&dir).unwrap();
            f.write_all(&image).unwrap();
        }
        let mapped = open_novarng1_mmap(&dir).expect("mmap open");
        let _ = std::fs::remove_file(&dir);

        let mut heap = ring.enumerate_spo();
        let mut map = mapped.enumerate_spo().expect("enum");
        heap.sort_unstable();
        map.sort_unstable();
        assert_eq!(map, heap, "enumerate_spo multiset");
        for s in 0..ring.universe {
            assert_eq!(mapped.range_s(s), Some(ring.range_s(s)));
        }
    }

    #[test]
    #[cfg(feature = "ring-huffman-cp")]
    fn huff_cp_novarng1_roundtrip_vs_heap() {
        // Role-local tiny graph (matches cyclic::tests::tiny_role shape).
        let triples = vec![
            [0, 0, 0],
            [0, 0, 1],
            [0, 1, 2],
            [1, 0, 1],
            [1, 2, 3],
            [1, 1, 0],
        ];
        let ns = 2u32;
        let np = 3u32;
        let no = 4u32;
        let ring = CyclicRing::build_from_role_local_huff_cp(&triples, ns, np, no);
        assert!(ring.c_p_is_huff());

        let image = write_novarng1_v1(&ring).expect("huff flatten");
        let h = parse_header(&image).unwrap();
        assert_eq!(h.flags & RNG_FLAG_HUFF_CP, RNG_FLAG_HUFF_CP);
        let cp = &image[h.off_cp as usize..h.off_cp as usize + h.len_cp as usize];
        assert_eq!(&cp[0..4], HQWA_MAGIC);

        let dir = std::env::temp_dir().join(format!("novarng1_huff_{}.bin", std::process::id()));
        std::fs::write(&dir, &image).unwrap();
        let mapped = open_novarng1_mmap(&dir).expect("mmap open huff");
        let _ = std::fs::remove_file(&dir);

        assert!(mapped.c_p_is_huff());
        assert_eq!(mapped.n, ring.n);
        assert_eq!(mapped.universe, ring.universe);

        for col in [Col::O, Col::P, Col::S] {
            for i in 0..ring.n {
                assert_eq!(
                    mapped.access(col, i),
                    Some(ring.access(col, i)),
                    "access {col:?} @{i}"
                );
            }
            for i in 0..=ring.n {
                for sym in 0..ring.universe.min(16) {
                    assert_eq!(
                        mapped.rank(col, sym, i),
                        Some(ring.rank(col, sym, i)),
                        "rank {col:?} sym={sym} i={i}"
                    );
                }
            }
            for sym in 0..ring.universe {
                let total = ring.rank(col, sym, ring.n);
                for occ in 0..total.min(3) {
                    assert_eq!(
                        mapped.select(col, sym, occ),
                        ring.select(col, sym, occ),
                        "select {col:?} sym={sym} occ={occ}"
                    );
                }
            }
            for i in 0..ring.n {
                let f_m = mapped.f(col, i).expect("f");
                let f_h = ring.f(col, i);
                assert_eq!(f_m, f_h, "f {col:?} i={i}");
                assert_eq!(
                    mapped.f_inverse(col, f_m),
                    Some(ring.f_inverse(col, f_h)),
                    "f_inv {col:?} i'={f_m}"
                );
            }
        }

        let full = RowRange::full(ring.n);
        for col in [Col::O, Col::P, Col::S] {
            for tgt in 0..ring.universe + 2 {
                assert_eq!(
                    mapped.range_next_value(col, full, tgt),
                    ring.range_next_value_native(col, full, tgt),
                    "RNV {col:?} t={tgt}"
                );
            }
            let mut map_it = mapped.range_distinct_iter(col, full).unwrap();
            let mut heap_it = ring.range_distinct_iter(col, full);
            loop {
                let m = map_it.next_symbol().map(|(s, c)| (s, c as u32));
                let hv = heap_it.next();
                assert_eq!(m, hv, "RDI {col:?}");
                if m.is_none() {
                    break;
                }
            }
        }

        let mut heap = ring.enumerate_spo();
        let mut mapv = mapped.enumerate_spo().expect("enum");
        heap.sort_unstable();
        mapv.sort_unstable();
        assert_eq!(mapv, heap);

        // Qwt image still opens with flags=0
        let q = CyclicRing::build_from_role_local_qwt_cp(&triples, ns, np, no);
        let qimg = write_novarng1_v1(&q).unwrap();
        let qh = parse_header(&qimg).unwrap();
        assert_eq!(qh.flags, 0);
    }

}
