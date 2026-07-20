//! E5.10 W0/W1 — Nova-owned immutable mapped QWT (`NOVAQWT1` + shared QWTA).
//!
//! Flatten one vendored `QWT256` into a page-aligned image, open as slice
//! views (no full deserialize), and serve `access` + level-local `rank` /
//! `rank_all` + full-symbol `rank` / `select` (W2) + native RNV + fixed-stack
//! RDI (W3) with the same arithmetic as qwt block size 256 / E5.9A.

//! **Not** on the LoudsStore path. Feature `cyclic-ring-pilot` only.
//!
//! Platform: little-endian only (v1).
//!
//! # File layout (`NOVAQWT1` v1)
//!
//! ```text
//! [0..64)   file header (NOVAQWT1)
//! [64..4096) pad to page
//! [4096..)  QWTA section (page-aligned)
//! ```
//!
//! The **QWTA section** is also embedded three times inside `NOVARNG1`
//! (see [`super::mapped_ring`]). W1 reuses [`build_qwta_section`] and
//! [`MappedQwtSection`] without the single-column file header.
//!
//! ## File header (64 B, LE)
//!
//! | off | field |
//! |-----|--------|
//! | 0   | magic `NOVAQWT1` |
//! | 8   | version u32 (=1) |
//! | 12  | page_align_log2 u8 (=12) |
//! | 13  | reserved [3] |
//! | 16  | header FNV-1a u64 (field zeroed while hashing) |
//! | 24  | n u64 |
//! | 32  | n_levels u16 |
//! | 34  | sigma u32 |
//! | 38  | reserved [2] |
//! | 40  | section_off u64 |
//! | 48  | section_len u64 |
//! | 56  | reserved [8] |
//!
//! ## QWTA section
//!
//! | off | field |
//! |-----|--------|
//! | 0   | magic `QWTA` |
//! | 4   | n u64 |
//! | 12  | n_levels u16 |
//! | 14  | sigma u32 |
//! | 18  | reserved [2] |
//! | 20  | n_occs_smaller level-0 [5×u64] (convenience) |
//! | 60  | reserved [4] |
//! | 64  | level dir: n_levels × 128 B |
//! | …   | payloads (64-aligned): data, superblocks, occs[5], select[4] |
//!
//! ## Level dir entry (128 B)
//!
//! | off | field |
//! |-----|--------|
//! | 0   | off_data u64 |
//! | 8   | n_data_lines u64 |
//! | 16  | off_super u64 |
//! | 24  | n_superblocks u64 |
//! | 32  | off_select[4] u64 |
//! | 64  | n_select[4] u32 |
//! | 80  | qv_position_bits u64 |
//! | 88  | off_occs u64 (5×u64) |
//! | 96  | pad to 128 |
//!
//! Open validates header/bounds/align only — no full-body checksum preload.

use qwt::utils::select_in_word_u128;
use qwt::{AccessQuad, DataLine, QWT256, RankQuad, SuperblockPlain};
use std::io::{self, Write};
use std::ops::Range;
use std::path::Path;

/// Select sample period (matches vendored `RSSupportPlain::SELECT_NUM_SAMPLES`).
const SELECT_NUM_SAMPLES: usize = 1 << 13;

/// Magic for single-column W0 image.
pub const NOVAQWT1_MAGIC: &[u8; 8] = b"NOVAQWT1";
/// QWT section magic.
pub const QWTA_MAGIC: &[u8; 4] = b"QWTA";
pub const FORMAT_VERSION: u32 = 1;
pub const PAGE_ALIGN_LOG2: u8 = 12; // 4096
pub const PAGE_ALIGN: usize = 1 << PAGE_ALIGN_LOG2;
pub const HEADER_SIZE: usize = 64;
pub const MAX_LEVELS: usize = 32;
pub const BLOCK_SIZE: usize = 256;
pub const BLOCKS_IN_SUPERBLOCK: usize = 8;

/// Directory entry size (bytes), fixed LE fields — see module docs.
pub(crate) const LEVEL_DIR_V1: usize = 128;

/// Errors opening or validating a mapped QWT image.
#[derive(Debug, thiserror::Error)]
pub enum MappedQwtError {
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("bad magic (expected NOVAQWT1)")]
    BadMagic,
    #[error("unsupported version {0}")]
    BadVersion(u32),
    #[error("host is not little-endian (NOVAQWT1 v1 requires LE)")]
    NotLittleEndian,
    #[error("header checksum mismatch")]
    HeaderChecksum,
    #[error("bounds/alignment: {0}")]
    Layout(&'static str),
    #[error("too many levels: {0}")]
    TooManyLevels(usize),
}

/// One wavelet level as zero-copy views into the image.
#[derive(Clone, Copy)]
pub struct MappedLevel<'m> {
    pub data: &'m [DataLine],
    pub superblocks: &'m [SuperblockPlain],
    pub select_samples: [&'m [u32]; 4],
    /// Bit-cursor from qwt (`2 * #symbols` at this level).
    pub qv_position_bits: usize,
    /// Number of 2-bit symbols on this level.
    pub n_symbols: usize,
}

// ── E5.11 B1: open-time validated hot views ──────────────────────────────────

/// Compact native hot view for one wavelet level (E5.11 B1).
///
/// Built once at open after bounds/align validation. Hot primitives
/// (`level_rank` / `rank_all` / `range_expand` / RDI / RNV / full-symbol rank)
/// load through raw pointers with no repeated dir parse, u64→usize conversion,
/// slice re-indexing, or Option construction. Safety is justified by the
/// immutable image + open-time checks; the owning image must outlive this view.
///
/// Select samples stay on the cold `MappedLevel` path (RDI never needs them).
#[derive(Clone, Copy)]
pub struct HotMappedLevel {
    data: *const DataLine,
    superblocks: *const SuperblockPlain,
    data_len: usize,
    super_len: usize,
    /// Number of 2-bit symbols on this level (`qv_position_bits / 2`).
    qv_len: usize,
    /// `n_occs_smaller` as native `usize` (no per-call u64 cast).
    occs: [usize; 5],
}

// SAFETY: HotMappedLevel only aliases immutable image bytes; POD loads only.
unsafe impl Send for HotMappedLevel {}
unsafe impl Sync for HotMappedLevel {}

impl HotMappedLevel {
    const EMPTY: Self = Self {
        data: std::ptr::null(),
        superblocks: std::ptr::null(),
        data_len: 0,
        super_len: 0,
        qv_len: 0,
        occs: [0; 5],
    };

    #[inline(always)]
    unsafe fn data_at(&self, id: usize) -> &DataLine {
        debug_assert!(id < self.data_len);
        // SAFETY: open-time bounds; caller guarantees id < data_len.
        unsafe { &*self.data.add(id) }
    }

    #[inline(always)]
    unsafe fn super_at(&self, id: usize) -> &SuperblockPlain {
        debug_assert!(id < self.super_len);
        // SAFETY: open-time bounds; caller guarantees id < super_len.
        unsafe { &*self.superblocks.add(id) }
    }
}

/// Open-time hot column: all levels + native metadata for one QWTA section.
///
/// Pointers alias the owning image (mmap or `AlignedBuf`). Construct only via
/// [`MappedQwtSection::build_hot`] after validation; embed in the owner so the
/// image cannot be dropped while this is live.
#[derive(Clone, Copy)]
pub struct HotQwtColumn {
    pub n: usize,
    pub n_levels: usize,
    pub sigma: u32,
    levels: [HotMappedLevel; MAX_LEVELS],
}

// SAFETY: only aliases immutable image bytes.
unsafe impl Send for HotQwtColumn {}
unsafe impl Sync for HotQwtColumn {}

impl HotQwtColumn {
    const EMPTY: Self = Self {
        n: 0,
        n_levels: 0,
        sigma: 0,
        levels: [HotMappedLevel::EMPTY; MAX_LEVELS],
    };

    /// Placeholder when MappedRingA stores Huffman C_p (no Qwt hot view).
    #[inline]
    pub fn empty_for_huff_placeholder() -> Self {
        Self::EMPTY
    }

    #[inline(always)]
    fn level(&self, level: usize) -> &HotMappedLevel {
        debug_assert!(level < self.n_levels);
        &self.levels[level]
    }

    /// Symbol at position `i` (wavelet-matrix access).
    #[inline]
    pub fn get(&self, i: usize) -> Option<u32> {
        if i >= self.n || self.n_levels == 0 {
            return None;
        }
        let mut result: u32 = 0;
        let mut cur_i = i;
        for level in 0..self.n_levels - 1 {
            let lv = self.level(level);
            let symbol = hot_level_get(lv, cur_i);
            result = (result << 2) | symbol as u32;
            let offset = lv.occs[symbol as usize];
            cur_i = hot_level_rank(lv, symbol, cur_i) + offset;
        }
        let lv = self.level(self.n_levels - 1);
        let symbol = hot_level_get(lv, cur_i);
        Some((result << 2) | symbol as u32)
    }

    /// Level-local rank of 2-bit symbol `b ∈ 0..3` up to `i` (excluded).
    #[inline]
    pub fn rank_level(&self, level: usize, b: u8, i: usize) -> Option<usize> {
        if b > 3 || level >= self.n_levels {
            return None;
        }
        let lv = self.level(level);
        if i > lv.qv_len {
            return None;
        }
        Some(hot_level_rank(lv, b, i))
    }

    /// Level-local rank-all-4 up to `i` (excluded).
    #[inline]
    pub fn rank_all_level(&self, level: usize, i: usize) -> Option<[usize; 4]> {
        if level >= self.n_levels {
            return None;
        }
        let lv = self.level(level);
        if i > lv.qv_len {
            return None;
        }
        Some(hot_level_rank_all(lv, i))
    }

    /// Full-symbol rank of `symbol` in `[0, i)`.
    #[inline]
    pub fn rank(&self, symbol: u32, i: usize) -> Option<usize> {
        if self.n_levels == 0 {
            return if i <= self.n { Some(0) } else { None };
        }
        if i > self.n {
            return None;
        }
        if symbol > self.sigma {
            return Some(0);
        }
        Some(self.rank_unchecked(symbol, i))
    }

    /// Full-symbol rank without outer bounds checks.
    #[inline]
    pub fn rank_unchecked(&self, symbol: u32, i: usize) -> usize {
        debug_assert!(self.n_levels > 0);
        debug_assert!(i <= self.n);
        debug_assert!(symbol <= self.sigma);

        let mut shift: i64 = (2 * (self.n_levels - 1)) as i64;
        let mut cur_i = i;
        let mut cur_p = 0usize;

        for level in 0..self.n_levels - 1 {
            let two_bits = ((symbol >> shift as usize) & 3) as u8;
            let lv = self.level(level);
            let offset = lv.occs[two_bits as usize];
            cur_p = hot_level_rank(lv, two_bits, cur_p) + offset;
            cur_i = hot_level_rank(lv, two_bits, cur_i) + offset;
            shift -= 2;
        }

        let two_bits = ((symbol >> shift as usize) & 3) as u8;
        let lv = self.level(self.n_levels - 1);
        cur_i = hot_level_rank(lv, two_bits, cur_i);
        cur_p = hot_level_rank(lv, two_bits, cur_p);
        cur_i - cur_p
    }

    /// Native guided `range_next_value` — hot-path port (no per-frame level re-parse).
    pub fn range_next_value(&self, range: Range<usize>, target: u32) -> Option<u32> {
        if range.start > range.end || range.end > self.n || range.start == range.end {
            return None;
        }
        if self.n_levels == 0 {
            return None;
        }

        #[derive(Clone, Copy)]
        struct Frame {
            start: usize,
            end: usize,
            level: usize,
            sym_lo: usize,
            log2_width: u32,
        }

        let n_levels = self.n_levels;
        let target_us = target as usize;
        let full_log2: u32 = 2 * n_levels as u32;

        let mut stack = [Frame {
            start: 0,
            end: 0,
            level: 0,
            sym_lo: 0,
            log2_width: 0,
        }; 128];
        let mut sp: usize = 1;
        stack[0] = Frame {
            start: range.start,
            end: range.end,
            level: 0,
            sym_lo: 0,
            log2_width: full_log2,
        };

        while sp > 0 {
            sp -= 1;
            let cur = stack[sp];

            if cur.start >= cur.end {
                continue;
            }

            if cur.log2_width == 0 {
                if cur.sym_lo >= target_us {
                    return Some(cur.sym_lo as u32);
                }
                continue;
            }

            if cur.level >= n_levels {
                continue;
            }
            let lv = self.level(cur.level);
            let child_log = cur.log2_width - 2;
            let child_width = 1usize << child_log;

            let mut cand_s = [0usize; 4];
            let mut cand_e = [0usize; 4];
            let mut cand_lo = [0usize; 4];
            let mut nc = 0usize;

            for b in 0..4u8 {
                let child_sym_lo = cur.sym_lo + (b as usize) * child_width;
                let child_sym_hi = child_sym_lo + child_width;
                if child_sym_hi <= target_us {
                    continue;
                }
                let lo = hot_level_rank(lv, b, cur.start);
                let hi = hot_level_rank(lv, b, cur.end);
                if hi > lo {
                    let offset = lv.occs[b as usize];
                    cand_s[nc] = offset + lo;
                    cand_e[nc] = offset + hi;
                    cand_lo[nc] = child_sym_lo;
                    nc += 1;
                }
            }

            for i in (0..nc).rev() {
                debug_assert!(sp < 128);
                stack[sp] = Frame {
                    start: cand_s[i],
                    end: cand_e[i],
                    level: cur.level + 1,
                    sym_lo: cand_lo[i],
                    log2_width: child_log,
                };
                sp += 1;
            }
        }

        None
    }

    /// E5.11 D1: braided two-range intersection successor.
    ///
    /// Smallest symbol `v ≥ target` that occurs in **both** `left` and `right`.
    /// Synchronized 4-ary symbol-prefix descent: a child is entered only if
    /// both projected ranges are nonempty (mask_left AND mask_right). Fixed
    /// stack; zero heap; uses B2 `hot_level_range_expand` at each frame.
    ///
    /// Algebraically matches dual-RNV leapfrog:
    /// `rnv(L,t) → rnv(R,v) → rnv(L,w) …` until `v == w`, but shares one
    /// symbol-tree walk instead of restarting independent RNVs from the root.
    #[inline]
    pub fn intersection_next_value2(
        &self,
        left: Range<usize>,
        right: Range<usize>,
        target: u32,
    ) -> Option<u32> {
        self.intersection_next_value2_counted(left, right, target).0
    }

    /// Like [`Self::intersection_next_value2`] with software counters for the
    /// E5.11 D1 harness (levels, expands, one-side empty skips).
    pub fn intersection_next_value2_counted(
        &self,
        left: Range<usize>,
        right: Range<usize>,
        target: u32,
    ) -> (Option<u32>, Intersection2Stats) {
        let mut stats = Intersection2Stats::default();
        if left.start > left.end
            || right.start > right.end
            || left.end > self.n
            || right.end > self.n
            || left.start == left.end
            || right.start == right.end
        {
            return (None, stats);
        }
        if self.n_levels == 0 {
            return (None, stats);
        }

        #[derive(Clone, Copy)]
        struct Frame {
            l_start: usize,
            l_end: usize,
            r_start: usize,
            r_end: usize,
            level: usize,
            sym_lo: usize,
            log2_width: u32,
        }

        let n_levels = self.n_levels;
        let target_us = target as usize;
        let full_log2: u32 = 2 * n_levels as u32;

        let mut stack = [Frame {
            l_start: 0,
            l_end: 0,
            r_start: 0,
            r_end: 0,
            level: 0,
            sym_lo: 0,
            log2_width: 0,
        }; 128];
        let mut sp: usize = 1;
        stack[0] = Frame {
            l_start: left.start,
            l_end: left.end,
            r_start: right.start,
            r_end: right.end,
            level: 0,
            sym_lo: 0,
            log2_width: full_log2,
        };
        stats.root_starts = 1;

        while sp > 0 {
            sp -= 1;
            let cur = stack[sp];

            if cur.l_start >= cur.l_end || cur.r_start >= cur.r_end {
                continue;
            }

            // Leaf: single-symbol interval.
            if cur.log2_width == 0 {
                if cur.sym_lo >= target_us {
                    stats.hits = 1;
                    return (Some(cur.sym_lo as u32), stats);
                }
                continue;
            }

            if cur.level >= n_levels {
                continue;
            }
            stats.levels_visited += 1;

            let lv = self.level(cur.level);
            let child_log = cur.log2_width - 2;
            let child_width = 1usize << child_log;

            // B2 fused expand on both projected ranges (one SB/line when local).
            let exp_l = hot_level_range_expand(lv, cur.l_start, cur.l_end);
            let exp_r = hot_level_range_expand(lv, cur.r_start, cur.r_end);
            stats.expands += 2;
            stats.data_line_loads += exp_l.data_line_loads + exp_r.data_line_loads;
            stats.superblock_loads += exp_l.superblock_loads + exp_r.superblock_loads;
            match exp_l.path {
                RangeCountsPath::SameLine => stats.same_line_hits += 1,
                RangeCountsPath::SameSuperblock => stats.same_sb_hits += 1,
                RangeCountsPath::General => stats.general_hits += 1,
            }
            match exp_r.path {
                RangeCountsPath::SameLine => stats.same_line_hits += 1,
                RangeCountsPath::SameSuperblock => stats.same_sb_hits += 1,
                RangeCountsPath::General => stats.general_hits += 1,
            }

            let mut cand_ls = [0usize; 4];
            let mut cand_le = [0usize; 4];
            let mut cand_rs = [0usize; 4];
            let mut cand_re = [0usize; 4];
            let mut cand_lo = [0usize; 4];
            let mut nc = 0usize;

            for b in 0..4u8 {
                let bi = b as usize;
                let child_sym_lo = cur.sym_lo + bi * child_width;
                let child_sym_hi = child_sym_lo + child_width;
                // Child entirely < target → skip.
                if child_sym_hi <= target_us {
                    stats.target_skips += 1;
                    continue;
                }
                let cl = exp_l.counts[bi];
                let cr = exp_r.counts[bi];
                // Braided mask: both sides must be nonempty.
                if cl == 0 || cr == 0 {
                    if cl != cr {
                        // Exactly one side present → independent RNV would
                        // explore this branch on one range; we prune both.
                        stats.one_side_empty += 1;
                    } else {
                        stats.both_empty += 1;
                    }
                    continue;
                }
                let offset = lv.occs[bi];
                cand_ls[nc] = offset + exp_l.ranks_s[bi];
                cand_le[nc] = offset + exp_l.ranks_s[bi] + cl;
                cand_rs[nc] = offset + exp_r.ranks_s[bi];
                cand_re[nc] = offset + exp_r.ranks_s[bi] + cr;
                cand_lo[nc] = child_sym_lo;
                nc += 1;
            }
            stats.children_pushed += nc as u32;

            // Push reverse so left-to-right (smallest symbol) is explored first.
            for i in (0..nc).rev() {
                debug_assert!(sp < 128);
                stack[sp] = Frame {
                    l_start: cand_ls[i],
                    l_end: cand_le[i],
                    r_start: cand_rs[i],
                    r_end: cand_re[i],
                    level: cur.level + 1,
                    sym_lo: cand_lo[i],
                    log2_width: child_log,
                };
                sp += 1;
            }
        }

        (None, stats)
    }

    /// Dual-RNV leapfrog oracle for D1 correctness / baseline cost.
    ///
    /// Restarts independent `range_next_value` from the root on each seek.
    /// Semantically identical to [`Self::intersection_next_value2`].
    pub fn intersection_next_value2_dual_rnv(
        &self,
        left: Range<usize>,
        right: Range<usize>,
        target: u32,
    ) -> Option<u32> {
        let mut t = target;
        // Cap iterations: at most σ+1 seeks (defensive; alphabet is finite).
        for _ in 0..=(self.sigma as usize).saturating_add(2) {
            let v = self.range_next_value(left.clone(), t)?;
            let w = self.range_next_value(right.clone(), v)?;
            if w == v {
                return Some(v);
            }
            // w > v: seek left to ≥ w.
            t = w;
        }
        None
    }

    /// E5.11 D2: braided three-range intersection successor.
    ///
    /// Returns the smallest symbol `v >= target` present in all three row
    /// ranges.  This is the product-triangle form of D1: all three projected
    /// ranges are expanded at the same wavelet level and a child is entered
    /// only when its three counts are non-zero.  The fixed stack is bounded by
    /// the four-way QWT depth and does not allocate.
    #[inline]
    pub fn intersection_next_value3(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> Option<u32> {
        if first.start > first.end
            || second.start > second.end
            || third.start > third.end
            || first.end > self.n
            || second.end > self.n
            || third.end > self.n
            || first.start == first.end
            || second.start == second.end
            || third.start == third.end
            || self.n_levels == 0
        {
            return None;
        }

        #[derive(Clone, Copy)]
        struct Frame {
            a_start: usize,
            a_end: usize,
            b_start: usize,
            b_end: usize,
            c_start: usize,
            c_end: usize,
            level: usize,
            sym_lo: usize,
            log2_width: u32,
        }

        let full_log2 = 2 * self.n_levels as u32;
        let target = target as usize;
        let mut stack = [Frame {
            a_start: 0,
            a_end: 0,
            b_start: 0,
            b_end: 0,
            c_start: 0,
            c_end: 0,
            level: 0,
            sym_lo: 0,
            log2_width: 0,
        }; 128];
        let mut sp = 1usize;
        stack[0] = Frame {
            a_start: first.start,
            a_end: first.end,
            b_start: second.start,
            b_end: second.end,
            c_start: third.start,
            c_end: third.end,
            level: 0,
            sym_lo: 0,
            log2_width: full_log2,
        };

        while sp > 0 {
            sp -= 1;
            let cur = stack[sp];
            if cur.a_start >= cur.a_end || cur.b_start >= cur.b_end || cur.c_start >= cur.c_end {
                continue;
            }
            if cur.log2_width == 0 {
                if cur.sym_lo >= target {
                    return Some(cur.sym_lo as u32);
                }
                continue;
            }
            if cur.level >= self.n_levels {
                continue;
            }

            let lv = self.level(cur.level);
            let exp_a = hot_level_range_expand(lv, cur.a_start, cur.a_end);
            let exp_b = hot_level_range_expand(lv, cur.b_start, cur.b_end);
            let exp_c = hot_level_range_expand(lv, cur.c_start, cur.c_end);
            let child_log = cur.log2_width - 2;
            let child_width = 1usize << child_log;
            let mut children = [Frame {
                a_start: 0,
                a_end: 0,
                b_start: 0,
                b_end: 0,
                c_start: 0,
                c_end: 0,
                level: 0,
                sym_lo: 0,
                log2_width: 0,
            }; 4];
            let mut nc = 0usize;
            for b in 0..4usize {
                let child_lo = cur.sym_lo + b * child_width;
                if child_lo + child_width <= target {
                    continue;
                }
                let ca = exp_a.counts[b];
                let cb = exp_b.counts[b];
                let cc = exp_c.counts[b];
                if ca == 0 || cb == 0 || cc == 0 {
                    continue;
                }
                let offset = lv.occs[b];
                children[nc] = Frame {
                    a_start: offset + exp_a.ranks_s[b],
                    a_end: offset + exp_a.ranks_s[b] + ca,
                    b_start: offset + exp_b.ranks_s[b],
                    b_end: offset + exp_b.ranks_s[b] + cb,
                    c_start: offset + exp_c.ranks_s[b],
                    c_end: offset + exp_c.ranks_s[b] + cc,
                    level: cur.level + 1,
                    sym_lo: child_lo,
                    log2_width: child_log,
                };
                nc += 1;
            }
            for i in (0..nc).rev() {
                debug_assert!(sp < 128);
                stack[sp] = children[i];
                sp += 1;
            }
        }
        None
    }

    /// D3-A diagnostic form of [`Self::intersection_next_value3`].
    ///
    /// The traversal and ordering are identical to the uncounted API.  Keep
    /// this out of the product path: it exists so the measurement harness can
    /// attribute level/range work and root restarts without guessing from wall
    /// time.
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_counted(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> (Option<u32>, Intersection3Stats) {
        let mut stats = Intersection3Stats::default();
        if first.start > first.end
            || second.start > second.end
            || third.start > third.end
            || first.end > self.n
            || second.end > self.n
            || third.end > self.n
            || first.start == first.end
            || second.start == second.end
            || third.start == third.end
            || self.n_levels == 0
        {
            return (None, stats);
        }
        stats.root_restarts = 1;

        #[derive(Clone, Copy)]
        struct Frame {
            a_start: usize,
            a_end: usize,
            b_start: usize,
            b_end: usize,
            c_start: usize,
            c_end: usize,
            level: usize,
            sym_lo: usize,
            log2_width: u32,
        }

        let full_log2 = 2 * self.n_levels as u32;
        let target = target as usize;
        let mut stack = [Frame {
            a_start: 0,
            a_end: 0,
            b_start: 0,
            b_end: 0,
            c_start: 0,
            c_end: 0,
            level: 0,
            sym_lo: 0,
            log2_width: 0,
        }; 128];
        let mut sp = 1usize;
        stack[0] = Frame {
            a_start: first.start,
            a_end: first.end,
            b_start: second.start,
            b_end: second.end,
            c_start: third.start,
            c_end: third.end,
            level: 0,
            sym_lo: 0,
            log2_width: full_log2,
        };

        while sp > 0 {
            sp -= 1;
            let cur = stack[sp];
            if cur.a_start >= cur.a_end || cur.b_start >= cur.b_end || cur.c_start >= cur.c_end {
                continue;
            }
            if cur.log2_width == 0 {
                if cur.sym_lo >= target {
                    stats.hit = 1;
                    return (Some(cur.sym_lo as u32), stats);
                }
                continue;
            }
            if cur.level >= self.n_levels {
                continue;
            }

            let lv = self.level(cur.level);
            stats.levels_visited += 1;
            stats.expands += 3;
            let exp_a = hot_level_range_expand(lv, cur.a_start, cur.a_end);
            let exp_b = hot_level_range_expand(lv, cur.b_start, cur.b_end);
            let exp_c = hot_level_range_expand(lv, cur.c_start, cur.c_end);
            let child_log = cur.log2_width - 2;
            let child_width = 1usize << child_log;
            let mut children = [Frame {
                a_start: 0,
                a_end: 0,
                b_start: 0,
                b_end: 0,
                c_start: 0,
                c_end: 0,
                level: 0,
                sym_lo: 0,
                log2_width: 0,
            }; 4];
            let mut nc = 0usize;

            for b in 0..4usize {
                let child_lo = cur.sym_lo + b * child_width;
                if child_lo + child_width <= target {
                    continue;
                }
                let ca = exp_a.counts[b];
                let cb = exp_b.counts[b];
                let cc = exp_c.counts[b];
                if ca == 0 || cb == 0 || cc == 0 {
                    continue;
                }
                let offset = lv.occs[b];
                children[nc] = Frame {
                    a_start: offset + exp_a.ranks_s[b],
                    a_end: offset + exp_a.ranks_s[b] + ca,
                    b_start: offset + exp_b.ranks_s[b],
                    b_end: offset + exp_b.ranks_s[b] + cb,
                    c_start: offset + exp_c.ranks_s[b],
                    c_end: offset + exp_c.ranks_s[b] + cc,
                    level: cur.level + 1,
                    sym_lo: child_lo,
                    log2_width: child_log,
                };
                nc += 1;
            }
            stats.children_pushed += nc as u32;

            for i in (0..nc).rev() {
                debug_assert!(sp < 128);
                stack[sp] = children[i];
                sp += 1;
            }
        }
        (None, stats)
    }

    /// D4-A: walk the D2 three-range braid and measure unique DataLine /
    /// Superblock overlap across the three independent expands at each frame.
    ///
    /// Does **not** change product semantics. Uses the same push-all DFS as
    /// [`Self::intersection_next_value3`] so counters align with product work.
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_overlap(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> (Option<u32>, Expand3OverlapStats) {
        let mut stats = Expand3OverlapStats::default();
        if first.start > first.end
            || second.start > second.end
            || third.start > third.end
            || first.end > self.n
            || second.end > self.n
            || third.end > self.n
            || first.start == first.end
            || second.start == second.end
            || third.start == third.end
            || self.n_levels == 0
        {
            return (None, stats);
        }

        #[derive(Clone, Copy)]
        struct Frame {
            a_start: usize,
            a_end: usize,
            b_start: usize,
            b_end: usize,
            c_start: usize,
            c_end: usize,
            level: usize,
            sym_lo: usize,
            log2_width: u32,
        }

        let full_log2 = 2 * self.n_levels as u32;
        let target = target as usize;
        let mut stack = [Frame {
            a_start: 0,
            a_end: 0,
            b_start: 0,
            b_end: 0,
            c_start: 0,
            c_end: 0,
            level: 0,
            sym_lo: 0,
            log2_width: 0,
        }; 128];
        let mut sp = 1usize;
        stack[0] = Frame {
            a_start: first.start,
            a_end: first.end,
            b_start: second.start,
            b_end: second.end,
            c_start: third.start,
            c_end: third.end,
            level: 0,
            sym_lo: 0,
            log2_width: full_log2,
        };

        while sp > 0 {
            sp -= 1;
            let cur = stack[sp];
            if cur.a_start >= cur.a_end || cur.b_start >= cur.b_end || cur.c_start >= cur.c_end {
                continue;
            }
            if cur.log2_width == 0 {
                if cur.sym_lo >= target {
                    stats.hit = 1;
                    return (Some(cur.sym_lo as u32), stats);
                }
                continue;
            }
            if cur.level >= self.n_levels {
                continue;
            }

            let lv = self.level(cur.level);
            stats.frames += 1;
            stats.expands_indep += 3;

            let touch_a = range_touch_ids(cur.a_start, cur.a_end);
            let touch_b = range_touch_ids(cur.b_start, cur.b_end);
            let touch_c = range_touch_ids(cur.c_start, cur.c_end);
            let (uline, usb) = unique_touch3(touch_a, touch_b, touch_c);
            stats.unique_lines += uline as u64;
            stats.unique_sbs += usb as u64;
            stats.line_loads_indep +=
                touch_a.line_loads as u64 + touch_b.line_loads as u64 + touch_c.line_loads as u64;
            stats.sb_loads_indep +=
                touch_a.sb_loads as u64 + touch_b.sb_loads as u64 + touch_c.sb_loads as u64;

            let all_same_line = touch_a.same_line && touch_b.same_line && touch_c.same_line;
            if all_same_line {
                if touch_a.line_s == touch_b.line_s && touch_b.line_s == touch_c.line_s {
                    stats.all_same_line_shared += 1;
                } else {
                    stats.all_same_line_distinct += 1;
                }
            }
            if touch_a.sb_s == touch_a.sb_e
                && touch_b.sb_s == touch_b.sb_e
                && touch_c.sb_s == touch_c.sb_e
                && touch_a.sb_s == touch_b.sb_s
                && touch_b.sb_s == touch_c.sb_s
            {
                stats.all_same_sb += 1;
            }
            let pair_line = lines_overlap(touch_a, touch_b)
                || lines_overlap(touch_a, touch_c)
                || lines_overlap(touch_b, touch_c);
            if pair_line {
                stats.pair_line_overlap += 1;
            } else {
                stats.no_line_overlap += 1;
            }

            let exp_a = hot_level_range_expand(lv, cur.a_start, cur.a_end);
            let exp_b = hot_level_range_expand(lv, cur.b_start, cur.b_end);
            let exp_c = hot_level_range_expand(lv, cur.c_start, cur.c_end);
            let child_log = cur.log2_width - 2;
            let child_width = 1usize << child_log;
            let mut children = [Frame {
                a_start: 0,
                a_end: 0,
                b_start: 0,
                b_end: 0,
                c_start: 0,
                c_end: 0,
                level: 0,
                sym_lo: 0,
                log2_width: 0,
            }; 4];
            let mut nc = 0usize;
            for b in 0..4usize {
                let child_lo = cur.sym_lo + b * child_width;
                if child_lo + child_width <= target {
                    continue;
                }
                let ca = exp_a.counts[b];
                let cb = exp_b.counts[b];
                let cc = exp_c.counts[b];
                if ca == 0 || cb == 0 || cc == 0 {
                    continue;
                }
                let offset = lv.occs[b];
                children[nc] = Frame {
                    a_start: offset + exp_a.ranks_s[b],
                    a_end: offset + exp_a.ranks_s[b] + ca,
                    b_start: offset + exp_b.ranks_s[b],
                    b_end: offset + exp_b.ranks_s[b] + cb,
                    c_start: offset + exp_c.ranks_s[b],
                    c_end: offset + exp_c.ranks_s[b] + cc,
                    level: cur.level + 1,
                    sym_lo: child_lo,
                    log2_width: child_log,
                };
                nc += 1;
            }
            for i in (0..nc).rev() {
                debug_assert!(sp < 128);
                stack[sp] = children[i];
                sp += 1;
            }
        }
        (None, stats)
    }

    /// D4-B prototype: mask-first three-range braid with fused expand3 bookkeeping.
    ///
    /// Cheap occupancy masks are formed first (same B2 expands, but the
    /// three-way AND is applied before child materialization). When the mask
    /// is empty the frame is pruned without pushing children. Full projected
    /// child ranges are only built for live bits. Unique line/SB ids are
    /// counted so D4-A locality can be compared against this prototype.
    ///
    /// Algebraically equivalent to [`Self::intersection_next_value3`]. Not on
    /// the product path until measurement shows a clear keep.
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_fused(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> (Option<u32>, Expand3FusedStats) {
        let mut stats = Expand3FusedStats::default();
        if first.start > first.end
            || second.start > second.end
            || third.start > third.end
            || first.end > self.n
            || second.end > self.n
            || third.end > self.n
            || first.start == first.end
            || second.start == second.end
            || third.start == third.end
            || self.n_levels == 0
        {
            return (None, stats);
        }
        stats.root_restarts = 1;

        #[derive(Clone, Copy)]
        struct Frame {
            a_start: usize,
            a_end: usize,
            b_start: usize,
            b_end: usize,
            c_start: usize,
            c_end: usize,
            level: usize,
            sym_lo: usize,
            log2_width: u32,
        }

        let full_log2 = 2 * self.n_levels as u32;
        let target = target as usize;
        let mut stack = [Frame {
            a_start: 0,
            a_end: 0,
            b_start: 0,
            b_end: 0,
            c_start: 0,
            c_end: 0,
            level: 0,
            sym_lo: 0,
            log2_width: 0,
        }; 128];
        let mut sp = 1usize;
        stack[0] = Frame {
            a_start: first.start,
            a_end: first.end,
            b_start: second.start,
            b_end: second.end,
            c_start: third.start,
            c_end: third.end,
            level: 0,
            sym_lo: 0,
            log2_width: full_log2,
        };

        while sp > 0 {
            sp -= 1;
            let cur = stack[sp];
            if cur.a_start >= cur.a_end || cur.b_start >= cur.b_end || cur.c_start >= cur.c_end {
                continue;
            }
            if cur.log2_width == 0 {
                if cur.sym_lo >= target {
                    stats.hit = 1;
                    return (Some(cur.sym_lo as u32), stats);
                }
                continue;
            }
            if cur.level >= self.n_levels {
                continue;
            }

            let lv = self.level(cur.level);
            stats.levels_visited += 1;

            // Locality accounting (same as D4-A).
            let touch_a = range_touch_ids(cur.a_start, cur.a_end);
            let touch_b = range_touch_ids(cur.b_start, cur.b_end);
            let touch_c = range_touch_ids(cur.c_start, cur.c_end);
            let (uline, usb) = unique_touch3(touch_a, touch_b, touch_c);
            stats.unique_lines += uline as u64;
            stats.unique_sbs += usb as u64;

            // Mask-first: expand all three, form occupancy masks, AND.
            let exp_a = hot_level_range_expand(lv, cur.a_start, cur.a_end);
            let exp_b = hot_level_range_expand(lv, cur.b_start, cur.b_end);
            let exp_c = hot_level_range_expand(lv, cur.c_start, cur.c_end);
            stats.expands_full += 1;

            let mut mask_a = 0u8;
            let mut mask_b = 0u8;
            let mut mask_c = 0u8;
            for b in 0..4usize {
                if exp_a.counts[b] != 0 {
                    mask_a |= 1 << b;
                }
                if exp_b.counts[b] != 0 {
                    mask_b |= 1 << b;
                }
                if exp_c.counts[b] != 0 {
                    mask_c |= 1 << b;
                }
            }
            let mut live = mask_a & mask_b & mask_c;
            if live == 0 {
                stats.mask_pruned += 1;
                continue;
            }

            let child_log = cur.log2_width - 2;
            let child_width = 1usize << child_log;
            let mut children = [Frame {
                a_start: 0,
                a_end: 0,
                b_start: 0,
                b_end: 0,
                c_start: 0,
                c_end: 0,
                level: 0,
                sym_lo: 0,
                log2_width: 0,
            }; 4];
            let mut nc = 0usize;

            // Materialize only live, target-viable children (ascending).
            while live != 0 {
                let b = live.trailing_zeros() as usize;
                live &= !(1 << b);
                let child_lo = cur.sym_lo + b * child_width;
                if child_lo + child_width <= target {
                    continue;
                }
                let ca = exp_a.counts[b];
                let cb = exp_b.counts[b];
                let cc = exp_c.counts[b];
                // live bits guarantee nonzero counts, but keep the guard.
                if ca == 0 || cb == 0 || cc == 0 {
                    continue;
                }
                let offset = lv.occs[b];
                children[nc] = Frame {
                    a_start: offset + exp_a.ranks_s[b],
                    a_end: offset + exp_a.ranks_s[b] + ca,
                    b_start: offset + exp_b.ranks_s[b],
                    b_end: offset + exp_b.ranks_s[b] + cb,
                    c_start: offset + exp_c.ranks_s[b],
                    c_end: offset + exp_c.ranks_s[b] + cc,
                    level: cur.level + 1,
                    sym_lo: child_lo,
                    log2_width: child_log,
                };
                nc += 1;
            }
            stats.children_pushed += nc as u32;
            for i in (0..nc).rev() {
                debug_assert!(sp < 128);
                stack[sp] = children[i];
                sp += 1;
            }
        }
        (None, stats)
    }

    /// D4-C: true unique-load expand3 three-range successor (diagnostic).
    ///
    /// Same push-all DFS as [`Self::intersection_next_value3`], but each frame
    /// expands all three ranges via [`hot_level_range_expand3`] which loads each
    /// unique DataLine / Superblock once. Not on the product path until
    /// measurement shows ≥10% stream gain (or ≥5% product-triangle gain).
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_shared(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> (Option<u32>, Expand3SharedStats) {
        let mut stats = Expand3SharedStats::default();
        if first.start > first.end
            || second.start > second.end
            || third.start > third.end
            || first.end > self.n
            || second.end > self.n
            || third.end > self.n
            || first.start == first.end
            || second.start == second.end
            || third.start == third.end
            || self.n_levels == 0
        {
            return (None, stats);
        }
        stats.root_restarts = 1;

        #[derive(Clone, Copy)]
        struct Frame {
            a_start: usize,
            a_end: usize,
            b_start: usize,
            b_end: usize,
            c_start: usize,
            c_end: usize,
            level: usize,
            sym_lo: usize,
            log2_width: u32,
        }

        let full_log2 = 2 * self.n_levels as u32;
        let target = target as usize;
        let mut stack = [Frame {
            a_start: 0,
            a_end: 0,
            b_start: 0,
            b_end: 0,
            c_start: 0,
            c_end: 0,
            level: 0,
            sym_lo: 0,
            log2_width: 0,
        }; 128];
        let mut sp = 1usize;
        stack[0] = Frame {
            a_start: first.start,
            a_end: first.end,
            b_start: second.start,
            b_end: second.end,
            c_start: third.start,
            c_end: third.end,
            level: 0,
            sym_lo: 0,
            log2_width: full_log2,
        };

        while sp > 0 {
            sp -= 1;
            let cur = stack[sp];
            if cur.a_start >= cur.a_end || cur.b_start >= cur.b_end || cur.c_start >= cur.c_end {
                continue;
            }
            if cur.log2_width == 0 {
                if cur.sym_lo >= target {
                    stats.hit = 1;
                    return (Some(cur.sym_lo as u32), stats);
                }
                continue;
            }
            if cur.level >= self.n_levels {
                continue;
            }

            let lv = self.level(cur.level);
            stats.frames += 1;
            let exp3 = hot_level_range_expand3(
                lv,
                cur.a_start,
                cur.a_end,
                cur.b_start,
                cur.b_end,
                cur.c_start,
                cur.c_end,
            );
            stats.logical_line_requests += exp3.logical_line_requests as u64;
            stats.unique_line_loads += exp3.unique_line_loads as u64;
            stats.logical_sb_requests += exp3.logical_sb_requests as u64;
            stats.unique_sb_loads += exp3.unique_sb_loads as u64;
            stats.line_loads_saved +=
                exp3.logical_line_requests
                    .saturating_sub(exp3.unique_line_loads) as u64;
            stats.sb_loads_saved += exp3
                .logical_sb_requests
                .saturating_sub(exp3.unique_sb_loads) as u64;
            match exp3.path {
                Expand3Path::AllSameLine => stats.all_same_line_fast_hits += 1,
                Expand3Path::TwoLine => stats.two_line_fast_hits += 1,
                Expand3Path::SharedSb => stats.shared_sb_hits += 1,
                Expand3Path::General => stats.general_hits += 1,
            }

            let child_log = cur.log2_width - 2;
            let child_width = 1usize << child_log;
            let mut children = [Frame {
                a_start: 0,
                a_end: 0,
                b_start: 0,
                b_end: 0,
                c_start: 0,
                c_end: 0,
                level: 0,
                sym_lo: 0,
                log2_width: 0,
            }; 4];
            let mut nc = 0usize;
            let mut live = exp3.common_mask;
            while live != 0 {
                let b = live.trailing_zeros() as usize;
                live &= !(1 << b);
                let child_lo = cur.sym_lo + b * child_width;
                if child_lo + child_width <= target {
                    continue;
                }
                let ca = exp3.first.counts[b];
                let cb = exp3.second.counts[b];
                let cc = exp3.third.counts[b];
                if ca == 0 || cb == 0 || cc == 0 {
                    continue;
                }
                let offset = lv.occs[b];
                children[nc] = Frame {
                    a_start: offset + exp3.first.ranks_s[b],
                    a_end: offset + exp3.first.ranks_s[b] + ca,
                    b_start: offset + exp3.second.ranks_s[b],
                    b_end: offset + exp3.second.ranks_s[b] + cb,
                    c_start: offset + exp3.third.ranks_s[b],
                    c_end: offset + exp3.third.ranks_s[b] + cc,
                    level: cur.level + 1,
                    sym_lo: child_lo,
                    log2_width: child_log,
                };
                nc += 1;
            }
            stats.children_pushed += nc as u32;
            for i in (0..nc).rev() {
                debug_assert!(sp < 128);
                stack[sp] = children[i];
                sp += 1;
            }
        }
        (None, stats)
    }

    /// Three independent RNV leapfrog used as the D2 correctness oracle.
    #[inline]
    pub fn intersection_next_value3_dual_rnv(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> Option<u32> {
        if first.start >= first.end || second.start >= second.end || third.start >= third.end {
            return None;
        }
        let mut t = target;
        for _ in 0..=(self.sigma as usize).saturating_add(3) {
            let a = self.range_next_value(first.clone(), t)?;
            let b = self.range_next_value(second.clone(), a)?;
            let c = self.range_next_value(third.clone(), b)?;
            if a == b && b == c {
                return Some(a);
            }
            t = c;
        }
        None
    }

    /// Persistent fixed-stack three-range intersection iterator (D3-B).
    ///
    /// The stack retains the projected node ranges and a remaining child mask
    /// between calls, so an emitted leaf resumes at the nearest unexplored
    /// common sibling instead of restarting at the root. No heap allocation or
    /// persistent image bytes are used.
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_iter3(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> IntersectionIter3<'_> {
        IntersectionIter3::new(self, first, second, third, target)
    }

    /// Fixed-stack ordered distinct-symbol iterator over a hot column.
    ///
    /// E5.11 C1: ranges of length ≤ [`c1_batch_k`] use level-batched short-range
    /// decode (fixed stack, no empty-branch exploration); longer ranges fall
    /// back to generic RDI.
    pub fn range_distinct_iter(&self, range: Range<usize>) -> MappedRangeDistinctIter<'_> {
        MappedRangeDistinctIter::new(self, range)
    }

    /// E5.11 C1: decode all symbols in a short range via level-batched access,
    /// then sort + RLE. Returns `None` if range is empty or longer than `k`.
    ///
    /// Algebraically identical to one full-symbol `get` per row followed by
    /// sort/dedup — **not** a tree RDI. Eliminates empty-branch exploration.
    #[inline]
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn short_range_batch_decode(
        &self,
        range: Range<usize>,
        k: usize,
    ) -> Option<ShortRangeBatchResult> {
        short_range_batch_decode(self, range, k)
    }
}

// ── E5.11 D1: two-range braided intersection stats ──────────────────────────

/// Software counters for [`HotQwtColumn::intersection_next_value2_counted`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Intersection2Stats {
    /// Always 1 per call that enters the search (not empty).
    pub root_starts: u32,
    /// Frames with log2_width > 0 that ran a dual expand.
    pub levels_visited: u32,
    /// Number of `hot_level_range_expand` calls (2 per level frame).
    pub expands: u32,
    /// Children pushed onto the stack (both sides nonempty).
    pub children_pushed: u32,
    /// Branches pruned because only one side had mass (braid win).
    pub one_side_empty: u32,
    /// Branches empty on both sides.
    pub both_empty: u32,
    /// Symbol-interval children skipped as entirely < target.
    pub target_skips: u32,
    /// 1 if a common value was returned.
    pub hits: u32,
    pub same_line_hits: u32,
    pub same_sb_hits: u32,
    pub general_hits: u32,
    pub data_line_loads: u64,
    pub superblock_loads: u64,
}

/// Software counters for the D3-A three-range braid probe.
///
/// These counters are deliberately separate from the hot successor API so the
/// measurement path does not add state or branches to the product primitive.
/// `root_restarts` is one for every valid call that enters the descent;
/// `expands` is three per visited level (one per projected range).
#[cfg(any(test, feature = "diagnostics"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct Intersection3Stats {
    pub root_restarts: u32,
    pub levels_visited: u32,
    pub expands: u32,
    pub children_pushed: u32,
    pub hit: u32,
}

/// D4-A software counters for three-range expand locality.
///
/// For each visited level frame the three projected ranges touch some set of
/// DataLines and Superblocks.  Naïve independent expands pay 3× those loads;
/// unique counts measure the theoretical upper bound of a fused expand3.
///
/// Kept off the product path: product D2 still uses three independent B2 expands.
#[cfg(any(test, feature = "diagnostics"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct Expand3OverlapStats {
    /// Level frames visited (one per non-leaf stack frame).
    pub frames: u32,
    /// Independent B2 expands (= 3 × frames).
    pub expands_indep: u32,
    /// DataLine loads if the three ranges expand independently.
    pub line_loads_indep: u64,
    /// Superblock loads if the three ranges expand independently.
    pub sb_loads_indep: u64,
    /// Unique DataLines touched by the three ranges at a frame (union).
    pub unique_lines: u64,
    /// Unique Superblocks touched by the three ranges at a frame (union).
    pub unique_sbs: u64,
    /// Frames where all three ranges are SameLine on the **same** DataLine id.
    pub all_same_line_shared: u32,
    /// Frames where all three ranges are SameLine but on **distinct** line ids.
    pub all_same_line_distinct: u32,
    /// Frames where all three ranges share one Superblock id (any path mix).
    pub all_same_sb: u32,
    /// Frames with pairwise line-id overlap (at least two ranges share a line).
    pub pair_line_overlap: u32,
    /// Frames with no shared line ids among the three ranges.
    pub no_line_overlap: u32,
    /// 1 if a common value was returned.
    pub hit: u32,
}

/// D4-B software counters for the fused mask-first expand3 prototype.
#[cfg(any(test, feature = "diagnostics"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct Expand3FusedStats {
    pub root_restarts: u32,
    pub levels_visited: u32,
    /// Full three-range expands performed (after mask-first pruning).
    pub expands_full: u32,
    /// Frames pruned after a cheap occupancy mask (no full expand3).
    pub mask_pruned: u32,
    pub children_pushed: u32,
    pub unique_lines: u64,
    pub unique_sbs: u64,
    pub hit: u32,
}

/// D4-C software counters for true unique-load expand3.
///
/// Unlike D4-B (mask-first bookkeeping over three independent expands), these
/// counters track **realized** physical DataLine / Superblock loads after
/// endpoint classification and load deduplication.
#[cfg(any(test, feature = "diagnostics"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct Expand3SharedStats {
    pub root_restarts: u32,
    pub frames: u32,
    pub children_pushed: u32,
    /// Logical line loads if three ranges expanded independently.
    pub logical_line_requests: u64,
    /// Actual unique DataLine loads performed by expand3.
    pub unique_line_loads: u64,
    /// Logical superblock loads if three ranges expanded independently.
    pub logical_sb_requests: u64,
    /// Actual unique Superblock loads performed by expand3.
    pub unique_sb_loads: u64,
    pub line_loads_saved: u64,
    pub sb_loads_saved: u64,
    /// Frames where all six endpoints share one DataLine id.
    pub all_same_line_fast_hits: u32,
    /// Frames with exactly two unique DataLine ids.
    pub two_line_fast_hits: u32,
    /// Frames with one unique Superblock (any line mix).
    pub shared_sb_hits: u32,
    /// Frames that took the general fixed cache path.
    pub general_hits: u32,
    pub hit: u32,
}

/// Which specialized path `hot_level_range_expand3` took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expand3Path {
    /// All six endpoints in one DataLine (1 SB + 1 line).
    AllSameLine,
    /// Exactly two unique DataLine ids (typically 1 SB + 2 lines).
    TwoLine,
    /// One unique Superblock, more than two lines.
    SharedSb,
    /// General fixed-cache path.
    General,
}

// ── E5.11 E1: concentric presence summaries (transient oracle) ──────────────

/// Cell size for child-digit presence masks (symbols per summary cell).
///
/// - **256** (E1-A): 4 bits per DataLine (~0.78% vs DataLine payload)
/// - **128** (E1-B): 8 bits per DataLine (~1.56%)
/// - **64**  (E1-C): 16 bits per DataLine (~3.13%) — preferred first candidate
///   (product triangle ranges cluster in 17–64).
#[cfg(any(test, feature = "diagnostics"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum PresenceCellSize {
    /// Whole DataLine (256 symbols).
    Line256 = 256,
    /// Half line (128 symbols).
    Half128 = 128,
    /// Quarter line (64 symbols).
    Quarter64 = 64,
}

#[cfg(any(test, feature = "diagnostics"))]
impl PresenceCellSize {
    pub const ALL: [Self; 3] = [Self::Quarter64, Self::Half128, Self::Line256];

    #[inline]
    pub fn symbols(self) -> usize {
        self as usize
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Line256 => "256",
            Self::Half128 => "128",
            Self::Quarter64 => "64",
        }
    }
}

/// E1 software counters for summary-gated three-range braid.
#[cfg(any(test, feature = "diagnostics"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct PresenceSummaryStats {
    pub root_restarts: u32,
    /// Level frames that reached a summary check.
    pub summary_checks: u32,
    /// Frames rejected because three-way presence AND was zero (no exact expand).
    pub summary_zero_rejects: u32,
    /// Frames where summary AND was nonzero → exact D2 expand ran.
    pub summary_nonzero_fallthroughs: u32,
    /// Fully covered summary cells consulted across all three ranges.
    pub fully_covered_cells: u64,
    /// Partial boundary fragments that required exact presence.
    pub partial_boundary_fragments: u64,
    /// Exact `hot_level_range_expand` triples avoided by zero-reject.
    pub exact_expands_avoided: u32,
    /// Frames that fell through and then exact expand also produced empty mask
    /// (summary was conservative / false-positive for rejection).
    pub false_positive_frames: u32,
    /// Approximate summary table bytes (all levels).
    pub summary_bytes: u64,
    pub frames_exact: u32,
    pub children_pushed: u32,
    pub hit: u32,
}

/// Transient per-level presence masks (heap; E1.0 oracle only — not in image).
///
/// One `u8` mask per cell: bit `b` set iff 2-bit digit `b` appears at least
/// once in that cell. Built by a full scan of the hot column after open.
#[cfg(any(test, feature = "diagnostics"))]
#[derive(Clone, Debug)]
pub struct PresenceSummaryTable {
    pub cell_size: PresenceCellSize,
    /// Per level: `ceil(qv_len / cell_size)` masks.
    levels: Vec<Vec<u8>>,
    /// Approximate heap bytes for the table.
    pub bytes: usize,
}

#[cfg(any(test, feature = "diagnostics"))]
impl PresenceSummaryTable {
    /// Build transient summaries for every level of `hot` (E1.0 oracle).
    ///
    /// Scans each level once via open-time hot pointers. Does **not** change
    /// image bytes. Cost is O(n × levels) and is paid once per harness open.
    pub fn build(hot: &HotQwtColumn, cell_size: PresenceCellSize) -> Self {
        let s = cell_size.symbols();
        debug_assert!(s == 64 || s == 128 || s == 256);
        let mut levels = Vec::with_capacity(hot.n_levels);
        let mut bytes = 0usize;
        for level in 0..hot.n_levels {
            let lv = hot.level(level);
            let n = lv.qv_len;
            let n_cells = if n == 0 { 0 } else { (n + s - 1) / s };
            let mut cells = vec![0u8; n_cells];
            for i in 0..n {
                let dig = hot_level_get(lv, i);
                debug_assert!(dig < 4);
                cells[i / s] |= 1u8 << dig;
            }
            bytes += cells.len(); // one byte per cell (4 bits used)
            levels.push(cells);
        }
        // Struct overhead (rough).
        bytes += levels.len() * std::mem::size_of::<Vec<u8>>() + std::mem::size_of::<Self>();
        Self {
            cell_size,
            levels,
            bytes,
        }
    }

    #[inline]
    fn cell_mask(&self, level: usize, cell: usize) -> u8 {
        self.levels
            .get(level)
            .and_then(|c| c.get(cell).copied())
            .unwrap_or(0)
    }

    /// Conservative presence mask for `[start, end)` at `level`.
    ///
    /// Fully covered cells use the summary; partial boundary fragments use
    /// exact B2 expand counts (no false negatives).
    #[inline]
    pub fn range_presence_mask(
        &self,
        hot: &HotQwtColumn,
        level: usize,
        start: usize,
        end: usize,
        stats: &mut PresenceSummaryStats,
    ) -> u8 {
        if start >= end {
            return 0;
        }
        let s = self.cell_size.symbols();
        let lv = hot.level(level);
        let mut mask = 0u8;
        let mut pos = start;
        while pos < end {
            let cell = pos / s;
            let cell_start = cell * s;
            let cell_end = cell_start + s;
            if pos == cell_start && end >= cell_end {
                // Fully covered summary cell — safe direct use.
                mask |= self.cell_mask(level, cell);
                stats.fully_covered_cells += 1;
                pos = cell_end;
            } else {
                // Partial boundary: exact presence only on the fragment.
                let frag_end = end.min(cell_end);
                let exp = hot_level_range_expand(lv, pos, frag_end);
                for b in 0..4 {
                    if exp.counts[b] != 0 {
                        mask |= 1 << b;
                    }
                }
                stats.partial_boundary_fragments += 1;
                pos = frag_end;
            }
            // Early exit: all four digits already present.
            if mask == 0b1111 {
                // Still must advance? No — conservative OR can stop; remaining
                // cells cannot clear bits. Safe for presence union.
                break;
            }
        }
        mask
    }
}

impl HotQwtColumn {
    /// E1.0: summary-gated three-range successor (diagnostic; not product path).
    ///
    /// Before any full three-way exact expand, compute conservative presence
    /// masks for the three projected ranges (full cells from `table`, partial
    /// boundaries exact). If the three-way AND is zero, prune the frame with
    /// **no** `hot_level_range_expand` triple. Otherwise fall through to the
    /// ordinary D2 expand path.
    ///
    /// Algebraically identical to [`Self::intersection_next_value3`] (no false
    /// negatives). Image bytes unchanged; `table` is transient heap only.
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_summary(
        &self,
        table: &PresenceSummaryTable,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> (Option<u32>, PresenceSummaryStats) {
        let mut stats = PresenceSummaryStats {
            summary_bytes: table.bytes as u64,
            ..Default::default()
        };
        if first.start > first.end
            || second.start > second.end
            || third.start > third.end
            || first.end > self.n
            || second.end > self.n
            || third.end > self.n
            || first.start == first.end
            || second.start == second.end
            || third.start == third.end
            || self.n_levels == 0
        {
            return (None, stats);
        }
        stats.root_restarts = 1;

        #[derive(Clone, Copy)]
        struct Frame {
            a_start: usize,
            a_end: usize,
            b_start: usize,
            b_end: usize,
            c_start: usize,
            c_end: usize,
            level: usize,
            sym_lo: usize,
            log2_width: u32,
        }

        let full_log2 = 2 * self.n_levels as u32;
        let target = target as usize;
        let mut stack = [Frame {
            a_start: 0,
            a_end: 0,
            b_start: 0,
            b_end: 0,
            c_start: 0,
            c_end: 0,
            level: 0,
            sym_lo: 0,
            log2_width: 0,
        }; 128];
        let mut sp = 1usize;
        stack[0] = Frame {
            a_start: first.start,
            a_end: first.end,
            b_start: second.start,
            b_end: second.end,
            c_start: third.start,
            c_end: third.end,
            level: 0,
            sym_lo: 0,
            log2_width: full_log2,
        };

        while sp > 0 {
            sp -= 1;
            let cur = stack[sp];
            if cur.a_start >= cur.a_end || cur.b_start >= cur.b_end || cur.c_start >= cur.c_end {
                continue;
            }
            if cur.log2_width == 0 {
                if cur.sym_lo >= target {
                    stats.hit = 1;
                    return (Some(cur.sym_lo as u32), stats);
                }
                continue;
            }
            if cur.level >= self.n_levels {
                continue;
            }

            // ── E1 summary check BEFORE exact expand ─────────────────────
            stats.summary_checks += 1;
            let ma = table.range_presence_mask(self, cur.level, cur.a_start, cur.a_end, &mut stats);
            let mb = table.range_presence_mask(self, cur.level, cur.b_start, cur.b_end, &mut stats);
            let mc = table.range_presence_mask(self, cur.level, cur.c_start, cur.c_end, &mut stats);
            let common_pres = ma & mb & mc;
            if common_pres == 0 {
                stats.summary_zero_rejects += 1;
                stats.exact_expands_avoided += 1;
                continue;
            }
            stats.summary_nonzero_fallthroughs += 1;

            // Exact D2 expand (only after summary admits possible children).
            let lv = self.level(cur.level);
            stats.frames_exact += 1;
            let exp_a = hot_level_range_expand(lv, cur.a_start, cur.a_end);
            let exp_b = hot_level_range_expand(lv, cur.b_start, cur.b_end);
            let exp_c = hot_level_range_expand(lv, cur.c_start, cur.c_end);

            let mut exact_mask = 0u8;
            for b in 0..4usize {
                if exp_a.counts[b] != 0 && exp_b.counts[b] != 0 && exp_c.counts[b] != 0 {
                    exact_mask |= 1 << b;
                }
            }
            // Summary said nonzero but exact is empty → conservative false positive.
            if exact_mask == 0 {
                stats.false_positive_frames += 1;
                continue;
            }

            let child_log = cur.log2_width - 2;
            let child_width = 1usize << child_log;
            let mut children = [Frame {
                a_start: 0,
                a_end: 0,
                b_start: 0,
                b_end: 0,
                c_start: 0,
                c_end: 0,
                level: 0,
                sym_lo: 0,
                log2_width: 0,
            }; 4];
            let mut nc = 0usize;
            let mut live = exact_mask;
            while live != 0 {
                let b = live.trailing_zeros() as usize;
                live &= !(1 << b);
                let child_lo = cur.sym_lo + b * child_width;
                if child_lo + child_width <= target {
                    continue;
                }
                let ca = exp_a.counts[b];
                let cb = exp_b.counts[b];
                let cc = exp_c.counts[b];
                if ca == 0 || cb == 0 || cc == 0 {
                    continue;
                }
                let offset = lv.occs[b];
                children[nc] = Frame {
                    a_start: offset + exp_a.ranks_s[b],
                    a_end: offset + exp_a.ranks_s[b] + ca,
                    b_start: offset + exp_b.ranks_s[b],
                    b_end: offset + exp_b.ranks_s[b] + cb,
                    c_start: offset + exp_c.ranks_s[b],
                    c_end: offset + exp_c.ranks_s[b] + cc,
                    level: cur.level + 1,
                    sym_lo: child_lo,
                    log2_width: child_log,
                };
                nc += 1;
            }
            stats.children_pushed += nc as u32;
            for i in (0..nc).rev() {
                debug_assert!(sp < 128);
                stack[sp] = children[i];
                sp += 1;
            }
        }
        (None, stats)
    }
}

/// Result of a true unique-load three-range expand (D4-C).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Expand3Result {
    pub first: RangeExpand,
    pub second: RangeExpand,
    pub third: RangeExpand,
    /// Bits set where all three ranges have nonzero count for symbol 0..3.
    pub common_mask: u8,
    pub path: Expand3Path,
    pub logical_line_requests: u8,
    pub unique_line_loads: u8,
    pub logical_sb_requests: u8,
    pub unique_sb_loads: u8,
}

#[cfg(any(test, feature = "diagnostics"))]
#[derive(Clone, Copy, Default)]
struct IntersectionChild3 {
    a_start: usize,
    a_end: usize,
    b_start: usize,
    b_end: usize,
    c_start: usize,
    c_end: usize,
}

/// Persistent D3-B traversal frame.  The child projections are cached after
/// the first expansion so successive `next()` calls do not repeat the same
/// three range expansions while walking sibling prefixes.
#[cfg(any(test, feature = "diagnostics"))]
#[derive(Clone, Copy, Default)]
pub struct IntersectionFrame3 {
    level: usize,
    prefix: usize,
    log2_width: u32,
    remaining_child_mask: u8,
    expanded: bool,
    children: [IntersectionChild3; 4],
}

/// Allocation-free, sorted iterator over values common to three row ranges.
///
/// The iterator owns only a bounded 128-frame stack.  It borrows the hot
/// column, which in turn aliases the immutable mapped/owned image.
#[cfg(any(test, feature = "diagnostics"))]
pub struct IntersectionIter3<'a> {
    hot: &'a HotQwtColumn,
    stack: [IntersectionFrame3; 128],
    sp: usize,
    target: u32,
}

#[cfg(any(test, feature = "diagnostics"))]
impl<'a> IntersectionIter3<'a> {
    fn new(
        hot: &'a HotQwtColumn,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> Self {
        let mut it = Self {
            hot,
            stack: [IntersectionFrame3::default(); 128],
            sp: 0,
            target,
        };
        if first.start <= first.end
            && second.start <= second.end
            && third.start <= third.end
            && first.end <= hot.n
            && second.end <= hot.n
            && third.end <= hot.n
            && first.start < first.end
            && second.start < second.end
            && third.start < third.end
            && hot.n_levels > 0
        {
            it.stack[0] = IntersectionFrame3 {
                level: 0,
                prefix: 0,
                log2_width: 2 * hot.n_levels as u32,
                remaining_child_mask: 0,
                expanded: false,
                children: [IntersectionChild3 {
                    a_start: first.start,
                    a_end: first.end,
                    b_start: second.start,
                    b_end: second.end,
                    c_start: third.start,
                    c_end: third.end,
                }; 4],
            };
            it.sp = 1;
        }
        it
    }
}

#[cfg(any(test, feature = "diagnostics"))]
impl Iterator for IntersectionIter3<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        // Labeled outer loop: after pushing a child frame we must re-enter the
        // outer iteration so the new top is processed, not immediately popped.
        'outer: while self.sp > 0 {
            let top = self.sp - 1;
            let log2_width = self.stack[top].log2_width;

            if log2_width == 0 {
                let value = self.stack[top].prefix as u32;
                self.sp -= 1;
                if value < self.target {
                    continue;
                }
                if value == u32::MAX {
                    self.target = u32::MAX;
                } else {
                    self.target = value + 1;
                }
                return Some(value);
            }

            if !self.stack[top].expanded {
                let level = self.stack[top].level;
                let parent = self.stack[top].children[0];
                let lv = self.hot.level(level);
                let exp_a = hot_level_range_expand(lv, parent.a_start, parent.a_end);
                let exp_b = hot_level_range_expand(lv, parent.b_start, parent.b_end);
                let exp_c = hot_level_range_expand(lv, parent.c_start, parent.c_end);
                let mut mask = 0u8;
                for b in 0..4usize {
                    let ca = exp_a.counts[b];
                    let cb = exp_b.counts[b];
                    let cc = exp_c.counts[b];
                    if ca == 0 || cb == 0 || cc == 0 {
                        continue;
                    }
                    let offset = lv.occs[b];
                    self.stack[top].children[b] = IntersectionChild3 {
                        a_start: offset + exp_a.ranks_s[b],
                        a_end: offset + exp_a.ranks_s[b] + ca,
                        b_start: offset + exp_b.ranks_s[b],
                        b_end: offset + exp_b.ranks_s[b] + cb,
                        c_start: offset + exp_c.ranks_s[b],
                        c_end: offset + exp_c.ranks_s[b] + cc,
                    };
                    mask |= 1 << b;
                }
                self.stack[top].remaining_child_mask = mask;
                self.stack[top].expanded = true;
            }

            let mut mask = self.stack[top].remaining_child_mask;
            while mask != 0 {
                let b = mask.trailing_zeros() as usize;
                mask &= !(1 << b);
                let child_width = 1usize << (log2_width - 2);
                let child_prefix = self.stack[top].prefix + b * child_width;
                if child_prefix + child_width <= self.target as usize {
                    continue;
                }
                self.stack[top].remaining_child_mask = mask;
                let child = self.stack[top].children[b];
                if self.sp >= self.stack.len() {
                    self.sp = 0;
                    return None;
                }
                self.stack[self.sp] = IntersectionFrame3 {
                    level: self.stack[top].level + 1,
                    prefix: child_prefix,
                    log2_width: log2_width - 2,
                    remaining_child_mask: 0,
                    expanded: false,
                    children: [child; 4],
                };
                self.sp += 1;
                // Restart outer loop to process the newly pushed child.
                continue 'outer;
            }
            // No remaining viable children: pop this frame.
            self.sp -= 1;
        }
        None
    }
}

// ── E5.11 C1: level-batched short-range decode ──────────────────────────────

/// Max fixed-stack batch size (K ≤ this). Compile-time bound; no heap.
pub const C1_MAX_BATCH: usize = 64;

/// Default C1 threshold after E5.11 measure: **0 (disabled)**.
///
/// K=32 regressed star ~65% on N=20k realistic (all ranges hit batch; ≈1
/// distinct/row so sort/RLE buys nothing vs RDI). K=16 only +4.5% and mostly
/// falls back. Keep code for experiments; product path uses generic RDI.
pub const C1_K_DEFAULT: usize = 0;

/// Process-wide C1 batch threshold K (harness sweeps 0/16/32/64).
static C1_BATCH_K: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(C1_K_DEFAULT);

/// Set C1 short-range batch threshold (`0` disables batch path).
#[cfg(any(test, feature = "diagnostics"))]
pub fn set_c1_batch_k(k: usize) {
    C1_BATCH_K.store(k.min(C1_MAX_BATCH), std::sync::atomic::Ordering::Relaxed);
}

/// Current C1 batch threshold K.
pub fn c1_batch_k() -> usize {
    C1_BATCH_K.load(std::sync::atomic::Ordering::Relaxed)
}

/// Result of a successful C1 short-range batch decode (fixed stack).
#[derive(Clone, Copy, Debug)]
pub struct ShortRangeBatchResult {
    /// Distinct symbols in numeric order (prefix of length `n_out`).
    pub symbols: [u32; C1_MAX_BATCH],
    /// Occurrence counts aligned with `symbols`.
    pub counts: [u32; C1_MAX_BATCH],
    pub n_out: usize,
    pub rows_decoded: u32,
    pub levels_processed: u32,
    pub data_line_loads: u32,
    pub sort_comparisons: u32,
    pub dedup_merges: u32,
}

/// Fixed-scratch level-batched short-range distinct decoder (E5.11 C1).
///
/// For `len = end - start` with `0 < len ≤ k ≤ C1_MAX_BATCH`:
/// 1. Seed positions `[start .. end)`.
/// 2. Walk all rows together one QWT level at a time (shared hot metadata).
/// 3. Reconstruct full symbols into fixed local storage.
/// 4. Insertion-sort + RLE in place.
/// 5. Emit numeric-order `(symbol, count)`.
///
/// Rejects (returns `None`) empty ranges, OOB, or `len > k` → caller falls
/// back to generic RDI.
fn short_range_batch_decode(
    hot: &HotQwtColumn,
    range: Range<usize>,
    k: usize,
) -> Option<ShortRangeBatchResult> {
    if k == 0 || hot.n_levels == 0 {
        return None;
    }
    let k = k.min(C1_MAX_BATCH);
    if range.start >= range.end || range.end > hot.n {
        return None;
    }
    let n = range.end - range.start;
    if n == 0 || n > k {
        return None;
    }

    // Fixed scratch: positions + accumulating symbols.
    let mut pos = [0u32; C1_MAX_BATCH];
    let mut sym = [0u32; C1_MAX_BATCH];
    for i in 0..n {
        pos[i] = (range.start + i) as u32;
        sym[i] = 0;
    }

    let mut data_line_loads = 0u32;
    let n_levels = hot.n_levels;

    // Level-batched access: same HotMappedLevel metadata for all rows.
    for level in 0..n_levels {
        let lv = hot.level(level);
        let last = level + 1 == n_levels;

        // Same-line fast path: all positions in one DataLine → one load.
        let mut same_line = true;
        let line0 = (pos[0] as usize) >> 8;
        for i in 1..n {
            if (pos[i] as usize) >> 8 != line0 {
                same_line = false;
                break;
            }
        }

        if same_line && line0 < lv.data_len {
            data_line_loads += 1;
            // SAFETY: line0 < data_len (open-validated).
            let d = unsafe { lv.data_at(line0) };
            for i in 0..n {
                let p = pos[i] as usize;
                let dig = unsafe { d.get_unchecked(p & 255) };
                sym[i] = (sym[i] << 2) | dig as u32;
                if !last {
                    let offset = lv.occs[dig as usize];
                    // Rank still needed for next-level position remap.
                    pos[i] = (hot_level_rank(lv, dig, p) + offset) as u32;
                }
            }
        } else {
            // General: per-row get (may share lines; count unique lines lightly).
            let mut last_line = usize::MAX;
            for i in 0..n {
                let p = pos[i] as usize;
                let line = p >> 8;
                if line != last_line {
                    if line < lv.data_len {
                        data_line_loads += 1;
                    }
                    last_line = line;
                }
                let dig = hot_level_get(lv, p);
                sym[i] = (sym[i] << 2) | dig as u32;
                if !last {
                    let offset = lv.occs[dig as usize];
                    pos[i] = (hot_level_rank(lv, dig, p) + offset) as u32;
                }
            }
        }
    }

    // Insertion sort (n ≤ 64; stable, few compares on nearly-sorted runs).
    let mut sort_comparisons = 0u32;
    for i in 1..n {
        let key = sym[i];
        let mut j = i;
        while j > 0 {
            sort_comparisons += 1;
            if sym[j - 1] <= key {
                break;
            }
            sym[j] = sym[j - 1];
            j -= 1;
        }
        sym[j] = key;
    }

    // RLE → distinct (symbol, count).
    let mut out_sym = [0u32; C1_MAX_BATCH];
    let mut out_cnt = [0u32; C1_MAX_BATCH];
    let mut n_out = 0usize;
    let mut dedup_merges = 0u32;
    if n > 0 {
        out_sym[0] = sym[0];
        out_cnt[0] = 1;
        n_out = 1;
        for i in 1..n {
            if sym[i] == out_sym[n_out - 1] {
                out_cnt[n_out - 1] += 1;
                dedup_merges += 1;
            } else {
                out_sym[n_out] = sym[i];
                out_cnt[n_out] = 1;
                n_out += 1;
            }
        }
    }

    Some(ShortRangeBatchResult {
        symbols: out_sym,
        counts: out_cnt,
        n_out,
        rows_decoded: n as u32,
        levels_processed: n_levels as u32,
        data_line_loads,
        sort_comparisons,
        dedup_merges,
    })
}

// ── Borrowed QWTA section (shared by W0 owned open + W1 mmap Ring) ──────────

/// Zero-copy view of one QWTA section (no ownership of bytes).
///
/// Lifetime `'m` is the image / mmap lifetime. Level views re-slice on demand
/// so this type is not self-referential. Prefer [`Self::build_hot`] once at open
/// and route hot primitives through [`HotQwtColumn`] (E5.11 B1).
#[derive(Clone, Copy)]
pub struct MappedQwtSection<'m> {
    sec: &'m [u8],
    pub n: usize,
    pub n_levels: usize,
    pub sigma: u32,
}

impl<'m> MappedQwtSection<'m> {
    /// Validate and open a QWTA section slice (dir bounds only; no payload walk).
    pub fn open(sec: &'m [u8]) -> Result<Self, MappedQwtError> {
        if sec.len() < 64 || &sec[0..4] != QWTA_MAGIC {
            return Err(MappedQwtError::Layout("bad section"));
        }
        let n = u64::from_le_bytes(sec[4..12].try_into().unwrap()) as usize;
        let n_levels = u16::from_le_bytes(sec[12..14].try_into().unwrap()) as usize;
        let sigma = u32::from_le_bytes(sec[14..18].try_into().unwrap());
        if n_levels > MAX_LEVELS {
            return Err(MappedQwtError::TooManyLevels(n_levels));
        }
        if n_levels > 0 {
            let dir_end = 64 + n_levels * LEVEL_DIR_V1;
            if dir_end > sec.len() {
                return Err(MappedQwtError::Layout("dir OOB"));
            }
        }
        Ok(Self {
            sec,
            n,
            n_levels,
            sigma,
        })
    }

    pub fn section_bytes(&self) -> &'m [u8] {
        self.sec
    }

    pub fn level_view(&self, level: usize) -> Result<MappedLevel<'m>, MappedQwtError> {
        if level >= self.n_levels {
            return Err(MappedQwtError::Layout("level OOB"));
        }
        let base = 64 + level * LEVEL_DIR_V1;
        if base + LEVEL_DIR_V1 > self.sec.len() {
            return Err(MappedQwtError::Layout("dir OOB"));
        }
        let e = &self.sec[base..base + LEVEL_DIR_V1];
        let off_data = u64::from_le_bytes(e[0..8].try_into().unwrap()) as usize;
        let n_data = u64::from_le_bytes(e[8..16].try_into().unwrap()) as usize;
        let off_super = u64::from_le_bytes(e[16..24].try_into().unwrap()) as usize;
        let n_super = u64::from_le_bytes(e[24..32].try_into().unwrap()) as usize;
        let mut off_sel = [0usize; 4];
        let mut n_sel = [0usize; 4];
        for s in 0..4 {
            off_sel[s] = u64::from_le_bytes(e[32 + s * 8..40 + s * 8].try_into().unwrap()) as usize;
            n_sel[s] = u32::from_le_bytes(e[64 + s * 4..68 + s * 4].try_into().unwrap()) as usize;
        }
        let qv_pos = u64::from_le_bytes(e[80..88].try_into().unwrap()) as usize;
        let off_occs = u64::from_le_bytes(e[88..96].try_into().unwrap()) as usize;

        let data = cast_slice::<DataLine>(self.sec, off_data, n_data)?;
        let superblocks = cast_slice::<SuperblockPlain>(self.sec, off_super, n_super)?;
        let mut select_samples: [&[u32]; 4] = [&[]; 4];
        for s in 0..4 {
            select_samples[s] = cast_u32_slice(self.sec, off_sel[s], n_sel[s])?;
        }
        if off_occs
            .checked_add(40)
            .map(|e| e > self.sec.len())
            .unwrap_or(true)
        {
            return Err(MappedQwtError::Layout("occs OOB"));
        }

        Ok(MappedLevel {
            data,
            superblocks,
            select_samples,
            qv_position_bits: qv_pos,
            n_symbols: qv_pos / 2,
        })
    }

    pub fn n_occs_smaller_level(&self, level: usize) -> Result<[u64; 5], MappedQwtError> {
        if level >= self.n_levels {
            return Err(MappedQwtError::Layout("level OOB"));
        }
        let base = 64 + level * LEVEL_DIR_V1;
        let e = &self.sec[base..base + LEVEL_DIR_V1];
        let off_occs = u64::from_le_bytes(e[88..96].try_into().unwrap()) as usize;
        if off_occs
            .checked_add(40)
            .map(|e| e > self.sec.len())
            .unwrap_or(true)
        {
            return Err(MappedQwtError::Layout("occs OOB"));
        }
        let mut o = [0u64; 5];
        for i in 0..5 {
            o[i] = u64::from_le_bytes(
                self.sec[off_occs + i * 8..off_occs + i * 8 + 8]
                    .try_into()
                    .unwrap(),
            );
        }
        Ok(o)
    }

    /// E5.11 B1: build open-time hot column (validated pointers + native occs).
    ///
    /// Walks every level once: dir parse, bounds/align cast, occs as `usize`.
    /// Image bytes unchanged. Callers must keep the owning image alive for the
    /// lifetime of the returned pointers.
    pub fn build_hot(&self) -> Result<HotQwtColumn, MappedQwtError> {
        let mut levels = [HotMappedLevel::EMPTY; MAX_LEVELS];
        for level in 0..self.n_levels {
            let lv = self.level_view(level)?;
            let occs_u64 = self.n_occs_smaller_level(level)?;
            let mut occs = [0usize; 5];
            for i in 0..5 {
                occs[i] = occs_u64[i] as usize;
            }
            levels[level] = HotMappedLevel {
                data: lv.data.as_ptr(),
                superblocks: lv.superblocks.as_ptr(),
                data_len: lv.data.len(),
                super_len: lv.superblocks.len(),
                qv_len: lv.n_symbols,
                occs,
            };
        }
        Ok(HotQwtColumn {
            n: self.n,
            n_levels: self.n_levels,
            sigma: self.sigma,
            levels,
        })
    }

    /// Symbol at position `i` (wavelet-matrix access).
    pub fn get(&self, i: usize) -> Option<u32> {
        if i >= self.n || self.n_levels == 0 {
            return None;
        }
        let mut result: u32 = 0;
        let mut cur_i = i;
        for level in 0..self.n_levels - 1 {
            let lv = self.level_view(level).ok()?;
            let occs = self.n_occs_smaller_level(level).ok()?;
            let symbol = level_get(&lv, cur_i);
            result = (result << 2) | symbol as u32;
            let offset = occs[symbol as usize] as usize;
            cur_i = level_rank(&lv, symbol, cur_i) + offset;
        }
        let lv = self.level_view(self.n_levels - 1).ok()?;
        let symbol = level_get(&lv, cur_i);
        Some((result << 2) | symbol as u32)
    }

    /// Level-local rank of 2-bit symbol `b ∈ 0..3` up to `i` (excluded).
    pub fn rank_level(&self, level: usize, b: u8, i: usize) -> Option<usize> {
        if b > 3 {
            return None;
        }
        let lv = self.level_view(level).ok()?;
        if i > lv.n_symbols {
            return None;
        }
        Some(level_rank(&lv, b, i))
    }

    /// Level-local rank-all-4 up to `i` (excluded).
    pub fn rank_all_level(&self, level: usize, i: usize) -> Option<[usize; 4]> {
        let lv = self.level_view(level).ok()?;
        if i > lv.n_symbols {
            return None;
        }
        Some(level_rank_all(&lv, i))
    }

    /// Full-symbol rank of `symbol` in `[0, i)` (wavelet-matrix walk).
    ///
    /// Matches vendored qwt for in-range symbols. For `symbol > sigma` with
    /// valid `i`, returns `Some(0)` so Ring callers match pilot
    /// `unwrap_or(0)` semantics (absent alphabet → zero count).
    pub fn rank(&self, symbol: u32, i: usize) -> Option<usize> {
        if self.n_levels == 0 {
            return if i <= self.n { Some(0) } else { None };
        }
        if i > self.n {
            return None;
        }
        if symbol > self.sigma {
            return Some(0);
        }
        Some(self.rank_unchecked(symbol, i))
    }

    /// Full-symbol rank without bounds checks (caller guarantees validity).
    #[inline]
    pub fn rank_unchecked(&self, symbol: u32, i: usize) -> usize {
        debug_assert!(self.n_levels > 0);
        debug_assert!(i <= self.n);
        debug_assert!(symbol <= self.sigma);

        let mut shift: i64 = (2 * (self.n_levels - 1)) as i64;
        let mut cur_i = i;
        let mut cur_p = 0usize;

        for level in 0..self.n_levels - 1 {
            let two_bits = ((symbol >> shift as usize) & 3) as u8;
            let occs = self.n_occs_smaller_level(level).expect("level");
            let offset = occs[two_bits as usize] as usize;
            let lv = self.level_view(level).expect("level");
            cur_p = level_rank(&lv, two_bits, cur_p) + offset;
            cur_i = level_rank(&lv, two_bits, cur_i) + offset;
            shift -= 2;
        }

        let two_bits = ((symbol >> shift as usize) & 3) as u8;
        let lv = self.level_view(self.n_levels - 1).expect("level");
        cur_i = level_rank(&lv, two_bits, cur_i);
        cur_p = level_rank(&lv, two_bits, cur_p);
        cur_i - cur_p
    }

    /// Position of the 0-based `occurrence`-th of `symbol` (wavelet select).
    ///
    /// Matches vendored qwt `SelectUnsigned::select` for balanced QWT256.
    /// Uses a fixed stack (no heap allocation on the hot path).
    pub fn select(&self, symbol: u32, occurrence: usize) -> Option<usize> {
        if self.n_levels == 0 || symbol > self.sigma {
            return None;
        }

        // Fixed-size path buffers (MAX_LEVELS); same arithmetic as qwt select
        // which uses Vec only for path_off / rank_path_off.
        let mut path_off = [0usize; MAX_LEVELS];
        let mut rank_path_off = [0usize; MAX_LEVELS];

        let mut b = 0usize;
        let mut shift: i64 = 2 * (self.n_levels - 1) as i64;

        for level in 0..self.n_levels {
            path_off[level] = b;
            let two_bits = ((symbol >> shift as usize) & 3) as u8;
            let lv = self.level_view(level).ok()?;
            if b > lv.n_symbols {
                return None;
            }
            let rank_b = level_rank(&lv, two_bits, b);
            let occs = self.n_occs_smaller_level(level).ok()?;
            b = rank_b + occs[two_bits as usize] as usize;
            shift -= 2;
            rank_path_off[level] = rank_b;
        }

        // After the last-level rank, `occurrence` must be in-range for the
        // leaf 2-bit symbol. Check via level select Option.
        shift = 0;
        let mut result = occurrence;
        for level in (0..self.n_levels).rev() {
            b = path_off[level];
            let rank_b = rank_path_off[level];
            let two_bits = ((symbol >> shift as usize) & 3) as u8;
            let lv = self.level_view(level).ok()?;
            let pos = level_select(&lv, two_bits, rank_b + result)?;
            result = pos - b;
            shift += 2;
        }
        Some(result)
    }

    // ── W3: native RNV + fixed-stack RDI (E5.9A algorithms, mapped) ───────

    /// Native guided `range_next_value` — O(log σ) rank work, independent of
    /// range length. Faithful fixed-stack port of vendored
    /// `QWaveletTree::range_next_value` (equal-width 4-ary symbol intervals).
    pub fn range_next_value(&self, range: Range<usize>, target: u32) -> Option<u32> {
        if range.start > range.end || range.end > self.n || range.start == range.end {
            return None;
        }
        if self.n_levels == 0 {
            return None;
        }

        #[derive(Clone, Copy)]
        struct Frame {
            start: usize,
            end: usize,
            level: usize,
            sym_lo: usize,
            /// log2 of alphabet interval width = 2 * (n_levels - level).
            log2_width: u32,
        }

        let n_levels = self.n_levels;
        let target_us = target as usize;
        let full_log2: u32 = 2 * n_levels as u32;

        let mut stack = [Frame {
            start: 0,
            end: 0,
            level: 0,
            sym_lo: 0,
            log2_width: 0,
        }; 128];
        let mut sp: usize = 1;
        stack[0] = Frame {
            start: range.start,
            end: range.end,
            level: 0,
            sym_lo: 0,
            log2_width: full_log2,
        };

        while sp > 0 {
            sp -= 1;
            let cur = stack[sp];

            if cur.start >= cur.end {
                continue;
            }

            // Leaf: single symbol interval of width 1.
            if cur.log2_width == 0 {
                if cur.sym_lo >= target_us {
                    return Some(cur.sym_lo as u32);
                }
                continue;
            }

            let lv = match self.level_view(cur.level) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let occs = match self.n_occs_smaller_level(cur.level) {
                Ok(o) => o,
                Err(_) => continue,
            };
            let child_log = cur.log2_width - 2;
            let child_width = 1usize << child_log;

            // Collect viable children b=0..3 left-to-right, then push reverse.
            let mut cand_s = [0usize; 4];
            let mut cand_e = [0usize; 4];
            let mut cand_lo = [0usize; 4];
            let mut nc = 0usize;

            for b in 0..4u8 {
                let child_sym_lo = cur.sym_lo + (b as usize) * child_width;
                let child_sym_hi = child_sym_lo + child_width;
                // Child entirely < target → skip.
                if child_sym_hi <= target_us {
                    continue;
                }
                let lo = level_rank(&lv, b, cur.start);
                let hi = level_rank(&lv, b, cur.end);
                if hi > lo {
                    let offset = occs[b as usize] as usize;
                    cand_s[nc] = offset + lo;
                    cand_e[nc] = offset + hi;
                    cand_lo[nc] = child_sym_lo;
                    nc += 1;
                }
            }

            for i in (0..nc).rev() {
                debug_assert!(sp < 128);
                stack[sp] = Frame {
                    start: cand_s[i],
                    end: cand_e[i],
                    level: cur.level + 1,
                    sym_lo: cand_lo[i],
                    log2_width: child_log,
                };
                sp += 1;
            }
        }

        None
    }

    /// Fixed-stack ordered distinct-symbol iterator (E5.9A RDI).
    ///
    /// Builds a hot column once, then runs RDI through open-time pointers (B1).
    pub fn range_distinct_iter(&self, range: Range<usize>) -> MappedRangeDistinctIter<'m> {
        let hot = self.build_hot().unwrap_or(HotQwtColumn::EMPTY);
        MappedRangeDistinctIter::new_owned(hot, range)
    }
}

// ── W3 RDI (E5.11 B1: hot column + B2 fused expand) ─────────────────────────

/// Fixed-stack distinct-symbol iterator over a mapped QWT row range.
///
/// Port of vendored `RangeDistinctIter` (E5.7C / E5.9A): rank-all-4, unary
/// path collapse, endpoint prefetch. No heap allocation on the hot path.
///
/// E5.11 C1: if the initial range length ≤ [`c1_batch_k`], symbols are produced
/// by level-batched short-range decode (fixed stack); otherwise generic RDI.
///
/// Holds a [`HotQwtColumn`] (open-time validated pointers). Lifetime `'a` is
/// only for API compatibility with callers that borrow an owner; the hot
/// column itself is `Copy` and aliases the image.
pub struct MappedRangeDistinctIter<'a> {
    hot: HotQwtColumn,
    stack: [MappedRdiFrame; 128],
    sp: usize,
    /// C1 batch emit buffer (used when `batch_n_out > 0`).
    batch_sym: [u32; C1_MAX_BATCH],
    batch_cnt: [u32; C1_MAX_BATCH],
    batch_n_out: usize,
    batch_emit_i: usize,
    _phantom: std::marker::PhantomData<&'a ()>,
    /// Diagnostic counters (match heap RDI on generic path only).
    pub rank_probes: u64,
    pub frames_popped: u64,
    pub children_pushed: u64,
    pub empty_branches: u64,
    pub branch_transitions: u64,
    pub symbols_yielded: u64,
    pub unary_collapses: u64,
    pub prefetch_attempts: u64,
    // E5.11 B2 path counters (mapped-only; not compared to heap)
    pub range_counts_calls: u64,
    pub same_line_hits: u64,
    pub same_superblock_hits: u64,
    pub general_hits: u64,
    pub same_line_symbols: u64,
    pub data_line_loads: u64,
    pub superblock_loads: u64,
    // E5.11 C1 batch counters
    pub batch_attempts: u64,
    pub batch_hits: u64,
    pub batch_fallbacks: u64,
    pub batch_rows_decoded: u64,
    pub batch_levels_processed: u64,
    pub batch_data_line_loads: u64,
    pub batch_sort_comparisons: u64,
    pub batch_dedup_merges: u64,
}

#[derive(Clone, Copy, Default)]
struct MappedRdiFrame {
    start: usize,
    end: usize,
    level: usize,
    bit_path: usize,
}

impl MappedRangeDistinctIter<'_> {
    fn new(hot: &HotQwtColumn, range: Range<usize>) -> Self {
        Self::new_owned(*hot, range)
    }

    fn new_owned(hot: HotQwtColumn, range: Range<usize>) -> Self {
        let mut stack = [MappedRdiFrame::default(); 128];
        let mut sp = 0usize;
        let mut batch_sym = [0u32; C1_MAX_BATCH];
        let mut batch_cnt = [0u32; C1_MAX_BATCH];
        let mut batch_n_out = 0usize;
        let mut batch_attempts = 0u64;
        let mut batch_hits = 0u64;
        let mut batch_fallbacks = 0u64;
        let mut batch_rows_decoded = 0u64;
        let mut batch_levels_processed = 0u64;
        let mut batch_data_line_loads = 0u64;
        let mut batch_sort_comparisons = 0u64;
        let mut batch_dedup_merges = 0u64;

        let k = c1_batch_k();
        let len = range.end.saturating_sub(range.start);
        let try_batch =
            range.start < range.end && range.end <= hot.n && hot.n_levels > 0 && k > 0 && len <= k;

        if try_batch {
            batch_attempts = 1;
            if let Some(res) = short_range_batch_decode(&hot, range.clone(), k) {
                batch_hits = 1;
                batch_n_out = res.n_out;
                batch_sym = res.symbols;
                batch_cnt = res.counts;
                batch_rows_decoded = res.rows_decoded as u64;
                batch_levels_processed = res.levels_processed as u64;
                batch_data_line_loads = res.data_line_loads as u64;
                batch_sort_comparisons = res.sort_comparisons as u64;
                batch_dedup_merges = res.dedup_merges as u64;
                // sp stays 0 — emit from batch buffer only.
            } else {
                batch_fallbacks = 1;
                stack[0] = MappedRdiFrame {
                    start: range.start,
                    end: range.end,
                    level: 0,
                    bit_path: 0,
                };
                sp = 1;
            }
        } else if range.start < range.end && range.end <= hot.n && hot.n_levels > 0 {
            if k > 0 && len > k {
                batch_attempts = 1;
                batch_fallbacks = 1;
            }
            stack[0] = MappedRdiFrame {
                start: range.start,
                end: range.end,
                level: 0,
                bit_path: 0,
            };
            sp = 1;
        }

        Self {
            hot,
            stack,
            sp,
            batch_sym,
            batch_cnt,
            batch_n_out,
            batch_emit_i: 0,
            _phantom: std::marker::PhantomData,
            rank_probes: 0,
            frames_popped: 0,
            children_pushed: 0,
            empty_branches: 0,
            branch_transitions: 0,
            symbols_yielded: 0,
            unary_collapses: 0,
            prefetch_attempts: 0,
            range_counts_calls: 0,
            same_line_hits: 0,
            same_superblock_hits: 0,
            general_hits: 0,
            same_line_symbols: 0,
            data_line_loads: 0,
            superblock_loads: 0,
            batch_attempts,
            batch_hits,
            batch_fallbacks,
            batch_rows_decoded,
            batch_levels_processed,
            batch_data_line_loads,
            batch_sort_comparisons,
            batch_dedup_merges,
        }
    }

    /// K9.3 R2: cheap in-place bounds reset for a live RDI cursor.
    ///
    /// Reuses existing iterator storage (stack / batch buffers / hot alias)
    /// instead of constructing a fresh `MappedRangeDistinctIter`. Diagnostic
    /// counters accumulate across resets (product path does not read them).
    #[inline]
    pub fn reset_bounds(&mut self, range: Range<usize>) {
        self.sp = 0;
        self.batch_n_out = 0;
        self.batch_emit_i = 0;

        let k = c1_batch_k();
        let len = range.end.saturating_sub(range.start);
        let try_batch = range.start < range.end
            && range.end <= self.hot.n
            && self.hot.n_levels > 0
            && k > 0
            && len <= k;

        if try_batch {
            self.batch_attempts += 1;
            if let Some(res) = short_range_batch_decode(&self.hot, range.clone(), k) {
                self.batch_hits += 1;
                self.batch_n_out = res.n_out;
                self.batch_sym = res.symbols;
                self.batch_cnt = res.counts;
                self.batch_rows_decoded += res.rows_decoded as u64;
                self.batch_levels_processed += res.levels_processed as u64;
                self.batch_data_line_loads += res.data_line_loads as u64;
                self.batch_sort_comparisons += res.sort_comparisons as u64;
                self.batch_dedup_merges += res.dedup_merges as u64;
            } else {
                self.batch_fallbacks += 1;
                self.stack[0] = MappedRdiFrame {
                    start: range.start,
                    end: range.end,
                    level: 0,
                    bit_path: 0,
                };
                self.sp = 1;
            }
        } else if range.start < range.end && range.end <= self.hot.n && self.hot.n_levels > 0 {
            if k > 0 && len > k {
                self.batch_attempts += 1;
                self.batch_fallbacks += 1;
            }
            self.stack[0] = MappedRdiFrame {
                start: range.start,
                end: range.end,
                level: 0,
                bit_path: 0,
            };
            self.sp = 1;
        }
    }

    /// Next distinct symbol and its occurrence count in the range (lex order).
    #[inline]
    pub fn next_symbol(&mut self) -> Option<(u32, usize)> {
        // E5.11 C1 batch emit path
        if self.batch_n_out > 0 {
            if self.batch_emit_i < self.batch_n_out {
                let i = self.batch_emit_i;
                self.batch_emit_i += 1;
                self.symbols_yielded += 1;
                return Some((self.batch_sym[i], self.batch_cnt[i] as usize));
            }
            return None;
        }

        while self.sp > 0 {
            self.sp -= 1;
            let mut cur = self.stack[self.sp];
            self.frames_popped += 1;

            if cur.start >= cur.end {
                continue;
            }

            if cur.level == self.hot.n_levels {
                self.symbols_yielded += 1;
                return Some((cur.bit_path as u32, cur.end - cur.start));
            }

            // Expand; collapse unary chains without re-stacking.
            loop {
                if cur.level >= self.hot.n_levels {
                    break;
                }
                let lv = self.hot.level(cur.level);

                // E5.11 B2 fused expand over B1 hot pointers.
                // Logical rank_probes stays +2 for heap identity; path stats separate.
                let exp = hot_level_range_expand(lv, cur.start, cur.end);
                self.rank_probes += 2;
                self.range_counts_calls += 1;
                match exp.path {
                    RangeCountsPath::SameLine => {
                        self.same_line_hits += 1;
                        self.same_line_symbols += (cur.end - cur.start) as u64;
                        self.data_line_loads += exp.data_line_loads;
                        self.superblock_loads += exp.superblock_loads;
                    }
                    RangeCountsPath::SameSuperblock => {
                        self.same_superblock_hits += 1;
                        self.data_line_loads += exp.data_line_loads;
                        self.superblock_loads += exp.superblock_loads;
                    }
                    RangeCountsPath::General => {
                        self.general_hits += 1;
                        self.data_line_loads += exp.data_line_loads;
                        self.superblock_loads += exp.superblock_loads;
                        hot_level_prefetch_endpoints(lv, cur.start, cur.end);
                        self.prefetch_attempts += 1;
                    }
                }

                let mut cand_s = [0usize; 4];
                let mut cand_e = [0usize; 4];
                let mut cand_path = [0usize; 4];
                let mut nc = 0usize;

                for b in 0..4u8 {
                    let lo = exp.ranks_s[b as usize];
                    let hi = lo + exp.counts[b as usize];
                    if hi > lo {
                        let offset = lv.occs[b as usize];
                        cand_s[nc] = offset + lo;
                        cand_e[nc] = offset + hi;
                        cand_path[nc] = (cur.bit_path << 2) | (b as usize);
                        nc += 1;
                    } else {
                        self.empty_branches += 1;
                    }
                }

                if nc == 0 {
                    break;
                }

                // Unary path collapse (E5.9A lever B).
                if nc == 1 {
                    cur.start = cand_s[0];
                    cur.end = cand_e[0];
                    cur.bit_path = cand_path[0];
                    cur.level += 1;
                    self.children_pushed += 1;
                    self.branch_transitions += 1;
                    self.unary_collapses += 1;
                    if cur.level == self.hot.n_levels {
                        self.symbols_yielded += 1;
                        return Some((cur.bit_path as u32, cur.end - cur.start));
                    }
                    continue;
                }

                // Branch: push children reverse for lex order.
                for i in (0..nc).rev() {
                    debug_assert!(self.sp < 128);
                    self.stack[self.sp] = MappedRdiFrame {
                        start: cand_s[i],
                        end: cand_e[i],
                        level: cur.level + 1,
                        bit_path: cand_path[i],
                    };
                    self.sp += 1;
                    self.children_pushed += 1;
                    self.branch_transitions += 1;
                }
                break;
            }
        }
        None
    }
}

impl Iterator for MappedRangeDistinctIter<'_> {
    type Item = (u32, usize);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.next_symbol()
    }
}

// ── E5.11 B5: prefetch policy matrix ────────────────────────────────────────

/// Endpoint prefetch policy for mapped RDI General path (E5.11 B5).
///
/// Star expands are ~96.9% SameLine (no prefetch); only General (~0.3%) hits
/// this. Matrix: None / T0 / T1 / NTA. Keep only if star/RDI gain ≥3–5%.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PrefetchPolicy {
    /// No software prefetch.
    None = 0,
    /// Temporal locality (reuse soon) — `_MM_HINT_T0`.
    T0 = 1,
    /// Temporal locality, weaker — `_MM_HINT_T1`.
    T1 = 2,
    /// Non-temporal (do not pollute cache) — `_MM_HINT_NTA` (legacy default).
    Nta = 3,
}

impl PrefetchPolicy {
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "0" => Some(Self::None),
            "t0" | "1" => Some(Self::T0),
            "t1" | "2" => Some(Self::T1),
            "nta" | "nt" | "3" => Some(Self::Nta),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::T0 => "t0",
            Self::T1 => "t1",
            Self::Nta => "nta",
        }
    }

    pub const ALL: [Self; 4] = [Self::None, Self::T0, Self::T1, Self::Nta];
}

/// Process-wide B5 policy (harness sweeps via [`set_prefetch_policy`]).
/// Default: **Nta** — E5.11 B5 matrix keep (≈3–5% RDI/star on N=20k realistic).
static PREFETCH_POLICY: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(PrefetchPolicy::Nta as u8);

/// Set process-wide mapped RDI endpoint prefetch policy (E5.11 B5).
/// Mutation knob for harness matrices; product path leaves NTA default.
#[cfg(any(test, feature = "diagnostics"))]
pub fn set_prefetch_policy(p: PrefetchPolicy) {
    PREFETCH_POLICY.store(p as u8, std::sync::atomic::Ordering::Relaxed);
}

/// Current process-wide prefetch policy.
pub fn prefetch_policy() -> PrefetchPolicy {
    match PREFETCH_POLICY.load(std::sync::atomic::Ordering::Relaxed) {
        1 => PrefetchPolicy::T0,
        2 => PrefetchPolicy::T1,
        3 => PrefetchPolicy::Nta,
        _ => PrefetchPolicy::None,
    }
}

/// Hot-path endpoint prefetch via raw pointers (E5.11 B1 + B5 policy).
#[inline]
fn hot_level_prefetch_endpoints(lv: &HotMappedLevel, start: usize, end: usize) {
    let policy = prefetch_policy();
    if policy == PrefetchPolicy::None {
        return;
    }
    for pos in [start, end] {
        if pos > lv.qv_len {
            continue;
        }
        let block = pos / BLOCK_SIZE;
        let sb_idx = block / BLOCKS_IN_SUPERBLOCK;
        if sb_idx < lv.super_len {
            // SAFETY: open-time validated; sb_idx < super_len.
            let p = unsafe { lv.superblocks.add(sb_idx) };
            hot_prefetch(p, policy);
        }
        let line_id = pos >> 8;
        if line_id < lv.data_len {
            let p = unsafe { lv.data.add(line_id) };
            hot_prefetch(p, policy);
        }
    }
}

#[inline(always)]
fn hot_prefetch<T>(p: *const T, policy: PrefetchPolicy) {
    let _p = p as *const i8;
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        #[cfg(target_arch = "x86")]
        use std::arch::x86::{_MM_HINT_NTA, _MM_HINT_T0, _MM_HINT_T1, _mm_prefetch};
        #[cfg(target_arch = "x86_64")]
        use std::arch::x86_64::{_MM_HINT_NTA, _MM_HINT_T0, _MM_HINT_T1, _mm_prefetch};
        // SAFETY: pure hardware hint; no load semantics required.
        unsafe {
            match policy {
                PrefetchPolicy::None => {}
                PrefetchPolicy::T0 => _mm_prefetch(_p, _MM_HINT_T0),
                PrefetchPolicy::T1 => _mm_prefetch(_p, _MM_HINT_T1),
                PrefetchPolicy::Nta => _mm_prefetch(_p, _MM_HINT_NTA),
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // aarch64: T0 → keep, NTA/T1 → stream (approximate).
        if policy != PrefetchPolicy::None {
            let keep = matches!(policy, PrefetchPolicy::T0);
            unsafe {
                if keep {
                    std::arch::asm!(
                        "prfm pldl1keep, [{}]",
                        in(reg) _p,
                        options(nostack, readonly, preserves_flags)
                    );
                } else {
                    std::arch::asm!(
                        "prfm pldl1strm, [{}]",
                        in(reg) _p,
                        options(nostack, readonly, preserves_flags)
                    );
                }
            }
        }
    }
    let _ = (_p, policy);
}

/// Owned image with open-time hot column (E5.11 B1).
///
/// Bytes live in a **64-byte-aligned** heap buffer so `DataLine` /
/// `SuperblockPlain` casts are valid (same absolute alignment as a page
/// mmap). Suitable for W0 tests. Product open for Ring A is
/// [`super::mapped_ring::MappedRingA`] over `memmap2::Mmap`.
///
/// Hot primitives (get/rank/RNV/RDI) route through [`HotQwtColumn`] built once
/// at open. Select stays on the cold section path (needs select samples).
pub struct MappedQwtOwned {
    bytes: AlignedBuf,
    hot: HotQwtColumn,
    pub n: usize,
    pub n_levels: usize,
    pub sigma: u32,
    sec_off: usize,
    sec_len: usize,
}

impl MappedQwtOwned {
    pub fn image_bytes(&self) -> usize {
        self.bytes.len()
    }

    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Borrowed open-time hot column (pointers alias `bytes`).
    #[inline]
    pub fn hot(&self) -> &HotQwtColumn {
        &self.hot
    }

    fn section_view(&self) -> MappedQwtSection<'_> {
        let b = self.bytes.as_slice();
        MappedQwtSection {
            sec: &b[self.sec_off..self.sec_off + self.sec_len],
            n: self.n,
            n_levels: self.n_levels,
            sigma: self.sigma,
        }
    }

    pub fn get(&self, i: usize) -> Option<u32> {
        self.hot.get(i)
    }

    pub fn rank_level(&self, level: usize, b: u8, i: usize) -> Option<usize> {
        self.hot.rank_level(level, b, i)
    }

    pub fn rank_all_level(&self, level: usize, i: usize) -> Option<[usize; 4]> {
        self.hot.rank_all_level(level, i)
    }

    /// Full-symbol rank of `symbol` in `[0, i)`.
    pub fn rank(&self, symbol: u32, i: usize) -> Option<usize> {
        self.hot.rank(symbol, i)
    }

    /// Full-symbol select of the 0-based `occurrence`-th of `symbol`.
    pub fn select(&self, symbol: u32, occurrence: usize) -> Option<usize> {
        // Select samples are cold; keep on section path.
        self.section_view().select(symbol, occurrence)
    }

    /// Native guided RNV (W3) via hot column.
    pub fn range_next_value(&self, range: Range<usize>, target: u32) -> Option<u32> {
        self.hot.range_next_value(range, target)
    }

    /// E5.11 D1: braided two-range intersection successor via hot column.
    pub fn intersection_next_value2(
        &self,
        left: Range<usize>,
        right: Range<usize>,
        target: u32,
    ) -> Option<u32> {
        self.hot.intersection_next_value2(left, right, target)
    }

    /// E5.11 D1: braided intersection with software counters.
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value2_counted(
        &self,
        left: Range<usize>,
        right: Range<usize>,
        target: u32,
    ) -> (Option<u32>, Intersection2Stats) {
        self.hot
            .intersection_next_value2_counted(left, right, target)
    }

    /// Dual-RNV leapfrog oracle (D1 baseline / correctness).
    pub fn intersection_next_value2_dual_rnv(
        &self,
        left: Range<usize>,
        right: Range<usize>,
        target: u32,
    ) -> Option<u32> {
        self.hot
            .intersection_next_value2_dual_rnv(left, right, target)
    }

    /// E5.11 D2: braided three-range intersection successor via the hot column.
    pub fn intersection_next_value3(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> Option<u32> {
        self.hot
            .intersection_next_value3(first, second, third, target)
    }

    /// Three independent RNV leapfrog oracle for D2 differential tests.
    pub fn intersection_next_value3_dual_rnv(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> Option<u32> {
        self.hot
            .intersection_next_value3_dual_rnv(first, second, third, target)
    }

    /// D4-A: three-range expand locality counters (diagnostic).
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_overlap(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> (Option<u32>, Expand3OverlapStats) {
        self.hot
            .intersection_next_value3_overlap(first, second, third, target)
    }

    /// D4-B: mask-first fused expand3 prototype (diagnostic).
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_fused(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> (Option<u32>, Expand3FusedStats) {
        self.hot
            .intersection_next_value3_fused(first, second, third, target)
    }

    /// D4-C: true unique-load expand3 successor (diagnostic).
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_shared(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> (Option<u32>, Expand3SharedStats) {
        self.hot
            .intersection_next_value3_shared(first, second, third, target)
    }

    /// E1.0: summary-gated three-range successor (diagnostic).
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_next_value3_summary(
        &self,
        table: &PresenceSummaryTable,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> (Option<u32>, PresenceSummaryStats) {
        self.hot
            .intersection_next_value3_summary(table, first, second, third, target)
    }

    /// Persistent allocation-free three-range iterator (D3-B).
    #[cfg(any(test, feature = "diagnostics"))]
    pub fn intersection_iter3(
        &self,
        first: Range<usize>,
        second: Range<usize>,
        third: Range<usize>,
        target: u32,
    ) -> IntersectionIter3<'_> {
        self.hot.intersection_iter3(first, second, third, target)
    }

    /// Fixed-stack RDI (W3) via open-time hot column.
    pub fn range_distinct_iter(&self, range: Range<usize>) -> MappedRangeDistinctIter<'_> {
        self.hot.range_distinct_iter(range)
    }

    pub fn n_occs_smaller_level(&self, level: usize) -> Result<[u64; 5], MappedQwtError> {
        self.section_view().n_occs_smaller_level(level)
    }
}

// ── flatten / open ──────────────────────────────────────────────────────────

/// Build a standalone QWTA section from a heap `QWT256` (no file header).
///
/// Used by `NOVAQWT1` and by `NOVARNG1` (three sections).
pub fn build_qwta_section(qwt: &QWT256<u32>) -> Result<Vec<u8>, MappedQwtError> {
    if !cfg!(target_endian = "little") {
        return Err(MappedQwtError::NotLittleEndian);
    }
    let n = qwt.len();

    // Empty sequence: minimal QWTA section (n_levels=0), no payload.
    if n == 0 {
        let mut sec = vec![0u8; 64];
        sec[0..4].copy_from_slice(QWTA_MAGIC);
        return Ok(sec);
    }

    let n_levels = qwt.n_levels();
    if n_levels > MAX_LEVELS {
        return Err(MappedQwtError::TooManyLevels(n_levels));
    }
    let sigma = qwt.sigma_raw();

    struct LP {
        data: Vec<u8>,
        super_b: Vec<u8>,
        select: [Vec<u8>; 4],
        occs: [u64; 5],
        qv_pos: u64,
        n_data: u64,
        n_super: u64,
        n_sel: [u32; 4],
    }

    let mut lps = Vec::with_capacity(n_levels);
    for lv in qwt.levels() {
        let qv = lv.qvector();
        let rs = lv.rs_support();
        let data = qv.data_lines();
        let supers = rs.superblocks();
        let o = lv.n_occs_smaller();
        let mut select: [Vec<u8>; 4] = Default::default();
        let mut n_sel = [0u32; 4];
        for s in 0..4 {
            let samples = rs.select_samples(s);
            n_sel[s] = samples.len() as u32;
            select[s] = samples.iter().flat_map(|x| x.to_le_bytes()).collect();
        }
        lps.push(LP {
            data: slice_as_bytes(data).to_vec(),
            super_b: slice_as_bytes(supers).to_vec(),
            select,
            occs: [
                o[0] as u64,
                o[1] as u64,
                o[2] as u64,
                o[3] as u64,
                o[4] as u64,
            ],
            qv_pos: qv.position_bits() as u64,
            n_data: data.len() as u64,
            n_super: supers.len() as u64,
            n_sel,
        });
    }

    let sec_hdr = 64usize;
    let dir_sz = n_levels * LEVEL_DIR_V1;
    let mut cur = align_up(sec_hdr + dir_sz, 64);

    struct Off {
        data: usize,
        super_b: usize,
        sel: [usize; 4],
        occs: usize,
    }
    let mut offs = Vec::with_capacity(n_levels);
    for lp in &lps {
        let data = cur;
        cur += lp.data.len();
        cur = align_up(cur, 64);
        let super_b = cur;
        cur += lp.super_b.len();
        cur = align_up(cur, 64);
        let occs = cur;
        cur += 40; // 5×u64
        cur = align_up(cur, 4);
        let mut sel = [0usize; 4];
        for s in 0..4 {
            sel[s] = cur;
            cur += lp.select[s].len();
            cur = align_up(cur, 4);
        }
        cur = align_up(cur, 64);
        offs.push(Off {
            data,
            super_b,
            sel,
            occs,
        });
    }

    let mut sec = vec![0u8; cur.max(64)];
    sec[0..4].copy_from_slice(QWTA_MAGIC);
    sec[4..12].copy_from_slice(&(n as u64).to_le_bytes());
    sec[12..14].copy_from_slice(&(n_levels as u16).to_le_bytes());
    sec[14..18].copy_from_slice(&sigma.to_le_bytes());
    if let Some(lp0) = lps.first() {
        for (i, &v) in lp0.occs.iter().enumerate() {
            let o = 20 + i * 8;
            sec[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }
    }

    for (li, lp) in lps.iter().enumerate() {
        let base = sec_hdr + li * LEVEL_DIR_V1;
        let o = &offs[li];
        let mut e = [0u8; LEVEL_DIR_V1];
        e[0..8].copy_from_slice(&(o.data as u64).to_le_bytes());
        e[8..16].copy_from_slice(&lp.n_data.to_le_bytes());
        e[16..24].copy_from_slice(&(o.super_b as u64).to_le_bytes());
        e[24..32].copy_from_slice(&lp.n_super.to_le_bytes());
        for s in 0..4 {
            e[32 + s * 8..40 + s * 8].copy_from_slice(&(o.sel[s] as u64).to_le_bytes());
        }
        for s in 0..4 {
            e[64 + s * 4..68 + s * 4].copy_from_slice(&lp.n_sel[s].to_le_bytes());
        }
        e[80..88].copy_from_slice(&lp.qv_pos.to_le_bytes());
        e[88..96].copy_from_slice(&(o.occs as u64).to_le_bytes());
        sec[base..base + LEVEL_DIR_V1].copy_from_slice(&e);

        sec[o.data..o.data + lp.data.len()].copy_from_slice(&lp.data);
        sec[o.super_b..o.super_b + lp.super_b.len()].copy_from_slice(&lp.super_b);
        for (i, &v) in lp.occs.iter().enumerate() {
            sec[o.occs + i * 8..o.occs + i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        for s in 0..4 {
            let b = &lp.select[s];
            sec[o.sel[s]..o.sel[s] + b.len()].copy_from_slice(b);
        }
    }

    Ok(sec)
}

/// Flatten a heap `QWT256<u32>` into a `NOVAQWT1` image (owned bytes).
pub fn write_novaqwt1_v1(qwt: &QWT256<u32>) -> Result<Vec<u8>, MappedQwtError> {
    let sec = build_qwta_section(qwt)?;
    let n = qwt.len();
    let (n_levels, sigma) = if n == 0 {
        (0usize, 0u32)
    } else {
        (qwt.n_levels(), qwt.sigma_raw())
    };
    finish_file(n, n_levels, sigma, &sec)
}

/// Public alias used by harnesses / W1.
pub use write_novaqwt1_v1 as flatten_qwt256;

/// Write image to path; returns byte length.
pub fn write_novaqwt1_file(path: &Path, qwt: &QWT256<u32>) -> Result<usize, MappedQwtError> {
    let bytes = write_novaqwt1_v1(qwt)?;
    let mut f = std::fs::File::create(path)?;
    f.write_all(&bytes)?;
    Ok(bytes.len())
}

/// Open a `NOVAQWT1` image (copies bytes into owned storage).
///
/// Validates: endianness, magic, version, header checksum, bounds, alignments.
/// Does **not** scan the full payload body (demand-paged open path).
pub fn open_novaqwt1(bytes: &[u8]) -> Result<MappedQwtOwned, MappedQwtError> {
    if !cfg!(target_endian = "little") {
        return Err(MappedQwtError::NotLittleEndian);
    }
    if bytes.len() < HEADER_SIZE {
        return Err(MappedQwtError::Layout("short file"));
    }
    if &bytes[0..8] != NOVAQWT1_MAGIC {
        return Err(MappedQwtError::BadMagic);
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(MappedQwtError::BadVersion(version));
    }
    if bytes[12] != PAGE_ALIGN_LOG2 {
        return Err(MappedQwtError::Layout("page_align_log2"));
    }
    let checksum_stored = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let mut hdr = [0u8; HEADER_SIZE];
    hdr.copy_from_slice(&bytes[..HEADER_SIZE]);
    hdr[16..24].fill(0);
    if fnv1a64(&hdr) != checksum_stored {
        return Err(MappedQwtError::HeaderChecksum);
    }
    let n = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
    let n_levels = u16::from_le_bytes(bytes[32..34].try_into().unwrap()) as usize;
    let sigma = u32::from_le_bytes(bytes[34..38].try_into().unwrap());
    let sec_off = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
    let sec_len = u64::from_le_bytes(bytes[48..56].try_into().unwrap()) as usize;
    if n_levels > MAX_LEVELS {
        return Err(MappedQwtError::TooManyLevels(n_levels));
    }
    if sec_off % PAGE_ALIGN != 0 {
        return Err(MappedQwtError::Layout("section not page-aligned"));
    }
    if sec_off
        .checked_add(sec_len)
        .map(|e| e > bytes.len())
        .unwrap_or(true)
    {
        return Err(MappedQwtError::Layout("section OOB"));
    }
    // Validate section magic/dir via shared opener, then build hot view from
    // the *aligned* owned buffer (pointers must alias `AlignedBuf`, not the
    // temporary input slice).
    let _ = MappedQwtSection::open(&bytes[sec_off..sec_off + sec_len])?;
    let aligned = AlignedBuf::from_slice(bytes);
    let sec = MappedQwtSection {
        sec: &aligned.as_slice()[sec_off..sec_off + sec_len],
        n,
        n_levels,
        sigma,
    };
    let hot = sec.build_hot()?;

    Ok(MappedQwtOwned {
        bytes: aligned,
        hot,
        n,
        n_levels,
        sigma,
        sec_off,
        sec_len,
    })
}

// ── level arithmetic (block size 256) ───────────────────────────────────────

#[inline]
pub(crate) fn level_get(lv: &MappedLevel<'_>, i: usize) -> u8 {
    debug_assert!(i < lv.n_symbols);
    let line = i >> 8;
    let pos = i & 255;
    unsafe { lv.data.get_unchecked(line).get_unchecked(pos) }
}

#[inline]
pub(crate) fn level_rank(lv: &MappedLevel<'_>, symbol: u8, i: usize) -> usize {
    let block = i / BLOCK_SIZE;
    let sb_idx = block / BLOCKS_IN_SUPERBLOCK;
    let block_in_sb = block % BLOCKS_IN_SUPERBLOCK;
    let block_rank = if sb_idx < lv.superblocks.len() {
        lv.superblocks[sb_idx].get_rank(symbol, block_in_sb)
    } else {
        0
    };
    let data_line_id = i >> 8;
    let offset = i & 255;
    let intra = if let Some(d) = lv.data.get(data_line_id) {
        unsafe { d.rank_unchecked(symbol, offset) }
    } else {
        0
    };
    block_rank + intra
}

#[inline]
pub(crate) fn level_rank_all(lv: &MappedLevel<'_>, i: usize) -> [usize; 4] {
    let block = i / BLOCK_SIZE;
    let sb_idx = block / BLOCKS_IN_SUPERBLOCK;
    let block_in_sb = block % BLOCKS_IN_SUPERBLOCK;
    let block_ranks = if sb_idx < lv.superblocks.len() {
        lv.superblocks[sb_idx].get_rank_all(block_in_sb)
    } else {
        [0; 4]
    };
    let data_line_id = i >> 8;
    let offset = i & 255;
    let intra = if let Some(d) = lv.data.get(data_line_id) {
        unsafe { d.rank_all_unchecked(offset) }
    } else {
        [0; 4]
    };
    [
        block_ranks[0] + intra[0],
        block_ranks[1] + intra[1],
        block_ranks[2] + intra[2],
        block_ranks[3] + intra[3],
    ]
}

// ── E5.11 D4: three-range expand locality helpers ───────────────────────────

#[derive(Clone, Copy, Debug)]
struct RangeTouchIds {
    line_s: usize,
    line_e: usize,
    sb_s: usize,
    sb_e: usize,
    same_line: bool,
    line_loads: u8,
    sb_loads: u8,
}

#[inline]
fn range_touch_ids(start: usize, end: usize) -> RangeTouchIds {
    // end is exclusive; empty ranges should not reach here.
    let end_incl = end.saturating_sub(1).max(start);
    let line_s = start >> 8;
    let line_e = end_incl >> 8;
    let block_s = start / BLOCK_SIZE;
    let block_e = end_incl / BLOCK_SIZE;
    let sb_s = block_s / BLOCKS_IN_SUPERBLOCK;
    let sb_e = block_e / BLOCKS_IN_SUPERBLOCK;
    let same_line = line_s == line_e;
    let line_loads = if same_line { 1 } else { 2 };
    let sb_loads = if sb_s == sb_e { 1 } else { 2 };
    RangeTouchIds {
        line_s,
        line_e,
        sb_s,
        sb_e,
        same_line,
        line_loads,
        sb_loads,
    }
}

#[inline]
fn push_unique_u32(buf: &mut [u32; 6], n: &mut usize, v: u32) {
    for i in 0..*n {
        if buf[i] == v {
            return;
        }
    }
    if *n < 6 {
        buf[*n] = v;
        *n += 1;
    }
}

#[inline]
fn unique_touch3(a: RangeTouchIds, b: RangeTouchIds, c: RangeTouchIds) -> (u8, u8) {
    let mut lines = [0u32; 6];
    let mut n_lines = 0usize;
    let mut sbs = [0u32; 6];
    let mut n_sbs = 0usize;
    for t in [a, b, c] {
        push_unique_u32(&mut lines, &mut n_lines, t.line_s as u32);
        push_unique_u32(&mut lines, &mut n_lines, t.line_e as u32);
        push_unique_u32(&mut sbs, &mut n_sbs, t.sb_s as u32);
        push_unique_u32(&mut sbs, &mut n_sbs, t.sb_e as u32);
    }
    (n_lines as u8, n_sbs as u8)
}

#[inline]
fn lines_overlap(x: RangeTouchIds, y: RangeTouchIds) -> bool {
    // Closed intervals on line ids [line_s, line_e].
    x.line_s <= y.line_e && y.line_s <= x.line_e
}

// ── E5.11 B1: hot-pointer level arithmetic ──────────────────────────────────

/// Access 2-bit symbol at position `i` via open-time validated pointers.
#[inline(always)]
fn hot_level_get(lv: &HotMappedLevel, i: usize) -> u8 {
    debug_assert!(i < lv.qv_len);
    let line = i >> 8;
    let pos = i & 255;
    // SAFETY: open validated data_len / qv_len; i < qv_len ⇒ line in range.
    unsafe { lv.data_at(line).get_unchecked(pos) }
}

/// Level-local rank of 2-bit `symbol` up to `i` (excluded) — unchecked loads.
#[inline(always)]
fn hot_level_rank(lv: &HotMappedLevel, symbol: u8, i: usize) -> usize {
    let block = i / BLOCK_SIZE;
    let sb_idx = block / BLOCKS_IN_SUPERBLOCK;
    let block_in_sb = block % BLOCKS_IN_SUPERBLOCK;
    let block_rank = if sb_idx < lv.super_len {
        // SAFETY: sb_idx < super_len (open-validated).
        unsafe { lv.super_at(sb_idx).get_rank(symbol, block_in_sb) }
    } else {
        0
    };
    let data_line_id = i >> 8;
    let offset = i & 255;
    let intra = if data_line_id < lv.data_len {
        // SAFETY: data_line_id < data_len; offset ≤ 255.
        unsafe { lv.data_at(data_line_id).rank_unchecked(symbol, offset) }
    } else {
        0
    };
    block_rank + intra
}

/// Level-local rank-all-4 up to `i` (excluded) — unchecked loads.
#[inline(always)]
fn hot_level_rank_all(lv: &HotMappedLevel, i: usize) -> [usize; 4] {
    let block = i / BLOCK_SIZE;
    let sb_idx = block / BLOCKS_IN_SUPERBLOCK;
    let block_in_sb = block % BLOCKS_IN_SUPERBLOCK;
    let block_ranks = if sb_idx < lv.super_len {
        unsafe { lv.super_at(sb_idx).get_rank_all(block_in_sb) }
    } else {
        [0; 4]
    };
    let data_line_id = i >> 8;
    let offset = i & 255;
    let intra = if data_line_id < lv.data_len {
        unsafe { lv.data_at(data_line_id).rank_all_unchecked(offset) }
    } else {
        [0; 4]
    };
    [
        block_ranks[0] + intra[0],
        block_ranks[1] + intra[1],
        block_ranks[2] + intra[2],
        block_ranks[3] + intra[3],
    ]
}

/// B2 fused range expand over B1 hot pointers (no slice/Option on hot path).
#[inline(always)]
fn hot_level_range_expand(lv: &HotMappedLevel, start: usize, end: usize) -> RangeExpand {
    debug_assert!(start <= end);
    debug_assert!(end <= lv.qv_len);

    let block_s = start / BLOCK_SIZE;
    let block_e = end / BLOCK_SIZE;
    let sb_s = block_s / BLOCKS_IN_SUPERBLOCK;
    let sb_e = block_e / BLOCKS_IN_SUPERBLOCK;

    // ── Same 256-block (same DataLine): one SB + one line ──────────────────
    if block_s == block_e {
        let block_in_sb = block_s % BLOCKS_IN_SUPERBLOCK;
        let mut sb_loads = 0u64;
        let block_ranks = if sb_s < lv.super_len {
            sb_loads = 1;
            unsafe { lv.super_at(sb_s).get_rank_all(block_in_sb) }
        } else {
            [0; 4]
        };
        let offset_s = start & 255;
        let offset_e = end & 255;
        let data_line_id = start >> 8;
        let (intra_s, intra_e, dl_loads) = if data_line_id < lv.data_len {
            let d = unsafe { lv.data_at(data_line_id) };
            let is = unsafe { d.rank_all_unchecked(offset_s) };
            let ie = unsafe { d.rank_all_unchecked(offset_e) };
            (is, ie, 1u64)
        } else {
            ([0; 4], [0; 4], 0u64)
        };
        let ranks_s = [
            block_ranks[0] + intra_s[0],
            block_ranks[1] + intra_s[1],
            block_ranks[2] + intra_s[2],
            block_ranks[3] + intra_s[3],
        ];
        let ranks_e = [
            block_ranks[0] + intra_e[0],
            block_ranks[1] + intra_e[1],
            block_ranks[2] + intra_e[2],
            block_ranks[3] + intra_e[3],
        ];
        return RangeExpand {
            path: RangeCountsPath::SameLine,
            ranks_s,
            counts: [
                ranks_e[0] - ranks_s[0],
                ranks_e[1] - ranks_s[1],
                ranks_e[2] - ranks_s[2],
                ranks_e[3] - ranks_s[3],
            ],
            data_line_loads: dl_loads,
            superblock_loads: sb_loads,
        };
    }

    // ── Same superblock, different blocks: one SB + two lines ───────────────
    if sb_s == sb_e {
        let mut sb_loads = 0u64;
        let (br_s, br_e) = if sb_s < lv.super_len {
            sb_loads = 1;
            let sb = unsafe { lv.super_at(sb_s) };
            (
                sb.get_rank_all(block_s % BLOCKS_IN_SUPERBLOCK),
                sb.get_rank_all(block_e % BLOCKS_IN_SUPERBLOCK),
            )
        } else {
            ([0; 4], [0; 4])
        };
        let mut dl_loads = 0u64;
        let line_s = start >> 8;
        let intra_s = if line_s < lv.data_len {
            dl_loads += 1;
            unsafe { lv.data_at(line_s).rank_all_unchecked(start & 255) }
        } else {
            [0; 4]
        };
        let line_e = end >> 8;
        let intra_e = if line_e < lv.data_len {
            dl_loads += 1;
            unsafe { lv.data_at(line_e).rank_all_unchecked(end & 255) }
        } else {
            [0; 4]
        };
        let ranks_s = [
            br_s[0] + intra_s[0],
            br_s[1] + intra_s[1],
            br_s[2] + intra_s[2],
            br_s[3] + intra_s[3],
        ];
        let ranks_e = [
            br_e[0] + intra_e[0],
            br_e[1] + intra_e[1],
            br_e[2] + intra_e[2],
            br_e[3] + intra_e[3],
        ];
        return RangeExpand {
            path: RangeCountsPath::SameSuperblock,
            ranks_s,
            counts: [
                ranks_e[0] - ranks_s[0],
                ranks_e[1] - ranks_s[1],
                ranks_e[2] - ranks_s[2],
                ranks_e[3] - ranks_s[3],
            ],
            data_line_loads: dl_loads,
            superblock_loads: sb_loads,
        };
    }

    // ── General: two independent rank_all ───────────────────────────────────
    let ranks_s = hot_level_rank_all(lv, start);
    let ranks_e = hot_level_rank_all(lv, end);
    let mut sb_loads = 0u64;
    let mut dl_loads = 0u64;
    if sb_s < lv.super_len {
        sb_loads += 1;
    }
    if sb_e < lv.super_len {
        sb_loads += 1;
    }
    if (start >> 8) < lv.data_len {
        dl_loads += 1;
    }
    if (end >> 8) < lv.data_len {
        dl_loads += 1;
    }
    RangeExpand {
        path: RangeCountsPath::General,
        ranks_s,
        counts: [
            ranks_e[0] - ranks_s[0],
            ranks_e[1] - ranks_s[1],
            ranks_e[2] - ranks_s[2],
            ranks_e[3] - ranks_s[3],
        ],
        data_line_loads: dl_loads,
        superblock_loads: sb_loads,
    }
}

// ── E5.11 D4-C: true unique-load three-range expand ─────────────────────────

/// Fixed local cache entry for a loaded DataLine (by index).
#[derive(Clone, Copy)]
struct LoadedLine {
    index: usize,
    value: DataLine,
}

/// Fixed local cache entry for a loaded SuperblockPlain (by index).
#[derive(Clone, Copy)]
struct LoadedSuperblock {
    index: usize,
    value: SuperblockPlain,
}

/// Tiny fixed line cache (max 6 unique endpoint lines). No heap.
struct LineCache {
    slots: [LoadedLine; 6],
    n: usize,
    loads: u8,
}

impl LineCache {
    #[inline(always)]
    fn new() -> Self {
        Self {
            // Dummy zeros; never read before write.
            slots: [LoadedLine {
                index: usize::MAX,
                value: DataLine { words: [0; 4] },
            }; 6],
            n: 0,
            loads: 0,
        }
    }

    #[inline(always)]
    fn get<'a>(&'a mut self, lv: &HotMappedLevel, id: usize) -> Option<&'a DataLine> {
        for i in 0..self.n {
            if self.slots[i].index == id {
                // SAFETY: slots[i] is initialized when n > i.
                return Some(&self.slots[i].value);
            }
        }
        if id >= lv.data_len {
            return None;
        }
        // SAFETY: id < data_len (open-validated).
        let value = unsafe { *lv.data_at(id) };
        debug_assert!(self.n < 6);
        self.slots[self.n] = LoadedLine { index: id, value };
        self.n += 1;
        self.loads += 1;
        Some(&self.slots[self.n - 1].value)
    }
}

/// Tiny fixed superblock cache (max 6 unique endpoint SBs). No heap.
struct SbCache {
    slots: [LoadedSuperblock; 6],
    n: usize,
    loads: u8,
}

impl SbCache {
    #[inline(always)]
    fn new() -> Self {
        Self {
            slots: [LoadedSuperblock {
                index: usize::MAX,
                value: SuperblockPlain { counters: [0; 4] },
            }; 6],
            n: 0,
            loads: 0,
        }
    }

    #[inline(always)]
    fn get<'a>(&'a mut self, lv: &HotMappedLevel, id: usize) -> Option<&'a SuperblockPlain> {
        for i in 0..self.n {
            if self.slots[i].index == id {
                return Some(&self.slots[i].value);
            }
        }
        if id >= lv.super_len {
            return None;
        }
        // SAFETY: id < super_len (open-validated).
        let value = unsafe { *lv.super_at(id) };
        debug_assert!(self.n < 6);
        self.slots[self.n] = LoadedSuperblock { index: id, value };
        self.n += 1;
        self.loads += 1;
        Some(&self.slots[self.n - 1].value)
    }
}

#[inline(always)]
fn rank_all_from_caches(
    lv: &HotMappedLevel,
    pos: usize,
    lines: &mut LineCache,
    sbs: &mut SbCache,
) -> [usize; 4] {
    let block = pos / BLOCK_SIZE;
    let sb_idx = block / BLOCKS_IN_SUPERBLOCK;
    let block_in_sb = block % BLOCKS_IN_SUPERBLOCK;
    let block_ranks = if let Some(sb) = sbs.get(lv, sb_idx) {
        sb.get_rank_all(block_in_sb)
    } else {
        [0; 4]
    };
    let line_id = pos >> 8;
    let offset = pos & 255;
    let intra = if let Some(d) = lines.get(lv, line_id) {
        unsafe { d.rank_all_unchecked(offset) }
    } else {
        [0; 4]
    };
    [
        block_ranks[0] + intra[0],
        block_ranks[1] + intra[1],
        block_ranks[2] + intra[2],
        block_ranks[3] + intra[3],
    ]
}

#[inline(always)]
fn range_expand_from_ranks(
    ranks_s: [usize; 4],
    ranks_e: [usize; 4],
    path: RangeCountsPath,
    dl: u64,
    sb: u64,
) -> RangeExpand {
    RangeExpand {
        path,
        ranks_s,
        counts: [
            ranks_e[0] - ranks_s[0],
            ranks_e[1] - ranks_s[1],
            ranks_e[2] - ranks_s[2],
            ranks_e[3] - ranks_s[3],
        ],
        data_line_loads: dl,
        superblock_loads: sb,
    }
}

#[inline(always)]
fn common_mask3(a: &RangeExpand, b: &RangeExpand, c: &RangeExpand) -> u8 {
    let mut m = 0u8;
    for i in 0..4 {
        if a.counts[i] != 0 && b.counts[i] != 0 && c.counts[i] != 0 {
            m |= 1 << i;
        }
    }
    m
}

#[inline(always)]
fn push_unique_usize(buf: &mut [usize; 6], n: &mut usize, v: usize) {
    for i in 0..*n {
        if buf[i] == v {
            return;
        }
    }
    if *n < 6 {
        buf[*n] = v;
        *n += 1;
    }
}

/// D4-C: expand three ranges with unique physical DataLine / Superblock loads.
///
/// Classifies the six endpoints first, loads each unique record once into a
/// fixed local cache, then computes all endpoint ranks from that cache.
/// Algebraically identical to three independent [`hot_level_range_expand`]
/// calls; the only difference is load deduplication.
#[inline(always)]
fn hot_level_range_expand3(
    lv: &HotMappedLevel,
    a_start: usize,
    a_end: usize,
    b_start: usize,
    b_end: usize,
    c_start: usize,
    c_end: usize,
) -> Expand3Result {
    debug_assert!(a_start <= a_end && a_end <= lv.qv_len);
    debug_assert!(b_start <= b_end && b_end <= lv.qv_len);
    debug_assert!(c_start <= c_end && c_end <= lv.qv_len);

    // Logical load requests (what three independent B2 expands would pay).
    let touch_a = range_touch_ids(a_start, a_end);
    let touch_b = range_touch_ids(b_start, b_end);
    let touch_c = range_touch_ids(c_start, c_end);
    let logical_line = touch_a.line_loads + touch_b.line_loads + touch_c.line_loads;
    let logical_sb = touch_a.sb_loads + touch_b.sb_loads + touch_c.sb_loads;

    // Unique endpoint line / SB ids among the six endpoints.
    let ends = [a_start, a_end, b_start, b_end, c_start, c_end];
    let mut line_ids = [0usize; 6];
    let mut n_lines = 0usize;
    let mut sb_ids = [0usize; 6];
    let mut n_sbs = 0usize;
    for &p in &ends {
        // end may equal start on empty (already filtered), or be exclusive.
        // For exclusive end that lands on a block boundary, end>>8 is the next
        // line id — matching B2 expand which uses end / BLOCK_SIZE for path
        // classification. rank_all at an exclusive end still needs that line
        // when offset==0 only for the block rank; B2 still loads line_e.
        let line = p >> 8;
        let block = p / BLOCK_SIZE;
        let sb = block / BLOCKS_IN_SUPERBLOCK;
        push_unique_usize(&mut line_ids, &mut n_lines, line);
        push_unique_usize(&mut sb_ids, &mut n_sbs, sb);
    }

    // ── Path 1: all six endpoints in one DataLine ───────────────────────────
    // Dominant on product triangle (~81% frames per D4-A).
    if n_lines == 1 {
        let line_id = line_ids[0];
        let sb_id = sb_ids[0]; // all same line ⇒ same SB
        let mut sb_loads = 0u8;
        let block_in_sb = (a_start / BLOCK_SIZE) % BLOCKS_IN_SUPERBLOCK;
        // All endpoints share the line, so block ranks for every endpoint use
        // the same superblock; but block_in_sb may differ only if lines were
        // different — here they are not. For SameLine per-range, start and end
        // share block_in_sb. Across three ranges on the same line they share
        // the same block as well (one DataLine = one block).
        let block_ranks = if sb_id < lv.super_len {
            sb_loads = 1;
            // SAFETY: sb_id < super_len.
            unsafe { lv.super_at(sb_id).get_rank_all(block_in_sb) }
        } else {
            [0; 4]
        };
        let mut dl_loads = 0u8;
        let (intra_as, intra_ae, intra_bs, intra_be, intra_cs, intra_ce) = if line_id < lv.data_len
        {
            dl_loads = 1;
            let d = unsafe { lv.data_at(line_id) };
            unsafe {
                (
                    d.rank_all_unchecked(a_start & 255),
                    d.rank_all_unchecked(a_end & 255),
                    d.rank_all_unchecked(b_start & 255),
                    d.rank_all_unchecked(b_end & 255),
                    d.rank_all_unchecked(c_start & 255),
                    d.rank_all_unchecked(c_end & 255),
                )
            }
        } else {
            ([0; 4], [0; 4], [0; 4], [0; 4], [0; 4], [0; 4])
        };
        let add = |br: [usize; 4], intra: [usize; 4]| -> [usize; 4] {
            [
                br[0] + intra[0],
                br[1] + intra[1],
                br[2] + intra[2],
                br[3] + intra[3],
            ]
        };
        let first = range_expand_from_ranks(
            add(block_ranks, intra_as),
            add(block_ranks, intra_ae),
            RangeCountsPath::SameLine,
            dl_loads as u64,
            sb_loads as u64,
        );
        let second = range_expand_from_ranks(
            add(block_ranks, intra_bs),
            add(block_ranks, intra_be),
            RangeCountsPath::SameLine,
            0,
            0,
        );
        let third = range_expand_from_ranks(
            add(block_ranks, intra_cs),
            add(block_ranks, intra_ce),
            RangeCountsPath::SameLine,
            0,
            0,
        );
        let mask = common_mask3(&first, &second, &third);
        return Expand3Result {
            first,
            second,
            third,
            common_mask: mask,
            path: Expand3Path::AllSameLine,
            logical_line_requests: logical_line,
            unique_line_loads: dl_loads,
            logical_sb_requests: logical_sb,
            unique_sb_loads: sb_loads,
        };
    }

    // ── Path 2 / 3 / general: fixed local caches ────────────────────────────
    // Preload unique lines and SBs, then rank all six endpoints.
    let mut lines = LineCache::new();
    let mut sbs = SbCache::new();

    // Eagerly touch unique ids so load counts reflect physical unique loads
    // even if a later rank path is empty (should not happen for valid ranges).
    for i in 0..n_sbs {
        let _ = sbs.get(lv, sb_ids[i]);
    }
    for i in 0..n_lines {
        let _ = lines.get(lv, line_ids[i]);
    }

    let a_s = rank_all_from_caches(lv, a_start, &mut lines, &mut sbs);
    let a_e = rank_all_from_caches(lv, a_end, &mut lines, &mut sbs);
    let b_s = rank_all_from_caches(lv, b_start, &mut lines, &mut sbs);
    let b_e = rank_all_from_caches(lv, b_end, &mut lines, &mut sbs);
    let c_s = rank_all_from_caches(lv, c_start, &mut lines, &mut sbs);
    let c_e = rank_all_from_caches(lv, c_end, &mut lines, &mut sbs);

    let path_for = |s: usize, e: usize| -> RangeCountsPath {
        let bs = s / BLOCK_SIZE;
        let be = e / BLOCK_SIZE;
        if bs == be {
            RangeCountsPath::SameLine
        } else if bs / BLOCKS_IN_SUPERBLOCK == be / BLOCKS_IN_SUPERBLOCK {
            RangeCountsPath::SameSuperblock
        } else {
            RangeCountsPath::General
        }
    };

    let first = range_expand_from_ranks(a_s, a_e, path_for(a_start, a_end), 0, 0);
    let second = range_expand_from_ranks(b_s, b_e, path_for(b_start, b_end), 0, 0);
    let third = range_expand_from_ranks(c_s, c_e, path_for(c_start, c_end), 0, 0);
    let mask = common_mask3(&first, &second, &third);

    let path = if n_lines == 2 {
        Expand3Path::TwoLine
    } else if n_sbs == 1 {
        Expand3Path::SharedSb
    } else {
        Expand3Path::General
    };

    Expand3Result {
        first,
        second,
        third,
        common_mask: mask,
        path,
        logical_line_requests: logical_line,
        unique_line_loads: lines.loads,
        logical_sb_requests: logical_sb,
        unique_sb_loads: sbs.loads,
    }
}

// ── E5.11 B2: fused range_counts4 (same-line / same-SB / general) ───────────

/// Which physical path `level_range_expand` took (mapped RDI diagnostics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RangeCountsPath {
    /// Both endpoints in the same 256-symbol DataLine (one SB + one line).
    SameLine,
    /// Same superblock (8 blocks), different lines (one SB + two lines).
    SameSuperblock,
    /// Cross-superblock (two independent rank_all).
    General,
}

/// Result of a fused range expand: ranks at `start` plus counts in `[start, end)`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RangeExpand {
    pub path: RangeCountsPath,
    /// Level-local rank-all-4 at `start` (excluded).
    pub ranks_s: [usize; 4],
    /// Per-symbol counts in `[start, end)` (= ranks_e − ranks_s).
    pub counts: [usize; 4],
    pub data_line_loads: u64,
    pub superblock_loads: u64,
}

/// Fused four-symbol range expand for RDI (E5.11 B2).
///
/// Algebraically identical to two `level_rank_all` probes, but reuses a single
/// superblock / data-line load when endpoints share a block or superblock.
/// Star locality A.1 measured ~90.6% same 256-block — that is the SameLine path.
#[inline]
pub(crate) fn level_range_expand(lv: &MappedLevel<'_>, start: usize, end: usize) -> RangeExpand {
    debug_assert!(start <= end);
    debug_assert!(end <= lv.n_symbols);

    let block_s = start / BLOCK_SIZE;
    let block_e = end / BLOCK_SIZE;
    let sb_s = block_s / BLOCKS_IN_SUPERBLOCK;
    let sb_e = block_e / BLOCKS_IN_SUPERBLOCK;

    // ── Same 256-block (same DataLine): one SB + one line, two intra offsets ──
    if block_s == block_e {
        let block_in_sb = block_s % BLOCKS_IN_SUPERBLOCK;
        let mut sb_loads = 0u64;
        let block_ranks = if sb_s < lv.superblocks.len() {
            sb_loads = 1;
            lv.superblocks[sb_s].get_rank_all(block_in_sb)
        } else {
            [0; 4]
        };
        let offset_s = start & 255;
        let offset_e = end & 255;
        let data_line_id = start >> 8;
        let (intra_s, intra_e, dl_loads) = if let Some(d) = lv.data.get(data_line_id) {
            // One DataLine load; two rank_all over the same cache line.
            let is = unsafe { d.rank_all_unchecked(offset_s) };
            let ie = unsafe { d.rank_all_unchecked(offset_e) };
            (is, ie, 1u64)
        } else {
            ([0; 4], [0; 4], 0u64)
        };
        let ranks_s = [
            block_ranks[0] + intra_s[0],
            block_ranks[1] + intra_s[1],
            block_ranks[2] + intra_s[2],
            block_ranks[3] + intra_s[3],
        ];
        let ranks_e = [
            block_ranks[0] + intra_e[0],
            block_ranks[1] + intra_e[1],
            block_ranks[2] + intra_e[2],
            block_ranks[3] + intra_e[3],
        ];
        return RangeExpand {
            path: RangeCountsPath::SameLine,
            ranks_s,
            counts: [
                ranks_e[0] - ranks_s[0],
                ranks_e[1] - ranks_s[1],
                ranks_e[2] - ranks_s[2],
                ranks_e[3] - ranks_s[3],
            ],
            data_line_loads: dl_loads,
            superblock_loads: sb_loads,
        };
    }

    // ── Same superblock, different blocks: one SB + two lines ───────────────
    if sb_s == sb_e {
        let mut sb_loads = 0u64;
        let (br_s, br_e) = if sb_s < lv.superblocks.len() {
            sb_loads = 1;
            let sb = &lv.superblocks[sb_s];
            (
                sb.get_rank_all(block_s % BLOCKS_IN_SUPERBLOCK),
                sb.get_rank_all(block_e % BLOCKS_IN_SUPERBLOCK),
            )
        } else {
            ([0; 4], [0; 4])
        };
        let mut dl_loads = 0u64;
        let intra_s = if let Some(d) = lv.data.get(start >> 8) {
            dl_loads += 1;
            unsafe { d.rank_all_unchecked(start & 255) }
        } else {
            [0; 4]
        };
        let intra_e = if let Some(d) = lv.data.get(end >> 8) {
            dl_loads += 1;
            unsafe { d.rank_all_unchecked(end & 255) }
        } else {
            [0; 4]
        };
        let ranks_s = [
            br_s[0] + intra_s[0],
            br_s[1] + intra_s[1],
            br_s[2] + intra_s[2],
            br_s[3] + intra_s[3],
        ];
        let ranks_e = [
            br_e[0] + intra_e[0],
            br_e[1] + intra_e[1],
            br_e[2] + intra_e[2],
            br_e[3] + intra_e[3],
        ];
        return RangeExpand {
            path: RangeCountsPath::SameSuperblock,
            ranks_s,
            counts: [
                ranks_e[0] - ranks_s[0],
                ranks_e[1] - ranks_s[1],
                ranks_e[2] - ranks_s[2],
                ranks_e[3] - ranks_s[3],
            ],
            data_line_loads: dl_loads,
            superblock_loads: sb_loads,
        };
    }

    // ── General: two independent rank_all ───────────────────────────────────
    let ranks_s = level_rank_all(lv, start);
    let ranks_e = level_rank_all(lv, end);
    let mut sb_loads = 0u64;
    let mut dl_loads = 0u64;
    if sb_s < lv.superblocks.len() {
        sb_loads += 1;
    }
    if sb_e < lv.superblocks.len() {
        sb_loads += 1;
    }
    if lv.data.get(start >> 8).is_some() {
        dl_loads += 1;
    }
    if lv.data.get(end >> 8).is_some() {
        dl_loads += 1;
    }
    RangeExpand {
        path: RangeCountsPath::General,
        ranks_s,
        counts: [
            ranks_e[0] - ranks_s[0],
            ranks_e[1] - ranks_s[1],
            ranks_e[2] - ranks_s[2],
            ranks_e[3] - ranks_s[3],
        ],
        data_line_loads: dl_loads,
        superblock_loads: sb_loads,
    }
}

/// Level-local select: position of the 0-based `i`-th occurrence of 2-bit `symbol`.
///
/// Matches vendored `RSQVector256::select` / `RSSupportPlain::select_block`
/// for block size 256.
#[inline]
pub(crate) fn level_select(lv: &MappedLevel<'_>, symbol: u8, i: usize) -> Option<usize> {
    if symbol > 3 {
        return None;
    }
    // Total occurrences of this 2-bit symbol = rank at end of sequence.
    let total = level_rank(lv, symbol, lv.n_symbols);
    if total <= i {
        return None;
    }
    let (mut pos, rank) = level_select_block(lv, symbol, i + 1);
    // Intra-block: find (i - rank + 1)-th occurrence starting at `pos`.
    pos += level_select_intra_block(lv, symbol, i - rank + 1, pos);
    Some(pos)
}

/// Returns `(block_start_pos, rank_at_block_start)` for the block containing
/// the 1-based `i`-th occurrence of `symbol` (qwt `select_block` convention).
#[inline]
fn level_select_block(lv: &MappedLevel<'_>, symbol: u8, i: usize) -> (usize, usize) {
    let samples = lv.select_samples[symbol as usize];
    debug_assert!(!samples.is_empty(), "select samples always have sentinel");
    let sampled_i = (i - 1) / SELECT_NUM_SAMPLES;
    // samples always has at least [0, last_sb] sentinel pair.
    let sampled_i = sampled_i.min(samples.len().saturating_sub(2));
    let mut first_sblock_id = samples[sampled_i] as usize;
    let last_sblock_id = 1 + samples[sampled_i + 1] as usize;

    let step = ((last_sblock_id - first_sblock_id) as f64).sqrt() as usize + 1;

    while first_sblock_id < last_sblock_id {
        if lv.superblocks[first_sblock_id].get_superblock_counter(symbol) >= i {
            break;
        }
        first_sblock_id += step;
    }
    first_sblock_id = first_sblock_id.saturating_sub(step);

    while first_sblock_id < last_sblock_id {
        if lv.superblocks[first_sblock_id].get_superblock_counter(symbol) >= i {
            break;
        }
        first_sblock_id += 1;
    }
    first_sblock_id = first_sblock_id.saturating_sub(1);

    let mut position = first_sblock_id * BLOCK_SIZE * BLOCKS_IN_SUPERBLOCK;
    let mut rank = lv.superblocks[first_sblock_id].get_superblock_counter(symbol);

    let (block_id, block_rank) =
        lv.superblocks[first_sblock_id].block_predecessor(symbol, i - rank);
    position += block_id * BLOCK_SIZE;
    rank += block_rank;
    (position, rank)
}

/// Find the 1-based `i`-th occurrence of `symbol` within the block at `pos`
/// (block-aligned). Returns offset from `pos`. Block size 256 → one DataLine.
#[inline]
fn level_select_intra_block(lv: &MappedLevel<'_>, symbol: u8, i: usize, pos: usize) -> usize {
    // qwt: i is 1-based within the residual; convert to 0-based for bit select.
    let line_id = pos >> 8;
    let mut rem = i - 1;
    let d = unsafe { lv.data.get_unchecked(line_id) };
    let (word_0, word_1) = d.normalize(symbol);

    let cnt_0 = word_0.count_ones() as usize;
    if cnt_0 > rem {
        return select_in_word_u128(word_0, rem as u64) as usize;
    }
    rem -= cnt_0;
    128 + select_in_word_u128(word_1, rem as u64) as usize
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn finish_file(
    n: usize,
    n_levels: usize,
    sigma: u32,
    sec: &[u8],
) -> Result<Vec<u8>, MappedQwtError> {
    let mut buf = vec![0u8; PAGE_ALIGN];
    buf[0..8].copy_from_slice(NOVAQWT1_MAGIC);
    buf[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf[12] = PAGE_ALIGN_LOG2;
    buf[24..32].copy_from_slice(&(n as u64).to_le_bytes());
    buf[32..34].copy_from_slice(&(n_levels as u16).to_le_bytes());
    buf[34..38].copy_from_slice(&sigma.to_le_bytes());
    let sec_off = PAGE_ALIGN;
    buf[40..48].copy_from_slice(&(sec_off as u64).to_le_bytes());
    buf[48..56].copy_from_slice(&(sec.len() as u64).to_le_bytes());
    let mut hdr = [0u8; HEADER_SIZE];
    hdr.copy_from_slice(&buf[..HEADER_SIZE]);
    hdr[16..24].fill(0);
    let ck = fnv1a64(&hdr);
    buf[16..24].copy_from_slice(&ck.to_le_bytes());
    buf.extend_from_slice(sec);
    Ok(buf)
}

#[inline]
pub(crate) fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

pub(crate) fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn slice_as_bytes<T>(s: &[T]) -> &[u8] {
    // SAFETY: T is POD (DataLine / SuperblockPlain); LE host.
    unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u8, std::mem::size_of_val(s)) }
}

fn cast_slice<'a, T>(sec: &'a [u8], off: usize, n: usize) -> Result<&'a [T], MappedQwtError> {
    let need = n
        .checked_mul(std::mem::size_of::<T>())
        .ok_or(MappedQwtError::Layout("overflow"))?;
    if off.checked_add(need).map(|e| e > sec.len()).unwrap_or(true) {
        return Err(MappedQwtError::Layout("slice OOB"));
    }
    // Absolute pointer alignment (relative off%align is insufficient for
    // arbitrary Vec bases; AlignedBuf / page mmap guarantee it).
    let ptr = unsafe { sec.as_ptr().add(off) };
    if (ptr as usize) % std::mem::align_of::<T>() != 0 {
        return Err(MappedQwtError::Layout("align"));
    }
    // SAFETY: absolute alignment checked; T is POD; LE host; bounds checked.
    Ok(unsafe { std::slice::from_raw_parts(ptr as *const T, n) })
}

/// 64-byte-aligned owned byte buffer (DataLine / SuperblockPlain cast safe).
pub(crate) struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: AlignedBuf is an owned byte buffer; exclusive access via &/&mut.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    const ALIGN: usize = 64;

    pub fn from_slice(src: &[u8]) -> Self {
        use std::alloc::{Layout, alloc_zeroed, handle_alloc_error};
        let len = src.len();
        if len == 0 {
            // Non-null dangling-ish empty: allocate 64 B aligned zero page.
            let layout = Layout::from_size_align(Self::ALIGN, Self::ALIGN).unwrap();
            let ptr = unsafe { alloc_zeroed(layout) };
            if ptr.is_null() {
                handle_alloc_error(layout);
            }
            return Self { ptr, len: 0 };
        }
        let layout = Layout::from_size_align(len, Self::ALIGN).expect("layout");
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), ptr, len);
        }
        Self { ptr, len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        use std::alloc::{Layout, dealloc};
        let size = self.len.max(Self::ALIGN);
        let layout = Layout::from_size_align(size, Self::ALIGN).expect("layout");
        unsafe { dealloc(self.ptr, layout) };
    }
}

impl Clone for AlignedBuf {
    fn clone(&self) -> Self {
        Self::from_slice(self.as_slice())
    }
}

fn cast_u32_slice(sec: &[u8], off: usize, n: usize) -> Result<&[u32], MappedQwtError> {
    cast_slice::<u32>(sec, off, n)
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use qwt::{AccessUnsigned, QWT256, RankQuad};

    #[test]
    fn w0_roundtrip_access_and_rank_all() {
        let data: Vec<u32> = (0..10_000).map(|i| (i * 7 + 3) % 500).collect();
        let qwt = QWT256::from(data.clone());
        let image = write_novaqwt1_v1(&qwt).expect("write");
        assert!(image.len() >= PAGE_ALIGN);
        assert_eq!(&image[0..8], NOVAQWT1_MAGIC);

        let mapped = open_novaqwt1(&image).expect("open");
        assert_eq!(mapped.n, data.len());
        assert_eq!(mapped.n_levels, qwt.n_levels());

        // Differential access
        for i in (0..data.len()).step_by(97) {
            assert_eq!(mapped.get(i), qwt.get(i), "access @ {i}");
        }
        for i in 0..500 {
            assert_eq!(mapped.get(i), qwt.get(i));
        }

        // Level-local rank_all vs heap RSQVector
        let levels = qwt.levels();
        for level in 0..qwt.n_levels() {
            let heap = &levels[level];
            let nsym = heap.len();
            for &i in &[
                0usize,
                1,
                17,
                255,
                256,
                257,
                nsym / 2,
                nsym.saturating_sub(1),
                nsym,
            ] {
                if i > nsym {
                    continue;
                }
                let got = mapped.rank_all_level(level, i).expect("rank_all");
                let exp = unsafe { heap.rank_all_unchecked(i) };
                assert_eq!(got, exp, "rank_all level={level} i={i}");
                for b in 0..4u8 {
                    let g = mapped.rank_level(level, b, i).unwrap();
                    let e = unsafe { heap.rank_unchecked(b, i) };
                    assert_eq!(g, e, "rank level={level} b={b} i={i}");
                }
            }
        }

        assert!(mapped.image_bytes() > 0);
    }

    #[test]
    fn w0_empty() {
        let qwt = QWT256::<u32>::from(Vec::<u32>::new());
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();
        assert_eq!(mapped.n, 0);
        assert_eq!(mapped.n_levels, 0);
        assert_eq!(mapped.get(0), None);
        assert!(mapped.image_bytes() >= PAGE_ALIGN + 64);
    }

    #[test]
    fn w0_open_rejects_bad_magic() {
        let mut image = write_novaqwt1_v1(&QWT256::from(vec![1u32, 2, 3])).unwrap();
        image[0] = b'X';
        assert!(matches!(
            open_novaqwt1(&image),
            Err(MappedQwtError::BadMagic)
        ));
    }

    #[test]
    fn w0_open_rejects_bad_checksum() {
        let mut image = write_novaqwt1_v1(&QWT256::from(vec![1u32, 2, 3])).unwrap();
        image[16] ^= 0xff;
        assert!(matches!(
            open_novaqwt1(&image),
            Err(MappedQwtError::HeaderChecksum)
        ));
    }

    #[test]
    fn w0_header_only_open_no_payload_walk() {
        // Open path must succeed after validating header + section magic/dir
        // bounds only — proven by empty + non-empty opens above. This test
        // checks that section is page-aligned and open does not require
        // touching data lines (get is separate).
        let qwt = QWT256::from(vec![0u32; 1000]);
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let sec_off = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
        assert_eq!(sec_off % PAGE_ALIGN, 0);
        let mapped = open_novaqwt1(&image).unwrap();
        assert_eq!(mapped.n, 1000);
        // Touch only header-derived fields — no get/rank.
        assert!(mapped.n_levels > 0);
        assert_eq!(mapped.sigma, qwt.sigma_raw());
    }

    #[test]
    fn qwta_section_open_matches_owned() {
        let qwt = QWT256::from(vec![1u32, 2, 3, 4, 5]);
        let sec = build_qwta_section(&qwt).unwrap();
        // Standalone Vec is not 64-aligned; mirror product open via AlignedBuf.
        let aligned = AlignedBuf::from_slice(&sec);
        let view = MappedQwtSection::open(aligned.as_slice()).unwrap();
        assert_eq!(view.n, 5);
        assert_eq!(view.n_levels, qwt.n_levels());
        for i in 0..5 {
            assert_eq!(view.get(i), qwt.get(i));
        }
    }

    #[test]
    fn w2_full_symbol_rank_select_vs_heap() {
        use qwt::{RankUnsigned, SelectUnsigned};
        let data: Vec<u32> = (0..5_000).map(|i| ((i * 11 + 5) % 300) as u32).collect();
        let qwt = QWT256::from(data.clone());
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();

        // Rank differential
        for &sym in &[0u32, 1, 7, 50, 100, 299] {
            for &i in &[0usize, 1, 17, 256, 1000, 2500, 5000] {
                if i > data.len() {
                    continue;
                }
                assert_eq!(
                    mapped.rank(sym, i),
                    qwt.rank(sym, i),
                    "rank sym={sym} i={i}"
                );
            }
        }

        // Select differential (first few occurrences of a few symbols)
        for &sym in &[0u32, 1, 7, 50, 100] {
            let total = qwt.rank(sym, data.len()).unwrap_or(0);
            for occ in 0..total.min(8) {
                assert_eq!(
                    mapped.select(sym, occ),
                    qwt.select(sym, occ),
                    "select sym={sym} occ={occ}"
                );
            }
            // Past end → None
            assert_eq!(mapped.select(sym, total), None);
        }
    }

    #[test]
    fn w3_rnv_matches_heap() {
        use qwt::QWT256;
        let data: Vec<u32> = (0..3_000).map(|i| ((i * 13 + 7) % 200) as u32).collect();
        let qwt = QWT256::from(data.clone());
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();

        // Empty range
        assert_eq!(mapped.range_next_value(0..0, 0), None);
        assert_eq!(qwt.range_next_value(0..0, 0), None);

        // Full range, low/mid/high targets
        let full = 0..data.len();
        for &t in &[0u32, 1, 50, 100, 150, 199, 200, 999] {
            assert_eq!(
                mapped.range_next_value(full.clone(), t),
                qwt.range_next_value(full.clone(), t),
                "RNV full target={t}"
            );
        }

        // Singleton ranges
        for i in (0..data.len()).step_by(97) {
            let r = i..i + 1;
            for &t in &[0u32, data[i], data[i].saturating_add(1), 999] {
                assert_eq!(
                    mapped.range_next_value(r.clone(), t),
                    qwt.range_next_value(r.clone(), t),
                    "RNV singleton i={i} t={t}"
                );
            }
        }

        // Mid-range slices
        for &(s, e) in &[(0, 100), (500, 1500), (2000, 3000), (100, 101)] {
            let r = s..e;
            for &t in &[0u32, 25, 100, 199, 500] {
                assert_eq!(
                    mapped.range_next_value(r.clone(), t),
                    qwt.range_next_value(r.clone(), t),
                    "RNV {s}..{e} t={t}"
                );
            }
        }
    }

    #[test]
    fn w3_rdi_matches_heap_ordered() {
        use qwt::QWT256;
        // Disable C1 so counter identity vs heap RDI still holds.
        set_c1_batch_k(0);
        let data: Vec<u32> = (0..2_000).map(|i| ((i * 17 + 3) % 80) as u32).collect();
        let qwt = QWT256::from(data.clone());
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();

        for &(s, e) in &[(0, 2000), (0, 0), (10, 11), (100, 500), (1500, 2000)] {
            let mut heap_it = qwt.range_distinct_iter(s..e);
            let mut map_it = mapped.range_distinct_iter(s..e);
            let mut prev: Option<u32> = None;
            loop {
                let h = heap_it.next_symbol();
                let m = map_it.next_symbol();
                assert_eq!(m, h, "RDI mismatch range={s}..{e}");
                match m {
                    None => break,
                    Some((sym, _cnt)) => {
                        if let Some(p) = prev {
                            assert!(sym > p, "RDI not ordered: {p} then {sym}");
                        }
                        prev = Some(sym);
                    }
                }
            }
            // Counters that must match (algorithm identity) on generic path
            assert_eq!(map_it.rank_probes, heap_it.rank_probes, "rank_probes");
            assert_eq!(map_it.symbols_yielded, heap_it.symbols_yielded, "symbols");
            assert_eq!(
                map_it.empty_branches, heap_it.empty_branches,
                "empty_branches"
            );
            assert_eq!(map_it.frames_popped, heap_it.frames_popped, "frames");
            assert_eq!(map_it.children_pushed, heap_it.children_pushed, "children");
        }
        set_c1_batch_k(C1_K_DEFAULT);
    }

    /// E5.11 C1: short-range batch matches heap RDI symbols/counts; hits batch path.
    ///
    /// Uses `short_range_batch_decode` / explicit K (not process-wide atomic) for
    /// symbol identity so parallel tests cannot race the global `c1_batch_k`.
    #[test]
    fn c1_short_range_batch_matches_heap_rdi() {
        use qwt::QWT256;
        let data: Vec<u32> = (0..4_000).map(|i| ((i * 19 + 5) % 120) as u32).collect();
        let qwt = QWT256::from(data.clone());
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();
        let hot = mapped.hot();
        let k = 32usize;

        // Star-like short ranges (len ≤ 32): direct batch API (no global K race).
        for &(s, e) in &[(0, 28), (100, 128), (500, 528), (2000, 2030), (10, 11)] {
            let batch = hot
                .short_range_batch_decode(s..e, k)
                .expect("batch should accept short range");
            let mut heap_it = qwt.range_distinct_iter(s..e);
            let mut i = 0usize;
            loop {
                let h = heap_it.next_symbol();
                match h {
                    None => {
                        assert_eq!(i, batch.n_out, "batch n_out vs heap");
                        break;
                    }
                    Some((sym, cnt)) => {
                        assert!(i < batch.n_out, "batch shorter than heap");
                        assert_eq!(batch.symbols[i], sym, "C1 vs heap range={s}..{e}");
                        assert_eq!(batch.counts[i] as usize, cnt, "count {s}..{e} sym={sym}");
                        i += 1;
                    }
                }
            }
            assert!(batch.rows_decoded > 0);
        }

        // Long range rejected by batch API; RDI still matches heap.
        assert!(hot.short_range_batch_decode(0..2000, k).is_none());
        set_c1_batch_k(0); // force generic RDI for counter-safe path
        let mut map_long = mapped.range_distinct_iter(0..2000);
        let mut heap_long = qwt.range_distinct_iter(0..2000);
        loop {
            let h = heap_long.next_symbol();
            let m = map_long.next_symbol();
            assert_eq!(m, h);
            if m.is_none() {
                break;
            }
        }
        set_c1_batch_k(C1_K_DEFAULT);
    }

    /// E5.11 B2: fused expand algebraically matches two rank_all probes, and
    /// path classification matches block/superblock geometry.
    #[test]
    fn b2_range_counts4_matches_dual_rank_all() {
        let data: Vec<u32> = (0..5_000).map(|i| ((i * 19 + 11) % 400) as u32).collect();
        let qwt = QWT256::from(data.clone());
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();
        let sec = mapped.section_view();

        for level in 0..sec.n_levels {
            let lv = sec.level_view(level).unwrap();
            let n = lv.n_symbols;
            // Cover same-line, same-SB, and general geometries.
            let pairs: &[(usize, usize)] = &[
                (0, 0),
                (0, 1),
                (10, 28),     // same line (avg star ~28)
                (100, 200),   // same line
                (200, 300),   // may cross line boundary inside SB
                (250, 260),   // cross 256 boundary (same SB if both in 0..2048)
                (255, 256),   // line boundary
                (256, 512),   // two full blocks same SB
                (2000, 2100), // later SB
                (100, 3000),  // general (cross SB)
                (0, n),
                (n.saturating_sub(30), n),
            ];
            for &(s, e) in pairs {
                if s > n || e > n || s > e {
                    continue;
                }
                let exp = level_range_expand(&lv, s, e);
                let rs = level_rank_all(&lv, s);
                let re = level_rank_all(&lv, e);
                assert_eq!(exp.ranks_s, rs, "ranks_s level={level} {s}..{e}");
                for b in 0..4 {
                    assert_eq!(
                        exp.counts[b],
                        re[b] - rs[b],
                        "counts[{b}] level={level} {s}..{e}"
                    );
                }
                let block_s = s / BLOCK_SIZE;
                let block_e = e / BLOCK_SIZE;
                let expected_path = if block_s == block_e {
                    RangeCountsPath::SameLine
                } else if block_s / BLOCKS_IN_SUPERBLOCK == block_e / BLOCKS_IN_SUPERBLOCK {
                    RangeCountsPath::SameSuperblock
                } else {
                    RangeCountsPath::General
                };
                assert_eq!(exp.path, expected_path, "path level={level} {s}..{e}");
            }
        }
    }

    /// E5.11 D1: braided intersection matches dual-RNV leapfrog on random pairs.
    #[test]
    fn d1_intersection_next_value2_matches_dual_rnv() {
        let data: Vec<u32> = (0..5_000).map(|i| ((i * 23 + 11) % 300) as u32).collect();
        let qwt = QWT256::from(data.clone());
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();
        let hot = mapped.hot();
        let n = data.len();

        // Empty / OOB
        assert_eq!(hot.intersection_next_value2(0..0, 0..10, 0), None);
        assert_eq!(hot.intersection_next_value2(0..10, 0..0, 0), None);

        let pairs: &[(usize, usize, usize, usize)] = &[
            (0, n, 0, n),             // full ∩ full
            (0, 500, 250, 750),       // overlap
            (0, 100, 200, 300),       // disjoint ranges (may still share symbols)
            (1000, 1100, 1000, 1100), // identical short
            (10, 11, 10, 11),         // singleton same
            (10, 11, 20, 21),         // singleton different
            (0, 28, 100, 128),        // star-like short
            (4000, 5000, 0, 1000),
        ];
        for &(ls, le, rs, re) in pairs {
            for &t in &[0u32, 1, 50, 100, 150, 200, 299, 300, 999] {
                let braid = hot.intersection_next_value2(ls..le, rs..re, t);
                let dual = hot.intersection_next_value2_dual_rnv(ls..le, rs..re, t);
                assert_eq!(braid, dual, "D1 L={ls}..{le} R={rs}..{re} t={t}");
            }
            // Streaming: successive successors from 0 must match dual.
            let mut t = 0u32;
            let mut n_common = 0u32;
            while let Some(v) = hot.intersection_next_value2(ls..le, rs..re, t) {
                assert_eq!(
                    Some(v),
                    hot.intersection_next_value2_dual_rnv(ls..le, rs..re, t)
                );
                n_common += 1;
                t = v.saturating_add(1);
                if t == 0 {
                    break; // overflow guard
                }
            }
            let _ = n_common;
        }

        // Stats: full∩full target=0 should visit levels and often prune one-side.
        let (_v, st) = hot.intersection_next_value2_counted(0..n, 0..n, 0);
        assert_eq!(st.root_starts, 1);
        assert!(st.levels_visited > 0);
        assert!(st.expands >= 2);
    }

    /// E5.11 D2: braided three-range intersection matches the independent
    /// three-RNV leapfrog oracle, including successive streaming seeks.
    #[test]
    fn d2_intersection_next_value3_matches_dual_rnv() {
        let data: Vec<u32> = (0..6_000).map(|i| ((i * 37 + 5) % 420) as u32).collect();
        let qwt = QWT256::from(data.clone());
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();
        let hot = mapped.hot();
        let n = data.len();

        assert_eq!(hot.intersection_next_value3(0..0, 0..10, 0..10, 0), None);
        assert_eq!(hot.intersection_next_value3(0..10, 0..10, 0..0, 0), None);

        let triples: &[(usize, usize, usize, usize, usize, usize)] = &[
            (0, n, 0, n, 0, n),
            (0, 700, 250, 950, 500, 1_200),
            (0, 100, 200, 300, 400, 500),
            (1000, 1100, 1000, 1100, 1000, 1100),
            (10, 11, 10, 11, 10, 11),
            (10, 11, 20, 21, 30, 31),
            (4_500, 6_000, 0, 1_000, 2_000, 3_500),
        ];
        for &(as_, ae, bs, be, cs, ce) in triples {
            for &target in &[0u32, 1, 50, 100, 210, 419, 420, 999] {
                let braid = hot.intersection_next_value3(as_..ae, bs..be, cs..ce, target);
                let dual = hot.intersection_next_value3_dual_rnv(as_..ae, bs..be, cs..ce, target);
                assert_eq!(
                    braid, dual,
                    "D2 A={as_}..{ae} B={bs}..{be} C={cs}..{ce} t={target}"
                );
            }

            let mut target = 0u32;
            while let Some(value) = hot.intersection_next_value3(as_..ae, bs..be, cs..ce, target) {
                assert_eq!(
                    Some(value),
                    hot.intersection_next_value3_dual_rnv(as_..ae, bs..be, cs..ce, target,)
                );
                target = value.saturating_add(1);
                if target == 0 {
                    break;
                }
            }
        }
    }

    /// D3-B differential coverage: the persistent iterator must produce the
    /// same complete sorted stream as repeated D2 successors and the
    /// independent three-RNV oracle across empty, disjoint, singleton,
    /// dense/identical, boundary, and resumed traversals.
    #[test]
    fn d3_intersection_iter3_matches_repeated_successor_and_oracle() {
        let data: Vec<u32> = (0..4_096)
            .map(|i| ((i * 73 + i / 17 + 11) % 256) as u32)
            .collect();
        let qwt = QWT256::from(data);
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();
        let hot = mapped.hot();
        let n = mapped.n;

        fn repeated(
            hot: &HotQwtColumn,
            a: Range<usize>,
            b: Range<usize>,
            c: Range<usize>,
            target: u32,
        ) -> Vec<u32> {
            let mut out = Vec::new();
            let mut t = target;
            while let Some(v) = hot.intersection_next_value3(a.clone(), b.clone(), c.clone(), t) {
                out.push(v);
                if v == u32::MAX {
                    break;
                }
                t = v + 1;
            }
            out
        }

        let cases = [
            (0..0, 0..10, 0..10, 0u32),               // empty
            (0..32, 128..160, 256..288, 0),           // disjoint positions
            (10..11, 10..11, 10..11, 0),              // identical singleton
            (10..11, 11..12, 12..13, 0),              // disjoint singletons
            (0..n, 0..n, 0..n, 0),                    // dense identical
            (255..257, 511..513, 767..769, 0),        // line boundaries
            (2047..2049, 2303..2305, 3071..3073, 64), // SB boundaries + target
            (0..n, 512..3584, 1024..3072, 128),
        ];

        for (a, b, c, target) in cases {
            let expected = repeated(hot, a.clone(), b.clone(), c.clone(), target);
            let oracle = repeated_oracle(hot, a.clone(), b.clone(), c.clone(), target);
            assert_eq!(expected, oracle, "D2/oracle case {a:?} {b:?} {c:?}");
            let got: Vec<u32> = hot
                .intersection_iter3(a.clone(), b.clone(), c.clone(), target)
                .collect();
            assert_eq!(got, expected, "iterator case {a:?} {b:?} {c:?}");
            assert!(got.windows(2).all(|w| w[0] < w[1]));

            // Resume from a non-zero target and compare the suffix only.
            let resumed_target = target.max(37);
            let resumed_expected =
                repeated_oracle(hot, a.clone(), b.clone(), c.clone(), resumed_target);
            let resumed: Vec<u32> = hot.intersection_iter3(a, b, c, resumed_target).collect();
            assert_eq!(resumed, resumed_expected, "resume target={resumed_target}");
        }
    }

    /// White-box DFS-state regression for the path-stack invariant in
    /// [`IntersectionIter3::next`].
    ///
    /// The iterator keeps a single root-to-leaf path: after pushing a child it
    /// must re-enter the outer loop so the new top is processed. A plain
    /// `continue` of the parent mask loop would either pop the newly pushed
    /// child immediately (no first yield) or nest siblings as ancestors
    /// (corrupt parent resume).
    #[test]
    fn d3_intersection_iter3_stack_invariants_child_push_parent_resume() {
        // Two QWT levels → alphabet width 16. Identical ranges mean every
        // symbol 0..15 is common, so each 4-way frame has all children live.
        let data: Vec<u32> = (0..16).collect();
        let qwt = QWT256::from(data);
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();
        let hot = mapped.hot();
        assert_eq!(hot.n_levels, 2);

        let expected: Vec<u32> = {
            let mut out = Vec::new();
            let mut t = 0u32;
            while let Some(v) = hot.intersection_next_value3(0..16, 0..16, 0..16, t) {
                out.push(v);
                if v == u32::MAX {
                    break;
                }
                t = v + 1;
            }
            out
        };
        assert_eq!(expected, (0..16).collect::<Vec<_>>());

        let mut it = hot.intersection_iter3(0..16, 0..16, 0..16, 0);

        // ── First yield + child push ─────────────────────────────────────
        // Path after emitting leaf 0 must still hold root + level-1 parent.
        // Root still has siblings 1..3; the level-1 parent still has 1..3.
        assert_eq!(it.next(), Some(0));
        assert_eq!(it.sp, 2, "first yield must leave root and parent on path");
        assert_eq!(it.stack[0].remaining_child_mask, 0b1110);
        assert_eq!(it.stack[1].remaining_child_mask, 0b1110);
        assert_eq!(it.target, 1);

        // ── Post-yield continuation ──────────────────────────────────────
        // Next leaf is sibling 1 under the same level-1 parent.
        assert_eq!(it.next(), Some(1));
        assert_eq!(it.sp, 2, "post-yield must resume under the same parent");
        assert_eq!(it.stack[0].remaining_child_mask, 0b1110);
        assert_eq!(it.stack[1].remaining_child_mask, 0b1100);
        assert_eq!(it.target, 2);

        // ── Sibling traversal ────────────────────────────────────────────
        assert_eq!(it.next(), Some(2));
        assert_eq!(it.sp, 2);
        assert_eq!(it.stack[1].remaining_child_mask, 0b1000);
        assert_eq!(it.next(), Some(3));
        // After the last sibling leaf is emitted, the exhausted level-1 parent
        // remains on the path with an empty mask until the next next() pops it.
        assert_eq!(it.sp, 2, "exhausted parent stays until next resume");
        assert_eq!(it.stack[0].remaining_child_mask, 0b1110);
        assert_eq!(it.stack[1].remaining_child_mask, 0);

        // ── Parent resume ────────────────────────────────────────────────
        // Next call must pop the empty parent, then enter root child 1
        // (prefix 4..8) instead of losing or nesting that sibling.
        assert_eq!(it.next(), Some(4));
        assert_eq!(it.sp, 2, "parent resume must push a fresh level-1 frame");
        assert_eq!(it.stack[0].remaining_child_mask, 0b1100);
        assert_eq!(it.stack[1].remaining_child_mask, 0b1110);
        assert_eq!(it.stack[1].prefix, 4);

        // ── Continuation / exhaustion ────────────────────────────────────
        let mut rest = vec![4u32];
        while let Some(v) = it.next() {
            rest.push(v);
        }
        assert_eq!(rest, (4..16).collect::<Vec<_>>());
        assert_eq!(it.sp, 0);
        assert_eq!(it.next(), None);
        assert_eq!(it.next(), None);

        // Full stream must still match the working D2 successor.
        let full: Vec<u32> = hot.intersection_iter3(0..16, 0..16, 0..16, 0).collect();
        assert_eq!(full, expected);
    }

    /// E1.0: summary-gated successor matches D2 (no false negatives).
    #[test]
    fn e1_presence_summary_matches_d2() {
        let data: Vec<u32> = (0..6_000).map(|i| ((i * 37 + 5) % 420) as u32).collect();
        let qwt = QWT256::from(data.clone());
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();
        let hot = mapped.hot();
        let n = data.len();
        for &cell in &PresenceCellSize::ALL {
            let table = PresenceSummaryTable::build(hot, cell);
            assert!(table.bytes > 0);
            let triples: &[(usize, usize, usize, usize, usize, usize)] = &[
                (0, n, 0, n, 0, n),
                (0, 700, 250, 950, 500, 1_200),
                (1000, 1100, 1000, 1100, 1000, 1100),
                (4_500, 6_000, 0, 1_000, 2_000, 3_500),
                (10, 38, 100, 128, 200, 228), // star-like short (partial cells)
            ];
            for &(as_, ae, bs, be, cs, ce) in triples {
                let mut t = 0u32;
                loop {
                    let d2 = hot.intersection_next_value3(as_..ae, bs..be, cs..ce, t);
                    let (sm, st) =
                        hot.intersection_next_value3_summary(&table, as_..ae, bs..be, cs..ce, t);
                    assert_eq!(sm, d2, "E1-{} vs D2 {as_}..{ae} t={t}", cell.as_str());
                    // Rejects never exceed checks; avoided expands ≤ rejects.
                    assert!(st.summary_zero_rejects <= st.summary_checks);
                    assert_eq!(st.exact_expands_avoided, st.summary_zero_rejects);
                    match d2 {
                        None => break,
                        Some(v) => {
                            if v == u32::MAX {
                                break;
                            }
                            t = v + 1;
                        }
                    }
                }
            }
        }
    }

    /// D4-A/B/C: overlap, mask-first, and unique-load successors match D2.
    #[test]
    fn d4_expand3_overlap_and_fused_match_d2() {
        let data: Vec<u32> = (0..6_000).map(|i| ((i * 37 + 5) % 420) as u32).collect();
        let qwt = QWT256::from(data.clone());
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();
        let hot = mapped.hot();
        let n = data.len();
        let triples: &[(usize, usize, usize, usize, usize, usize)] = &[
            (0, n, 0, n, 0, n),
            (0, 700, 250, 950, 500, 1_200),
            (1000, 1100, 1000, 1100, 1000, 1100),
            (4_500, 6_000, 0, 1_000, 2_000, 3_500),
        ];
        let mut any_frames = false;
        let mut any_shared_fast = false;
        for &(as_, ae, bs, be, cs, ce) in triples {
            for &target in &[0u32, 50, 210, 419] {
                let d2 = hot.intersection_next_value3(as_..ae, bs..be, cs..ce, target);
                let (ov, ost) =
                    hot.intersection_next_value3_overlap(as_..ae, bs..be, cs..ce, target);
                let (fu, fst) = hot.intersection_next_value3_fused(as_..ae, bs..be, cs..ce, target);
                let (sh, sst) =
                    hot.intersection_next_value3_shared(as_..ae, bs..be, cs..ce, target);
                assert_eq!(ov, d2, "D4-A vs D2 {as_}..{ae}");
                assert_eq!(fu, d2, "D4-B vs D2 {as_}..{ae}");
                assert_eq!(sh, d2, "D4-C vs D2 {as_}..{ae}");
                if ost.frames > 0 {
                    any_frames = true;
                    assert_eq!(ost.expands_indep, ost.frames * 3);
                    assert!(ost.unique_lines >= 1);
                    assert!(ost.unique_sbs >= 1);
                    assert!(ost.line_loads_indep >= ost.unique_lines);
                    assert!(ost.sb_loads_indep >= ost.unique_sbs);
                }
                if fst.levels_visited > 0 {
                    assert_eq!(
                        fst.expands_full + fst.mask_pruned,
                        fst.levels_visited,
                        "every frame is full-expand or mask-pruned"
                    );
                }
                if sst.frames > 0 {
                    assert!(sst.unique_line_loads <= sst.logical_line_requests);
                    assert!(sst.unique_sb_loads <= sst.logical_sb_requests);
                    assert_eq!(
                        sst.line_loads_saved,
                        sst.logical_line_requests - sst.unique_line_loads
                    );
                    if sst.all_same_line_fast_hits > 0 {
                        any_shared_fast = true;
                    }
                }
            }
            let mut t = 0u32;
            loop {
                let d2 = hot.intersection_next_value3(as_..ae, bs..be, cs..ce, t);
                let (fu, _) = hot.intersection_next_value3_fused(as_..ae, bs..be, cs..ce, t);
                let (sh, _) = hot.intersection_next_value3_shared(as_..ae, bs..be, cs..ce, t);
                assert_eq!(fu, d2);
                assert_eq!(sh, d2, "D4-C stream vs D2");
                match d2 {
                    None => break,
                    Some(v) => {
                        if v == u32::MAX {
                            break;
                        }
                        t = v + 1;
                    }
                }
            }
        }
        assert!(any_frames, "expected some D4-A frames on dense triples");
        assert!(
            any_shared_fast,
            "expected some all-same-line expand3 fast hits"
        );
    }

    fn repeated_oracle(
        hot: &HotQwtColumn,
        a: Range<usize>,
        b: Range<usize>,
        c: Range<usize>,
        target: u32,
    ) -> Vec<u32> {
        let mut out = Vec::new();
        let mut t = target;
        while let Some(v) =
            hot.intersection_next_value3_dual_rnv(a.clone(), b.clone(), c.clone(), t)
        {
            out.push(v);
            if v == u32::MAX {
                break;
            }
            t = v + 1;
        }
        out
    }

    /// Short same-line ranges (star-like) should hit SameLine and keep RDI identity.
    #[test]
    fn b2_rdi_same_line_path_and_identity() {
        // Dense small alphabet → short distinct ranges under RDI expand.
        // Disable C1: full-range still uses generic RDI (len > K), but keep
        // counter identity checks clean.
        set_c1_batch_k(0);
        let data: Vec<u32> = (0..4_000).map(|i| ((i * 3) % 64) as u32).collect();
        let qwt = QWT256::from(data.clone());
        let image = write_novaqwt1_v1(&qwt).unwrap();
        let mapped = open_novaqwt1(&image).unwrap();

        // Full-range RDI exercises many short child ranges after first expand.
        let mut heap_it = qwt.range_distinct_iter(0..data.len());
        let mut map_it = mapped.range_distinct_iter(0..data.len());
        loop {
            let h = heap_it.next_symbol();
            let m = map_it.next_symbol();
            assert_eq!(m, h);
            if m.is_none() {
                break;
            }
        }
        assert_eq!(map_it.rank_probes, heap_it.rank_probes);
        assert!(
            map_it.range_counts_calls > 0,
            "expected range_counts4 calls"
        );
        // Same-line should dominate for short residual ranges.
        let total = map_it.same_line_hits + map_it.same_superblock_hits + map_it.general_hits;
        assert_eq!(total, map_it.range_counts_calls);
        assert!(
            map_it.same_line_hits > 0,
            "expected some SameLine hits on short-range corpus"
        );
        set_c1_batch_k(C1_K_DEFAULT);
    }
}
