//! LSM delta — a `BTreeMap<u128, bool>` absorbing live writes at O(log n) per op.
//!
//! ## Key layout
//!
//! ```text
//! g[127:120] | s[119:80] | p[79:40] | o[39:0]   (8 + 40 + 40 + 40 = 128 bits)
//! ```
//!
//! `true`  = inserted since last merge (the triple is live).
//! `false` = tombstone (the triple was in the Ring and has been deleted).
//!
//! Because the graph field is in the high byte, `BTreeMap` orders graph-major:
//! all triples in the same named graph are contiguous, so a per-graph range
//! query is a single `btree_map::range(lo..=hi)` call.
//!
//! ## Merge policy
//!
//! When the delta exceeds a threshold (default: 1 M triples), a background
//! thread can call `RingStore::compact()` which:
//! 1. Merges Ring ∪ delta_inserts \ tombstones.
//! 2. Rebuilds Ring via `RingBuilder::build()`.
//! 3. Clears this delta.
//!
//! Queries during a merge read both the old Ring and the delta — no read downtime.

use oxigraph_nova_core::{Dictionary, GraphId, TermId};
use std::collections::BTreeMap;

/// LSM write buffer: `quad_key → is_insert` (true = insert, false = tombstone).
pub struct Delta {
    inner: BTreeMap<u128, bool>,
}

impl Default for Delta {
    fn default() -> Self {
        Self::new()
    }
}

impl Delta {
    pub fn new() -> Self {
        Self {
            inner: BTreeMap::new(),
        }
    }

    /// Insert a quad key (marks it as a live insert).
    #[inline]
    pub fn insert_key(&mut self, key: u128) {
        self.inner.insert(key, true);
    }

    /// Remove a quad key (marks it as a tombstone).
    #[inline]
    pub fn tombstone_key(&mut self, key: u128) {
        self.inner.insert(key, false);
    }

    /// Get the delta status for a key: `Some(true)` = insert, `Some(false)` = tombstone,
    /// `None` = not in delta (may or may not be in the Ring).
    #[inline]
    pub fn get(&self, key: u128) -> Option<bool> {
        self.inner.get(&key).copied()
    }

    /// Total entries in the delta (inserts + tombstones).
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Number of live inserts (tombstones excluded).
    pub fn insert_count(&self) -> usize {
        self.inner.values().filter(|&&v| v).count()
    }

    /// Number of tombstones.
    pub fn tombstone_count(&self) -> usize {
        self.inner.values().filter(|&&v| !v).count()
    }

    /// Clear all entries (called after a successful merge into the Ring).
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Iterate over all entries in the delta (for merge and query operations).
    pub fn iter(&self) -> impl Iterator<Item = (&u128, &bool)> {
        self.inner.iter()
    }

    /// Iterate over entries in the u128 key range `[lo, hi]` (inclusive both ends).
    ///
    /// Used by `quads_for_pattern` to restrict the delta scan to a specific graph
    /// (and optionally subject) without scanning the entire delta.
    pub fn range_inclusive(&self, lo: u128, hi: u128) -> impl Iterator<Item = (&u128, &bool)> {
        self.inner.range(lo..=hi)
    }

    // ── Range bound helpers ───────────────────────────────────────────────────

    /// Compute `[lo, hi]` u128 bounds for a per-graph range query.
    ///
    /// The returned range covers exactly the keys for graph `g` and optionally
    /// bound subject, predicate, and object fields.  Any unbound trailing fields
    /// are set to their maximum value so the range is as tight as possible.
    ///
    /// For patterns like `(s_bound, p_none, o_bound)` the range covers all (s, ?, ?)
    /// for the graph; callers must post-filter for `o` within the range.
    pub fn graph_range(
        g: GraphId,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
    ) -> (u128, u128) {
        let g_base = (g.as_u8() as u128) << 120;

        let (lo, hi) = match (s, p, o) {
            // All three bound → exact key
            (Some(sv), Some(pv), Some(ov)) => {
                let key = Dictionary::pack_quad(g, sv, pv, ov);
                (key, key)
            }
            // s + p bound → vary o
            (Some(sv), Some(pv), None) => {
                let lo = g_base | ((sv.as_u64() as u128) << 80) | ((pv.as_u64() as u128) << 40);
                let hi = lo | ((1u128 << 40) - 1); // all 40 o-bits set
                (lo, hi)
            }
            // s only bound → vary p + o
            (Some(sv), None, _) => {
                let lo = g_base | ((sv.as_u64() as u128) << 80);
                let hi = lo | ((1u128 << 80) - 1); // all 80 (p+o)-bits set
                (lo, hi)
            }
            // p + o bound (no s): can't express as a tight range without s —
            // fall back to full-graph range and post-filter
            (None, Some(_), Some(_)) | (None, Some(_), None) | (None, None, Some(_)) => {
                let lo = g_base;
                let hi = g_base | ((1u128 << 120) - 1); // all 120 (s+p+o)-bits set
                (lo, hi)
            }
            // Nothing bound → full graph range
            (None, None, None) => {
                let lo = g_base;
                let hi = g_base | ((1u128 << 120) - 1);
                (lo, hi)
            }
        };
        (lo, hi)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{GRAPH_DEFAULT, MAX_TERM_ID, TermId};

    fn tid(v: u64) -> TermId {
        TermId::new(v).unwrap()
    }

    #[test]
    fn insert_and_get() {
        let mut d = Delta::new();
        let key = Dictionary::pack_quad(GRAPH_DEFAULT, tid(1), tid(2), tid(3));
        d.insert_key(key);
        assert_eq!(d.get(key), Some(true));
        assert_eq!(d.get(key ^ 1), None);
    }

    #[test]
    fn tombstone() {
        let mut d = Delta::new();
        let key = Dictionary::pack_quad(GRAPH_DEFAULT, tid(1), tid(2), tid(3));
        d.insert_key(key);
        d.tombstone_key(key);
        assert_eq!(d.get(key), Some(false));
    }

    #[test]
    fn counts() {
        let mut d = Delta::new();
        let k1 = Dictionary::pack_quad(GRAPH_DEFAULT, tid(1), tid(2), tid(3));
        let k2 = Dictionary::pack_quad(GRAPH_DEFAULT, tid(4), tid(5), tid(6));
        d.insert_key(k1);
        d.insert_key(k2);
        d.tombstone_key(k1);
        assert_eq!(d.insert_count(), 1);
        assert_eq!(d.tombstone_count(), 1);
    }

    #[test]
    fn graph_range_exact() {
        let (lo, hi) = Delta::graph_range(GRAPH_DEFAULT, Some(tid(1)), Some(tid(2)), Some(tid(3)));
        assert_eq!(lo, hi);
        let key = Dictionary::pack_quad(GRAPH_DEFAULT, tid(1), tid(2), tid(3));
        assert_eq!(lo, key);
    }

    #[test]
    fn graph_range_s_bound_covers_p_o() {
        let (lo, hi) = Delta::graph_range(GRAPH_DEFAULT, Some(tid(5)), None, None);
        // The range should cover [5<<80, 5<<80 | (2^80-1)]
        let s = 5u128 << 80;
        assert_eq!(lo, s);
        assert_eq!(hi, s | ((1u128 << 80) - 1));

        // A key with s=5, p=0, o=0 should be in range
        let k_lo = Dictionary::pack_quad(GRAPH_DEFAULT, tid(5), tid(0), tid(0));
        let k_hi = Dictionary::pack_quad(GRAPH_DEFAULT, tid(5), tid(MAX_TERM_ID), tid(MAX_TERM_ID));
        assert!(k_lo >= lo && k_lo <= hi);
        assert!(k_hi >= lo && k_hi <= hi);
    }

    #[test]
    fn range_inclusive_query() {
        let mut d = Delta::new();
        let k1 = Dictionary::pack_quad(GRAPH_DEFAULT, tid(1), tid(2), tid(3));
        let k2 = Dictionary::pack_quad(GRAPH_DEFAULT, tid(1), tid(2), tid(4));
        let k3 = Dictionary::pack_quad(GRAPH_DEFAULT, tid(2), tid(2), tid(3));
        d.insert_key(k1);
        d.insert_key(k2);
        d.insert_key(k3);

        let (lo, hi) = Delta::graph_range(GRAPH_DEFAULT, Some(tid(1)), None, None);
        let found: Vec<_> = d.range_inclusive(lo, hi).collect();
        assert_eq!(found.len(), 2, "should find both k1 and k2 but not k3");
    }
}
