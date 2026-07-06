//! On-disk persistence for `Dictionary` state — `nova.dict.<gen>` files.
//!
//! ## Why this exists
//!
//! A MANIFEST + segment-rotation scheme (see `manifest.rs`) replays only the
//! *tail* WAL segment(s) on top of a loaded snapshot generation, instead of
//! the full WAL history. Under a naive "always replay everything from byte 0"
//! model, a fresh `Dictionary::new()` plus full replay would be safe: the
//! same terms get interned in the same order every time, producing
//! identical `TermId`s deterministically.
//!
//! Under tail-only replay, that assumption breaks: `open()` loads a snapshot
//! generation whose index structures embed raw `TermId`/`GraphId` integers,
//! and then replays *only* the WAL segment(s) written after that snapshot
//! was taken. If the `Dictionary` used during replay starts empty,
//! `intern()` reassigns `TermId`s starting from `0` for whatever terms
//! appear in the tail — colliding with (rather than extending) the ID space
//! the loaded snapshot's index structures already assume, corrupting or
//! losing data.
//!
//! The fix: persist the `Dictionary`'s state (every interned term, in
//! `TermId` order, plus the `GraphId ↔ GraphName` mapping and the next free
//! `GraphId`) alongside every snapshot generation, and reconstruct it via
//! [`oxigraph_nova_core::Dictionary::rebuild`] *before* replaying the WAL
//! tail — so replay's `intern()` calls only ever append new terms after the
//! snapshot's high-water-mark.
//!
//! This is a generic "any backend using `nova-core::Dictionary`" concern,
//! not specific to any particular index structure — any storage backend
//! (Ring, RocksDB, ...) that reuses `Dictionary` for term interning needs
//! byte-for-byte the same persistence strategy.
//!
//! ## On-disk format — Plain Front-Coding (PFC) + permutation
//!
//! `TermId`s are still assigned
//! in pure insertion order in memory (`Dictionary` itself is untouched by
//! this module) — only the *serialized bytes* are reorganized: terms are
//! sorted by their "primary" string payload (IRI / blank-node id / literal
//! value) so that adjacent entries share long common prefixes, then each
//! entry stores only the suffix past its predecessor's longest common
//! prefix (LCP) instead of the full string. A permutation array
//! (`rank2id`) records which original `TermId` each sorted entry belongs
//! to, so [`load`] can reconstruct `terms: Vec<Term>` in exact original
//! `TermId` order before calling
//! [`Dictionary::rebuild`](oxigraph_nova_core::Dictionary::rebuild) —
//! callers never observe the sorted/FC-encoded representation.
//!
//! All integers little-endian, `lcp`/suffix-length/aux-length fields are
//! unsigned LEB128 varints (cheap for the common case of short lengths):
//!
//! ```text
//! [u8 next_graph_id]
//! [u32 term_count]
//! term_count * [ varint lcp                 -- bytes shared with previous entry's primary string
//!                 varint suffix_len          -- primary string bytes NOT shared
//!                 suffix_len bytes           -- the suffix itself
//!                 u8 tag                     -- term kind (see `term_sort_key`/`term_from_parts`)
//!                 varint aux_len
//!                 aux_len bytes ]            -- kind-specific extra fields (lang/datatype/
//!                                                direction, or a fully self-contained nested
//!                                                encoding for RDF-star quoted-triple terms)
//! term_count * [ u64 original TermId ]       -- rank2id permutation, one entry per sorted rank
//! [u32 graph_count]
//! graph_count * [ u8 graph_id, encoded GraphName via
//!                 wal.rs's write_graph_name/read_graph_name ]
//! ```
//!
//! Quoted-triple (`Term::Triple`) terms carry no useful shared string
//! prefix, so they are not front-coded: their `primary` key is empty and
//! their `aux` payload is simply the existing recursive
//! `wal::write_object_term` encoding (tag 6, self-decodable via
//! `wal::read_object_term`).
//!
//! No length-prefix/CRC framing at the record level (unlike `wal.rs`) since
//! this file is written once, atomically (tmp + rename), and never appended
//! to — a torn write can only happen if the process crashes mid-write, in
//! which case the tmp file simply never gets renamed and the previous
//! generation's dict file (referenced by the previous MANIFEST) remains the
//! durable truth.
//!
//! ## Compression
//!
//! The FC-encoded buffer above is *additionally* compressed with zstd
//! (level 3 — fast, good ratio) before being written to disk, and
//! transparently decompressed on load — the two layers are complementary:
//! FC removes cross-string redundancy that a generic compressor would
//! otherwise have to rediscover from scratch on every run, while zstd still
//! mops up whatever intra-string / structural redundancy remains.
//! zstd-3 alone already achieves ~25× on typical
//! BSBM-style data, so this phase's primary value is validating the
//! encode/sort/permute/decode round trip against real data ahead of the
//! (much higher-risk) in-memory two-tier FC dictionary, not a large
//! additional disk-size win.

use crate::wal::{
    read_graph_name, read_object_term, read_string, read_tag, write_graph_name, write_object_term,
    write_string,
};
use oxigraph_nova_core::{BlankNode, Dictionary, Literal, NamedNode, Oxigraph, Term};
use std::path::{Path, PathBuf};

// ── Varint (unsigned LEB128) ─────────────────────────────────────────────────

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

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64, Oxigraph> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        if *pos >= buf.len() {
            return Err(Oxigraph::Storage("dict snapshot: truncated varint".into()));
        }
        let byte = buf[*pos];
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return Err(Oxigraph::Storage("dict snapshot: varint too long".into()));
        }
    }
    Ok(result)
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32, Oxigraph> {
    if *pos + 4 > buf.len() {
        return Err(Oxigraph::Storage("dict snapshot: truncated u32".into()));
    }
    let v = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    Ok(v)
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

// ── Term ↔ (tag, primary, aux) ────────────────────────────────────────────────
//
// `primary` is the string payload used for sorting + front-coding (the part
// with real cross-term prefix redundancy: IRIs, blank-node ids, literal
// values). `aux` carries whatever else is needed to fully reconstruct the
// term (language tag, datatype IRI, base direction, or — for quoted triples,
// which have no useful `primary` — the entire nested encoding).
//
// Tags 0..=5 mirror `wal.rs`'s term tag scheme; tag 6 (quoted triple) reuses
// `write_object_term`/`read_object_term` wholesale for `aux` rather than
// duplicating subject/predicate/object encoding here.

fn term_sort_key(term: &Term) -> (u8, Vec<u8>) {
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
        Term::Triple(_) => (6, Vec::new()),
    }
}

fn term_aux_bytes(term: &Term) -> Vec<u8> {
    let mut buf = Vec::new();
    match term {
        Term::NamedNode(_) | Term::BlankNode(_) => {}
        Term::Literal(l) => {
            if let Some(direction) = l.direction() {
                write_string(&mut buf, l.language().unwrap_or(""));
                buf.push(match direction {
                    oxrdf::BaseDirection::Ltr => 0,
                    oxrdf::BaseDirection::Rtl => 1,
                });
            } else if let Some(lang) = l.language() {
                write_string(&mut buf, lang);
            } else if l.datatype() == oxrdf::vocab::xsd::STRING {
                // simple literal: nothing extra to store
            } else {
                write_string(&mut buf, l.datatype().as_str());
            }
        }
        Term::Triple(_) => {
            // Self-contained: already carries its own tag (6) + recursively
            // encoded subject/predicate/object, decodable via
            // `read_object_term` alone with no outer context needed.
            write_object_term(&mut buf, term);
        }
    }
    buf
}

fn term_from_parts(tag: u8, primary: Vec<u8>, aux: &[u8]) -> Result<Term, Oxigraph> {
    fn utf8(bytes: Vec<u8>) -> Result<String, Oxigraph> {
        String::from_utf8(bytes)
            .map_err(|e| Oxigraph::Storage(format!("dict snapshot: invalid utf8: {e}")))
    }
    match tag {
        0 => Ok(Term::NamedNode(NamedNode::new_unchecked(utf8(primary)?))),
        1 => Ok(Term::BlankNode(BlankNode::new_unchecked(utf8(primary)?))),
        2 => Ok(Term::Literal(Literal::new_simple_literal(utf8(primary)?))),
        3 => {
            let mut pos = 0usize;
            let lang = read_string(aux, &mut pos)?;
            Ok(Term::Literal(
                Literal::new_language_tagged_literal_unchecked(utf8(primary)?, lang),
            ))
        }
        4 => {
            let mut pos = 0usize;
            let lang = read_string(aux, &mut pos)?;
            let dir_byte = read_tag(aux, &mut pos)?;
            let direction = match dir_byte {
                0 => oxrdf::BaseDirection::Ltr,
                1 => oxrdf::BaseDirection::Rtl,
                other => {
                    return Err(Oxigraph::Storage(format!(
                        "dict snapshot: invalid direction byte {other}"
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
            let datatype = read_string(aux, &mut pos)?;
            Ok(Term::Literal(Literal::new_typed_literal(
                utf8(primary)?,
                NamedNode::new_unchecked(datatype),
            )))
        }
        6 => {
            let mut pos = 0usize;
            read_object_term(aux, &mut pos)
        }
        other => Err(Oxigraph::Storage(format!(
            "dict snapshot: unknown term tag {other}"
        ))),
    }
}

/// Serialize `dict`'s full persistable state to `path` (tmp-file + atomic
/// rename, matching `manifest.rs`/backend snapshot write discipline).
///
/// Terms are Front-Coded: sorted by `(tag, primary string)` so neighboring
/// entries share long common prefixes, then only the suffix past each
/// entry's LCP with its predecessor is stored, alongside a `rank2id`
/// permutation so [`load`] can restore the exact original `TermId`
/// assignment (see module docs).
pub fn save(dict: &Dictionary, path: &Path) -> Result<(), Oxigraph> {
    let mut buf = Vec::new();

    buf.push(dict.next_graph_id_raw());

    let terms: Vec<Term> = dict.terms_in_order().collect();
    let term_count = terms.len();

    buf.extend_from_slice(&(term_count as u32).to_le_bytes());

    // (tag, primary, aux, original_id), sorted by (tag, primary) so adjacent
    // entries share long common prefixes.
    let mut entries: Vec<(u8, Vec<u8>, Vec<u8>, u64)> = terms
        .iter()
        .enumerate()
        .map(|(id, t)| {
            let (tag, primary) = term_sort_key(t);
            let aux = term_aux_bytes(t);
            (tag, primary, aux, id as u64)
        })
        .collect();
    entries.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));

    let mut rank2id = Vec::with_capacity(term_count);
    let mut prev_primary: Vec<u8> = Vec::new();
    for (tag, primary, aux, orig_id) in &entries {
        let lcp = common_prefix_len(&prev_primary, primary);
        write_varint(&mut buf, lcp as u64);
        let suffix = &primary[lcp..];
        write_varint(&mut buf, suffix.len() as u64);
        buf.extend_from_slice(suffix);
        buf.push(*tag);
        write_varint(&mut buf, aux.len() as u64);
        buf.extend_from_slice(aux);

        rank2id.push(*orig_id);
        prev_primary = primary.clone();
    }

    for id in &rank2id {
        buf.extend_from_slice(&id.to_le_bytes());
    }

    let graphs: Vec<_> = dict.all_graphs().collect();
    buf.extend_from_slice(&(graphs.len() as u32).to_le_bytes());
    for (gid, gname) in graphs {
        buf.push(gid.as_u8());
        write_graph_name(&mut buf, gname);
    }

    let compressed = zstd::encode_all(&buf[..], 3)
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot compress failed: {e}")))?;

    let tmp_path = tmp_sibling(path);
    std::fs::write(&tmp_path, &compressed)
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot write failed: {e}")))?;
    std::fs::rename(&tmp_path, path)
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot rename failed: {e}")))?;
    Ok(())
}

/// Load a `Dictionary` previously written by [`save`], reconstructing exact
/// `TermId`/`GraphId` assignments via [`Dictionary::rebuild`].
///
/// Decodes the Front-Coded sorted tier back into `terms: Vec<Term>` indexed
/// by *original* `TermId` (via the persisted `rank2id` permutation) before
/// calling `rebuild` — callers never observe the sorted/FC representation.
pub fn load(path: &Path) -> Result<Dictionary, Oxigraph> {
    let compressed = std::fs::read(path)
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot read failed: {e}")))?;
    let data = zstd::decode_all(&compressed[..])
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot decompress failed: {e}")))?;
    let mut pos = 0usize;

    let next_graph_id = read_tag(&data, &mut pos)?;
    let term_count = read_u32(&data, &mut pos)? as usize;

    let mut sorted_terms: Vec<Term> = Vec::with_capacity(term_count);
    let mut prev_primary: Vec<u8> = Vec::new();
    for _ in 0..term_count {
        let lcp = read_varint(&data, &mut pos)? as usize;
        if lcp > prev_primary.len() {
            return Err(Oxigraph::Storage(
                "dict snapshot: lcp exceeds previous entry length".into(),
            ));
        }
        let suffix_len = read_varint(&data, &mut pos)? as usize;
        if pos + suffix_len > data.len() {
            return Err(Oxigraph::Storage(
                "dict snapshot: truncated FC suffix".into(),
            ));
        }
        let mut primary = prev_primary[..lcp].to_vec();
        primary.extend_from_slice(&data[pos..pos + suffix_len]);
        pos += suffix_len;

        let tag = read_tag(&data, &mut pos)?;
        let aux_len = read_varint(&data, &mut pos)? as usize;
        if pos + aux_len > data.len() {
            return Err(Oxigraph::Storage("dict snapshot: truncated aux".into()));
        }
        let aux = &data[pos..pos + aux_len];
        pos += aux_len;

        let term = term_from_parts(tag, primary.clone(), aux)?;
        sorted_terms.push(term);
        prev_primary = primary;
    }

    let mut rank2id = Vec::with_capacity(term_count);
    for _ in 0..term_count {
        if pos + 8 > data.len() {
            return Err(Oxigraph::Storage(
                "dict snapshot: truncated rank2id entry".into(),
            ));
        }
        rank2id.push(u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()));
        pos += 8;
    }

    let mut terms_by_id: Vec<Option<Term>> = (0..term_count).map(|_| None).collect();
    for (rank, term) in sorted_terms.into_iter().enumerate() {
        let orig_id = rank2id[rank] as usize;
        if orig_id >= term_count {
            return Err(Oxigraph::Storage(
                "dict snapshot: rank2id entry out of range".into(),
            ));
        }
        terms_by_id[orig_id] = Some(term);
    }
    let mut terms = Vec::with_capacity(term_count);
    for opt in terms_by_id {
        terms.push(opt.ok_or_else(|| Oxigraph::Storage("dict snapshot: missing term id".into()))?);
    }

    let graph_count = read_u32(&data, &mut pos)? as usize;
    let mut graphs = Vec::with_capacity(graph_count);
    for _ in 0..graph_count {
        let gid = read_tag(&data, &mut pos)?;
        let gname = read_graph_name(&data, &mut pos)?;
        graphs.push((gid, gname));
    }

    Dictionary::rebuild(terms, graphs, next_graph_id)
}

fn tmp_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{GraphName, Literal, NamedNode, Term};

    fn temp_path(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("nova_dict_snapshot_test_{pid}_{n}_{name}"))
    }

    #[test]
    fn save_load_round_trip_preserves_term_ids() {
        let mut dict = Dictionary::new();
        let a = dict
            .intern(&Term::NamedNode(NamedNode::new_unchecked("http://ex/a")))
            .unwrap();
        let b = dict
            .intern(&Term::Literal(Literal::new_simple_literal("hello")))
            .unwrap();
        let g = dict
            .intern_graph(&GraphName::NamedNode(NamedNode::new_unchecked(
                "http://ex/g",
            )))
            .unwrap();

        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        save(&dict, &path).unwrap();

        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.get_id(&Term::NamedNode(NamedNode::new_unchecked("http://ex/a"))),
            Some(a)
        );
        assert_eq!(
            loaded.get_id(&Term::Literal(Literal::new_simple_literal("hello"))),
            Some(b)
        );
        assert_eq!(
            loaded.get_graph_id(&GraphName::NamedNode(NamedNode::new_unchecked(
                "http://ex/g"
            ))),
            Some(g)
        );
        // A fresh intern after reload must NOT collide with existing IDs.
        let mut loaded = loaded;
        let c = loaded
            .intern(&Term::NamedNode(NamedNode::new_unchecked("http://ex/c")))
            .unwrap();
        assert_ne!(c, a);
        assert_ne!(c, b);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_dict_round_trip() {
        let dict = Dictionary::new();
        let path = temp_path("empty");
        let _ = std::fs::remove_file(&path);
        save(&dict, &path).unwrap();
        let loaded = load(&path).unwrap();
        assert!(loaded.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn shared_prefix_terms_round_trip_out_of_sorted_order() {
        // Deliberately intern terms in an order that does NOT match sorted
        // (front-coding) order, to make sure the rank2id permutation
        // correctly restores original TermId order regardless of how the
        // on-disk tier happens to be sorted for compression.
        let mut dict = Dictionary::new();
        let ids: Vec<_> = [
            "http://example.org/product/999",
            "http://example.org/product/123",
            "http://example.org/product/1234",
            "http://example.org/other/abc",
        ]
        .iter()
        .map(|s| {
            dict.intern(&Term::NamedNode(NamedNode::new_unchecked(*s)))
                .unwrap()
        })
        .collect();

        let path = temp_path("shared_prefix");
        let _ = std::fs::remove_file(&path);
        save(&dict, &path).unwrap();
        let loaded = load(&path).unwrap();

        for (s, expected_id) in [
            "http://example.org/product/999",
            "http://example.org/product/123",
            "http://example.org/product/1234",
            "http://example.org/other/abc",
        ]
        .iter()
        .zip(ids.iter())
        {
            assert_eq!(
                loaded.get_id(&Term::NamedNode(NamedNode::new_unchecked(*s))),
                Some(*expected_id)
            );
            assert_eq!(
                loaded.get_term_arc(*expected_id).as_deref(),
                Some(&Term::NamedNode(NamedNode::new_unchecked(*s)))
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn quoted_triple_term_round_trips() {
        let mut dict = Dictionary::new();
        let s = Term::NamedNode(NamedNode::new_unchecked("http://ex/s"));
        let p = NamedNode::new_unchecked("http://ex/p");
        let o = Term::Literal(Literal::new_simple_literal("v"));
        let triple_term = Term::Triple(Box::new(oxrdf::Triple {
            subject: oxrdf::NamedOrBlankNode::NamedNode(NamedNode::new_unchecked("http://ex/s")),
            predicate: p.clone(),
            object: o.clone(),
        }));
        let _ = dict.intern(&s).unwrap();
        let _ = dict.intern(&Term::NamedNode(p.clone())).unwrap();
        let _ = dict.intern(&o).unwrap();
        let triple_id = dict.intern(&triple_term).unwrap();

        let path = temp_path("quoted_triple");
        let _ = std::fs::remove_file(&path);
        save(&dict, &path).unwrap();
        let loaded = load(&path).unwrap();

        assert_eq!(loaded.get_id(&triple_term), Some(triple_id));
        assert_eq!(
            loaded.get_term_arc(triple_id).as_deref(),
            Some(&triple_term)
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            let mut pos = 0usize;
            assert_eq!(read_varint(&buf, &mut pos).unwrap(), v);
            assert_eq!(pos, buf.len());
        }
    }
}
