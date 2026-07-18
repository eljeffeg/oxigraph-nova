//! ID-level Braided Ring facade (Phase 4).
//!
//! Thin ownership wrapper over heap [`CyclicRing`] and optional mmap
//! [`MappedRingA`]. This is **not** a [`oxigraph_nova_core::QuadStore`]: no
//! dictionary, no SPARQL/LFTJ cutover, no live writes. Callers build from
//! shared-alphabet `u32` triples and use navigation / D2 primitives.
//!
//! Differential tests in this module compare enumerate / lead-range / D2
//! results against a sorted triple-list oracle and heap↔mmap parity.

use super::{
    Col, CyclicRing, MappedRingA, MappedRingError, RowRange, open_novarng1_mmap, write_novarng1_file,
    write_novarng1_v1,
};
use std::path::Path;

/// Read-only ID-level Braided Ring (Ring A + optional `NOVARNG1` mmap image).
///
/// Product triangle path is D2 via [`Self::intersection_next_value3`] when a
/// mapped image is present (hot QWT columns). Heap-only instances still support
/// enumerate / lead-range / RNV for oracle and build checks.
pub struct BraidedRingIndex {
    heap: CyclicRing,
    /// Present after [`Self::materialize_mapped`] / [`Self::open_mapped`].
    mapped: Option<MappedRingA>,
}

impl BraidedRingIndex {
    /// Build Ring A from shared-alphabet triples (`[s,p,o]` with symbols in `0..universe`).
    pub fn from_shared_triples(triples: &[[u32; 3]], universe: u32) -> Self {
        Self {
            heap: CyclicRing::build_shared(triples, universe),
            mapped: None,
        }
    }

    /// Build Ring A after remapping role-local S/P/O IDs into a shared alphabet.
    pub fn from_role_local(triples: &[[u32; 3]], ns: u32, np: u32, no: u32) -> Self {
        Self {
            heap: CyclicRing::build_from_role_local(triples, ns, np, no),
            mapped: None,
        }
    }

    /// Triple count.
    #[inline]
    pub fn n(&self) -> u32 {
        self.heap.n()
    }

    /// Shared alphabet size.
    #[inline]
    pub fn universe(&self) -> u32 {
        self.heap.universe
    }

    /// Borrow the heap pilot (tests / oracles).
    #[inline]
    pub fn heap(&self) -> &CyclicRing {
        &self.heap
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

    /// Flatten heap → `NOVARNG1` bytes, write to `path`, open mmap, and keep it.
    pub fn materialize_mapped(&mut self, path: &Path) -> Result<(), MappedRingError> {
        write_novarng1_file(path, &self.heap)?;
        self.mapped = Some(open_novarng1_mmap(path)?);
        Ok(())
    }

    /// Open an existing `NOVARNG1` image (does not rebuild heap).
    ///
    /// Prefer [`Self::materialize_mapped`] when both heap and mmap are needed
    /// for differentials; this is for open-only product paths.
    pub fn open_mapped(path: &Path) -> Result<MappedRingA, MappedRingError> {
        open_novarng1_mmap(path)
    }

    /// Encode heap as owned `NOVARNG1` image bytes (no file).
    pub fn write_image_bytes(&self) -> Result<Vec<u8>, MappedRingError> {
        write_novarng1_v1(&self.heap)
    }

    /// Full SPO enumeration (shared-alphabet). Prefers mmap when present.
    pub fn enumerate_spo(&self) -> Vec<[u32; 3]> {
        if let Some(m) = &self.mapped {
            m.enumerate_spo().unwrap_or_else(|| self.heap.enumerate_spo())
        } else {
            self.heap.enumerate_spo()
        }
    }

    /// Lead range for symbol on column `col`.
    pub fn lead_range(&self, col: Col, symbol: u32) -> RowRange {
        if let Some(m) = &self.mapped {
            m.lead_range(col, symbol)
                .unwrap_or_else(|| self.heap.lead_range(col, symbol))
        } else {
            self.heap.lead_range(col, symbol)
        }
    }

    /// Subject prefix range on T_spo.
    #[inline]
    pub fn range_s(&self, s: u32) -> RowRange {
        self.lead_range(Col::S, s)
    }

    /// Object prefix range.
    #[inline]
    pub fn range_o(&self, o: u32) -> RowRange {
        self.lead_range(Col::O, o)
    }

    /// Predicate prefix range.
    #[inline]
    pub fn range_p(&self, p: u32) -> RowRange {
        self.lead_range(Col::P, p)
    }

    /// Product triangle: D2 braided three-range successor (requires mapped image).
    ///
    /// Returns `None` if no mapped image is open or if no common symbol ≥ `target`.
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
    /// Sort + dedup optional: returns sorted copy of triples as multiset.
    pub fn sorted_triples(triples: &[[u32; 3]]) -> Vec<[u32; 3]> {
        let mut v = triples.to_vec();
        v.sort_unstable();
        v
    }

    /// Multiset equality after sorting both sides.
    pub fn multisets_equal(a: &[[u32; 3]], b: &[[u32; 3]]) -> bool {
        let mut aa = a.to_vec();
        let mut bb = b.to_vec();
        aa.sort_unstable();
        bb.sort_unstable();
        aa == bb
    }

    /// Count triples with subject `s` (shared alphabet).
    pub fn count_subject(triples: &[[u32; 3]], s: u32) -> u32 {
        triples.iter().filter(|t| t[0] == s).count() as u32
    }

    /// Count triples with object `o`.
    pub fn count_object(triples: &[[u32; 3]], o: u32) -> u32 {
        triples.iter().filter(|t| t[2] == o).count() as u32
    }

    /// Count triples with predicate `p`.
    pub fn count_predicate(triples: &[[u32; 3]], p: u32) -> u32 {
        triples.iter().filter(|t| t[1] == p).count() as u32
    }

    /// Symbols present in all three filtered projections on column `col_idx`
    /// (0=s, 1=p, 2=o), ordered ascending — naive set intersection oracle for D2.
    ///
    /// For product D2 tests we typically intersect three *row ranges* on one
    /// QWT column; this helper instead intersects symbol sets from three
    /// triple filters when the caller already has filtered triple lists.
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
    use super::*;
    use super::oracle;
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
        assert!(
            oracle::multisets_equal(&got, &t),
            "heap enumerate vs input multiset"
        );
    }

    #[test]
    fn lead_range_lens_match_oracle_counts() {
        let t = sample_triples();
        let idx = BraidedRingIndex::from_shared_triples(&t, 3);
        for s in 0..3 {
            assert_eq!(
                idx.range_s(s).len(),
                oracle::count_subject(&t, s),
                "subject {s}"
            );
        }
        for p in 0..2 {
            assert_eq!(
                idx.range_p(p).len(),
                oracle::count_predicate(&t, p),
                "pred {p}"
            );
        }
        for o in 0..3 {
            assert_eq!(
                idx.range_o(o).len(),
                oracle::count_object(&t, o),
                "object {o}"
            );
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
        idx.materialize_mapped(&path).expect("materialize");
        let _ = std::fs::remove_file(&path);

        assert!(idx.has_mapped());
        let heap_enum = idx.heap().enumerate_spo();
        let map_enum = idx.mapped().unwrap().enumerate_spo().expect("map enum");
        assert!(oracle::multisets_equal(&heap_enum, &map_enum));

        for col in [Col::S, Col::P, Col::O] {
            for sym in 0..idx.universe() {
                let h = idx.heap().lead_range(col, sym);
                let m = idx.mapped().unwrap().lead_range(col, sym).unwrap();
                assert_eq!(h, m, "lead_range {col:?} {sym}");
            }
        }
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
        // Three copies of full range → common symbols = all symbols that appear
        // at least once in that column's last-column sequence; dual_rnv must match D2.
        for col in [Col::S, Col::P, Col::O] {
            let mut t = 0u32;
            loop {
                let d2 = idx
                    .intersection_next_value3(col, full, full, full, t)
                    .or_else(|| {
                        // empty product
                        None
                    });
                let oracle = idx.intersection_next_value3_dual_rnv(col, full, full, full, t);
                assert_eq!(d2, oracle, "D2 vs dual_rnv col={col:?} target={t}");
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
        // Three subject lead-ranges on Col::O (objects under those subjects).
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
        idx.materialize_mapped(&path).expect("mmap");
        let _ = std::fs::remove_file(&path);

        let r0 = idx.range_s(0);
        let r1 = idx.range_s(1);
        let r2 = idx.range_s(2);
        // Objects under each subject (from oracle list).
        let objs = |s: u32| -> Vec<u32> {
            t.iter()
                .filter(|x| x[0] == s)
                .map(|x| x[2])
                .collect()
        };
        let common = oracle::sorted_common_symbols(&[objs(0), objs(1), objs(2)]);
        let got = idx
            .collect_intersection3(Col::O, r0, r1, r2)
            .expect("mapped");
        assert_eq!(got, common, "D2 common objects under s=0,1,2");
    }
}
