//! Per-graph read-only canonical-ID image adapter (Phase 4b).
//!
//! Layers identity remapping, deduplication, and mmap-only reopen over
//! [`BraidedRingIndex`] **without** dictionary, live delta, or `QuadStore`.
//!
//! ## Why this exists
//!
//! Production LOUDS stores use global 40-bit term IDs and per-graph rings.
//! Braided Ring's pilot alphabet is dense shared `u32`. Before a store seam
//! can accept global IDs, we need a stable adapter that:
//!
//! 1. Accepts arbitrary (possibly sparse / role-local) triple IDs
//! 2. Remaps them into a dense shared alphabet `[0, universe)`
//! 3. Deduplicates triples
//! 4. Builds a heap index and optionally a `NOVARNG1` mmap image
//! 5. Round-trips SPO enumeration back through the inverse map
//!
//! This is the narrowest safe next seam after the ID facade — no dependency
//! on `nova-storage-louds` beyond the temporary crate re-export, and no
//! changes to `nova-store` query routing.

use super::facade::BraidedRingIndex;
use super::{MappedRingA, MappedRingError, open_novarng1_mmap};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Bidirectional map between external (caller) IDs and dense shared-alphabet IDs.
#[derive(Clone, Debug, Default)]
pub struct IdRemap {
    /// external → dense
    to_dense: BTreeMap<u64, u32>,
    /// dense → external
    to_external: Vec<u64>,
}

impl IdRemap {
    /// Build a dense shared alphabet from the symbols appearing in `triples`.
    ///
    /// Symbols are assigned dense IDs in ascending external order so the
    /// mapping is deterministic across rebuilds of the same triple multiset.
    pub fn from_triples(triples: &[[u64; 3]]) -> Self {
        let mut symbols = BTreeMap::new();
        for t in triples {
            for &id in t {
                symbols.entry(id).or_insert(true);
            }
        }
        let mut to_external = Vec::with_capacity(symbols.len());
        let mut to_dense = BTreeMap::new();
        for (dense, &ext) in symbols.keys().enumerate() {
            let d = dense as u32;
            to_dense.insert(ext, d);
            to_external.push(ext);
        }
        Self {
            to_dense,
            to_external,
        }
    }

    #[inline]
    pub fn universe(&self) -> u32 {
        self.to_external.len() as u32
    }

    #[inline]
    pub fn to_dense(&self, external: u64) -> Option<u32> {
        self.to_dense.get(&external).copied()
    }

    #[inline]
    pub fn to_external(&self, dense: u32) -> Option<u64> {
        self.to_external.get(dense as usize).copied()
    }

    /// Smallest dense id whose external is ≥ `target`, if any.
    ///
    /// Dense ids are assigned in ascending external order, so this is the
    /// correct lower_bound for leapfrog `seek` on external TermIds.
    #[inline]
    pub fn dense_ceil(&self, target: u64) -> Option<u32> {
        let i = self.to_external.partition_point(|&e| e < target);
        if i >= self.to_external.len() {
            None
        } else {
            Some(i as u32)
        }
    }


    /// Map an external triple into shared-alphabet coordinates.
    pub fn map_triple(&self, t: [u64; 3]) -> Option<[u32; 3]> {
        Some([
            self.to_dense(t[0])?,
            self.to_dense(t[1])?,
            self.to_dense(t[2])?,
        ])
    }

    /// Map a dense triple back to external IDs.
    pub fn unmap_triple(&self, t: [u32; 3]) -> Option<[u64; 3]> {
        Some([
            self.to_external(t[0])?,
            self.to_external(t[1])?,
            self.to_external(t[2])?,
        ])
    }
}

/// Read-only per-graph Braided Ring image with external↔dense ID remapping.
///
/// Owns a [`BraidedRingIndex`] (heap + optional mmap) and the remap tables
/// needed to round-trip SPO in caller coordinates.
pub struct BraidedGraphImage {
    remap: IdRemap,
    index: BraidedRingIndex,
    /// Path of the last materialised `NOVARNG1` image, if any.
    image_path: Option<PathBuf>,
}

impl BraidedGraphImage {
    /// Build from external-ID triples: remap → dedup → heap Ring A.
    ///
    /// Empty input yields an empty image (`universe = 0`, `n = 0`).
    pub fn from_external_triples(triples: &[[u64; 3]]) -> Self {
        if triples.is_empty() {
            return Self {
                remap: IdRemap::default(),
                index: BraidedRingIndex::from_shared_triples(&[], 0),
                image_path: None,
            };
        }
        let remap = IdRemap::from_triples(triples);
        let mut dense: Vec<[u32; 3]> = triples
            .iter()
            .filter_map(|t| remap.map_triple(*t))
            .collect();
        dense.sort_unstable();
        dense.dedup();
        let universe = remap.universe();
        // CyclicRing requires universe ≥ max_symbol+1; empty dense after
        // filter shouldn't happen if remap was built from the same triples.
        let index = if dense.is_empty() {
            BraidedRingIndex::from_shared_triples(&[], universe.max(1))
        } else {
            BraidedRingIndex::from_shared_triples(&dense, universe.max(1))
        };
        Self {
            remap,
            index,
            image_path: None,
        }
    }

    /// Dense shared-alphabet index (heap + optional mmap).
    #[inline]
    pub fn index(&self) -> &BraidedRingIndex {
        &self.index
    }

    #[inline]
    pub fn remap(&self) -> &IdRemap {
        &self.remap
    }

    #[inline]
    pub fn n(&self) -> u32 {
        self.index.n()
    }

    #[inline]
    pub fn universe(&self) -> u32 {
        self.remap.universe()
    }

    /// Write `NOVARNG1` and keep the mmap open on the inner index.
    pub fn materialize_mapped(&mut self, path: &Path) -> Result<(), MappedRingError> {
        self.index.materialize_mapped(path)?;
        self.image_path = Some(path.to_path_buf());
        Ok(())
    }

    /// Open a previously written `NOVARNG1` as a bare mapped ring (no remap).
    ///
    /// The adapter's remap is not persisted in the image; keep the
    /// [`BraidedGraphImage`] (or its remap tables) alongside the file for
    /// external-ID round-trips.
    pub fn open_mapped(path: &Path) -> Result<MappedRingA, MappedRingError> {
        open_novarng1_mmap(path)
    }

    /// Whether a mapped image is open on the inner index.
    #[inline]
    pub fn has_mapped(&self) -> bool {
        self.index.has_mapped()
    }

    /// Path of the last materialised image, if known.
    #[inline]
    pub fn image_path(&self) -> Option<&Path> {
        self.image_path.as_deref()
    }

    /// Enumerate triples in **external** ID coordinates (sorted multiset).
    pub fn enumerate_spo_external(&self) -> Vec<[u64; 3]> {
        let dense = self.index.enumerate_spo();
        let mut out: Vec<[u64; 3]> = dense
            .into_iter()
            .filter_map(|t| self.remap.unmap_triple(t))
            .collect();
        out.sort_unstable();
        out
    }

    /// Enumerate triples in dense shared-alphabet coordinates.
    pub fn enumerate_spo_dense(&self) -> Vec<[u32; 3]> {
        self.index.enumerate_spo()
    }

    // join_scan_external / join_scan_streaming / estimate_count_external live
    // in `scan.rs` (impl BraidedGraphImage) so streaming RNV/RDI stays colocated
    // with the LFTJ seam.

    /// Rebuild a heap-only image from this image's external SPO enumeration
    /// (round-trip stress).
    pub fn rebuild_from_external_roundtrip(&self) -> Self {

        let ext = self.enumerate_spo_external();
        Self::from_external_triples(&ext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::scan::oracle_join_scan;

    fn sparse_triples() -> Vec<[u64; 3]> {
        // Sparse external IDs with a duplicate.
        vec![
            [100, 10, 200],
            [100, 10, 201],
            [100, 11, 200],
            [101, 10, 200],
            [101, 11, 202],
            [100, 10, 200], // dup
            [102, 10, 201],
        ]
    }

    #[test]
    fn remap_is_dense_and_deterministic() {
        let t = sparse_triples();
        let r1 = IdRemap::from_triples(&t);
        let r2 = IdRemap::from_triples(&t);
        assert_eq!(r1.universe(), r2.universe());
        // symbols: 10,11,100,101,102,200,201,202 → 8
        assert_eq!(r1.universe(), 8);
        for &ext in &[10u64, 11, 100, 101, 102, 200, 201, 202] {
            let d = r1.to_dense(ext).unwrap();
            assert_eq!(r1.to_external(d), Some(ext));
            assert_eq!(r2.to_dense(ext), Some(d));
        }
    }

    #[test]
    fn build_dedups_and_roundtrips_spo() {
        let t = sparse_triples();
        let img = BraidedGraphImage::from_external_triples(&t);
        // 6 unique after dedup
        assert_eq!(img.n(), 6);
        let mut want = t.clone();
        want.sort_unstable();
        want.dedup();
        let got = img.enumerate_spo_external();
        assert_eq!(got, want);
    }

    #[test]
    fn mmap_materialize_and_external_enum_parity() {
        let t = sparse_triples();
        let mut img = BraidedGraphImage::from_external_triples(&t);
        let path = std::env::temp_dir().join(format!(
            "braided_image_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        img.materialize_mapped(&path).expect("materialize");
        assert!(img.has_mapped());
        let heap_only = BraidedGraphImage::from_external_triples(&t);
        assert_eq!(
            img.enumerate_spo_external(),
            heap_only.enumerate_spo_external()
        );
        // Bare mapped open still works
        let mapped = BraidedGraphImage::open_mapped(&path).expect("open");
        assert_eq!(mapped.n, img.n());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn join_scan_external_matches_oracle() {
        let t = sparse_triples();
        let img = BraidedGraphImage::from_external_triples(&t);
        let mut want_triples = t.clone();
        want_triples.sort_unstable();
        want_triples.dedup();

        // Dense oracle path: map to dense, filter, unmap.
        let dense: Vec<[u32; 3]> = want_triples
            .iter()
            .filter_map(|x| img.remap().map_triple(*x))
            .collect();
        let dense_vals = oracle_join_scan(&dense, None, None, None, 0);
        let want: Vec<u64> = dense_vals
            .into_iter()
            .filter_map(|d| img.remap().to_external(d as u32))
            .collect();
        let got = img.join_scan_external(None, None, None, 0);
        assert_eq!(got, want);

        // Bound subject 100 → objects
        let got_o = img.join_scan_external(Some(100), None, None, 2);
        let sd = img.remap().to_dense(100).map(u64::from);
        let dense_o = oracle_join_scan(&dense, sd, None, None, 2);
        let want_o: Vec<u64> = dense_o
            .into_iter()
            .filter_map(|d| img.remap().to_external(d as u32))
            .collect();
        assert_eq!(got_o, want_o);
        assert!(!got_o.is_empty());
    }

    #[test]
    fn unknown_external_bind_yields_empty() {
        let t = sparse_triples();
        let img = BraidedGraphImage::from_external_triples(&t);
        assert!(img.join_scan_external(Some(9999), None, None, 0).is_empty());
    }

    #[test]
    fn rebuild_roundtrip_preserves_multiset() {
        let t = sparse_triples();
        let img = BraidedGraphImage::from_external_triples(&t);
        let again = img.rebuild_from_external_roundtrip();
        assert_eq!(img.enumerate_spo_external(), again.enumerate_spo_external());
        assert_eq!(img.n(), again.n());
    }
}
