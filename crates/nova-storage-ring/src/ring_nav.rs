//! Unified navigation view over heap [`CyclicRing`] or mmap [`MappedRingA`].
//!
//! Phase 1A: after successful `materialize_mapped`, product paths may drop the
//! heap QWT/A payloads. Callers that previously took `&CyclicRing` should use
//! [`RingRef`] so MiddleRuns / match / VEO keep working on mmap-only residency.

use crate::{Col, CyclicRing, MappedRingA, RowRange};

/// Borrowed Ring A navigation surface (heap or mmap).
#[derive(Clone, Copy)]
pub enum RingRef<'a> {
    Heap(&'a CyclicRing),
    Mapped(&'a MappedRingA),
}

impl<'a> RingRef<'a> {
    #[inline]
    pub fn n(self) -> u32 {
        match self {
            Self::Heap(r) => r.n(),
            Self::Mapped(m) => m.n,
        }
    }

    #[inline]
    pub fn universe(self) -> u32 {
        match self {
            Self::Heap(r) => r.universe,
            Self::Mapped(m) => m.universe,
        }
    }

    #[inline]
    pub fn access(self, col: Col, pos: u32) -> u32 {
        match self {
            Self::Heap(r) => r.access(col, pos),
            Self::Mapped(m) => m.access(col, pos).expect("mapped access in bounds"),
        }
    }

    #[inline]
    pub fn f(self, col: Col, i: u32) -> u32 {
        match self {
            Self::Heap(r) => r.f(col, i),
            Self::Mapped(m) => m.f(col, i).expect("mapped f in bounds"),
        }
    }

    #[inline]
    pub fn lead_range(self, col: Col, symbol: u32) -> RowRange {
        match self {
            Self::Heap(r) => r.lead_range(col, symbol),
            Self::Mapped(m) => m
                .lead_range(col, symbol)
                .unwrap_or_else(RowRange::empty),
        }
    }

    #[inline]
    pub fn range_s(self, s: u32) -> RowRange {
        self.lead_range(Col::S, s)
    }

    #[inline]
    pub fn range_o(self, o: u32) -> RowRange {
        self.lead_range(Col::O, o)
    }

    #[inline]
    pub fn range_p(self, p: u32) -> RowRange {
        self.lead_range(Col::P, p)
    }

    #[inline]
    pub fn range_next_value(self, col: Col, r: RowRange, target: u32) -> Option<u32> {
        match self {
            Self::Heap(ring) => ring.range_next_value(col, r, target),
            Self::Mapped(m) => m.range_next_value(col, r, target),
        }
    }

    #[inline]
    pub fn as_heap(self) -> Option<&'a CyclicRing> {
        match self {
            Self::Heap(r) => Some(r),
            Self::Mapped(_) => None,
        }
    }

    #[inline]
    pub fn as_mapped(self) -> Option<&'a MappedRingA> {
        match self {
            Self::Heap(_) => None,
            Self::Mapped(m) => Some(m),
        }
    }
}
