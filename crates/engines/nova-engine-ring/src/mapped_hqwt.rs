//! mapped Huffman QWT section (`HQWA` outer + upstream `HQWB`) for **C_p only**.
//!
//! Feature-gated: `ring-huffman-cp`. Embeds a locally densified `HQWT256`
//! (alphabet `[0, np)`) plus Nova shared-alphabet metadata (`np`, `p_base`,
//! `local_to_shared`) around an upstream [`qwt::bytes`] `HQWB` blob.
//!
//! Hot path: access / full-symbol rank / select via code LUT + level rank.
//! RNV / RDI: O(σ_P) numeric scan (same as heap [`HuffColP`]) — **not** the
//! crate's `occs_range` / non-contiguous Huffman RNV.
//!
//! # Section layout (`HQWA` v2)
//!
//! ```text
//! [0..4)     magic `HQWA`
//! [4..8)     version u32 (=2)
//! [8..12)    np u32
//! [12..16)   p_base u32
//! [16..24)   off_hqwb u64   (64-aligned absolute offset of HQWB blob)
//! [24..32)   len_hqwb u64
//! [32..40)   off_shared_map u64  (np × u32 LE; 0 if np==0)
//! [40..64)   reserved
//! [off_shared_map..)  local→shared map
//! [off_hqwb..)        upstream HQWB container (HqwtView / hqwt256_to_bytes)
//! ```
//!
//! Open validates the outer header, then [`HqwtView::from_bytes`] on the HQWB
//! payload. Level POD comes from [`qwt::bytes::RSQVectorView`] accessors.

#[cfg(test)]
use crate::mapped_qwt::AlignedBuf;
use crate::mapped_qwt::{
    MAX_LEVELS, MappedLevel, MappedQwtError, PAGE_ALIGN, align_up, level_get, level_rank,
    level_select, map_qwt_layout,
};
use qwt::bytes::{HQWB_MAGIC, HqwtView, hqwt256_to_bytes};
use qwt::{AccessQuad, DataLine, HQWT256, RankQuad, SuperblockPlain};
use std::ops::Range;

/// HQWA section magic (Nova outer wrapper; payload is upstream `HQWB`).
pub const HQWA_MAGIC: &[u8; 4] = b"HQWA";
/// Outer wrapper format version (v2 = HQWB payload; v1 was hand-rolled levels).
pub const HQWA_VERSION: u32 = 2;
/// NOVARNG1 header flag: C_p section is HQWA (Huffman), not QWTA.
pub const RNG_FLAG_HUFF_CP: u32 = 1;

const SEC_HDR: usize = 64;

/// Zero-copy view of an HQWA section (borrowed from mmap / buffer).
pub struct MappedHqwtSection<'m> {
    sec: &'m [u8],
    pub n: usize,
    pub n_levels: usize,
    pub np: u32,
    pub p_base: u32,
    off_hqwb: usize,
    len_hqwb: usize,
    off_shared_map: usize,
}

/// Open-time hot view: level pointers + densified code LUTs (small; σ_P schema-sized).
pub struct HotHuffColumn {
    pub n: usize,
    pub n_levels: usize,
    pub np: u32,
    pub p_base: u32,
    /// local → shared (length np).
    local_to_shared: Vec<u32>,
    /// Prefix codes indexed by local symbol (len==0 ⇒ absent).
    encode: Vec<(u32, u32)>, // content, len
    /// codes_decode[bit_len] = sorted (content, local_symbol).
    decode: Vec<Vec<(u32, u32)>>,
    /// Per-level sequence lengths for early-leaf `get`.
    level_lens: Vec<usize>,
    levels: [HotHuffLevel; MAX_LEVELS],
}

#[derive(Clone, Copy)]
struct HotHuffLevel {
    data: *const DataLine,
    superblocks: *const SuperblockPlain,
    data_len: usize,
    super_len: usize,
    #[allow(dead_code)]
    qv_len: usize,
    occs: [usize; 5],
}

// SAFETY: aliases immutable image bytes + owned small LUTs.
unsafe impl Send for HotHuffColumn {}
unsafe impl Sync for HotHuffColumn {}

impl HotHuffLevel {
    const EMPTY: Self = Self {
        data: std::ptr::null(),
        superblocks: std::ptr::null(),
        data_len: 0,
        super_len: 0,
        qv_len: 0,
        occs: [0; 5],
    };
}

// ── build ────────────────────────────────────────────────────────────────────

/// Flatten densified `HQWT256` + shared-alphabet metadata into an HQWA section.
///
/// Emits Nova outer header + `local_to_shared` map + upstream `HQWB` via
/// [`hqwt256_to_bytes`]. Callers no longer touch Group B layout fields.
///
/// `local_to_shared` maps local symbol → shared-alphabet id (length = np).
/// Contiguous role-local: `local_to_shared[i] == p_base + i`.
pub fn build_hqwa_section(
    wt: &HQWT256<u32>,
    local_to_shared: &[u32],
) -> Result<Vec<u8>, MappedQwtError> {
    let np = local_to_shared.len() as u32;
    let p_base = local_to_shared.first().copied().unwrap_or(0);
    if !cfg!(target_endian = "little") {
        return Err(MappedQwtError::NotLittleEndian);
    }
    if wt.n_levels() > MAX_LEVELS {
        return Err(MappedQwtError::TooManyLevels(wt.n_levels()));
    }

    let hqwb = hqwt256_to_bytes(wt).map_err(map_qwt_layout)?;

    // Layout: header | shared_map | pad64 | HQWB
    let mut cur = SEC_HDR;
    let off_shared_map = if np == 0 {
        0usize
    } else {
        let o = cur;
        cur += (np as usize) * 4;
        o
    };
    cur = align_up(cur, 64);
    let off_hqwb = cur;
    let len_hqwb = hqwb.len();
    cur += len_hqwb;

    let mut sec = vec![0u8; cur.max(SEC_HDR)];
    sec[0..4].copy_from_slice(HQWA_MAGIC);
    sec[4..8].copy_from_slice(&HQWA_VERSION.to_le_bytes());
    sec[8..12].copy_from_slice(&np.to_le_bytes());
    sec[12..16].copy_from_slice(&p_base.to_le_bytes());
    sec[16..24].copy_from_slice(&(off_hqwb as u64).to_le_bytes());
    sec[24..32].copy_from_slice(&(len_hqwb as u64).to_le_bytes());
    sec[32..40].copy_from_slice(&(off_shared_map as u64).to_le_bytes());

    if np > 0 {
        for (i, &s) in local_to_shared.iter().enumerate() {
            let o = off_shared_map + i * 4;
            sec[o..o + 4].copy_from_slice(&s.to_le_bytes());
        }
    }
    sec[off_hqwb..off_hqwb + len_hqwb].copy_from_slice(&hqwb);

    let _ = (PAGE_ALIGN, HQWB_MAGIC); // page align is caller's responsibility
    Ok(sec)
}

// ── open ─────────────────────────────────────────────────────────────────────

impl<'m> MappedHqwtSection<'m> {
    /// Validate and open an HQWA v2 section (outer metadata + `HQWB` via [`HqwtView`]).
    ///
    /// The absolute base of `sec` must be 64-byte aligned so HQWB POD casts succeed.
    pub fn open(sec: &'m [u8]) -> Result<Self, MappedQwtError> {
        if sec.len() < SEC_HDR {
            return Err(MappedQwtError::Layout("HQWA short"));
        }
        if &sec[0..4] != HQWA_MAGIC {
            return Err(MappedQwtError::Layout("HQWA bad magic"));
        }
        let version = u32::from_le_bytes(sec[4..8].try_into().unwrap());
        if version != HQWA_VERSION {
            return Err(MappedQwtError::Layout("HQWA bad version (need v2/HQWB)"));
        }
        let np = u32::from_le_bytes(sec[8..12].try_into().unwrap());
        let p_base = u32::from_le_bytes(sec[12..16].try_into().unwrap());
        let off_hqwb = u64::from_le_bytes(sec[16..24].try_into().unwrap()) as usize;
        let len_hqwb = u64::from_le_bytes(sec[24..32].try_into().unwrap()) as usize;
        let off_shared_map = u64::from_le_bytes(sec[32..40].try_into().unwrap()) as usize;

        if off_hqwb
            .checked_add(len_hqwb)
            .map(|e| e > sec.len())
            .unwrap_or(true)
        {
            return Err(MappedQwtError::Layout("HQWB OOB"));
        }
        if np > 0 {
            let need = (np as usize)
                .checked_mul(4)
                .and_then(|b| off_shared_map.checked_add(b))
                .ok_or(MappedQwtError::Layout("shared map OOB"))?;
            if need > sec.len() {
                return Err(MappedQwtError::Layout("shared map OOB"));
            }
        }

        let hqwb = &sec[off_hqwb..off_hqwb + len_hqwb];
        let view = HqwtView::<u32, 256>::from_bytes(hqwb).map_err(map_qwt_layout)?;
        if view.n_levels() > MAX_LEVELS {
            return Err(MappedQwtError::TooManyLevels(view.n_levels()));
        }

        Ok(Self {
            sec,
            n: view.len(),
            n_levels: view.n_levels(),
            np,
            p_base,
            off_hqwb,
            len_hqwb,
            off_shared_map,
        })
    }

    #[inline]
    fn hqwt_view(&self) -> Result<HqwtView<'m, u32, 256>, MappedQwtError> {
        let hqwb = &self.sec[self.off_hqwb..self.off_hqwb + self.len_hqwb];
        HqwtView::<u32, 256>::from_bytes(hqwb).map_err(map_qwt_layout)
    }

    fn level_view(&self, level: usize) -> Result<MappedLevel<'m>, MappedQwtError> {
        if level >= self.n_levels {
            return Err(MappedQwtError::Layout("level OOB"));
        }
        let view = self.hqwt_view()?;
        let lv = &view.levels()[level];
        let data = lv.data_lines();
        let superblocks = lv.superblocks();
        let select_samples = [
            lv.select_samples(0),
            lv.select_samples(1),
            lv.select_samples(2),
            lv.select_samples(3),
        ];
        Ok(MappedLevel {
            data,
            superblocks,
            select_samples,
            qv_position_bits: lv.position_bits(),
            n_symbols: lv.len(),
        })
    }

    fn n_occs_smaller_level(&self, level: usize) -> Result<[u64; 5], MappedQwtError> {
        if level >= self.n_levels {
            return Err(MappedQwtError::Layout("level OOB"));
        }
        let view = self.hqwt_view()?;
        let o = view.levels()[level].n_occs_smaller();
        Ok([
            o[0] as u64,
            o[1] as u64,
            o[2] as u64,
            o[3] as u64,
            o[4] as u64,
        ])
    }

    /// Build open-time hot column from [`HqwtView`] POD + outer shared map.
    pub fn build_hot(&self) -> Result<HotHuffColumn, MappedQwtError> {
        let view = self.hqwt_view()?;
        let mut levels = [HotHuffLevel::EMPTY; MAX_LEVELS];
        for (level, slot) in levels.iter_mut().enumerate().take(self.n_levels) {
            let lv = &view.levels()[level];
            let data = lv.data_lines();
            let superblocks = lv.superblocks();
            *slot = HotHuffLevel {
                data: data.as_ptr(),
                superblocks: superblocks.as_ptr(),
                data_len: data.len(),
                super_len: superblocks.len(),
                qv_len: lv.len(),
                occs: lv.n_occs_smaller(),
            };
        }

        let encode: Vec<(u32, u32)> = view
            .codes_encode()
            .iter()
            .map(|pc| (pc.content, pc.len))
            .collect();
        let decode: Vec<Vec<(u32, u32)>> = view
            .codes_decode()
            .iter()
            .map(|row| row.iter().map(|&(c, s)| (c, s)).collect())
            .collect();
        let level_lens = view.level_lens().to_vec();

        let mut local_to_shared = Vec::with_capacity(self.np as usize);
        if self.np > 0 && self.off_shared_map > 0 {
            for i in 0..self.np as usize {
                let o = self.off_shared_map + i * 4;
                if o + 4 <= self.sec.len() {
                    local_to_shared
                        .push(u32::from_le_bytes(self.sec[o..o + 4].try_into().unwrap()));
                }
            }
        }
        // Backward-compat / empty map → contiguous [p_base, p_base+np).
        if local_to_shared.len() != self.np as usize {
            local_to_shared = (0..self.np).map(|i| self.p_base + i).collect();
        }

        Ok(HotHuffColumn {
            n: self.n,
            n_levels: self.n_levels,
            np: self.np,
            p_base: self.p_base,
            local_to_shared,
            encode,
            decode,
            level_lens,
            levels,
        })
    }

    fn level_len(&self, level: usize) -> usize {
        self.hqwt_view()
            .ok()
            .and_then(|v| v.level_lens().get(level).copied())
            .unwrap_or(0)
    }

    fn encode_at(&self, local: usize) -> (u32, u32) {
        let Ok(view) = self.hqwt_view() else {
            return (0, 0);
        };
        match view.codes_encode().get(local) {
            Some(pc) => (pc.content, pc.len),
            None => (0, 0),
        }
    }

    fn decode_lookup(&self, bit_len: usize, content: u32) -> Option<u32> {
        let view = self.hqwt_view().ok()?;
        let row = view.codes_decode().get(bit_len)?;
        // binary search pairs (content, symbol)
        let mut lo = 0usize;
        let mut hi = row.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let c = row[mid].0;
            if c < content {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < row.len() && row[lo].0 == content {
            Some(row[lo].1)
        } else {
            None
        }
    }

    /// Local-alphabet get (no p_base).
    pub fn get_local(&self, i: usize) -> Option<u32> {
        if i >= self.n {
            return None;
        }
        if self.n_levels == 0 {
            return Some(0);
        }
        let mut cur_i = i;
        let mut result: u32 = 0;
        let mut shift = 0u32;
        for level in 0..self.n_levels {
            if cur_i >= self.level_len(level) {
                break;
            }
            let lv = self.level_view(level).ok()?;
            let symbol = level_get(&lv, cur_i);
            result = (result << 2) | symbol as u32;
            let occs = self.n_occs_smaller_level(level).ok()?;
            let offset = occs[symbol as usize] as usize;
            cur_i = level_rank(&lv, symbol, cur_i) + offset;
            shift += 2;
        }
        self.decode_lookup(shift as usize, result)
    }

    pub fn get_shared(&self, i: usize) -> Option<u32> {
        let l = self.get_local(i)?;
        if self.off_shared_map > 0 && (l as usize) < self.np as usize {
            let o = self.off_shared_map + (l as usize) * 4;
            if o + 4 <= self.sec.len() {
                return Some(u32::from_le_bytes(self.sec[o..o + 4].try_into().unwrap()));
            }
        }
        Some(l + self.p_base)
    }

    pub fn rank_local(&self, local: u32, i: usize) -> Option<usize> {
        if i > self.n {
            return None;
        }
        let (content, len) = self.encode_at(local as usize);
        if len == 0 {
            return None;
        }
        Some(self.rank_code(content, len, i))
    }

    fn shared_to_local_sec(&self, symbol: u32) -> Option<u32> {
        if self.np == 0 {
            return None;
        }
        if self.off_shared_map > 0 {
            // linear/binary over map
            let mut lo = 0usize;
            let mut hi = self.np as usize;
            while lo < hi {
                let mid = (lo + hi) / 2;
                let o = self.off_shared_map + mid * 4;
                let s = u32::from_le_bytes(self.sec[o..o + 4].try_into().unwrap());
                if s < symbol {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            if lo < self.np as usize {
                let o = self.off_shared_map + lo * 4;
                let s = u32::from_le_bytes(self.sec[o..o + 4].try_into().unwrap());
                if s == symbol {
                    return Some(lo as u32);
                }
            }
            return None;
        }
        if symbol < self.p_base || symbol >= self.p_base + self.np {
            None
        } else {
            Some(symbol - self.p_base)
        }
    }

    pub fn rank_shared(&self, symbol: u32, i: usize) -> Option<usize> {
        if i > self.n {
            return None;
        }
        let Some(local) = self.shared_to_local_sec(symbol) else {
            return Some(0);
        };
        match self.rank_local(local, i) {
            Some(r) => Some(r),
            None => Some(0), // absent code
        }
    }

    fn rank_code(&self, content: u32, code_len: u32, i: usize) -> usize {
        let mut cur_i = i;
        let mut cur_p = 0usize;
        let mut shift: i64 = code_len as i64 - 2;
        let mut level = 0usize;
        while shift >= 0 {
            let two_bits = ((content >> shift as usize) & 3) as u8;
            let lv = self.level_view(level).expect("level");
            let occs = self.n_occs_smaller_level(level).expect("occs");
            let offset = occs[two_bits as usize] as usize;
            cur_p = level_rank(&lv, two_bits, cur_p) + offset;
            cur_i = level_rank(&lv, two_bits, cur_i) + offset;
            level += 1;
            shift -= 2;
        }
        cur_i - cur_p
    }

    pub fn select_local(&self, local: u32, occurrence: usize) -> Option<usize> {
        let (content, len) = self.encode_at(local as usize);
        if len == 0 {
            return None;
        }
        let mut path_off = [0usize; MAX_LEVELS];
        let mut rank_path_off = [0usize; MAX_LEVELS];
        let mut shift: i64 = len as i64 - 2;
        let mut b = 0usize;
        let mut level = 0usize;
        while shift >= 0 {
            path_off[level] = b;
            let two_bits = ((content >> shift as usize) & 3) as u8;
            let lv = self.level_view(level).ok()?;
            if b > lv.n_symbols {
                return None;
            }
            let rank_b = level_rank(&lv, two_bits, b);
            let occs = self.n_occs_smaller_level(level).ok()?;
            b = rank_b + occs[two_bits as usize] as usize;
            rank_path_off[level] = rank_b;
            level += 1;
            shift -= 2;
        }
        shift = 0;
        let mut result = occurrence;
        for lvl in (0..level).rev() {
            b = path_off[lvl];
            let rank_b = rank_path_off[lvl];
            let two_bits = ((content >> shift as usize) & 3) as u8;
            let lv = self.level_view(lvl).ok()?;
            let pos = level_select(&lv, two_bits, rank_b + result)?;
            result = pos - b;
            shift += 2;
        }
        Some(result)
    }

    pub fn select_shared(&self, symbol: u32, occurrence: usize) -> Option<usize> {
        let local = self.shared_to_local_sec(symbol)?;
        self.select_local(local, occurrence)
    }
}

impl HotHuffColumn {
    #[inline]
    fn level(&self, i: usize) -> &HotHuffLevel {
        &self.levels[i]
    }

    #[inline]
    fn hot_get(lv: &HotHuffLevel, i: usize) -> u8 {
        let line = i >> 8;
        let pos = i & 255;
        debug_assert!(line < lv.data_len);
        unsafe { (*lv.data.add(line)).get_unchecked(pos) }
    }

    #[inline]
    fn hot_rank(lv: &HotHuffLevel, symbol: u8, i: usize) -> usize {
        // Mirror mapped_qwt::hot_level_rank arithmetic
        const BLOCK_SIZE: usize = 256;
        const BLOCKS_IN_SUPERBLOCK: usize = 8;
        let block = i / BLOCK_SIZE;
        let sb_idx = block / BLOCKS_IN_SUPERBLOCK;
        let block_in_sb = block % BLOCKS_IN_SUPERBLOCK;
        let block_rank = if sb_idx < lv.super_len {
            unsafe { (*lv.superblocks.add(sb_idx)).get_rank(symbol, block_in_sb) }
        } else {
            0
        };
        let data_line_id = i >> 8;
        let offset = i & 255;
        let intra = if data_line_id < lv.data_len {
            unsafe { (*lv.data.add(data_line_id)).rank_unchecked(symbol, offset) }
        } else {
            0
        };
        block_rank + intra
    }

    /// Shared-alphabet access.
    #[inline]
    pub fn get(&self, i: usize) -> Option<u32> {
        if i >= self.n {
            return None;
        }
        if self.n_levels == 0 {
            return Some(self.p_base);
        }
        let mut cur_i = i;
        let mut result: u32 = 0;
        let mut shift = 0u32;
        for level in 0..self.n_levels {
            if cur_i >= self.level_lens[level] {
                break;
            }
            let lv = self.level(level);
            let symbol = Self::hot_get(lv, cur_i);
            result = (result << 2) | symbol as u32;
            let offset = lv.occs[symbol as usize];
            cur_i = Self::hot_rank(lv, symbol, cur_i) + offset;
            shift += 2;
        }
        let row = self.decode.get(shift as usize)?;
        let idx = row.binary_search_by_key(&result, |(c, _)| *c).ok()?;
        let local = row[idx].1;
        Some(self.local_to_shared[local as usize])
    }

    /// Shared-alphabet rank in `[0, i)`.
    #[inline]
    pub fn rank(&self, symbol: u32, i: usize) -> Option<usize> {
        if i > self.n {
            return None;
        }
        let local = match self.local_to_shared.binary_search(&symbol) {
            Ok(i) => i,
            Err(_) => return Some(0),
        };
        if local >= self.encode.len() {
            return Some(0);
        }
        let (content, len) = self.encode[local];
        if len == 0 {
            return Some(0);
        }
        Some(self.rank_code(content, len, i))
    }

    #[inline]
    fn rank_code(&self, content: u32, code_len: u32, i: usize) -> usize {
        let mut cur_i = i;
        let mut cur_p = 0usize;
        let mut shift: i64 = code_len as i64 - 2;
        let mut level = 0usize;
        while shift >= 0 {
            let two_bits = ((content >> shift as usize) & 3) as u8;
            let lv = self.level(level);
            let offset = lv.occs[two_bits as usize];
            cur_p = Self::hot_rank(lv, two_bits, cur_p) + offset;
            cur_i = Self::hot_rank(lv, two_bits, cur_i) + offset;
            level += 1;
            shift -= 2;
        }
        cur_i - cur_p
    }

    /// O(σ_P) numeric RNV in shared coordinates (same semantics as HuffColP).
    pub fn range_next_value_scan(&self, range: Range<usize>, target: u32) -> Option<u32> {
        if range.start >= range.end || range.end > self.n || self.np == 0 {
            return None;
        }
        let local_start = self.local_to_shared.partition_point(|&s| s < target) as u32;
        for local in local_start..self.np {
            let shared = self.local_to_shared[local as usize];
            let lo = self.rank(shared, range.start).unwrap_or(0);
            let hi = self.rank(shared, range.end).unwrap_or(0);
            if hi > lo {
                return Some(shared);
            }
        }
        None
    }

    /// O(σ_P) RDI pairs in shared coordinates, numeric ascending.
    pub fn range_distinct_scan(&self, range: Range<usize>) -> Vec<(u32, u32)> {
        if range.start >= range.end || range.end > self.n {
            return Vec::new();
        }
        let mut out = Vec::new();
        for local in 0..self.np {
            let shared = self.local_to_shared[local as usize];
            let lo = self.rank(shared, range.start).unwrap_or(0);
            let hi = self.rank(shared, range.end).unwrap_or(0);
            if hi > lo {
                out.push((shared, (hi - lo) as u32));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qwt::RankUnsigned;

    #[test]
    fn hqwa_v2_roundtrip_vs_heap() {
        // Local densified alphabet 0..4 with shared base 10.
        let data: Vec<u32> = (0..2_000).map(|i| (i % 5) as u32).collect();
        let wt = HQWT256::from(data.clone());
        let map: Vec<u32> = (0..5).map(|i| 10 + i).collect();
        let sec = build_hqwa_section(&wt, &map).expect("build");
        assert_eq!(&sec[0..4], HQWA_MAGIC);
        assert_eq!(
            u32::from_le_bytes(sec[4..8].try_into().unwrap()),
            HQWA_VERSION
        );
        let aligned = AlignedBuf::from_slice(&sec);
        let view = MappedHqwtSection::open(aligned.as_slice()).expect("open");
        assert_eq!(view.n, data.len());
        assert_eq!(view.np, 5);
        assert_eq!(view.p_base, 10);
        let hot = view.build_hot().expect("hot");
        for i in (0..data.len()).step_by(97) {
            assert_eq!(hot.get(i), Some(10 + data[i]), "get {i}");
        }
        for &sym in &[10u32, 12, 14] {
            for &i in &[0usize, 100, 1000, data.len()] {
                // densified heap ranks are local; compare via local rank
                let local = sym - 10;
                assert_eq!(
                    hot.rank(sym, i),
                    wt.rank(local, i).or(Some(0)),
                    "rank shared={sym} i={i}"
                );
            }
        }
    }

    #[test]
    fn hqwa_empty() {
        let wt = HQWT256::<u32>::from(Vec::<u32>::new());
        let sec = build_hqwa_section(&wt, &[]).unwrap();
        let aligned = AlignedBuf::from_slice(&sec);
        let view = MappedHqwtSection::open(aligned.as_slice()).unwrap();
        assert_eq!(view.n, 0);
        assert_eq!(view.np, 0);
        let hot = view.build_hot().unwrap();
        assert_eq!(hot.get(0), None);
    }
}
