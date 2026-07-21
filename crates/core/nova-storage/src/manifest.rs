//! MANIFEST — the single crash-safe commit point for a persistent
//! `QuadStore` backend's on-disk state.
//!
//! ## Why a MANIFEST at all
//!
//! A naive single mutable snapshot file, with the WAL replayed in full on
//! every `open()`, is safe but means the WAL grows unboundedly and startup
//! cost is O(total history), not O(data since last compaction).
//!
//! This MANIFEST fixes that with two changes tied together:
//!
//! - **Generation-numbered snapshots** (`nova.snapshot.<gen>`) instead of a
//!   single mutable file — a new generation is written in full before
//!   anything referencing it is committed, so a crash mid-write never
//!   corrupts the previously-committed generation.
//! - **Segment-numbered WAL files** (`nova.wal.<seq>`) instead of one
//!   ever-growing file — compaction rotates to a fresh empty segment, and
//!   `open()` only has to replay segments at-or-after the segment number
//!   recorded in the MANIFEST.
//!
//! The MANIFEST is the *only* file that is ever overwritten in place (always
//! via tmp-write + atomic rename), and it is what ties a specific snapshot
//! generation to "the WAL segment where live writes not yet in that
//! snapshot begin". Everything else (`nova.snapshot.<gen>`, `nova.wal.<seq>`)
//! is written once and never mutated after being made reachable from a
//! committed MANIFEST, which makes reasoning about crash windows tractable:
//! whatever the MANIFEST says right now is the durable truth, no matter what
//! partially-written files might also be sitting in the directory.
//!
//! This module has no knowledge of any particular index structure — it
//! operates purely on abstract snapshot-generation/WAL-segment numbers, and
//! is reusable by any storage backend that adopts the same
//! generation/segment persistence scheme.
//!
//! ## On-disk format (`v1`)
//!
//! A tiny, human-inspectable text format — not performance-sensitive since
//! it's read/written once per compaction, and being human-readable makes
//! debugging a corrupted data directory far easier than a binary format
//! would:
//!
//! ```text
//! NOVA_MANIFEST v1
//! snapshot_gen <u64>
//! wal_seq <u64>
//! crc32 <8 lowercase hex digits>
//! ```
//!
//! `crc32` is the CRC32 of the exact bytes of the first three lines
//! (including their trailing `\n`s), guarding against a partially-written or
//! bit-rotted MANIFEST being silently misread as valid (even though, per the
//! tmp+rename write discipline below, a torn MANIFEST should never happen in
//! practice — this is cheap insurance, not a load-bearing recovery
//! mechanism). A MANIFEST that fails to parse or fails its CRC check is
//! treated as **absent** if there is no other evidence of prior data in the
//! directory (fresh store), and as a **hard error** otherwise (a directory
//! that clearly has snapshot/WAL segment files but an unreadable MANIFEST
//! indicates real corruption that should not be silently papered over by
//! falling back to "empty store").
//!
//! ## Future evolution (documented limitation)
//!
//! This format assumes **exactly one active WAL segment** at a time — it
//! matches a single-writer concurrency model. If a backend ever grows
//! concurrent writers with multiple simultaneously-active segments, the
//! MANIFEST format would need to evolve (e.g. a `v2` header listing a *set*
//! of active segments rather than a single `wal_seq`); the version header
//! makes that migration detectable and unambiguous.

use oxigraph_nova_core::Oxigraph;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE_NAME: &str = "nova.manifest";
const HEADER_LINE: &str = "NOVA_MANIFEST v1";

/// The parsed contents of a `nova.manifest` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Manifest {
    /// The generation number of the currently-committed snapshot file
    /// (`nova.snapshot.<snapshot_gen>`). `0` means "no snapshot has ever
    /// been committed" (fresh store, or a store that has only ever had
    /// WAL-only writes with no compaction yet).
    pub snapshot_gen: u64,
    /// The WAL segment number (`nova.wal.<wal_seq>`) that is the *first*
    /// segment not yet reflected in `snapshot_gen` — i.e. `open()` must
    /// replay this segment (and any later ones) on top of the loaded
    /// snapshot. Segments with a smaller number are fully covered by the
    /// snapshot and are safe to delete.
    pub wal_seq: u64,
}

impl Manifest {
    pub fn fresh() -> Self {
        Self {
            snapshot_gen: 0,
            wal_seq: 1,
        }
    }

    fn body(&self) -> String {
        format!(
            "{HEADER_LINE}\nsnapshot_gen {}\nwal_seq {}\n",
            self.snapshot_gen, self.wal_seq
        )
    }

    fn encode(&self) -> String {
        let body = self.body();
        let crc = crc32fast::hash(body.as_bytes());
        format!("{body}crc32 {crc:08x}\n")
    }

    fn decode(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != HEADER_LINE {
            return None;
        }
        let snapshot_gen = lines.next()?.strip_prefix("snapshot_gen ")?.parse().ok()?;
        let wal_seq = lines.next()?.strip_prefix("wal_seq ")?.parse().ok()?;
        let crc_line = lines.next()?;
        let stored_crc = u32::from_str_radix(crc_line.strip_prefix("crc32 ")?, 16).ok()?;

        let manifest = Manifest {
            snapshot_gen,
            wal_seq,
        };
        let actual_crc = crc32fast::hash(manifest.body().as_bytes());
        if actual_crc != stored_crc {
            return None;
        }
        Some(manifest)
    }

    /// Atomically write this MANIFEST to `path` (tmp-file + rename, so a
    /// crash mid-write never leaves a torn MANIFEST visible at `path`).
    pub fn save(&self, path: &Path) -> Result<(), Oxigraph> {
        let tmp_path = tmp_sibling(path);
        std::fs::write(&tmp_path, self.encode())
            .map_err(|e| Oxigraph::Storage(format!("manifest write failed: {e}")))?;
        std::fs::rename(&tmp_path, path)
            .map_err(|e| Oxigraph::Storage(format!("manifest rename failed: {e}")))?;
        Ok(())
    }

    /// Load the MANIFEST at `path`.
    ///
    /// - If `path` doesn't exist: returns `Manifest::fresh()` (brand-new
    ///   store directory).
    /// - If `path` exists but fails to parse/checksum: this is only
    ///   tolerated (falls back to `fresh()`) when `dir` contains no other
    ///   snapshot/WAL-segment evidence of prior data; otherwise it is a hard
    ///   error, since silently treating a corrupt MANIFEST as "empty store"
    ///   in a directory that clearly has data would risk masking real data
    ///   loss.
    pub fn load(path: &Path, dir: &Path) -> Result<Self, Oxigraph> {
        if !path.exists() {
            return Ok(Self::fresh());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| Oxigraph::Storage(format!("manifest read failed: {e}")))?;
        match Self::decode(&text) {
            Some(m) => Ok(m),
            None => {
                if dir_has_any_data_files(dir) {
                    Err(Oxigraph::Storage(format!(
                        "manifest at {} is corrupt/unreadable, but {} contains data files — \
                         refusing to silently treat this as an empty store",
                        path.display(),
                        dir.display()
                    )))
                } else {
                    Ok(Self::fresh())
                }
            }
        }
    }
}

fn dir_has_any_data_files(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("nova.snapshot.")
            || name.starts_with("nova.wal.")
            || name.starts_with("nova.dict.")
            // Ring backend per-graph compacted images + remap sidecars
            // (`nova.ring.<gen>.<gid>` / `nova.ringmap.<gen>.<gid>`).
            || name.starts_with("nova.ring.")
            || name.starts_with("nova.ringmap.")
        {
            return true;
        }
    }
    false
}

fn tmp_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

// ── Path helpers ─────────────────────────────────────────────────────────────

pub fn snapshot_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("nova.snapshot.{generation}"))
}

/// Path of the dictionary persistence file for generation `generation` (see
/// `dict_snapshot.rs`), written alongside `nova.snapshot.<generation>`.
pub fn dict_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("nova.dict.{generation}"))
}

pub fn wal_segment_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("nova.wal.{seq:06}"))
}

/// Path of a Ring-backend per-graph compacted `NOVARNG1` image for generation
/// `generation` and graph id `graph_id` (see `nova-engine-ring`).
///
/// Layout: `nova.ring.<gen>.<gid>` — one file per graph per generation so the
/// existing single-image mmap materialize/open APIs can be reused without a
/// multi-graph container format.
pub fn ring_image_path(dir: &Path, generation: u64, graph_id: u8) -> PathBuf {
    dir.join(format!("nova.ring.{generation}.{graph_id}"))
}

/// Path of the Ring-backend external↔dense remap sidecar for generation
/// `generation` and graph id `graph_id`. Written alongside
/// [`ring_image_path`]; required to reopen a `BraidedGraphImage` because the
/// `NOVARNG1` format only stores dense shared-alphabet coordinates.
pub fn ring_remap_path(dir: &Path, generation: u64, graph_id: u8) -> PathBuf {
    dir.join(format!("nova.ringmap.{generation}.{graph_id}"))
}

/// Parse a `nova.snapshot.<gen>` file name, returning `gen` if it matches.
fn parse_snapshot_gen(file_name: &str) -> Option<u64> {
    file_name.strip_prefix("nova.snapshot.")?.parse().ok()
}

/// Parse a `nova.dict.<gen>` file name, returning `gen` if it matches.
fn parse_dict_gen(file_name: &str) -> Option<u64> {
    file_name.strip_prefix("nova.dict.")?.parse().ok()
}

/// Parse a `nova.wal.<seq>` file name, returning `seq` if it matches.
fn parse_wal_seq(file_name: &str) -> Option<u64> {
    file_name.strip_prefix("nova.wal.")?.parse().ok()
}

/// Parse a `nova.ring.<gen>.<gid>` file name, returning `(gen, gid)`.
fn parse_ring_image(file_name: &str) -> Option<(u64, u8)> {
    let rest = file_name.strip_prefix("nova.ring.")?;
    let (gen_s, gid_s) = rest.rsplit_once('.')?;
    let generation = gen_s.parse().ok()?;
    let gid = gid_s.parse().ok()?;
    Some((generation, gid))
}

/// Parse a `nova.ringmap.<gen>.<gid>` file name, returning `(gen, gid)`.
fn parse_ring_remap(file_name: &str) -> Option<(u64, u8)> {
    let rest = file_name.strip_prefix("nova.ringmap.")?;
    let (gen_s, gid_s) = rest.rsplit_once('.')?;
    let generation = gen_s.parse().ok()?;
    let gid = gid_s.parse().ok()?;
    Some((generation, gid))
}

/// Delete snapshot / dict / ring-image / ring-remap files strictly older than
/// `keep_gen`, and WAL segment files strictly older than `keep_seq`. Never
/// touches a file whose numeric suffix fails to parse (never delete something
/// we don't understand), and never touches the currently-committed
/// generation/segment even if asked (the `< keep_*` comparison already
/// excludes them).
///
/// Best-effort: I/O errors deleting an individual orphan are ignored (they
/// just leave a harmless stale file behind for a future cleanup pass) rather
/// than failing the whole compaction that already durably committed.
pub fn cleanup_orphans(dir: &Path, keep_gen: u64, keep_seq: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(generation) = parse_snapshot_gen(&name) {
            if generation < keep_gen {
                let _ = std::fs::remove_file(entry.path());
            }
        } else if let Some(generation) = parse_dict_gen(&name) {
            if generation < keep_gen {
                let _ = std::fs::remove_file(entry.path());
            }
        } else if let Some((generation, _)) = parse_ring_image(&name) {
            if generation < keep_gen {
                let _ = std::fs::remove_file(entry.path());
            }
        } else if let Some((generation, _)) = parse_ring_remap(&name) {
            if generation < keep_gen {
                let _ = std::fs::remove_file(entry.path());
            }
        } else if let Some(seq) = parse_wal_seq(&name)
            && seq < keep_seq
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("nova_manifest_test_{pid}_{n}_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fresh_manifest_when_missing() {
        let dir = temp_dir("missing");
        let path = dir.join(MANIFEST_FILE_NAME);
        let m = Manifest::load(&path, &dir).unwrap();
        assert_eq!(m, Manifest::fresh());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join(MANIFEST_FILE_NAME);
        let m = Manifest {
            snapshot_gen: 3,
            wal_seq: 4,
        };
        m.save(&path).unwrap();
        let loaded = Manifest::load(&path, &dir).unwrap();
        assert_eq!(loaded, m);
    }

    #[test]
    fn corrupt_manifest_falls_back_to_fresh_when_dir_empty() {
        let dir = temp_dir("corrupt_empty");
        let path = dir.join(MANIFEST_FILE_NAME);
        std::fs::write(&path, "garbage not a manifest").unwrap();
        let loaded = Manifest::load(&path, &dir).unwrap();
        assert_eq!(loaded, Manifest::fresh());
    }

    #[test]
    fn corrupt_manifest_errors_when_dir_has_data() {
        let dir = temp_dir("corrupt_with_data");
        let path = dir.join(MANIFEST_FILE_NAME);
        std::fs::write(&path, "garbage not a manifest").unwrap();
        std::fs::write(dir.join("nova.snapshot.1"), b"x").unwrap();
        let result = Manifest::load(&path, &dir);
        assert!(result.is_err());
    }

    #[test]
    fn bit_flip_detected_by_crc() {
        let dir = temp_dir("bitflip");
        let path = dir.join(MANIFEST_FILE_NAME);
        let m = Manifest {
            snapshot_gen: 5,
            wal_seq: 6,
        };
        m.save(&path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a byte inside the "snapshot_gen 5" line.
        let pos = bytes
            .iter()
            .position(|&b| b == b'5')
            .expect("digit '5' present");
        bytes[pos] = b'9';
        std::fs::write(&path, &bytes).unwrap();

        // Corrupted but dir has no other data files -> falls back to fresh.
        let loaded = Manifest::load(&path, &dir).unwrap();
        assert_eq!(loaded, Manifest::fresh());
    }

    #[test]
    fn cleanup_deletes_only_strictly_older_files() {
        let dir = temp_dir("cleanup");
        for generation in 1..=3u64 {
            std::fs::write(snapshot_path(&dir, generation), b"x").unwrap();
        }

        for seq in 1..=3u64 {
            std::fs::write(wal_segment_path(&dir, seq), b"x").unwrap();
        }
        // An unrelated/unparseable file must never be touched.
        std::fs::write(dir.join("nova.manifest"), b"x").unwrap();

        cleanup_orphans(&dir, 3, 3);

        assert!(!snapshot_path(&dir, 1).exists());
        assert!(!snapshot_path(&dir, 2).exists());
        assert!(snapshot_path(&dir, 3).exists());
        assert!(!wal_segment_path(&dir, 1).exists());
        assert!(!wal_segment_path(&dir, 2).exists());
        assert!(wal_segment_path(&dir, 3).exists());
        assert!(dir.join("nova.manifest").exists());
    }

    #[test]
    fn cleanup_also_deletes_older_ring_images_and_remaps() {
        let dir = temp_dir("cleanup_ring");
        for generation in 1..=3u64 {
            std::fs::write(ring_image_path(&dir, generation, 0), b"img").unwrap();
            std::fs::write(ring_remap_path(&dir, generation, 0), b"map").unwrap();
            std::fs::write(ring_image_path(&dir, generation, 1), b"img").unwrap();
            std::fs::write(ring_remap_path(&dir, generation, 1), b"map").unwrap();
        }
        std::fs::write(wal_segment_path(&dir, 1), b"w").unwrap();
        std::fs::write(wal_segment_path(&dir, 2), b"w").unwrap();

        cleanup_orphans(&dir, 3, 2);

        assert!(!ring_image_path(&dir, 1, 0).exists());
        assert!(!ring_image_path(&dir, 2, 0).exists());
        assert!(ring_image_path(&dir, 3, 0).exists());
        assert!(!ring_remap_path(&dir, 1, 1).exists());
        assert!(ring_remap_path(&dir, 3, 1).exists());
        assert!(!wal_segment_path(&dir, 1).exists());
        assert!(wal_segment_path(&dir, 2).exists());
    }

    #[test]
    fn parse_helpers_reject_malformed_names() {
        assert_eq!(parse_snapshot_gen("nova.snapshot.abc"), None);
        assert_eq!(parse_snapshot_gen("nova.snapshot.5"), Some(5));
        assert_eq!(parse_wal_seq("nova.wal.abc"), None);
        assert_eq!(parse_wal_seq("nova.wal.000007"), Some(7));
        assert_eq!(parse_snapshot_gen("unrelated.txt"), None);
        assert_eq!(parse_ring_image("nova.ring.3.0"), Some((3, 0)));
        assert_eq!(parse_ring_image("nova.ring.abc.0"), None);
        assert_eq!(parse_ring_remap("nova.ringmap.2.7"), Some((2, 7)));
        assert_eq!(parse_ring_remap("nova.ringmap.2"), None);
    }
}
