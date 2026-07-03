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
//! ## On-disk format
//!
//! All integers little-endian:
//!
//! ```text
//! [u8 next_graph_id]
//! [u32 term_count]
//! term_count * [ encoded Term, RDF-star-aware — reuses wal.rs's
//!                write_object_term/read_object_term so quoted-triple
//!                terms round-trip correctly ]
//! [u32 graph_count]
//! graph_count * [ u8 graph_id, encoded GraphName via
//!                 wal.rs's write_graph_name/read_graph_name ]
//! ```
//!
//! No length-prefix/CRC framing at the record level (unlike `wal.rs`) since
//! this file is written once, atomically (tmp + rename), and never appended
//! to — a torn write can only happen if the process crashes mid-write, in
//! which case the tmp file simply never gets renamed and the previous
//! generation's dict file (referenced by the previous MANIFEST) remains the
//! durable truth.

use crate::wal::{
    read_graph_name, read_object_term, read_tag, write_graph_name, write_object_term,
};
use oxigraph_nova_core::{Dictionary, Oxigraph};
use std::path::{Path, PathBuf};

/// Serialize `dict`'s full persistable state to `path` (tmp-file + atomic
/// rename, matching `manifest.rs`/backend snapshot write discipline).
pub fn save(dict: &Dictionary, path: &Path) -> Result<(), Oxigraph> {
    let mut buf = Vec::new();

    buf.push(dict.next_graph_id_raw());

    let terms: Vec<_> = dict.terms_in_order().collect();
    buf.extend_from_slice(&(terms.len() as u32).to_le_bytes());
    for term in terms {
        write_object_term(&mut buf, term);
    }

    let graphs: Vec<_> = dict.all_graphs().collect();
    buf.extend_from_slice(&(graphs.len() as u32).to_le_bytes());
    for (gid, gname) in graphs {
        buf.push(gid.as_u8());
        write_graph_name(&mut buf, gname);
    }

    let tmp_path = tmp_sibling(path);
    std::fs::write(&tmp_path, &buf)
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot write failed: {e}")))?;
    std::fs::rename(&tmp_path, path)
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot rename failed: {e}")))?;
    Ok(())
}

/// Load a `Dictionary` previously written by [`save`], reconstructing exact
/// `TermId`/`GraphId` assignments via [`Dictionary::rebuild`].
pub fn load(path: &Path) -> Result<Dictionary, Oxigraph> {
    let data = std::fs::read(path)
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot read failed: {e}")))?;
    let mut pos = 0usize;

    let next_graph_id = read_tag(&data, &mut pos)?;

    if pos + 4 > data.len() {
        return Err(Oxigraph::Storage(
            "dict snapshot: truncated term count".into(),
        ));
    }
    let term_count = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;

    let mut terms = Vec::with_capacity(term_count);
    for _ in 0..term_count {
        terms.push(read_object_term(&data, &mut pos)?);
    }

    if pos + 4 > data.len() {
        return Err(Oxigraph::Storage(
            "dict snapshot: truncated graph count".into(),
        ));
    }
    let graph_count = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;

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
}
