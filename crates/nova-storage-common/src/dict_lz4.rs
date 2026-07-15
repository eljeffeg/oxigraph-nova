//! lz4_flex block-container for `nova.dict.<gen>`.
//!
//! On-disk layout (version 2):
//! - fixed header (`NOVA_DICT_LZ4` magic + version + block_size + raw_len + n_blocks)
//! - slim ε-serde **index** payload ([`DictIndexSnapshot`] — a `DictSnapshot`
//!   with empty `compacted.buf`)
//! - independent size-prepended lz4 blocks over the Front-Coded `buf` only
//!
//! Open installs [`oxigraph_nova_core::Dictionary::from_block_cached`] so cold
//! RSS tracks indexes + compressed buf + a small decompress LRU, not the full
//! Front-Coded arena. The navigable LOUDS index (`nova.snapshot.*`) stays
//! uncompressed mmap; only the dictionary uses this container.

use epserde::deser::Deserialize;
use epserde::ser::Serialize;
use lz4_flex::block::compress_prepend_size;
use oxigraph_nova_core::{
    DEFAULT_BUF_BLOCK_CACHE, DEFAULT_BUF_LZ4_BLOCK, DictIndexSnapshot, DictSnapshot, Dictionary,
    Oxigraph,
};
use std::io::{Cursor, Read};
use std::path::Path;

/// Default uncompressed lz4 block size over the Front-Coded `buf`.
pub const DEFAULT_BLOCK_SIZE: usize = DEFAULT_BUF_LZ4_BLOCK;

/// File magic (16 bytes, NUL-padded).
pub const MAGIC: &[u8; 16] = b"NOVA_DICT_LZ4\0\0\0";

/// Container format version.
pub const VERSION: u32 = 2;

const HEADER_FIXED_LEN: usize = 40; // magic + version + block_size + raw_len + n_blocks + pad

#[inline]
pub fn is_lz4_container(bytes: &[u8]) -> bool {
    bytes.len() >= MAGIC.len() && &bytes[..MAGIC.len()] == MAGIC.as_slice()
}

pub fn path_is_lz4_container(path: &Path) -> Result<bool, Oxigraph> {
    let mut f = std::fs::File::open(path)
        .map_err(|e| Oxigraph::Storage(format!("dict lz4 open failed: {e}")))?;
    let mut magic = [0u8; 16];
    match f.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == MAGIC),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(Oxigraph::Storage(format!("dict lz4 peek failed: {e}"))),
    }
}

/// Compress a full owned [`DictSnapshot`] into a container:
/// index payload (empty `buf`) + lz4 blocks over the real Front-Coded `buf`.
pub fn compress_dict(snap: DictSnapshot, block_size: usize) -> Result<Vec<u8>, Oxigraph> {
    if block_size == 0 {
        return Err(Oxigraph::Storage(
            "dict lz4: block_size must be non-zero".into(),
        ));
    }
    let (index, buf) = DictIndexSnapshot::from_full_snapshot(snap);
    let mut index_bytes: Vec<u8> = Vec::new();
    unsafe {
        index
            .serialize(&mut index_bytes)
            .map_err(|e| Oxigraph::Storage(format!("dict index serialize failed: {e}")))?;
    }

    let n_blocks = if buf.is_empty() {
        0
    } else {
        buf.len().div_ceil(block_size)
    };
    let mut compressed_blocks: Vec<Vec<u8>> = Vec::with_capacity(n_blocks);
    let mut i = 0usize;
    while i < buf.len() {
        let end = (i + block_size).min(buf.len());
        compressed_blocks.push(compress_prepend_size(&buf[i..end]));
        i = end;
    }

    // Layout after fixed header:
    //   index_len: u64
    //   index_bytes
    //   offsets: (n_blocks+1)×u64  (relative to start of compressed payload)
    //   compressed payload
    let offsets_bytes = (n_blocks + 1) * 8;
    let payload_len: usize = compressed_blocks.iter().map(|b| b.len()).sum();
    let after_fixed = 8 + index_bytes.len() + offsets_bytes + payload_len;
    let mut out = Vec::with_capacity(HEADER_FIXED_LEN + after_fixed);

    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(block_size as u32).to_le_bytes());
    out.extend_from_slice(&(buf.len() as u64).to_le_bytes()); // raw_len = FC buf len
    out.extend_from_slice(&(n_blocks as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // pad

    out.extend_from_slice(&(index_bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(&index_bytes);

    let offsets_start = out.len();
    out.resize(out.len() + offsets_bytes, 0);

    let payload_base = out.len();
    let mut rel_offsets: Vec<u64> = Vec::with_capacity(n_blocks + 1);
    for block in &compressed_blocks {
        rel_offsets.push((out.len() - payload_base) as u64);
        out.extend_from_slice(block);
    }
    rel_offsets.push((out.len() - payload_base) as u64);

    for (k, off) in rel_offsets.iter().enumerate() {
        let at = offsets_start + k * 8;
        out[at..at + 8].copy_from_slice(&off.to_le_bytes());
    }

    Ok(out)
}

/// Open a `NOVA_DICT_LZ4` container, or reject non-magic / wrong-version bytes.
pub fn load_dictionary_from_container(container: &[u8]) -> Result<Dictionary, Oxigraph> {
    if !is_lz4_container(container) {
        return Err(Oxigraph::Storage(
            "dict lz4: missing NOVA_DICT_LZ4 magic".into(),
        ));
    }
    if container.len() < HEADER_FIXED_LEN {
        return Err(Oxigraph::Storage(
            "dict lz4: container truncated in fixed header".into(),
        ));
    }
    let version = u32::from_le_bytes(container[16..20].try_into().unwrap());
    if version != VERSION {
        return Err(Oxigraph::Storage(format!(
            "dict lz4: unsupported version {version}"
        )));
    }
    load_container(container)
}

fn load_container(container: &[u8]) -> Result<Dictionary, Oxigraph> {
    let block_size = u32::from_le_bytes(container[20..24].try_into().unwrap()) as usize;
    let raw_len = u64::from_le_bytes(container[24..32].try_into().unwrap()) as usize;
    let n_blocks = u32::from_le_bytes(container[32..36].try_into().unwrap()) as usize;
    if block_size == 0 {
        return Err(Oxigraph::Storage("dict lz4: block_size is zero".into()));
    }
    let expected_blocks = if raw_len == 0 {
        0
    } else {
        raw_len.div_ceil(block_size)
    };
    if expected_blocks != n_blocks {
        return Err(Oxigraph::Storage(format!(
            "dict lz4: n_blocks {n_blocks} inconsistent with raw_len {raw_len} / block_size {block_size}"
        )));
    }

    let mut pos = HEADER_FIXED_LEN;
    if container.len() < pos + 8 {
        return Err(Oxigraph::Storage(
            "dict lz4: truncated before index_len".into(),
        ));
    }
    let index_len = u64::from_le_bytes(container[pos..pos + 8].try_into().unwrap()) as usize;
    pos += 8;
    if container.len() < pos + index_len {
        return Err(Oxigraph::Storage(
            "dict lz4: truncated in index payload".into(),
        ));
    }
    let index_bytes = &container[pos..pos + index_len];
    pos += index_len;

    let offsets_bytes = (n_blocks + 1) * 8;
    if container.len() < pos + offsets_bytes {
        return Err(Oxigraph::Storage(
            "dict lz4: truncated in offset table".into(),
        ));
    }
    let mut rel_offsets = Vec::with_capacity(n_blocks + 1);
    for k in 0..=n_blocks {
        let at = pos + k * 8;
        rel_offsets.push(u64::from_le_bytes(
            container[at..at + 8].try_into().unwrap(),
        ));
    }
    pos += offsets_bytes;
    let payload = &container[pos..];

    // Rebuild absolute-style offsets into a contiguous payload slice for Lz4BufBlocks.
    let mut data = Vec::with_capacity(payload.len());
    let mut abs_offsets = Vec::with_capacity(n_blocks + 1);
    for b in 0..n_blocks {
        let start = rel_offsets[b] as usize;
        let end = rel_offsets[b + 1] as usize;
        if end > payload.len() || start > end {
            return Err(Oxigraph::Storage(format!(
                "dict lz4: block {b} offsets out of range"
            )));
        }
        abs_offsets.push(data.len() as u64);
        data.extend_from_slice(&payload[start..end]);
    }
    abs_offsets.push(data.len() as u64);

    // Deserialize index (full heap copy — small).
    let mut cursor = Cursor::new(index_bytes);
    let index_snap: DictIndexSnapshot = unsafe {
        DictIndexSnapshot::deserialize_full(&mut cursor)
            .map_err(|e| Oxigraph::Storage(format!("dict index deserialize failed: {e}")))?
    };

    if index_snap.buf_len as usize != raw_len {
        return Err(Oxigraph::Storage(format!(
            "dict lz4: header raw_len {raw_len} != index buf_len {}",
            index_snap.buf_len
        )));
    }

    let next_graph_id = index_snap.next_graph_id;
    let triple_ids = index_snap.triple_ids.clone();
    let triple_s = index_snap.triple_s.clone();
    let triple_p = index_snap.triple_p.clone();
    let triple_o = index_snap.triple_o.clone();
    let graph_ids = index_snap.graph_ids.clone();
    let graph_kinds = index_snap.graph_kinds.clone();
    let graph_str_flat = index_snap.graph_str_flat.clone();
    let graph_str_offsets = index_snap.graph_str_offsets.clone();

    let cached = oxigraph_nova_core::dict_block_cache_types::from_parts(
        index_snap,
        block_size,
        raw_len,
        abs_offsets,
        data,
        DEFAULT_BUF_BLOCK_CACHE,
    )?;

    Dictionary::from_block_cached(
        cached,
        next_graph_id,
        triple_ids,
        triple_s,
        triple_p,
        triple_o,
        graph_ids,
        graph_kinds,
        graph_str_flat,
        graph_str_offsets,
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{Literal, NamedNode, Term};

    #[test]
    fn round_trip_terms() {
        let mut dict = oxigraph_nova_core::Dictionary::new();
        let mut ids = Vec::new();
        for i in 0..200 {
            ids.push(
                dict.intern(&Term::NamedNode(NamedNode::new_unchecked(format!(
                    "http://ex/{i}"
                ))))
                .unwrap(),
            );
            ids.push(
                dict.intern(&Term::Literal(Literal::new_simple_literal(format!(
                    "label {i}"
                ))))
                .unwrap(),
            );
        }
        dict.compact().unwrap();
        let snap = dict.to_snapshot();
        let container = compress_dict(snap, 4096).unwrap();
        assert!(is_lz4_container(&container));
        let version = u32::from_le_bytes(container[16..20].try_into().unwrap());
        assert_eq!(version, VERSION);

        let loaded = load_dictionary_from_container(&container).unwrap();
        for &id in &ids {
            let expected = dict.get_term_arc(id).unwrap();
            assert_eq!(loaded.get_term_arc(id).as_deref(), Some(expected.as_ref()));
            assert_eq!(loaded.get_id(expected.as_ref()), Some(id));
        }
    }
}
