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
//! The fix: persist the `Dictionary`'s state (the compacted Front-Coded
//! tier, every quoted-triple term, and the `GraphId ↔ GraphName` mapping)
//! alongside every snapshot generation, and reconstruct it via
//! [`oxigraph_nova_core::Dictionary::from_mapped`] *before* replaying the
//! WAL tail — so replay's `intern()` calls only ever append new terms after
//! the snapshot's high-water-mark.
//!
//! This is a generic "any backend using `nova-core::Dictionary`" concern,
//! not specific to any particular index structure — any storage backend
//! (Ring, RocksDB, ...) that reuses `Dictionary` for term interning needs
//! byte-for-byte the same persistence strategy.
//!
//! ## On-disk format — zero-copy ε-serde, uncompressed
//!
//! `Dictionary::to_snapshot()` (in `oxigraph-nova-core`) already produces
//! the entire persistable representation as a single
//! `oxigraph_nova_core::DictSnapshot` — the compacted tier's Front-Coded
//! byte buffer plus its `id2rank`/`rank2id` bit-packed permutation arrays
//! (see `oxigraph_nova_core::dict_compact`'s module docs), every
//! quoted-triple term (as parallel `TermId` arrays), and the graph table.
//! This module's only job is to serialize that struct to disk via ε-serde
//! and load it back — `save`/`load` are thin wrappers around
//! `Dictionary::to_snapshot`/`Dictionary::from_mapped`.
//!
//! Like `nova-storage-ring`'s `nova.snapshot.<gen>`, this file is written
//! **uncompressed** (no zstd) so it can be `load_mmap`'d directly: mmap-based
//! zero-copy loading is only possible against a file whose bytes are
//! byte-identical to the in-memory ε-serde layout, which compression would
//! break. This trades the previous zstd-compressed on-disk footprint for a
//! genuinely mapped (not merely process-resident) compacted dictionary tier
//! — the same trade-off already made for the Ring index's
//! `nova.snapshot.<gen>` (see `nova-storage-ring`'s `snapshot.rs` module
//! docs).

use epserde::deser::{Deserialize, Flags};
use epserde::ser::Serialize;
use oxigraph_nova_core::{DictSnapshot, Dictionary, Oxigraph};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tmp_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Serialize `dict`'s full persistable state to `path` (tmp-file + atomic
/// rename, matching `manifest.rs`/`nova-storage-ring`'s snapshot write
/// discipline), then `load_mmap` the just-written file back in and
/// reconstruct a `Dictionary` whose compacted tier is a genuine zero-copy
/// view of exactly the bytes on disk — never a redundant owned heap copy.
///
/// Requires that `dict` has just been `compact()`ed (its compacted tier
/// must be the `Owned` variant — see
/// [`oxigraph_nova_core::Dictionary::to_snapshot`]'s doc comment); this is
/// always true at `commit_compaction`'s call site, which calls
/// `dict.compact()` immediately before persisting.
pub fn write_and_load_mmap(dict: &Dictionary, path: &Path) -> Result<Dictionary, Oxigraph> {
    let snap = dict.to_snapshot();
    let mut buf: Vec<u8> = Vec::new();
    unsafe {
        snap.serialize(&mut buf)
            .map_err(|e| Oxigraph::Storage(format!("dict snapshot serialize failed: {e}")))?;
    }

    let tmp_path = tmp_sibling(path);
    std::fs::write(&tmp_path, &buf)
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot write failed: {e}")))?;
    std::fs::rename(&tmp_path, path)
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot rename failed: {e}")))?;

    load_mmap_from_file(path)
}

/// Load a `Dictionary` from `path` via zero-copy `load_mmap` (rather than a
/// full heap-copy deserialize). Returns a fresh, empty `Dictionary::new()`
/// if `path` doesn't exist (fresh store, or a persistent store that has
/// never been compacted).
///
/// Used by `RingStore::open()` (so a reopened persistent store's dictionary
/// is zero-copy mapped from the moment it's loaded, not just after the next
/// `compact()`) and by [`write_and_load_mmap`] (right after writing a fresh
/// snapshot generation during `commit_compaction`).
pub fn load_mmap_from_file(path: &Path) -> Result<Dictionary, Oxigraph> {
    if !path.exists() {
        return Ok(Dictionary::new());
    }
    let mem = Arc::new(unsafe {
        DictSnapshot::load_mmap(path, Flags::empty())
            .map_err(|e| Oxigraph::Storage(format!("dict snapshot load_mmap failed: {e}")))?
    });
    Dictionary::from_mapped(mem)
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
        dict.compact().unwrap();

        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let loaded = write_and_load_mmap(&dict, &path).unwrap();

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
        let loaded = write_and_load_mmap(&dict, &path).unwrap();
        assert!(loaded.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_yields_fresh_dictionary() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        let loaded = load_mmap_from_file(&path).unwrap();
        assert!(loaded.is_empty());
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
        dict.compact().unwrap();

        let path = temp_path("shared_prefix");
        let _ = std::fs::remove_file(&path);
        let loaded = write_and_load_mmap(&dict, &path).unwrap();

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
        dict.compact().unwrap();

        let path = temp_path("quoted_triple");
        let _ = std::fs::remove_file(&path);
        let loaded = write_and_load_mmap(&dict, &path).unwrap();

        assert_eq!(loaded.get_id(&triple_term), Some(triple_id));
        assert_eq!(
            loaded.get_term_arc(triple_id).as_deref(),
            Some(&triple_term)
        );

        let _ = std::fs::remove_file(&path);
    }
}
