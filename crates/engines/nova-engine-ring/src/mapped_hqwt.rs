//! E5.9B Phase 4 — mapped Huffman QWT section (`HQWA`) for **C_p only**.
//!
//! Feature-gated: `ring-huffman-cp`. Embeds a locally densified `HQWT256`
//! (alphabet `[0, np)`) plus `p_base` into a page-aligned section used as the
//! C_p payload of `NOVARNG1` v2 (flag bit 0 = Huffman C_p).
//!
//! Level payloads reuse the same RSQ256 layout as QWTA (DataLine /
//! SuperblockPlain / select samples / occs). Code tables are dense LE arrays.
//!
//! Hot path: access / full-symbol rank / select via code LUT + level rank.
//! RNV / RDI: O(σ_P) numeric scan (same as heap [`HuffColP`]) — **not** the
//! crate's `occs_range` / non-contiguous Huffman RNV.
//!
//! # Section layout (`HQWA` v1)
//!
//! ```text
//! [0..4)     magic `HQWA`
//! [4..12)    n u64
//! [12..14)   n_levels u16
//! [14..18)   np u32            (local alphabet size)
//! [18..22)   p_base u32        (shared-alphabet offset)
//! [22..24)   encode_len u16    (= codes_encode.len(), typically np)
//! [24..26)   decode_bit_rows u16  (= codes_decode.len() = max_code_len+1)
//! [26..28)   reserved u16
//! [28..32)   reserved u32
//! [32..40)   off_encode u64
//! [40..48)   off_decode_dir u64
//! [48..56)   off_level_lens u64   (n_levels × u64)
//! [56..64)   reserved
//! [64..)     level dir: n_levels × 128 B  (identical to QWTA LEVEL_DIR_V1)
//! …          payloads (64-aligned): encode, decode dir+pairs, level_lens,
//!            per-level data/super/occs/select
//! ```
//!
//! Encode entry (8 B): `content u32` + `len u32` (PrefixCode).
//! Decode dir entry (16 B): `off u64` + `n_pairs u64` (pairs are content,u32 + symbol,u32).

#[cfg(test)]
use crate::mapped_qwt::AlignedBuf;
use crate::mapped_qwt::{
    MAX_LEVELS, MappedLevel, MappedQwtError, PAGE_ALIGN, align_up, level_get, level_rank,
    level_select,
};
use qwt::{AccessQuad, DataLine, HQWT256, RankQuad, SuperblockPlain};
use std::ops::Range;

/// HQWA section magic.
pub const HQWA_MAGIC: &[u8; 4] = b"HQWA";
/// NOVARNG1 header flag: C_p section is HQWA (Huffman), not QWTA.
pub const RNG_FLAG_HUFF_CP: u32 = 1;

const SEC_HDR: usize = 64;
const LEVEL_DIR: usize = 128;
const ENCODE_ENTRY: usize = 8;
const DECODE_DIR_ENTRY: usize = 16;
const PAIR_ENTRY: usize = 8;

/// Zero-copy view of an HQWA section (borrowed from mmap / buffer).
pub struct MappedHqwtSection<'m> {
    sec: &'m [u8],
    pub n: usize,
    pub n_levels: usize,
    pub np: u32,
    pub p_base: u32,
    encode_len: usize,
    decode_bit_rows: usize,
    off_encode: usize,
    off_decode_dir: usize,
    off_level_lens: usize,
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
    let n = wt.len();
    let n_levels = wt.n_levels();
    if n_levels > MAX_LEVELS {
        return Err(MappedQwtError::TooManyLevels(n_levels));
    }

    let codes_enc = wt.codes_encode();
    let codes_dec = wt.codes_decode();
    let level_lens = wt.level_lens();
    let levels = wt.levels();

    // Empty / trivial
    if n == 0 {
        let mut sec = vec![0u8; SEC_HDR];
        sec[0..4].copy_from_slice(HQWA_MAGIC);
        sec[14..18].copy_from_slice(&np.to_le_bytes());
        sec[18..22].copy_from_slice(&p_base.to_le_bytes());
        return Ok(sec);
    }

    debug_assert_eq!(levels.len(), n_levels);
    debug_assert_eq!(level_lens.len(), n_levels);

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
    for lv in levels {
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

    let encode_len = codes_enc.len();
    let decode_bit_rows = codes_dec.len();
    let encode_bytes = encode_len * ENCODE_ENTRY;
    let decode_dir_bytes = decode_bit_rows * DECODE_DIR_ENTRY;
    let mut decode_pair_bytes = 0usize;
    for row in codes_dec {
        decode_pair_bytes += row.len() * PAIR_ENTRY;
    }
    let level_lens_bytes = n_levels * 8;
    let shared_map_bytes = (np as usize) * 4;

    let dir_sz = n_levels * LEVEL_DIR;
    let mut cur = align_up(SEC_HDR + dir_sz, 64);

    let off_encode = cur;
    cur += encode_bytes;
    cur = align_up(cur, 8);
    let off_decode_dir = cur;
    cur += decode_dir_bytes;
    cur = align_up(cur, 8);
    let off_decode_pairs = cur;
    cur += decode_pair_bytes;
    cur = align_up(cur, 8);
    let off_level_lens = cur;
    cur += level_lens_bytes;
    cur = align_up(cur, 4);
    let off_shared_map = cur;
    cur += shared_map_bytes;
    cur = align_up(cur, 64);

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
        cur += 40;
        cur = align_up(cur, 4);
        let mut sel = [0usize; 4];
        for (s, sel_off) in sel.iter_mut().enumerate() {
            *sel_off = cur;
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

    let mut sec = vec![0u8; cur.max(SEC_HDR)];
    sec[0..4].copy_from_slice(HQWA_MAGIC);
    sec[4..12].copy_from_slice(&(n as u64).to_le_bytes());
    sec[12..14].copy_from_slice(&(n_levels as u16).to_le_bytes());
    sec[14..18].copy_from_slice(&np.to_le_bytes());
    sec[18..22].copy_from_slice(&p_base.to_le_bytes());
    sec[22..24].copy_from_slice(&(encode_len as u16).to_le_bytes());
    sec[24..26].copy_from_slice(&(decode_bit_rows as u16).to_le_bytes());
    sec[32..40].copy_from_slice(&(off_encode as u64).to_le_bytes());
    sec[40..48].copy_from_slice(&(off_decode_dir as u64).to_le_bytes());
    sec[48..56].copy_from_slice(&(off_level_lens as u64).to_le_bytes());
    sec[56..64].copy_from_slice(&(off_shared_map as u64).to_le_bytes());

    // Encode LUT
    for (i, pc) in codes_enc.iter().enumerate() {
        let o = off_encode + i * ENCODE_ENTRY;
        sec[o..o + 4].copy_from_slice(&pc.content.to_le_bytes());
        sec[o + 4..o + 8].copy_from_slice(&pc.len.to_le_bytes());
    }

    // Decode dir + pairs
    let mut pair_cur = off_decode_pairs;
    for (bi, row) in codes_dec.iter().enumerate() {
        let dbase = off_decode_dir + bi * DECODE_DIR_ENTRY;
        sec[dbase..dbase + 8].copy_from_slice(&(pair_cur as u64).to_le_bytes());
        sec[dbase + 8..dbase + 16].copy_from_slice(&(row.len() as u64).to_le_bytes());
        for &(content, sym) in row {
            // HQWT256<u32>: symbol is already u32
            sec[pair_cur..pair_cur + 4].copy_from_slice(&content.to_le_bytes());
            sec[pair_cur + 4..pair_cur + 8].copy_from_slice(&sym.to_le_bytes());
            pair_cur += PAIR_ENTRY;
        }
    }

    // Level lens
    for (i, &len) in level_lens.iter().enumerate() {
        let o = off_level_lens + i * 8;
        sec[o..o + 8].copy_from_slice(&(len as u64).to_le_bytes());
    }

    // Shared-alphabet map (local → shared)
    for (i, &s) in local_to_shared.iter().enumerate() {
        let o = off_shared_map + i * 4;
        sec[o..o + 4].copy_from_slice(&s.to_le_bytes());
    }

    // Level dir + payloads
    for (li, lp) in lps.iter().enumerate() {
        let base = SEC_HDR + li * LEVEL_DIR;
        let o = &offs[li];
        let mut e = [0u8; LEVEL_DIR];
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
        sec[base..base + LEVEL_DIR].copy_from_slice(&e);

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

    let _ = PAGE_ALIGN; // documented page align is caller's responsibility
    Ok(sec)
}

// ── open ─────────────────────────────────────────────────────────────────────

impl<'m> MappedHqwtSection<'m> {
    pub fn open(sec: &'m [u8]) -> Result<Self, MappedQwtError> {
        if sec.len() < SEC_HDR {
            return Err(MappedQwtError::Layout("HQWA short"));
        }
        if &sec[0..4] != HQWA_MAGIC {
            return Err(MappedQwtError::Layout("HQWA bad magic"));
        }
        let n = u64::from_le_bytes(sec[4..12].try_into().unwrap()) as usize;
        let n_levels = u16::from_le_bytes(sec[12..14].try_into().unwrap()) as usize;
        let np = u32::from_le_bytes(sec[14..18].try_into().unwrap());
        let p_base = u32::from_le_bytes(sec[18..22].try_into().unwrap());
        let encode_len = u16::from_le_bytes(sec[22..24].try_into().unwrap()) as usize;
        let decode_bit_rows = u16::from_le_bytes(sec[24..26].try_into().unwrap()) as usize;
        if n_levels > MAX_LEVELS {
            return Err(MappedQwtError::TooManyLevels(n_levels));
        }
        let off_encode = u64::from_le_bytes(sec[32..40].try_into().unwrap()) as usize;
        let off_decode_dir = u64::from_le_bytes(sec[40..48].try_into().unwrap()) as usize;
        let off_level_lens = u64::from_le_bytes(sec[48..56].try_into().unwrap()) as usize;
        let off_shared_map = u64::from_le_bytes(sec[56..64].try_into().unwrap()) as usize;

        if n > 0 {
            let need_enc = off_encode
                .checked_add(encode_len * ENCODE_ENTRY)
                .ok_or(MappedQwtError::Layout("encode OOB"))?;
            if need_enc > sec.len() {
                return Err(MappedQwtError::Layout("encode OOB"));
            }
            let need_dd = off_decode_dir
                .checked_add(decode_bit_rows * DECODE_DIR_ENTRY)
                .ok_or(MappedQwtError::Layout("decode dir OOB"))?;
            if need_dd > sec.len() {
                return Err(MappedQwtError::Layout("decode dir OOB"));
            }
            let need_ll = off_level_lens
                .checked_add(n_levels * 8)
                .ok_or(MappedQwtError::Layout("level_lens OOB"))?;
            if need_ll > sec.len() {
                return Err(MappedQwtError::Layout("level_lens OOB"));
            }
            // Touch each level dir entry bounds via level_view.
            let tmp = Self {
                sec,
                n,
                n_levels,
                np,
                p_base,
                encode_len,
                decode_bit_rows,
                off_encode,
                off_decode_dir,
                off_level_lens,
                off_shared_map,
            };
            for li in 0..n_levels {
                let _ = tmp.level_view(li)?;
            }
        }

        Ok(Self {
            sec,
            n,
            n_levels,
            np,
            p_base,
            encode_len,
            decode_bit_rows,
            off_encode,
            off_decode_dir,
            off_level_lens,
            off_shared_map,
        })
    }

    fn level_view(&self, level: usize) -> Result<MappedLevel<'m>, MappedQwtError> {
        if level >= self.n_levels {
            return Err(MappedQwtError::Layout("level OOB"));
        }
        let base = SEC_HDR + level * LEVEL_DIR;
        if base + LEVEL_DIR > self.sec.len() {
            return Err(MappedQwtError::Layout("level dir OOB"));
        }
        let e = &self.sec[base..base + LEVEL_DIR];
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
        let _ = off_occs; // occs read separately

        let data = cast_slice::<DataLine>(self.sec, off_data, n_data)?;
        let superblocks = cast_slice::<SuperblockPlain>(self.sec, off_super, n_super)?;
        let mut select_samples: [&[u32]; 4] = [&[]; 4];
        for s in 0..4 {
            select_samples[s] = cast_slice::<u32>(self.sec, off_sel[s], n_sel[s])?;
        }
        Ok(MappedLevel {
            data,
            superblocks,
            select_samples,
            qv_position_bits: qv_pos,
            n_symbols: qv_pos / 2,
        })
    }

    fn n_occs_smaller_level(&self, level: usize) -> Result<[u64; 5], MappedQwtError> {
        let base = SEC_HDR + level * LEVEL_DIR;
        let e = &self.sec[base..base + LEVEL_DIR];
        let off_occs = u64::from_le_bytes(e[88..96].try_into().unwrap()) as usize;
        if off_occs + 40 > self.sec.len() {
            return Err(MappedQwtError::Layout("occs OOB"));
        }
        let mut o = [0u64; 5];
        for (i, slot) in o.iter_mut().enumerate() {
            *slot = u64::from_le_bytes(
                self.sec[off_occs + i * 8..off_occs + i * 8 + 8]
                    .try_into()
                    .unwrap(),
            );
        }
        Ok(o)
    }

    fn level_len(&self, level: usize) -> usize {
        let o = self.off_level_lens + level * 8;
        u64::from_le_bytes(self.sec[o..o + 8].try_into().unwrap()) as usize
    }

    fn encode_at(&self, local: usize) -> (u32, u32) {
        if local >= self.encode_len {
            return (0, 0);
        }
        let o = self.off_encode + local * ENCODE_ENTRY;
        let content = u32::from_le_bytes(self.sec[o..o + 4].try_into().unwrap());
        let len = u32::from_le_bytes(self.sec[o + 4..o + 8].try_into().unwrap());
        (content, len)
    }

    fn decode_lookup(&self, bit_len: usize, content: u32) -> Option<u32> {
        if bit_len >= self.decode_bit_rows {
            return None;
        }
        let dbase = self.off_decode_dir + bit_len * DECODE_DIR_ENTRY;
        let off = u64::from_le_bytes(self.sec[dbase..dbase + 8].try_into().unwrap()) as usize;
        let n = u64::from_le_bytes(self.sec[dbase + 8..dbase + 16].try_into().unwrap()) as usize;
        // binary search pairs
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let p = off + mid * PAIR_ENTRY;
            let c = u32::from_le_bytes(self.sec[p..p + 4].try_into().unwrap());
            if c < content {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < n {
            let p = off + lo * PAIR_ENTRY;
            let c = u32::from_le_bytes(self.sec[p..p + 4].try_into().unwrap());
            if c == content {
                let sym = u32::from_le_bytes(self.sec[p + 4..p + 8].try_into().unwrap());
                return Some(sym);
            }
        }
        None
    }

    /// Build hot column with owned code LUTs (σ_P-sized) + level views.
    pub fn build_hot(&self) -> Result<HotHuffColumn, MappedQwtError> {
        let mut encode = Vec::with_capacity(self.encode_len);
        for i in 0..self.encode_len {
            encode.push(self.encode_at(i));
        }
        let mut decode = Vec::with_capacity(self.decode_bit_rows);
        for bi in 0..self.decode_bit_rows {
            let dbase = self.off_decode_dir + bi * DECODE_DIR_ENTRY;
            if self.n == 0 {
                decode.push(Vec::new());
                continue;
            }
            let off = u64::from_le_bytes(self.sec[dbase..dbase + 8].try_into().unwrap()) as usize;
            let n =
                u64::from_le_bytes(self.sec[dbase + 8..dbase + 16].try_into().unwrap()) as usize;
            let mut row = Vec::with_capacity(n);
            for i in 0..n {
                let p = off + i * PAIR_ENTRY;
                let c = u32::from_le_bytes(self.sec[p..p + 4].try_into().unwrap());
                let s = u32::from_le_bytes(self.sec[p + 4..p + 8].try_into().unwrap());
                row.push((c, s));
            }
            decode.push(row);
        }
        let mut level_lens = Vec::with_capacity(self.n_levels);
        for i in 0..self.n_levels {
            level_lens.push(if self.n == 0 { 0 } else { self.level_len(i) });
        }
        let mut levels = [HotHuffLevel::EMPTY; MAX_LEVELS];
        for (level, slot) in levels.iter_mut().enumerate().take(self.n_levels) {
            let lv = self.level_view(level)?;
            let occs_u64 = self.n_occs_smaller_level(level)?;
            let mut occs = [0usize; 5];
            for (i, o) in occs.iter_mut().enumerate() {
                *o = occs_u64[i] as usize;
            }
            *slot = HotHuffLevel {
                data: lv.data.as_ptr(),
                superblocks: lv.superblocks.as_ptr(),
                data_len: lv.data.len(),
                super_len: lv.superblocks.len(),
                qv_len: lv.n_symbols,
                occs,
            };
        }
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
        // Backward-compat: old images with empty map → contiguous [p_base, p_base+np).
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

// ── helpers (local copies; slice_as_bytes / cast_slice not pub in mapped_qwt) ─

fn slice_as_bytes<T>(s: &[T]) -> &[u8] {
    // SAFETY: POD payloads (DataLine / SuperblockPlain) on LE host.
    unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u8, std::mem::size_of_val(s)) }
}

fn cast_slice<T>(sec: &[u8], off: usize, n: usize) -> Result<&[T], MappedQwtError> {
    if n == 0 {
        return Ok(&[]);
    }
    let bytes = n
        .checked_mul(std::mem::size_of::<T>())
        .ok_or(MappedQwtError::Layout("cast overflow"))?;
    let end = off
        .checked_add(bytes)
        .ok_or(MappedQwtError::Layout("cast OOB"))?;
    if end > sec.len() {
        return Err(MappedQwtError::Layout("cast OOB"));
    }
    // Absolute pointer alignment (Vec bases are not 64-aligned; AlignedBuf/mmap are).
    let ptr = unsafe { sec.as_ptr().add(off) };
    if !(ptr as usize).is_multiple_of(std::mem::align_of::<T>()) {
        return Err(MappedQwtError::Layout("cast align"));
    }
    // SAFETY: bounds + absolute align checked; T is POD.
    Ok(unsafe { std::slice::from_raw_parts(ptr as *const T, n) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use qwt::{AccessUnsigned, HQWT256, RankUnsigned, SelectUnsigned};

    #[test]
    fn hqwa_roundtrip_access_rank_select() {
        let data: Vec<u32> = (0..200u32).map(|i| i % 7).collect();
        let wt = HQWT256::<u32>::from(data.clone());
        let p_base = 1000u32;
        let np = 7u32;
        let map: Vec<u32> = (0..np).map(|i| p_base + i).collect();
        let sec = build_hqwa_section(&wt, &map).expect("build");
        // Vec is not 64-aligned; product open uses page mmap / AlignedBuf.
        let aligned = AlignedBuf::from_slice(&sec);
        let mapped = MappedHqwtSection::open(aligned.as_slice()).expect("open");
        let hot = mapped.build_hot().expect("hot");

        assert_eq!(mapped.n, data.len());
        assert_eq!(mapped.np, np);
        assert_eq!(mapped.p_base, p_base);

        for (i, &local) in data.iter().enumerate() {
            assert_eq!(mapped.get_local(i), Some(local));
            assert_eq!(mapped.get_shared(i), Some(local + p_base));
            assert_eq!(hot.get(i), Some(local + p_base));
            assert_eq!(wt.get(i), Some(local));
        }
        for local in 0..np {
            for pos in [0usize, 1, 50, 100, 200] {
                let hr = wt.rank(local, pos);
                let mr = mapped.rank_local(local, pos);
                let hr2 = hot.rank(p_base + local, pos);
                assert_eq!(mr, hr, "rank local={local} pos={pos}");
                assert_eq!(hr2, hr);
            }
            let total = wt.rank(local, data.len()).unwrap_or(0);
            for occ in 0..total {
                assert_eq!(
                    mapped.select_local(local, occ),
                    wt.select(local, occ),
                    "select local={local} occ={occ}"
                );
            }
        }
        // RNV / RDI scan
        let full = 0..data.len();
        for t in 0..np + 2 {
            let target = p_base + t;
            let mut expect = None;
            for local in 0..np {
                if p_base + local < target {
                    continue;
                }
                let lo = wt.rank(local, full.start).unwrap_or(0);
                let hi = wt.rank(local, full.end).unwrap_or(0);
                if hi > lo {
                    expect = Some(p_base + local);
                    break;
                }
            }
            assert_eq!(hot.range_next_value_scan(full.clone(), target), expect);
        }
        let pairs = hot.range_distinct_scan(full);
        assert!(!pairs.is_empty());
        for w in pairs.windows(2) {
            assert!(w[0].0 < w[1].0);
        }
    }
}
