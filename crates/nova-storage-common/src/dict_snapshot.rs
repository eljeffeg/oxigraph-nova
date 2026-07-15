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
//! [`oxigraph_nova_core::Dictionary::from_block_cached`] *before* replaying
//! the WAL tail — so replay's `intern()` calls only ever append new terms
//! after the snapshot's high-water-mark.
//!
//! This is a generic "any backend using `nova-core::Dictionary`" concern,
//! not specific to any particular index structure — any storage backend
//! (Ring, RocksDB, ...) that reuses `Dictionary` for term interning needs
//! byte-for-byte the same persistence strategy.
//!
//! ## On-disk format — lz4 block container
//!
//! `Dictionary::to_snapshot()` (in `oxigraph-nova-core`) produces the entire
//! persistable representation as a single
//! `oxigraph_nova_core::DictSnapshot` — the compacted tier's Front-Coded
//! byte buffer plus its `id2rank`/`rank2id` bit-packed permutation arrays
//! (see `oxigraph_nova_core::dict_compact`'s module docs), every
//! quoted-triple term (as parallel `TermId` arrays), and the graph table.
//!
//! The write path wraps that snapshot in the [`crate::dict_lz4`] container:
//! a slim ε-serde index payload (`DictIndexSnapshot` with empty
//! `compacted.buf`) plus independent size-prepended lz4 blocks over the
//! Front-Coded `buf` (default 64 KiB blocks). Open reconstructs a
//! `Dictionary` via a bounded decompress LRU over those blocks rather than
//! materializing the full buffer.
//!
//! **Why compress the dict but not the index:** the navigable LOUDS index
//! (`nova.snapshot.<gen>`) must stay uncompressed mmap for larger-than-
//! memory zero-copy LFTJ. The dictionary is a separate file, accessed via
//! point decode + a TermId LRU, so residual lz4 after Front-Coding is a
//! pure residency/disk win without blocking zero-copy index navigation.

use crate::dict_lz4::{self, DEFAULT_BLOCK_SIZE};
use oxigraph_nova_core::{Dictionary, Oxigraph};
use std::path::{Path, PathBuf};

fn tmp_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Serialize `dict`'s full persistable state to `path` as an lz4 block
/// container (tmp-file + atomic rename, matching `manifest.rs` /
/// `nova-storage-ring`'s snapshot write discipline), then load it back
/// and reconstruct a `Dictionary`.
///
/// Requires that `dict` has just been `compact()`ed (its compacted tier
/// must be the `Owned` variant — see
/// [`oxigraph_nova_core::Dictionary::to_snapshot`]'s doc comment); this is
/// always true at `commit_compaction`'s call site, which calls
/// `dict.compact()` immediately before persisting.
pub fn write_and_load_mmap(dict: &Dictionary, path: &Path) -> Result<Dictionary, Oxigraph> {
    let snap = dict.to_snapshot();
    let container = dict_lz4::compress_dict(snap, DEFAULT_BLOCK_SIZE)?;

    let tmp_path = tmp_sibling(path);
    std::fs::write(&tmp_path, &container)
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot write failed: {e}")))?;
    std::fs::rename(&tmp_path, path)
        .map_err(|e| Oxigraph::Storage(format!("dict snapshot rename failed: {e}")))?;

    load_mmap_from_file(path)
}

/// Load a `Dictionary` from `path`.
///
/// Files must be a `NOVA_DICT_LZ4` container (the only write format).
/// Returns a fresh, empty `Dictionary::new()` if `path` doesn't exist
/// (fresh store, or a persistent store that has never been compacted).
///
/// Used by `RingStore::open()` and by [`write_and_load_mmap`].
///
/// Requires the `mmap` cargo feature (default-on; disabled for the wasm32
/// build, see this crate's `Cargo.toml`). Disk-backed persistence
/// (`RingStore::open`) is unavailable without it — see the `not(feature =
/// "mmap")` fallback below.
#[cfg(feature = "mmap")]
pub fn load_mmap_from_file(path: &Path) -> Result<Dictionary, Oxigraph> {
    if !path.exists() {
        return Ok(Dictionary::new());
    }
    if !dict_lz4::path_is_lz4_container(path)? {
        return Err(Oxigraph::Storage(format!(
            "dict snapshot: expected NOVA_DICT_LZ4 container at {}",
            path.display()
        )));
    }
    let container =
        std::fs::read(path).map_err(|e| Oxigraph::Storage(format!("dict lz4 read failed: {e}")))?;
    dict_lz4::load_dictionary_from_container(&container)
}

/// `mmap`-disabled fallback (see the gated definition above): disk-backed
/// dictionary persistence is unavailable in this build.
#[cfg(not(feature = "mmap"))]
pub fn load_mmap_from_file(_path: &Path) -> Result<Dictionary, Oxigraph> {
    Err(Oxigraph::Storage(
        "disk-backed persistence requires the \"mmap\" cargo feature, which is disabled in this build".into(),
    ))
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

        let on_disk = std::fs::read(&path).unwrap();
        assert!(
            dict_lz4::is_lz4_container(&on_disk),
            "write_and_load_mmap must produce a NOVA_DICT_LZ4 container"
        );

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
        let on_disk = std::fs::read(&path).unwrap();
        assert!(dict_lz4::is_lz4_container(&on_disk));
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

    /// Large-enough dict that spans multiple lz4 blocks still round-trips
    /// every TermId.
    #[test]
    fn multi_block_dict_round_trip() {
        let mut dict = Dictionary::new();
        let mut ids = Vec::new();
        // Enough unique literals to force multi-block Front-Coded and lz4
        // coverage under the default block size.
        for i in 0..20_000 {
            let t = Term::Literal(Literal::new_simple_literal(format!(
                "label for entity {i:05} with some padding text to bulk the dict"
            )));
            ids.push(dict.intern(&t).unwrap());
        }
        dict.compact().unwrap();

        let path = temp_path("multi_block");
        let _ = std::fs::remove_file(&path);
        let loaded = write_and_load_mmap(&dict, &path).unwrap();

        let on_disk = std::fs::read(&path).unwrap();
        assert!(dict_lz4::is_lz4_container(&on_disk));
        // Container header: raw_len at [24..32) is the Front-Coded buf length.
        let raw_len = u64::from_le_bytes(on_disk[24..32].try_into().unwrap());
        assert!(raw_len > 0);

        for (i, &id) in ids.iter().enumerate() {
            let expected = Term::Literal(Literal::new_simple_literal(format!(
                "label for entity {i:05} with some padding text to bulk the dict"
            )));
            assert_eq!(loaded.get_id(&expected), Some(id));
            assert_eq!(loaded.get_term_arc(id).as_deref(), Some(&expected));
        }

        let _ = std::fs::remove_file(&path);
    }
}
