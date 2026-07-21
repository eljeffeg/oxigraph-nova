//! Bounded lz4 block cache over a Front-Coded `CompactedTier.buf`.
//!
//! The on-disk `NOVA_DICT_LZ4` v2 container stores a slim ε-serde **index**
//! payload (`DictSnapshot` with empty `compacted.buf`) plus independent lz4
//! blocks over the real Front-Coded `buf`. At open, indexes are
//! heap-deserialized once; `buf` stays compressed.
//! [`BlockCachedCompactedTier`] decompresses only the lz4 blocks needed for
//! a given Front-Coding decode into a bounded LRU
//! (`DEFAULT_BUF_BLOCK_CACHE` × `DEFAULT_BUF_LZ4_BLOCK`). The navigable
//! LOUDS index is never compressed; only the dictionary uses this path.

use crate::Oxigraph;
use crate::dict_compact::{
    BLOCK_SIZE, CompactedTier, DictSnapshot, term_aux_bytes, term_from_parts, term_sort_key,
};
use epserde::Epserde;
#[cfg(test)]
use lz4_flex::block::compress_prepend_size;
use lz4_flex::block::decompress_size_prepended;
use oxrdf::Term;
use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::sync::Arc;
use sux::prelude::*;
use value_traits::slices::SliceByValue;

/// Default uncompressed lz4 block size over `CompactedTier.buf`.
///
/// Smaller blocks buy more LRU slots under a fixed raw-cache MiB budget,
/// which reduces thrash on high-cardinality term decode; 64 KiB keeps
/// compression ratio within ~1.5% of larger blocks.
pub const DEFAULT_BUF_LZ4_BLOCK: usize = 64 * 1024;

/// Max decompressed lz4 blocks retained in the process-local cache.
///
/// 128 × 64 KiB ≈ 8 MiB worst-case resident raw-buf cache — enough for a
/// hot working set without pinning a multi-hundred-MiB dictionary.
pub const DEFAULT_BUF_BLOCK_CACHE: usize = 128;

/// Owned index fields of a compacted tier, without the Front-Coded `buf`.
///
/// Built from a full [`CompactedTier`] by moving every field except `buf`,
/// or reconstructed from a v2 on-disk index payload.
#[derive(Clone)]
pub(crate) struct CompactedTierIndex {
    pub(crate) block_size: usize,
    pub(crate) high_water: u64,
    pub(crate) encoded_count: u64,
    pub(crate) buf_len: usize,
    pub(crate) block_starts: Vec<u32>,
    pub(crate) block_tags: Vec<u8>,
    pub(crate) key_flat: Vec<u8>,
    pub(crate) key_offsets: Vec<u32>,
    pub(crate) rank2id: BitFieldVec,
    pub(crate) id2rank: BitFieldVec,
    pub(crate) rank2id_bit_width: usize,
    pub(crate) id2rank_bit_width: usize,
}

impl CompactedTierIndex {
    /// Build indexes from a full owned tier (unit tests only; open path uses
    /// [`dict_block_cache_types::from_parts`]).
    #[cfg(test)]
    pub(crate) fn from_owned_tier(tier: &CompactedTier) -> Self {
        Self {
            block_size: tier.block_size,
            high_water: tier.high_water,
            encoded_count: tier.encoded_count,
            buf_len: tier.buf.len(),
            block_starts: tier.block_starts.clone(),
            block_tags: tier.block_tags.clone(),
            key_flat: tier.key_flat.clone(),
            key_offsets: tier.key_offsets.clone(),
            rank2id: tier.rank2id.clone(),
            id2rank: tier.id2rank.clone(),
            rank2id_bit_width: tier.rank2id_bit_width,
            id2rank_bit_width: tier.id2rank_bit_width,
        }
    }

    /// Rebuild a full owned [`CompactedTier`] with the given `buf` (used when
    /// a caller needs a complete in-memory tier, e.g. tests).
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn with_buf(&self, buf: Vec<u8>) -> CompactedTier {
        CompactedTier {
            block_size: self.block_size,
            high_water: self.high_water,
            encoded_count: self.encoded_count,
            buf,
            block_starts: self.block_starts.clone(),
            block_tags: self.block_tags.clone(),
            key_flat: self.key_flat.clone(),
            key_offsets: self.key_offsets.clone(),
            rank2id: self.rank2id.clone(),
            id2rank: self.id2rank.clone(),
            rank2id_bit_width: self.rank2id_bit_width,
            id2rank_bit_width: self.id2rank_bit_width,
        }
    }

    fn num_fc_blocks(&self) -> usize {
        self.block_starts.len()
    }

    fn block_key_bytes(&self, block_idx: usize) -> &[u8] {
        let start = self.key_offsets[block_idx] as usize;
        let end = self
            .key_offsets
            .get(block_idx + 1)
            .map(|&x| x as usize)
            .unwrap_or(self.key_flat.len());
        &self.key_flat[start..end]
    }

    fn fc_block_range(&self, fc_block_idx: usize) -> (usize, usize) {
        let start = self.block_starts[fc_block_idx] as usize;
        let end = self
            .block_starts
            .get(fc_block_idx + 1)
            .map(|&x| x as usize)
            .unwrap_or(self.buf_len);
        (start, end)
    }
}

/// Compressed Front-Coded `buf` as independent size-prepended lz4 blocks.
#[derive(Clone)]
pub(crate) struct Lz4BufBlocks {
    pub(crate) block_size: usize,
    pub(crate) raw_len: usize,
    /// Absolute offsets into `data` for each compressed block start;
    /// length = n_blocks + 1 (end sentinel).
    pub(crate) offsets: Vec<u64>,
    /// Concatenated compressed block payloads (no container header).
    pub(crate) data: Arc<[u8]>,
}

impl Lz4BufBlocks {
    /// Compress a raw FC `buf` (unit tests only; writes use
    /// `nova-storage::dict_lz4::compress_dict`).
    #[cfg(test)]
    pub(crate) fn compress(raw: &[u8], block_size: usize) -> Result<Self, Oxigraph> {
        if block_size == 0 {
            return Err(Oxigraph::Storage(
                "dict block cache: block_size must be non-zero".into(),
            ));
        }
        let n_blocks = if raw.is_empty() {
            0
        } else {
            raw.len().div_ceil(block_size)
        };
        let mut compressed: Vec<Vec<u8>> = Vec::with_capacity(n_blocks);
        let mut i = 0usize;
        while i < raw.len() {
            let end = (i + block_size).min(raw.len());
            compressed.push(compress_prepend_size(&raw[i..end]));
            i = end;
        }
        let payload_len: usize = compressed.iter().map(|b| b.len()).sum();
        let mut data = Vec::with_capacity(payload_len);
        let mut offsets = Vec::with_capacity(n_blocks + 1);
        for block in &compressed {
            offsets.push(data.len() as u64);
            data.extend_from_slice(block);
        }
        offsets.push(data.len() as u64);
        Ok(Self {
            block_size,
            raw_len: raw.len(),
            offsets,
            data: Arc::from(data.into_boxed_slice()),
        })
    }

    pub(crate) fn n_blocks(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub(crate) fn decompress_block(&self, idx: usize) -> Result<Vec<u8>, Oxigraph> {
        if idx >= self.n_blocks() {
            return Err(Oxigraph::Storage(format!(
                "dict block cache: lz4 block {idx} out of range (n={})",
                self.n_blocks()
            )));
        }
        let start = self.offsets[idx] as usize;
        let end = self.offsets[idx + 1] as usize;
        decompress_size_prepended(&self.data[start..end]).map_err(|e| {
            Oxigraph::Storage(format!(
                "dict block cache: decompress lz4 block {idx} failed: {e}"
            ))
        })
    }

    /// Which lz4 block contains raw byte offset `pos` (or None if OOB).
    fn block_for_offset(&self, pos: usize) -> Option<usize> {
        if self.raw_len == 0 || pos >= self.raw_len {
            return None;
        }
        Some(pos / self.block_size)
    }
}

/// Compacted tier backed by lz4-blocked `buf` + a bounded decompress cache.
pub struct BlockCachedCompactedTier {
    index: CompactedTierIndex,
    lz4: Lz4BufBlocks,
    /// lz4-block-index → decompressed raw bytes for that block.
    cache: RefCell<lru::LruCache<usize, Arc<[u8]>>>,
}

impl BlockCachedCompactedTier {
    pub(crate) fn new(
        index: CompactedTierIndex,
        lz4: Lz4BufBlocks,
        cache_capacity: usize,
    ) -> Result<Self, Oxigraph> {
        if index.buf_len != lz4.raw_len {
            return Err(Oxigraph::Storage(format!(
                "dict block cache: index buf_len {} != lz4 raw_len {}",
                index.buf_len, lz4.raw_len
            )));
        }
        let cap = NonZeroUsize::new(cache_capacity.max(1)).expect("nonzero");
        Ok(Self {
            index,
            lz4,
            cache: RefCell::new(lru::LruCache::new(cap)),
        })
    }

    /// Build from a freshly-owned compacted tier (compresses `buf` in place;
    /// unit tests only). Open path uses [`dict_block_cache_types::from_parts`].
    #[cfg(test)]
    pub(crate) fn from_owned_tier(
        tier: &CompactedTier,
        lz4_block_size: usize,
        cache_capacity: usize,
    ) -> Result<Self, Oxigraph> {
        let index = CompactedTierIndex::from_owned_tier(tier);
        let lz4 = Lz4BufBlocks::compress(&tier.buf, lz4_block_size)?;
        Self::new(index, lz4, cache_capacity)
    }

    fn get_lz4_block(&self, idx: usize) -> Result<Arc<[u8]>, Oxigraph> {
        {
            let mut cache = self.cache.borrow_mut();
            if let Some(hit) = cache.get(&idx) {
                return Ok(Arc::clone(hit));
            }
        }
        let raw = self.lz4.decompress_block(idx)?;
        let arc: Arc<[u8]> = Arc::from(raw.into_boxed_slice());
        self.cache.borrow_mut().put(idx, Arc::clone(&arc));
        Ok(arc)
    }

    /// Materialize raw `buf[start..end]` via the lz4 block cache.
    fn raw_slice(&self, start: usize, end: usize) -> Result<Vec<u8>, Oxigraph> {
        if start > end || end > self.index.buf_len {
            return Err(Oxigraph::Storage(format!(
                "dict block cache: bad range {start}..{end} (buf_len={})",
                self.index.buf_len
            )));
        }
        if start == end {
            return Ok(Vec::new());
        }
        let first = self
            .lz4
            .block_for_offset(start)
            .ok_or_else(|| Oxigraph::Storage("dict block cache: start OOB".into()))?;
        let last = self
            .lz4
            .block_for_offset(end - 1)
            .ok_or_else(|| Oxigraph::Storage("dict block cache: end OOB".into()))?;

        let mut out = Vec::with_capacity(end - start);
        for b in first..=last {
            let block = self.get_lz4_block(b)?;
            let block_raw_start = b * self.lz4.block_size;
            let local_start = start.saturating_sub(block_raw_start);
            let local_end = (end - block_raw_start).min(block.len());
            out.extend_from_slice(&block[local_start..local_end]);
        }
        debug_assert_eq!(out.len(), end - start);
        Ok(out)
    }

    fn decode_fc_block(
        &self,
        fc_block_idx: usize,
    ) -> Result<Vec<(u8, Vec<u8>, Vec<u8>)>, Oxigraph> {
        let (start, end) = self.index.fc_block_range(fc_block_idx);
        let buf = self.raw_slice(start, end)?;
        let mut pos = 0usize;
        let mut prev_primary: Vec<u8> = Vec::new();
        let mut out = Vec::new();
        while pos < buf.len() {
            let lcp = read_varint(&buf, &mut pos) as usize;
            let suffix_len = read_varint(&buf, &mut pos) as usize;
            let mut primary = prev_primary[..lcp].to_vec();
            primary.extend_from_slice(&buf[pos..pos + suffix_len]);
            pos += suffix_len;
            let tag = buf[pos];
            pos += 1;
            let aux_len = read_varint(&buf, &mut pos) as usize;
            let aux = buf[pos..pos + aux_len].to_vec();
            pos += aux_len;
            prev_primary = primary.clone();
            out.push((tag, primary, aux));
        }
        Ok(out)
    }

    pub(crate) fn get_id(&self, term: &Term) -> Option<u64> {
        if self.index.encoded_count == 0 {
            return None;
        }
        let (tag, primary) = term_sort_key(term);
        let aux = term_aux_bytes(term);
        let target = (tag, primary);

        let num_blocks = self.index.num_fc_blocks();
        let block_tags = &self.index.block_tags;

        let mut lo = 0usize;
        let mut hi = num_blocks;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let mid_tag = block_tags[mid];
            let mid_key = self.index.block_key_bytes(mid);
            let is_le =
                mid_tag < target.0 || (mid_tag == target.0 && mid_key <= target.1.as_slice());
            if is_le {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let blk = lo.saturating_sub(1);

        let mut rank = blk * self.index.block_size;
        for b in blk..num_blocks {
            let decoded = self.decode_fc_block(b).ok()?;
            for (t, p, a) in decoded {
                let key = (t, p);
                if key > target {
                    return None;
                }
                if key == target && a == aux {
                    return Some(self.index.rank2id.index_value(rank) as u64);
                }
                rank += 1;
            }
        }
        None
    }

    pub(crate) fn decode_id(&self, id: u64) -> Option<Result<Arc<Term>, Oxigraph>> {
        if id >= self.index.high_water || self.index.encoded_count == 0 {
            return None;
        }
        let rank = self.index.id2rank.index_value(id as usize);
        let block = rank / self.index.block_size;
        let offset = rank % self.index.block_size;
        let decoded = match self.decode_fc_block(block) {
            Ok(d) => d,
            Err(e) => return Some(Err(e)),
        };
        let (tag, primary, aux) = decoded.get(offset)?.clone();
        Some(term_from_parts(tag, primary, &aux).map(Arc::new))
    }

    pub(crate) fn decode_block_for_id(
        &self,
        id: u64,
    ) -> Option<Vec<(u64, Result<Arc<Term>, Oxigraph>)>> {
        if id >= self.index.high_water || self.index.encoded_count == 0 {
            return None;
        }
        let rank = self.index.id2rank.index_value(id as usize);
        let block = rank / self.index.block_size;
        let block_start_rank = block * self.index.block_size;
        let decoded = match self.decode_fc_block(block) {
            Ok(d) => d,
            Err(e) => return Some(vec![(id, Err(e))]),
        };
        let mut out = Vec::with_capacity(decoded.len());
        for (i, (tag, primary, aux)) in decoded.into_iter().enumerate() {
            let orig_id = self.index.rank2id.index_value(block_start_rank + i) as u64;
            out.push((orig_id, term_from_parts(tag, primary, &aux).map(Arc::new)));
        }
        Some(out)
    }

    /// Resident bytes: indexes + compressed buf + currently cached raw blocks.
    pub(crate) fn mem_size_bytes(&self) -> usize {
        let idx = &self.index;
        let index_bytes = std::mem::size_of_val(idx.block_starts.as_slice())
            + idx.block_tags.len()
            + idx.key_flat.len()
            + std::mem::size_of_val(idx.key_offsets.as_slice())
            + (idx.encoded_count as usize).div_ceil(8) * idx.rank2id_bit_width / 8
            + 8
            + (idx.high_water as usize).div_ceil(8) * idx.id2rank_bit_width / 8
            + 8;
        let compressed = self.lz4.data.len() + self.lz4.offsets.len() * 8;
        let cached: usize = self.cache.borrow().iter().map(|(_, v)| v.len()).sum();
        index_bytes + compressed + cached + std::mem::size_of::<Self>()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.index.encoded_count == 0
    }

    pub(crate) fn high_water(&self) -> u64 {
        self.index.high_water
    }
}

// Local varint readers (same LEB128 as dict_compact; kept private to avoid
// exporting dict_compact internals).
fn read_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = buf[*pos];
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    result
}

/// Slim ε-serde payload written beside the lz4 `buf` blocks in a v2 container:
/// full [`DictSnapshot`] with `compacted.buf` cleared (indexes only).
///
/// Callers set `compacted.buf` to empty before serializing; `buf_len` is
/// carried separately in the container header so the loader can rebuild
/// [`CompactedTierIndex::buf_len`].
#[derive(Epserde)]
pub struct DictIndexSnapshot {
    pub next_graph_id: u8,
    /// Compacted tier with **empty** `buf`; all other fields intact.
    pub compacted: CompactedTier,
    /// Original `compacted.buf.len()` before stripping (for the block cache).
    pub buf_len: u64,
    pub triple_ids: Vec<u64>,
    pub triple_s: Vec<u64>,
    pub triple_p: Vec<u64>,
    pub triple_o: Vec<u64>,
    pub graph_ids: Vec<u8>,
    pub graph_kinds: Vec<u8>,
    pub graph_str_flat: Vec<u8>,
    pub graph_str_offsets: Vec<u32>,
}

impl DictIndexSnapshot {
    /// Build from a full owned snapshot: moves indexes, records `buf_len`,
    /// clears `buf` so the ε-serde payload stays small.
    pub fn from_full_snapshot(mut snap: DictSnapshot) -> (Self, Vec<u8>) {
        let buf = std::mem::take(&mut snap.compacted.buf);
        let buf_len = buf.len() as u64;
        let index = DictIndexSnapshot {
            next_graph_id: snap.next_graph_id,
            compacted: snap.compacted,
            buf_len,
            triple_ids: snap.triple_ids,
            triple_s: snap.triple_s,
            triple_p: snap.triple_p,
            triple_o: snap.triple_o,
            graph_ids: snap.graph_ids,
            graph_kinds: snap.graph_kinds,
            graph_str_flat: snap.graph_str_flat,
            graph_str_offsets: snap.graph_str_offsets,
        };
        (index, buf)
    }

    pub(crate) fn into_tier_index(self) -> CompactedTierIndex {
        CompactedTierIndex {
            block_size: self.compacted.block_size,
            high_water: self.compacted.high_water,
            encoded_count: self.compacted.encoded_count,
            buf_len: self.buf_len as usize,
            block_starts: self.compacted.block_starts,
            block_tags: self.compacted.block_tags,
            key_flat: self.compacted.key_flat,
            key_offsets: self.compacted.key_offsets,
            rank2id: self.compacted.rank2id,
            id2rank: self.compacted.id2rank,
            rank2id_bit_width: self.compacted.rank2id_bit_width,
            id2rank_bit_width: self.compacted.id2rank_bit_width,
        }
    }
}

/// Public bridge for `nova-storage` to assemble a
/// [`BlockCachedCompactedTier`] from a deserialized v2 container without
/// exposing every private field of the block-cache module.
pub mod dict_block_cache_types {
    pub use super::BlockCachedCompactedTier;
    use super::*;

    pub fn from_parts(
        index_snap: DictIndexSnapshot,
        block_size: usize,
        raw_len: usize,
        offsets: Vec<u64>,
        data: Vec<u8>,
        cache_capacity: usize,
    ) -> Result<BlockCachedCompactedTier, Oxigraph> {
        let mut index = index_snap.into_tier_index();
        index.buf_len = raw_len;
        let lz4 = Lz4BufBlocks {
            block_size,
            raw_len,
            offsets,
            data: Arc::from(data.into_boxed_slice()),
        };
        BlockCachedCompactedTier::new(index, lz4, cache_capacity)
    }
}

// Re-export BLOCK_SIZE usage silence
#[allow(dead_code)]
const _ASSERT_FC_BLOCK: usize = BLOCK_SIZE;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dictionary;
    use oxrdf::{Literal, NamedNode, Term};

    fn nn(s: &str) -> Term {
        Term::NamedNode(NamedNode::new_unchecked(s))
    }
    fn lit(s: &str) -> Term {
        Term::Literal(Literal::new_simple_literal(s))
    }

    #[test]
    fn block_cached_round_trip_terms() {
        let mut d = Dictionary::new();
        let mut ids = Vec::new();
        for i in 0..500 {
            ids.push(d.intern(&nn(&format!("http://ex/item/{i}"))).unwrap());
            ids.push(d.intern(&lit(&format!("label {i} padding text"))).unwrap());
        }
        d.compact().unwrap();
        let snap = d.to_snapshot();
        let tier = &snap.compacted;
        let cached = BlockCachedCompactedTier::from_owned_tier(
            tier, 4096, // small blocks to force multi-block
            4,
        )
        .unwrap();

        for &id in &ids {
            let expected = d.get_term_arc(id).unwrap();
            let got = cached.decode_id(id.as_u64()).unwrap().unwrap();
            assert_eq!(got.as_ref(), expected.as_ref());
            assert_eq!(cached.get_id(expected.as_ref()), Some(id.as_u64()));
        }
    }

    #[test]
    fn cache_stays_bounded() {
        let mut d = Dictionary::new();
        for i in 0..5_000 {
            d.intern(&lit(&format!(
                "unique free text label number {i:05} with filler words here"
            )))
            .unwrap();
        }
        d.compact().unwrap();
        let snap = d.to_snapshot();
        let cached = BlockCachedCompactedTier::from_owned_tier(&snap.compacted, 1024, 3).unwrap();
        // Touch many ids → cache must never exceed capacity.
        for id in 0..snap.compacted.high_water {
            let _ = cached.decode_id(id);
            assert!(cached.cache.borrow().len() <= 3);
        }
    }
}
