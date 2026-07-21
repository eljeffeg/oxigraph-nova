//! Compacted (immutable) Front-Coded tier for `Dictionary`, plus its
//! persistent, mmap-able on-disk representation.
//!
//! ## Design
//!
//! Every regular (non-quoted-triple) term present at the moment
//! [`crate::dict::Dictionary::compact`] runs is sorted by `(tag, primary
//! string)` and Front-Coded (LCP + suffix) into a single byte buffer,
//! partitioned into fixed-size blocks (`BLOCK_SIZE` entries each). Each block
//! resets its LCP chain (first entry of a block always stores its full
//! primary string as "suffix" with `lcp = 0`), so a block can be decoded
//! independently without first decoding every earlier block — this bounds
//! decode cost to O(block_size) instead of O(n).
//!
//! Two bit-packed permutation arrays (`sux::BitFieldVec`, exactly the
//! pattern already used for the LOUDS L-array — see `louds.rs` in
//! `nova-engine-ring`) bridge insertion-order-stable `TermId`s to sorted
//! ranks within this tier:
//!
//! - `rank2id[rank]` → original `TermId` (dense, one entry per encoded term).
//! - `id2rank[id]` → rank (dense over `0..high_water`; **gaps** at
//!   quoted-triple `TermId`s are never populated meaningfully, but are also
//!   never read — `Dictionary` always checks its `triple_terms`/delta
//!   tables before ever consulting this tier for such an id).
//!
//! Quoted-triple (`Term::Triple`) terms are **excluded** entirely — they
//! stay in `Dictionary`'s existing `triple_terms`/`triple_index` side
//! tables forever (their byte encoding has no useful shared string prefix
//! and terminator-based framing would be unsafe against literals containing
//! arbitrary bytes).
//!
//! ## `TermId` stability
//!
//! This module never reassigns `TermId`s. `Dictionary::compact` computes a
//! brand new sorted order (and thus new *ranks*) every time it runs, but
//! `orig_id` (the `TermId`) traveling alongside each entry never changes —
//! only where it sits in the sorted rank space changes, and that
//! indirection is fully absorbed by `id2rank`/`rank2id`.
//!
//! ## Substrate-generic storage (mmap residency)
//!
//! [`CompactedTier`] is generic over every variable-length field's backing
//! storage (`Buf`, `BlockStarts`, `BlockTags`, `KeyFlat`, `KeyOffsets`,
//! `Rank2Id`, `Id2Rank`), each defaulting to an owned, heap-allocated type
//! (`Vec<u8>`/`Vec<u32>`/`BitFieldVec`). This "bare generic parameter with a
//! default" pattern — every field's declared type is literally the bare
//! generic parameter, never wrapped — is exactly what lets `epserde`'s
//! `load_mmap` substitute a borrowed, zero-copy `DeserType` field (e.g.
//! `&[u8]` in place of `Vec<u8>`) with **no additional code**, mirroring the
//! pattern used throughout `nova-engine-ring`'s `louds.rs`/`cltj.rs`/
//! `snapshot.rs`.
//!
//! Block-keyed binary search stores first-keys as three parallel arrays
//! (epserde does not support tuples or `Vec<Vec<u8>>` directly), mirroring
//! `SidecarCore`'s "flatten variable-length payloads into parallel `Vec` +
//! offset arrays" pattern: `block_tags` (one tag per block), `key_flat`
//! (concatenated first-key primary-string bytes for every block), and
//! `key_offsets` (the starting offset of each block's key within `key_flat`;
//! a block's key ends at the next block's offset, or at `key_flat`'s end for
//! the last block).

use crate::Oxigraph;
use epserde::Epserde;
use oxrdf::{BaseDirection, BlankNode, Literal, NamedNode, Term};
use std::sync::Arc;
use sux::prelude::*;
use value_traits::slices::SliceByValue;

/// Entries per Front-Coding block.
///
/// **tuning result:** swept `8` vs `16` against
/// the `wikidata_slice` benchmark suite (with the decode cache in
/// place). `8` won on every measured axis:
///
/// Smaller blocks mean less to decode per cache-miss (`decode_block` scans
/// at most `BLOCK_SIZE` entries), which dominates `query/triangle/ring`'s
/// remaining cost after the decode cache absorbs most repeat lookups —
/// without meaningfully increasing `compact_build_index/ring`'s per-block
/// bookkeeping overhead at these sizes.
pub(crate) const BLOCK_SIZE: usize = 8;

// ── Varint (unsigned LEB128) — self-contained, always well-formed since ────
// this module both encodes and decodes its own byte buffer (no external/
// untrusted input), so decode helpers are infallible.

fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            buf.push(byte | 0x80);
        } else {
            buf.push(byte);
            break;
        }
    }
}

fn read_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
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

fn write_str(buf: &mut Vec<u8>, s: &str) {
    write_varint(buf, s.len() as u64);
    buf.extend_from_slice(s.as_bytes());
}

fn read_str(buf: &[u8], pos: &mut usize) -> String {
    let len = read_varint(buf, pos) as usize;
    let bytes = buf[*pos..*pos + len].to_vec();
    *pos += len;
    String::from_utf8(bytes).expect("dict_compact: term bytes are always valid utf8 on encode")
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn bit_width_for(max_val: u64) -> usize {
    ((u64::BITS - max_val.leading_zeros()) as usize).max(1)
}

// ── Term ↔ (tag, primary, aux) ──────────────────────────────────────────────
//
// Term codec for the Front-Coded tier (tag 6 / quoted triples never reach this tier).

pub(crate) fn term_sort_key(term: &Term) -> (u8, Vec<u8>) {
    match term {
        Term::NamedNode(n) => (0, n.as_str().as_bytes().to_vec()),
        Term::BlankNode(b) => (1, b.as_str().as_bytes().to_vec()),
        Term::Literal(l) => {
            let tag = if l.direction().is_some() {
                4
            } else if l.language().is_some() {
                3
            } else if l.datatype() == oxrdf::vocab::xsd::STRING {
                2
            } else {
                5
            };
            (tag, l.value().as_bytes().to_vec())
        }
        Term::Triple(_) => {
            unreachable!("quoted triples never enter the compacted FC tier")
        }
    }
}

pub(crate) fn term_aux_bytes(term: &Term) -> Vec<u8> {
    let mut buf = Vec::new();
    match term {
        Term::NamedNode(_) | Term::BlankNode(_) => {}
        Term::Literal(l) => {
            if let Some(direction) = l.direction() {
                write_str(&mut buf, l.language().unwrap_or(""));
                buf.push(match direction {
                    BaseDirection::Ltr => 0,
                    BaseDirection::Rtl => 1,
                });
            } else if let Some(lang) = l.language() {
                write_str(&mut buf, lang);
            } else if l.datatype() == oxrdf::vocab::xsd::STRING {
                // simple literal: nothing extra to store
            } else {
                write_str(&mut buf, l.datatype().as_str());
            }
        }
        Term::Triple(_) => unreachable!("quoted triples never enter the compacted FC tier"),
    }
    buf
}

pub(crate) fn term_from_parts(tag: u8, primary: Vec<u8>, aux: &[u8]) -> Result<Term, Oxigraph> {
    fn utf8(bytes: Vec<u8>) -> Result<String, Oxigraph> {
        String::from_utf8(bytes)
            .map_err(|e| Oxigraph::Storage(format!("dict compact: invalid utf8: {e}")))
    }
    match tag {
        0 => Ok(Term::NamedNode(NamedNode::new_unchecked(utf8(primary)?))),
        1 => Ok(Term::BlankNode(BlankNode::new_unchecked(utf8(primary)?))),
        2 => Ok(Term::Literal(Literal::new_simple_literal(utf8(primary)?))),
        3 => {
            let mut pos = 0usize;
            let lang = read_str(aux, &mut pos);
            Ok(Term::Literal(
                Literal::new_language_tagged_literal_unchecked(utf8(primary)?, lang),
            ))
        }
        4 => {
            let mut pos = 0usize;
            let lang = read_str(aux, &mut pos);
            let dir_byte = aux[pos];
            let direction = match dir_byte {
                0 => BaseDirection::Ltr,
                1 => BaseDirection::Rtl,
                other => {
                    return Err(Oxigraph::Storage(format!(
                        "dict compact: invalid direction byte {other}"
                    )));
                }
            };
            Ok(Term::Literal(
                Literal::new_directional_language_tagged_literal_unchecked(
                    utf8(primary)?,
                    lang,
                    direction,
                ),
            ))
        }
        5 => {
            let mut pos = 0usize;
            let datatype = read_str(aux, &mut pos);
            Ok(Term::Literal(Literal::new_typed_literal(
                utf8(primary)?,
                NamedNode::new_unchecked(datatype),
            )))
        }
        other => Err(Oxigraph::Storage(format!(
            "dict compact: unknown term tag {other}"
        ))),
    }
}

// ── CompactedTier ────────────────────────────────────────────────────────────

/// One sorted, Front-Coded, immutable generation of the compacted dictionary
/// tier, plus the `id2rank`/`rank2id` permutation bridging it to
/// insertion-order-stable `TermId`s.
///
/// Generic over every variable-length field's backing storage — see the
/// module docs above. All existing callers use the default, fully-owned
/// instantiation (plain `CompactedTier`).
#[derive(Epserde, Clone)]
pub struct CompactedTier<
    Buf = Vec<u8>,
    BlockStarts = Vec<u32>,
    BlockTags = Vec<u8>,
    KeyFlat = Vec<u8>,
    KeyOffsets = Vec<u32>,
    Rank2Id = BitFieldVec,
    Id2Rank = BitFieldVec,
> {
    pub(crate) block_size: usize,
    /// Number of `TermId` slots this tier's `id2rank` covers (`0..high_water`).
    /// Includes gaps at quoted-triple ids (never populated/read).
    pub(crate) high_water: u64,
    /// Number of actual encoded (non-triple) entries — the `rank` domain.
    pub(crate) encoded_count: u64,
    /// Front-Coded byte buffer, block-partitioned (see module docs).
    pub(crate) buf: Buf,
    /// Byte offset into `buf` where each block starts.
    pub(crate) block_starts: BlockStarts,
    /// Tag (see `term_sort_key`) of each block's first entry — parallel to
    /// `key_offsets`, for binary search in [`Self::get_id`].
    pub(crate) block_tags: BlockTags,
    /// Concatenated first-key primary-string bytes for every block, in
    /// block order.
    pub(crate) key_flat: KeyFlat,
    /// Starting offset of each block's key within `key_flat`; a block's key
    /// ends at the next block's offset (or at `key_flat`'s end for the last
    /// block).
    pub(crate) key_offsets: KeyOffsets,
    /// `rank2id[rank]` → original `TermId` (dense, len == `encoded_count`).
    pub(crate) rank2id: Rank2Id,
    /// `id2rank[id]` → rank (dense over `0..high_water`, gaps at triple ids).
    pub(crate) id2rank: Id2Rank,
    /// Bit width `rank2id` was built with — stored explicitly (rather than
    /// relying on a generic `BitFieldVec<B>::bit_width()` call) so
    /// [`Self::mem_size_bytes`] works identically for every substrate
    /// instantiation without an extra trait bound.
    pub(crate) rank2id_bit_width: usize,
    /// Bit width `id2rank` was built with — see `rank2id_bit_width`.
    pub(crate) id2rank_bit_width: usize,
}

impl CompactedTier {
    /// An empty tier — the initial state before any compaction has run.
    pub(crate) fn empty() -> Self {
        Self {
            block_size: BLOCK_SIZE,
            high_water: 0,
            encoded_count: 0,
            buf: Vec::new(),
            block_starts: Vec::new(),
            block_tags: Vec::new(),
            key_flat: Vec::new(),
            key_offsets: Vec::new(),
            rank2id: BitFieldVec::new(1, 0),
            id2rank: BitFieldVec::new(1, 0),
            rank2id_bit_width: 1,
            id2rank_bit_width: 1,
        }
    }

    /// Build a new compacted tier from `entries` — `(tag, primary, aux,
    /// orig_id)` tuples for every currently-interned non-triple term,
    /// **already sorted** by `(tag, primary)` by the caller.
    ///
    /// `high_water` is the total `TermId` count (`Dictionary::next_id`) at
    /// the time of this compaction — the domain of `id2rank`, including any
    /// quoted-triple id gaps.
    pub(crate) fn build(entries: &[(u8, Vec<u8>, Vec<u8>, u64)], high_water: u64) -> Self {
        let block_size = BLOCK_SIZE;
        let encoded_count = entries.len() as u64;

        let mut buf = Vec::new();
        let mut block_starts = Vec::new();
        let mut block_tags = Vec::new();
        let mut key_flat = Vec::new();
        let mut key_offsets = Vec::new();
        let mut rank2id_vals: Vec<u64> = Vec::with_capacity(entries.len());

        for (i, (tag, primary, aux, orig_id)) in entries.iter().enumerate() {
            let in_block_pos = i % block_size;
            if in_block_pos == 0 {
                block_starts.push(buf.len() as u32);
                block_tags.push(*tag);
                key_offsets.push(key_flat.len() as u32);
                key_flat.extend_from_slice(primary);
            }
            let prev_primary: &[u8] = if in_block_pos == 0 {
                &[]
            } else {
                &entries[i - 1].1
            };
            let lcp = common_prefix_len(prev_primary, primary);
            write_varint(&mut buf, lcp as u64);
            let suffix = &primary[lcp..];
            write_varint(&mut buf, suffix.len() as u64);
            buf.extend_from_slice(suffix);
            buf.push(*tag);
            write_varint(&mut buf, aux.len() as u64);
            buf.extend_from_slice(aux);

            rank2id_vals.push(*orig_id);
        }

        let rank2id_bits = bit_width_for(high_water.saturating_sub(1));
        let mut rank2id = BitFieldVec::new(rank2id_bits, 0);
        for v in &rank2id_vals {
            rank2id.push(*v as usize);
        }

        let id2rank_bits = bit_width_for(encoded_count.saturating_sub(1));
        let mut id2rank_vals = vec![0u64; high_water as usize];
        for (rank, (_, _, _, orig_id)) in entries.iter().enumerate() {
            id2rank_vals[*orig_id as usize] = rank as u64;
        }
        let mut id2rank = BitFieldVec::new(id2rank_bits, 0);
        for v in &id2rank_vals {
            id2rank.push(*v as usize);
        }

        Self {
            block_size,
            high_water,
            encoded_count,
            buf,
            block_starts,
            block_tags,
            key_flat,
            key_offsets,
            rank2id,
            id2rank,
            rank2id_bit_width: rank2id_bits,
            id2rank_bit_width: id2rank_bits,
        }
    }

    /// Deep-clone this owned tier — used by [`crate::dict::Dictionary::to_snapshot`]
    /// to build a persistable snapshot without mutating (or taking
    /// ownership of) the live `Dictionary`.
    pub(crate) fn clone_owned(&self) -> Self {
        Self {
            block_size: self.block_size,
            high_water: self.high_water,
            encoded_count: self.encoded_count,
            buf: self.buf.clone(),
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
}

impl<Buf, BlockStarts, BlockTags, KeyFlat, KeyOffsets, Rank2Id, Id2Rank>
    CompactedTier<Buf, BlockStarts, BlockTags, KeyFlat, KeyOffsets, Rank2Id, Id2Rank>
where
    Buf: AsRef<[u8]>,
    BlockStarts: AsRef<[u32]>,
    BlockTags: AsRef<[u8]>,
    KeyFlat: AsRef<[u8]>,
    KeyOffsets: AsRef<[u32]>,
    Rank2Id: SliceByValue<Value = usize>,
    Id2Rank: SliceByValue<Value = usize>,
{
    fn num_blocks(&self) -> usize {
        self.block_starts.as_ref().len()
    }

    /// Decode every entry of block `block_idx` in order:
    /// `(tag, primary, aux)` triples.
    fn decode_block(&self, block_idx: usize) -> Vec<(u8, Vec<u8>, Vec<u8>)> {
        let buf = self.buf.as_ref();
        let block_starts = self.block_starts.as_ref();
        let start = block_starts[block_idx] as usize;
        let end = block_starts
            .get(block_idx + 1)
            .map(|&x| x as usize)
            .unwrap_or(buf.len());
        let mut pos = start;
        let mut prev_primary: Vec<u8> = Vec::new();
        let mut out = Vec::new();
        while pos < end {
            let lcp = read_varint(buf, &mut pos) as usize;
            let suffix_len = read_varint(buf, &mut pos) as usize;
            let mut primary = prev_primary[..lcp].to_vec();
            primary.extend_from_slice(&buf[pos..pos + suffix_len]);
            pos += suffix_len;
            let tag = buf[pos];
            pos += 1;
            let aux_len = read_varint(buf, &mut pos) as usize;
            let aux = buf[pos..pos + aux_len].to_vec();
            pos += aux_len;
            prev_primary = primary.clone();
            out.push((tag, primary, aux));
        }
        out
    }

    /// The bytes of block `block_idx`'s first-entry key (in `key_flat`).
    fn block_key_bytes(&self, block_idx: usize) -> &[u8] {
        let key_flat = self.key_flat.as_ref();
        let key_offsets = self.key_offsets.as_ref();
        let start = key_offsets[block_idx] as usize;
        let end = key_offsets
            .get(block_idx + 1)
            .map(|&x| x as usize)
            .unwrap_or(key_flat.len());
        &key_flat[start..end]
    }

    /// Binary-search (over block first-keys) + linear block scan for
    /// `term`'s `TermId`, or `None` if not present in this tier.
    pub(crate) fn get_id(&self, term: &Term) -> Option<u64> {
        if self.encoded_count == 0 {
            return None;
        }
        let (tag, primary) = term_sort_key(term);
        let aux = term_aux_bytes(term);
        let target = (tag, primary);

        let num_blocks = self.num_blocks();
        let block_tags = self.block_tags.as_ref();

        // Equivalent to the pre-refactor
        // `block_first_keys.partition_point(|k| *k <= target)` over a
        // `Vec<(u8, Vec<u8>)>`, but reading the parallel
        // `block_tags`/`key_flat`/`key_offsets` arrays instead of a single
        // tuple-Vec (which epserde cannot serialize directly).
        let mut lo = 0usize;
        let mut hi = num_blocks;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let mid_tag = block_tags[mid];
            let mid_key = self.block_key_bytes(mid);
            let is_le =
                mid_tag < target.0 || (mid_tag == target.0 && mid_key <= target.1.as_slice());
            if is_le {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let blk = lo.saturating_sub(1);

        let mut rank = blk * self.block_size;
        for b in blk..num_blocks {
            let decoded = self.decode_block(b);
            for (t, p, a) in decoded {
                let key = (t, p);
                if key > target {
                    return None;
                }
                if key == target && a == aux {
                    return Some(self.rank2id.index_value(rank) as u64);
                }
                rank += 1;
            }
        }
        None
    }

    /// Decode the term at `id` (a raw `TermId::as_u64()`), or `None` if `id`
    /// is out of this tier's range (belongs to the delta tier, or is a
    /// quoted-triple id the caller should never route here).
    pub(crate) fn decode_id(&self, id: u64) -> Option<Result<Arc<Term>, Oxigraph>> {
        if id >= self.high_water || self.encoded_count == 0 {
            return None;
        }
        let rank = self.id2rank.index_value(id as usize);
        let block = rank / self.block_size;
        let offset = rank % self.block_size;
        let decoded = self.decode_block(block);
        let (tag, primary, aux) = decoded.get(offset)?.clone();
        Some(term_from_parts(tag, primary, &aux).map(Arc::new))
    }

    /// Decode **every** entry of the block containing `id`, returning
    /// `(orig_id, decoded_term)` pairs for all of them (not just the one
    /// originally requested).
    ///
    /// `decode_block` already pays the full O(block_size) cost to decode a
    /// block on any single lookup within it (Front-Coding requires decoding
    /// from the block's first entry to reconstruct any later entry's LCP
    /// chain) — but the original `decode_id` discarded every entry except
    /// the one requested, meaning a sequential scan touching many distinct
    /// terms (e.g. `scan`'s 500K-row `?s wdt:P31 ?o`) paid this O(block_size)
    /// cost on *every* row instead of amortizing it across the whole block.
    /// Callers (`Dictionary::get_term_arc`) should insert every returned
    /// entry into the decode cache, turning each miss into up to
    /// `block_size` future hits instead of just one.
    pub(crate) fn decode_block_for_id(
        &self,
        id: u64,
    ) -> Option<Vec<(u64, Result<Arc<Term>, Oxigraph>)>> {
        if id >= self.high_water || self.encoded_count == 0 {
            return None;
        }
        let rank = self.id2rank.index_value(id as usize);
        let block = rank / self.block_size;
        let block_start_rank = block * self.block_size;
        let decoded = self.decode_block(block);
        let mut out = Vec::with_capacity(decoded.len());
        for (i, (tag, primary, aux)) in decoded.into_iter().enumerate() {
            let orig_id = self.rank2id.index_value(block_start_rank + i) as u64;
            out.push((orig_id, term_from_parts(tag, primary, &aux).map(Arc::new)));
        }
        Some(out)
    }

    /// Real allocated byte size — buffer + block index + bit-packed
    /// permutation arrays. Uses the explicitly-stored `encoded_count`/
    /// `high_water`/`*_bit_width` fields (rather than a generic
    /// `BitFieldVec<B>` API call) so this works identically for the owned
    /// and borrowed/mmap'd substrate instantiations.
    pub(crate) fn mem_size_bytes(&self) -> usize {
        let buf_bytes = self.buf.as_ref().len();
        let block_starts_bytes = std::mem::size_of_val(self.block_starts.as_ref());
        let block_tags_bytes = self.block_tags.as_ref().len();
        let key_flat_bytes = self.key_flat.as_ref().len();
        let key_offsets_bytes = std::mem::size_of_val(self.key_offsets.as_ref());

        let rank2id_bytes =
            (self.encoded_count as usize).div_ceil(8) * self.rank2id_bit_width / 8 + 8;
        let id2rank_bytes = (self.high_water as usize).div_ceil(8) * self.id2rank_bit_width / 8 + 8;
        buf_bytes
            + block_starts_bytes
            + block_tags_bytes
            + key_flat_bytes
            + key_offsets_bytes
            + rank2id_bytes
            + id2rank_bytes
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.encoded_count == 0
    }
}

// ── DictSnapshot ─────────────────────────────────────────────────────────────

/// ε-serde-serializable snapshot of a whole [`crate::dict::Dictionary`]:
/// the compacted Front-Coded tier (see [`CompactedTier`]) plus every
/// quoted-triple term (never covered by the compacted tier — see module
/// docs) and the graph table.
///
/// This is the persistent on-disk representation — see
/// `oxigraph_nova_storage::dict_snapshot`'s `write_and_load_mmap`/
/// `load_mmap_from_file`, and [`crate::dict::Dictionary::to_snapshot`]/
/// [`crate::dict::Dictionary::from_block_cached`].
///
/// Quoted-triple terms are persisted as just their three component
/// `TermId`s (`triple_ids`/`triple_s`/`triple_p`/`triple_o`, parallel
/// arrays in ascending-id order) rather than any encoded `Term` bytes —
/// intern() always recursively interns a quoted triple's components
/// *before* the triple itself, so every component is guaranteed to already
/// be reconstructible (either from the compacted tier, or from an
/// earlier-processed nested quoted triple) by the time
/// [`crate::dict::Dictionary::from_block_cached`] processes it.
///
/// The graph table is similarly flattened: `graph_ids` (one `GraphId` per
/// entry) plus `graph_kinds` (0=NamedNode, 1=BlankNode, 2=DefaultGraph) and
/// a `graph_str_flat`/`graph_str_offsets` pair (concatenated IRI/blank-node
/// identifier bytes, empty for `DefaultGraph`) — the same
/// flatten-into-parallel-arrays pattern used for the compacted tier's block
/// keys, since epserde has no direct `HashMap`/tuple/`GraphName` support.
///
/// Generic over `Compacted` (default [`CompactedTier`]) so ε-serde can
/// specialize field storage if needed — mirrors the "bare generic parameter
/// with a default" pattern used throughout `nova-engine-ring`.
#[derive(Epserde)]
pub struct DictSnapshot<Compacted = CompactedTier> {
    pub(crate) next_graph_id: u8,
    pub(crate) compacted: Compacted,
    pub(crate) triple_ids: Vec<u64>,
    pub(crate) triple_s: Vec<u64>,
    pub(crate) triple_p: Vec<u64>,
    pub(crate) triple_o: Vec<u64>,
    pub(crate) graph_ids: Vec<u8>,
    pub(crate) graph_kinds: Vec<u8>,
    pub(crate) graph_str_flat: Vec<u8>,
    pub(crate) graph_str_offsets: Vec<u32>,
}

// ── CompactedTierHandle ──────────────────────────────────────────────────────

/// A compacted-tier handle: either the owned in-memory form (built by
/// [`CompactedTier::build`]) or a **block-cached** form whose Front-Coded
/// `buf` is lz4-blocked with a bounded decompress LRU.
///
/// Both variants expose the same read-only method surface so
/// `Dictionary::get_term_arc` / `get_id` do not care which substrate is live.
pub(crate) enum CompactedTierHandle {
    Owned(Box<CompactedTier>),
    /// Indexes resident; `buf` decompressed on demand into a small LRU.
    BlockCached(Box<crate::dict_block_cache::BlockCachedCompactedTier>),
}

impl CompactedTierHandle {
    pub(crate) fn get_id(&self, term: &Term) -> Option<u64> {
        match self {
            CompactedTierHandle::Owned(t) => t.get_id(term),
            CompactedTierHandle::BlockCached(t) => t.get_id(term),
        }
    }

    pub(crate) fn decode_id(&self, id: u64) -> Option<Result<Arc<Term>, Oxigraph>> {
        match self {
            CompactedTierHandle::Owned(t) => t.decode_id(id),
            CompactedTierHandle::BlockCached(t) => t.decode_id(id),
        }
    }

    pub(crate) fn decode_block_for_id(
        &self,
        id: u64,
    ) -> Option<Vec<(u64, Result<Arc<Term>, Oxigraph>)>> {
        match self {
            CompactedTierHandle::Owned(t) => t.decode_block_for_id(id),
            CompactedTierHandle::BlockCached(t) => t.decode_block_for_id(id),
        }
    }

    pub(crate) fn mem_size_bytes(&self) -> usize {
        match self {
            CompactedTierHandle::Owned(t) => t.mem_size_bytes(),
            CompactedTierHandle::BlockCached(t) => t.mem_size_bytes(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            CompactedTierHandle::Owned(t) => t.is_empty(),
            CompactedTierHandle::BlockCached(t) => t.is_empty(),
        }
    }

    /// Test-only accessor for directly inspecting the owned tier's
    /// permutation arrays (see `dict.rs`'s `#[cfg(test)]` module). Panics if
    /// this handle is not `Owned`.
    #[cfg(test)]
    pub(crate) fn expect_owned(&self) -> &CompactedTier {
        match self {
            CompactedTierHandle::Owned(t) => t,
            CompactedTierHandle::BlockCached(_) => {
                panic!("expected an Owned compacted tier, got BlockCached")
            }
        }
    }
}
