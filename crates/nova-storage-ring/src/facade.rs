//! ID-level Braided Ring facade (Phase 4 / 4b).
//!
//! Thin ownership wrapper over optional heap [`CyclicRing`] and optional mmap
//! [`MappedRingA`]. This is **not** a [`oxigraph_nova_core::QuadStore`]: no
//! dictionary, no SPARQL cutover, no live writes. Callers build from
//! shared-alphabet `u32` triples and use navigation / D2 primitives.
//!
//! Phase 4b adds [`BraidedRingIndex::join_scan`] (see `scan` module) — an
//! ID-level `TrieIterator` seam matching `LftjSource::lftj_join_scan`
//! semantics, still without dictionary/delta/`QuadStore`.
//!
//! ## Dual residency (Phase 1A)
//!
//! After a successful [`Self::materialize_mapped`], the product path **drops**
//! the heap QWT/A payloads by default so process RSS is not charged twice for
//! the same Ring A. Escape hatch: `NOVA_RING_KEEP_HEAP=1` (see
//! [`crate::product_path::ring_keep_heap`]) keeps both for differentials.
//! Navigation after drop uses mmap via [`crate::ring_nav::RingRef`].
//!
//! Differential tests in this module compare enumerate / lead-range / D2
//! results against a sorted triple-list oracle and heap↔mmap parity.

use crate::product_path::ring_keep_heap;
use crate::ring_nav::RingRef;
use crate::{
    Col, CyclicRing, MappedRingA, MappedRingError, RowRange, open_novarng1_mmap, write_novarng1_file,
    write_novarng1_v1,
};
use std::path::Path;

/// Read-only ID-level Braided Ring (Ring A + optional `NOVARNG1` mmap image).
///
/// Product triangle path is D2 via [`Self::intersection_next_value3`] when a
/// mapped image is present (hot QWT columns). Heap-only instances still support
/// enumerate / lead-range / RNV for oracle and build checks.
///
/// Invariant: at least one of `heap` / `mapped` is `Some` after a successful
/// build (empty rings may have a zero-size heap only).
pub struct BraidedRingIndex {
    /// Heap pilot. Cleared after successful mmap materialize unless
    /// `NOVA_RING_KEEP_HEAP=1`.
    heap: Option<CyclicRing>,
    /// Present after [`Self::materialize_mapped`] / product open.
    mapped: Option<MappedRingA>,
    /// Cached dimensions so `n` / `universe` work after heap drop.
    n: u32,
    universe: u32,
}

impl BraidedRingIndex {
    /// Build Ring A from shared-alphabet triples (`[s,p,o]` with symbols in `0..universe`).
    pub fn from_shared_triples(triples: &[[u32; 3]], universe: u32) -> Self {
        let heap = CyclicRing::build_shared(triples, universe);
        Self {
            n: heap.n(),
            universe: heap.universe,
            heap: Some(heap),
            mapped: None,
        }
    }

    /// Build Ring A after remapping role-local S/P/O IDs into a shared alphabet.
    pub fn from_role_local(triples: &[[u32; 3]], ns: u32, np: u32, no: u32) -> Self {
        let heap = CyclicRing::build_from_role_local(triples, ns, np, no);
        Self {
            n: heap.n(),
            universe: heap.universe,
            heap: Some(heap),
            mapped: None,
        }
    }

    /// Triple count.
    #[inline]
    pub fn n(&self) -> u32 {
        self.n
    }

    /// Shared alphabet size.
    #[inline]
    pub fn universe(&self) -> u32 {
        self.universe
    }

    /// Borrow the heap pilot when still resident (tests / oracles / KEEP_HEAP).
    #[inline]
    pub fn heap(&self) -> Option<&CyclicRing> {
        self.heap.as_ref()
    }

    /// Whether the heap QWT/A payload is still resident.
    #[inline]
    pub fn has_heap(&self) -> bool {
        self.heap.is_some()
    }

    /// Unified nav view: prefer mmap when open, else heap.
    #[inline]
    pub fn ring_ref(&self) -> RingRef<'_> {
        if let Some(m) = self.mapped.as_ref() {
            RingRef::Mapped(m)
        } else if let Some(h) = self.heap.as_ref() {
            RingRef::Heap(h)
        } else {
            panic!("BraidedRingIndex has neither heap nor mapped residency");
        }
    }

    /// Borrow the mmap image if materialised.
    #[inline]
    pub fn mapped(&self) -> Option<&MappedRingA> {
        self.mapped.as_ref()
    }

    /// Whether a `NOVARNG1` mmap image is open.
    #[inline]
    pub fn has_mapped(&self) -> bool {
        self.mapped.is_some()
    }

    /// Flatten heap → `NOVARNG1` bytes, write to `path`, open mmap.
    ///
    /// By default drops the heap payload after a successful open (Phase 1A).
    /// Set `NOVA_RING_KEEP_HEAP=1` to retain both for differentials.
    pub fn materialize_mapped(&mut self, path: &Path) -> Result<(), MappedRingError> {
        self.materialize_mapped_ex(path, ring_keep_heap())
    }

    /// Like [`Self::materialize_mapped`] with an explicit heap-retention flag
    /// (avoids process-wide env races in parallel tests).
    pub fn materialize_mapped_ex(
        &mut self,
        path: &Path,
        keep_heap: bool,
    ) -> Result<(), MappedRingError> {
        let heap = self.heap.as_ref().ok_or(MappedRingError::Layout(
            "materialize_mapped requires a resident heap CyclicRing (rebuild from triples)",
        ))?;
        write_novarng1_file(path, heap)?;
        self.mapped = Some(open_novarng1_mmap(path)?);
        if !keep_heap {
            self.heap = None;
        }
        Ok(())
    }

    /// Open an existing `NOVARNG1` image (does not rebuild heap).
    pub fn open_mapped(path: &Path) -> Result<MappedRingA, MappedRingError> {
        open_novarng1_mmap(path)
    }

    /// Encode heap as owned `NOVARNG1` image bytes (no file).
    pub fn write_image_bytes(&self) -> Result<Vec<u8>, MappedRingError> {
        let heap = self.heap.as_ref().ok_or(MappedRingError::Layout(
            "write_image_bytes requires resident heap; rebuild or KEEP_HEAP",
        ))?;
        write_novarng1_v1(heap)
    }

    /// Full SPO enumeration (shared-alphabet). Prefers mmap when present.
    pub fn enumerate_spo(&self) -> Vec<[u32; 3]> {
        match (&self.mapped, &self.heap) {
            (Some(m), _) => m
                .enumerate_spo()
                .or_else(|| self.heap.as_ref().map(|h| h.enumerate_spo()))
                .unwrap_or_default(),
            (None, Some(h)) => h.enumerate_spo(),
            (None, None) => Vec::new(),
        }
    }

    /// Lead range for symbol on column `col`.
    pub fn lead_range(&self, col: Col, symbol: u32) -> RowRange {
        self.ring_ref().lead_range(col, symbol)
    }

    #[inline]
    pub fn range_s(&self, s: u32) -> RowRange {
        self.lead_range(Col::S, s)
    }

    #[inline]
    pub fn range_o(&self, o: u32) -> RowRange {
        self.lead_range(Col::O, o)
    }

    #[inline]
    pub fn range_p(&self, p: u32) -> RowRange {
        self.lead_range(Col::P, p)
    }

    /// D1 braided two-range successor (requires mapped image).
    pub fn intersection_next_value2(
        &self,
        col: Col,
        first: RowRange,
        second: RowRange,
        target: u32,
    ) -> Option<u32> {
        self.mapped
            .as_ref()?
            .intersection_next_value2(col, first, second, target)
    }

    /// Product triangle: D2 braided three-range successor (requires mapped image).
    pub fn intersection_next_value3(
        &self,
        col: Col,
        first: RowRange,
        second: RowRange,
        third: RowRange,
        target: u32,
    ) -> Option<u32> {
        self.mapped
            .as_ref()?
            .intersection_next_value3(col, first, second, third, target)
    }

    /// D2 dual-RNV correctness oracle (same signature as product D2).
    pub fn intersection_next_value3_dual_rnv(
        &self,
        col: Col,
        first: RowRange,
        second: RowRange,
        third: RowRange,
        target: u32,
    ) -> Option<u32> {
        self.mapped
            .as_ref()?
            .intersection_next_value3_dual_rnv(col, first, second, third, target)
    }

    /// Collect all common symbols ≥ 0 on three ranges via repeated D2 (mapped only).
    pub fn collect_intersection3(
        &self,
        col: Col,
        first: RowRange,
        second: RowRange,
        third: RowRange,
    ) -> Option<Vec<u32>> {
        let m = self.mapped.as_ref()?;
        let mut out = Vec::new();
        let mut t = 0u32;
        while let Some(v) = m.intersection_next_value3(col, first, second, third, t) {
            out.push(v);
            t = v.saturating_add(1);
            if t == 0 {
                break;
            }
        }
        Some(out)
    }
}

// ── Sorted-list oracle (differential, no QWT) ───────────────────────────────

/// Ground-truth multiset helpers over sorted shared-alphabet triples.
pub mod oracle {
    pub fn sorted_triples(triples: &[[u32; 3]]) -> Vec<[u32; 3]> {
        let mut v = triples.to_vec();
        v.sort_unstable();
        v
    }

    pub fn multisets_equal(a: &[[u32; 3]], b: &[[u32; 3]]) -> bool {
        let mut aa = a.to_vec();
        let mut bb = b.to_vec();
        aa.sort_unstable();
        bb.sort_unstable();
        aa == bb
    }

    pub fn count_subject(triples: &[[u32; 3]], s: u32) -> u32 {
        triples.iter().filter(|t| t[0] == s).count() as u32
    }

    pub fn count_object(triples: &[[u32; 3]], o: u32) -> u32 {
        triples.iter().filter(|t| t[2] == o).count() as u32
    }

    pub fn count_predicate(triples: &[[u32; 3]], p: u32) -> u32 {
        triples.iter().filter(|t| t[1] == p).count() as u32
    }

    pub fn sorted_common_symbols(sets: &[Vec<u32>]) -> Vec<u32> {
        if sets.is_empty() {
            return Vec::new();
        }
        let mut iter = sets.iter();
        let mut acc: std::collections::BTreeSet<u32> =
            iter.next().unwrap().iter().copied().collect();
        for s in iter {
            let other: std::collections::BTreeSet<u32> = s.iter().copied().collect();
            acc = acc.intersection(&other).copied().collect();
        }
        acc.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::oracle;
    use super::*;
    use std::io::Write;

    fn sample_triples() -> Vec<[u32; 3]> {
        vec![
            [0, 0, 1],
            [0, 0, 2],
            [0, 1, 1],
            [1, 0, 0],
            [1, 1, 2],
            [2, 0, 1],
            [2, 1, 0],
            [1, 0, 1],
        ]
    }

    #[test]
    fn enumerate_matches_sorted_oracle() {
        let t = sample_triples();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let got = idx.enumerate_spo();
        assert!(oracle::multisets_equal(&got, &t));
    }

    #[test]
    fn lead_range_lens_match_oracle_counts() {
        let t = sample_triples();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        for s in 0..3 {
            assert_eq!(idx.range_s(s).len(), oracle::count_subject(&t, s));
        }
        for p in 0..2 {
            assert_eq!(idx.range_p(p).len(), oracle::count_predicate(&t, p));
        }
        for o in 0..3 {
            assert_eq!(idx.range_o(o).len(), oracle::count_object(&t, o));
        }
    }

    #[test]
    fn heap_mmap_enumerate_and_lead_parity() {
        let t = sample_triples();
        let mut idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let path = std::env::temp_dir().join(format!(
            "braided_facade_enum_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        idx.materialize_mapped_ex(&path, true).expect("materialize");
        let _ = std::fs::remove_file(&path);
        assert!(idx.has_mapped());
        assert!(idx.has_heap());
        let heap_enum = idx.heap().unwrap().enumerate_spo();
        let map_enum = idx.mapped().unwrap().enumerate_spo().expect("map enum");
        assert!(oracle::multisets_equal(&heap_enum, &map_enum));
        for col in [Col::S, Col::P, Col::O] {
            for sym in 0..idx.universe() {
                let h = idx.heap().unwrap().lead_range(col, sym);
                let m = idx.mapped().unwrap().lead_range(col, sym).unwrap();
                assert_eq!(h, m);
            }
        }
    }

    #[test]
    fn materialize_drops_heap_by_default() {
        let t = sample_triples();
        let mut idx = BraidedRingIndex::from_shared_triples(&t, 3);
        assert!(idx.has_heap());
        let path = std::env::temp_dir().join(format!(
            "braided_facade_drop_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        idx.materialize_mapped_ex(&path, false).expect("materialize");
        let _ = std::fs::remove_file(&path);
        assert!(idx.has_mapped());
        assert!(!idx.has_heap());
        assert!(oracle::multisets_equal(&idx.enumerate_spo(), &t));
        assert_eq!(idx.range_s(0).len(), oracle::count_subject(&t, 0));
    }

    #[test]
    fn d2_matches_dual_rnv_on_full_ranges() {
        let t = sample_triples();
        let mut idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let path = std::env::temp_dir().join(format!(
            "braided_facade_d2_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let image = idx.write_image_bytes().unwrap();
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&image).unwrap();
        }
        idx.mapped = Some(open_novarng1_mmap(&path).unwrap());
        let _ = std::fs::remove_file(&path);
        let full = RowRange::full(idx.n());
        for col in [Col::S, Col::P, Col::O] {
            let mut t = 0u32;
            loop {
                let d2 = idx.intersection_next_value3(col, full, full, full, t);
                let oracle = idx.intersection_next_value3_dual_rnv(col, full, full, full, t);
                assert_eq!(d2, oracle);
                match d2 {
                    Some(v) => {
                        if v == u32::MAX {
                            break;
                        }
                        t = v + 1;
                    }
                    None => break,
                }
            }
        }
    }

    #[test]
    fn d2_subject_range_intersection_matches_oracle_stream() {
        let t = sample_triples();
        let mut idx = BraidedRingIndex::from_shared_triples(&t, 3);
        let path = std::env::temp_dir().join(format!(
            "braided_facade_d2s_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        idx.materialize_mapped_ex(&path, true).expect("mmap");
        let _ = std::fs::remove_file(&path);
        let r0 = idx.range_s(0);
        let r1 = idx.range_s(1);
        let r2 = idx.range_s(2);
        let objs = |s: u32| -> Vec<u32> {
            t.iter().filter(|x| x[0] == s).map(|x| x[2]).collect()
        };
        let common = oracle::sorted_common_symbols(&[objs(0), objs(1), objs(2)]);
        let got = idx.collect_intersection3(Col::O, r0, r1, r2).expect("mapped D2");
        assert_eq!(got, common);
    }
}
