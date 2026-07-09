//! `SortedVecTrie` — a transient, in-memory [`TrieIterator`] over a small
//! relation of `[u64; 3]` tuples (already interned `TermId`s).
//!
//! ## Why this exists
//!
//! Nova's LFTJ evaluator (`crates/nova-query/src/lftj.rs`) only runs over the
//! compacted LOUDS index — `Dataset::lftj_has_delta()` returning `true`
//! disables LFTJ entirely and falls back to nested-loop join (see
//! `oxigraph_nova_core::LftjSource::lftj_has_delta`'s doc comment). A
//! semi-naive datalog fixpoint round produces a new relation of derived
//! tuples every iteration, so neither of the two obvious storage strategies
//! works: writing each round's output into the store makes the delta
//! non-empty (collapsing every subsequent round to nested-loop, discarding
//! the entire worst-case-optimal-join advantage), while recompacting after
//! every round pays a full LOUDS rebuild (`O(n)` in the *total* index, not
//! the round's small delta) on every iteration of what may be a many-round
//! closure.
//!
//! `TrieIterator` (`key`/`seek`/`open`/`at_end`) is a backend-agnostic trait —
//! nothing in its contract is LOUDS-specific. A sorted `Vec<[u64; 3]>` over
//! one ordering (e.g. SPO) implements it directly: `seek()` is a
//! `partition_point` binary search over the sorted slice, matching the
//! SIMD-friendly access pattern the LOUDS backend's own vocab arrays are
//! deliberately kept as (`&[u64]`, see CLAUDE.md Hard-Won Lesson #1).
//!
//! This lets a fixpoint round's `lftj_step` recursion mix atom sources
//! freely: some triple-pattern atoms scan the stable, large LOUDS tries
//! (the `total` relation so far), while at least one atom (the recursive
//! rule reference) scans a `SortedVecTrie` built fresh over the *previous
//! round's newly-derived facts* (`delta`). The leapfrog recursion itself is
//! completely unaware of the difference — it only ever holds
//! `Box<dyn TrieIterator>`s.
//!
//! ## Depth model
//!
//! A `SortedVecTrie` is built once per ordering (e.g. sorted by `(s, p, o)`
//! for an SPO-ordered scan) and represents depth 0 (unique first-column
//! values). Calling `open()` narrows to the sub-slice matching the current
//! key and returns a depth-1 iterator over the second column, and so on.
//! This exactly mirrors the LOUDS trie's own three-level (S/P/O) descent
//! protocol described in `oxigraph_nova_core::trie`'s module docs.

use oxigraph_nova_core::TrieIterator;

/// A sorted, deduplicated column slice at one trie depth, restricted to the
/// rows sharing the parent's bound prefix.
///
/// `rows` is the *entire* sorted relation (shared, never copied per level);
/// `range` is the `[lo, hi)` sub-slice of `rows` visible at this depth (all
/// rows sharing the same prefix bound by ancestor `open()` calls); `col` is
/// which of the 3 tuple positions this depth iterates over; `pos` is the
/// current cursor within `range`.
pub struct SortedVecTrie {
    rows: std::sync::Arc<Vec<[u64; 3]>>,
    range: (usize, usize),
    col: usize,
    pos: usize,
}

impl SortedVecTrie {
    /// Build a depth-0 `SortedVecTrie` over `rows`, which **must** already be
    /// sorted ascending by the 3-tuple (lexicographic on `[u64;3]`, i.e. by
    /// column 0 first) and deduplicated — exactly the invariant the LOUDS
    /// backend's own per-ordering triple arrays maintain. `rows` should be
    /// small (one fixpoint round's worth of newly-derived facts); this is
    /// not intended for large relations, where the compacted LOUDS index is
    /// the correct tool.
    pub fn new(rows: Vec<[u64; 3]>) -> Self {
        debug_assert!(
            rows.windows(2).all(|w| w[0] <= w[1]),
            "SortedVecTrie::new requires pre-sorted, deduplicated input"
        );
        let len = rows.len();
        Self {
            rows: std::sync::Arc::new(rows),
            range: (0, len),
            col: 0,
            pos: 0,
        }
    }

    /// Build a `SortedVecTrie` from arbitrary (unsorted, possibly duplicated)
    /// rows, sorting and deduplicating internally. Convenience constructor
    /// for callers (e.g. the fixpoint driver) that just accumulated a
    /// `Vec<[u64; 3]>` of newly-derived triples in encounter order.
    pub fn from_unsorted(mut rows: Vec<[u64; 3]>) -> Self {
        rows.sort_unstable();
        rows.dedup();
        Self::new(rows)
    }

    /// `true` if this depth-0 trie has no rows at all (fixpoint round
    /// produced nothing new — the caller should treat this the same as an
    /// absent/empty delta relation).
    pub fn is_empty(&self) -> bool {
        self.range.0 >= self.range.1
    }

    /// Current value at `col` for row index `i` within `rows`.
    #[inline]
    fn val_at(&self, i: usize) -> u64 {
        self.rows[i][self.col]
    }

    /// Binary search within `self.range` for the first row index whose
    /// `col`-value is `>= target`. Mirrors `&[u64]`'s `partition_point`
    /// (Hard-Won Lesson #1: SIMD-eligible, no bit-packing).
    #[inline]
    fn lower_bound(&self, target: u64) -> usize {
        let (lo, hi) = self.range;
        lo + self.rows[lo..hi].partition_point(|row| row[self.col] < target)
    }
}

impl SortedVecTrie {
    /// Build a `SortedVecTrie` positioned directly at column `col`, from a
    /// pre-sorted, deduplicated list of values for that column.
    ///
    /// Used by [`join_scan`] to answer a query against a *filtered
    /// projection* of a delta relation — e.g. "give me all distinct object
    /// values, given the subject and predicate are bound" — without needing
    /// the full 3-column lexicographic sort order [`new`][Self::new]
    /// requires. Other tuple positions in the synthetic backing rows are
    /// unused (zero-filled) since only `col` is ever read at this depth, and
    /// no further `open()` past this depth is expected from this
    /// constructor's callers.
    fn at_column(values: Vec<u64>, col: usize) -> Self {
        debug_assert!(
            values.windows(2).all(|w| w[0] < w[1]),
            "at_column requires strictly-ascending, deduplicated input"
        );
        let rows: Vec<[u64; 3]> = values
            .into_iter()
            .map(|v| {
                let mut r = [0u64; 3];
                r[col] = v;
                r
            })
            .collect();
        let len = rows.len();
        Self {
            rows: std::sync::Arc::new(rows),
            range: (0, len),
            col,
            pos: 0,
        }
    }
}

/// Query a transient, in-memory relation of `[u64; 3]` rows the same way
/// [`oxigraph_nova_core::LftjSource::lftj_join_scan`] queries the stable
/// LOUDS index — this is the `Delta`-side counterpart used by
/// `crate::join::AtomSource::Delta`.
///
/// `s`/`p`/`o`: `Some(id)` = field is bound to this value; `None` = field is
/// either the target or an unbound later variable. `target_field`: 0=s,
/// 1=p, 2=o — identifies which field is being iterated.
///
/// Unlike the LOUDS backend (which picks one of 6 pre-built sort orders to
/// make bound-field descent efficient), this does a linear filter over
/// `rows` — correct for any bound-field/target combination regardless of
/// `rows`' original ordering, and cheap enough for the small, per-round
/// relations a semi-naive fixpoint round produces.
pub fn join_scan(
    rows: &[[u64; 3]],
    s: Option<u64>,
    p: Option<u64>,
    o: Option<u64>,
    target_field: usize,
) -> Box<dyn TrieIterator> {
    let bound = [s, p, o];
    let mut vals: Vec<u64> = rows
        .iter()
        .filter(|row| {
            (0..3).all(|i| i == target_field || bound[i].is_none_or(|v| v == row[i]))
        })
        .map(|row| row[target_field])
        .collect();
    vals.sort_unstable();
    vals.dedup();
    if vals.is_empty() {
        return Box::new(oxigraph_nova_core::EmptyTrieIter);
    }
    Box::new(SortedVecTrie::at_column(vals, target_field))
}

impl TrieIterator for SortedVecTrie {

    fn key(&self) -> u64 {
        self.val_at(self.range.0 + self.pos)
    }

    fn seek(&mut self, target: u64) {
        if self.at_end() {
            return;
        }
        let cur = self.range.0 + self.pos;
        if self.val_at(cur) >= target {
            return; // no-op per trait contract
        }
        let new_idx = self.lower_bound(target);
        self.pos = new_idx - self.range.0;
    }

    fn advance(&mut self) {
        if self.at_end() {
            return;
        }
        let cur_key = self.key();
        // Skip past every row sharing the current key (dedup at this depth —
        // multiple rows can share the same col-0 value while differing in
        // later columns, e.g. two triples with the same subject).
        let next_key = cur_key.saturating_add(1);
        self.seek(next_key);
    }

    fn open(&self) -> Box<dyn TrieIterator> {
        if self.at_end() {
            return Box::new(oxigraph_nova_core::EmptyTrieIter);
        }
        let cur_key = self.key();
        let cur = self.range.0 + self.pos;
        // Narrow to the contiguous sub-range sharing `cur_key` at `self.col`
        // — since `rows` is sorted lexicographically, this range is
        // contiguous and can itself be found via two more partition_points.
        let lo = cur;
        let hi = {
            let (_, range_hi) = self.range;
            cur + self.rows[cur..range_hi].partition_point(|row| row[self.col] == cur_key)
        };
        Box::new(SortedVecTrie {
            rows: std::sync::Arc::clone(&self.rows),
            range: (lo, hi),
            col: self.col + 1,
            pos: 0,
        })
    }

    fn at_end(&self) -> bool {
        self.range.0 + self.pos >= self.range.1
    }

    fn remaining_count(&self) -> u64 {
        if self.at_end() {
            return 0;
        }
        // Real (not estimated) count of distinct remaining col-values from
        // the current position to the end of this node's range — mirrors
        // the LOUDS backend's O(1) `hi - pos + 1` answer to the same
        // question, just computed via a linear dedup-count here since these
        // relations are expected to be small per round.
        let (_, hi) = self.range;
        let mut count = 0u64;
        let mut i = self.range.0 + self.pos;
        while i < hi {
            count += 1;
            let v = self.rows[i][self.col];
            i += self.rows[i..hi].partition_point(|row| row[self.col] == v);
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the canonical 3-triple fixture: (1,10,2), (2,10,3), (1,11,3) —
    /// sorted lexicographically by (s,p,o).
    fn fixture() -> Vec<[u64; 3]> {
        let mut rows = vec![[1, 10, 2], [2, 10, 3], [1, 11, 3]];
        rows.sort_unstable();
        rows
    }

    #[test]
    fn depth0_iterates_distinct_subjects() {
        let mut t = SortedVecTrie::new(fixture());
        assert!(!t.at_end());
        assert_eq!(t.key(), 1);
        t.advance();
        assert_eq!(t.key(), 2);
        t.advance();
        assert!(t.at_end());
    }

    #[test]
    fn open_descends_to_predicates_under_subject() {
        let t = SortedVecTrie::new(fixture());
        // subject=1 has two rows: (1,10,2) and (1,11,3) → predicates {10,11}.
        let child = t.open();
        assert_eq!(child.key(), 10);
        let mut child = child;
        child.advance();
        assert_eq!(child.key(), 11);
        child.advance();
        assert!(child.at_end());
    }

    #[test]
    fn open_three_levels_reaches_object() {
        let t = SortedVecTrie::new(fixture());
        let p_level = t.open(); // subject=1
        let o_level = p_level.open(); // predicate=10 under subject=1
        assert_eq!(o_level.key(), 2); // object of (1,10,2)
    }

    #[test]
    fn seek_skips_ahead() {
        let mut t = SortedVecTrie::new(fixture());
        t.seek(2);
        assert_eq!(t.key(), 2);
        t.seek(100);
        assert!(t.at_end());
    }

    #[test]
    fn seek_is_noop_when_target_leq_current() {
        let mut t = SortedVecTrie::new(fixture());
        t.seek(2);
        assert_eq!(t.key(), 2);
        t.seek(1); // target <= current key → no-op per trait contract
        assert_eq!(t.key(), 2);
    }

    #[test]
    fn empty_relation_is_immediately_at_end() {
        let t = SortedVecTrie::new(vec![]);
        assert!(t.at_end());
        assert!(t.is_empty());
    }

    #[test]
    fn from_unsorted_sorts_and_dedups() {
        let t = SortedVecTrie::from_unsorted(vec![[2, 10, 3], [1, 10, 2], [1, 10, 2]]);
        assert_eq!(t.key(), 1);
    }

    #[test]
    fn remaining_count_matches_distinct_keys() {
        let t = SortedVecTrie::new(fixture());
        // Two distinct subjects (1, 2) at depth 0.
        assert_eq!(t.remaining_count(), 2);
        let child = t.open(); // subject=1 → predicates {10, 11}
        assert_eq!(child.remaining_count(), 2);
    }
}
