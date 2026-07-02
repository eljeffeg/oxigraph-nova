//! `TrieIterator` — the common interface consumed by the Leapfrog Triejoin evaluator.
//!
//! ## Why this abstraction exists
//!
//! WCOJ (Worst-Case Optimal Join) algorithms like Leapfrog Triejoin (LFTJ, Veldhuizen ICDT
//! 2014) require **seek-capable, depth-first iterators** over an ordered integer trie — they
//! do not operate on `Term`-level pattern matching or row-at-a-time `Iterator<Item=Quad>`.
//!
//! This trait is the seam between the storage layer (Ring index + delta) and the
//! LFTJ evaluator. Both the Ring's sorted arrays and the delta `BTreeMap<u128, bool>` expose
//! a `TrieIterator` implementation; the evaluator consumes a merged view without knowing
//! which tier answered each row.
//!
//! ## Protocol
//!
//! An iterator is positioned at one **depth level** of the trie. At each level it yields
//! successive distinct keys (integer term IDs). The caller descends via `open()` to create
//! a child iterator restricted to the subtrie under the current key.
//!
//! ```text
//! depth 0: unique subjects  (or predicates, depending on the sort ordering)
//!   depth 1: unique predicates within the current subject
//!     depth 2: unique objects within the current (subject, predicate)
//! ```
//!
//! ### Typical LFTJ inner loop:
//!
//! ```text
//! 1. Gather one TrieIterator per triple pattern, all at depth `d`.
//! 2. Repeat:
//!    a. Let max = largest current key across all iterators.
//!    b. Seek every iterator to max.
//!    c. If all keys equal max → emit; open children for depth d+1; advance past max.
//!    d. Otherwise → one iterator advanced beyond max; that new key becomes the target.
//! ```
//!
//! ## Compatibility with nested-loop evaluation
//!
//! The `Dataset` trait and nested-loop evaluator are **completely unchanged**.
//! `TrieIterator` is additive — an optional capability that storage backends may expose
//! on top of `QuadStore`. The LFTJ evaluator will check for this capability at runtime
//! and fall back to the nested-loop path if not available.

/// A depth-first iterator over one level of an ordered ID trie.
///
/// Used by the LFTJ evaluator; implemented by both the Ring index (sorted arrays)
/// and the delta `BTreeMap<u128, bool>`.
///
/// ## Invariants
///
/// * `key()` and `open()` must only be called when `at_end()` is `false`.
/// * After `seek(t)`, `key() >= t` (or `at_end() == true`).
/// * After `advance()`, if not `at_end()`, `key() > previous_key`.
/// * `open()` returns an iterator over the **subtrie** of the current key.  
///   The caller must not call `seek()` / `advance()` on the parent while the
///   child iterator is alive.
pub trait TrieIterator: Send {
    /// The current key (term ID) at this depth level.
    ///
    /// **Precondition:** `!self.at_end()`
    fn key(&self) -> u64;

    /// Advance to the first key ≥ `target` at the current depth level.
    ///
    /// If no such key exists, the iterator becomes exhausted (`at_end() == true`).
    /// If `target <= self.key()`, this is a no-op (the iterator stays put).
    fn seek(&mut self, target: u64);

    /// Advance past the current key to the next distinct key at this level.
    ///
    /// Equivalent to `seek(self.key() + 1)` but may be implemented more
    /// efficiently (e.g., using a stored "end of current key's run" cursor).
    ///
    /// **Precondition:** `!self.at_end()`
    fn advance(&mut self) {
        let next = self.key().saturating_add(1);
        self.seek(next);
    }

    /// Step into the subtrie under the current key, returning a new
    /// `TrieIterator` at `depth + 1`.
    ///
    /// The returned iterator is positioned at the first child key (or exhausted
    /// if the current node has no children, which should not occur for valid data).
    ///
    /// **Precondition:** `!self.at_end()` and `depth < MAX_DEPTH`.
    fn open(&self) -> Box<dyn TrieIterator>;

    /// `true` when the iterator has no more keys at the current depth level.
    fn at_end(&self) -> bool;
}

// ── Sentinel iterator ─────────────────────────────────────────────────────────

/// Always-exhausted [`TrieIterator`] sentinel.
///
/// Returned by `RingStore::lftj_join_scan` when a graph exists in the
/// dictionary but has no compacted Ring entries (no triples have been
/// flushed via `compact()` for that graph yet). Using an empty iterator
/// rather than `None` lets the LFTJ evaluator treat it uniformly with
/// real iterators: any join that includes this scan immediately terminates.
pub struct EmptyTrieIter;

impl TrieIterator for EmptyTrieIter {
    fn key(&self) -> u64 {
        0
    }
    fn seek(&mut self, _: u64) {}
    fn open(&self) -> Box<dyn TrieIterator> {
        Box::new(EmptyTrieIter)
    }
    fn at_end(&self) -> bool {
        true
    }
}
