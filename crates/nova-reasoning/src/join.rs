//! A generic, heterogeneous-source Leapfrog Triejoin (LFTJ) helper.
//!
//! `nova-query`'s `lftj_step` (`crates/nova-query/src/lftj.rs`) is hard-wired
//! to a single [`Dataset`](oxigraph_nova_core) scan source per join — every
//! atom in a BGP calls the same `dataset.lftj_join_scan(...)`. A semi-naive
//! datalog fixpoint round needs to mix sources *within one join*: some
//! atoms scan the stable, already-compacted relation (`Total`, everything
//! derived up to and including the previous round), while at least one atom
//! scans the transient, in-memory relation of facts derived by the
//! *immediately preceding* round (`Delta`) — see `sorted_vec_trie`'s module
//! docs for why this split matters.
//!
//! This module is deliberately independent of `oxigraph-nova-query` (no
//! dependency on `spargebra`/`Dataset`/`GraphSelector`): rule bodies are
//! small, fixed-shape join patterns known entirely at Rust compile time (no
//! SPARQL algebra to walk), so a much smaller, self-contained leapfrog
//! implementation suffices here. Variable ordering is fixed
//! (first-appearance, `0..num_vars`) rather than CLTJ*'s adaptive VEO —
//! correctness, not join-order optimality, is this module's job; rule
//! bodies in OWL 2 RL are 1–3 atoms, so ordering barely matters at this
//! scale.

use oxigraph_nova_core::TrieIterator;

/// One field (subject/predicate/object) of a rule-body atom.
#[derive(Clone, Copy, Debug)]
pub enum AtomField {
    /// A constant TermId (e.g. the interned `rdfs:subClassOf` predicate).
    Const(u64),
    /// A rule variable, identified by a stable index into the rule's
    /// variable list (0..num_vars).
    Var(usize),
}

/// Something that can answer an LFTJ join-scan request: "give me a
/// `TrieIterator` over the values `target_field` can take, given the other
/// two fields are bound to `s`/`p`/`o`".
///
/// Implemented by both [`SliceSource`] (a transient in-memory relation —
/// the `Delta` or `Total` side of a fixpoint round) and, in a full
/// production integration, a thin wrapper over
/// `oxigraph_nova_core::LftjSource` (the stable, compacted LOUDS index —
/// the `EDB`/base-facts side, unaffected by fixpoint rounds).
pub trait AtomSource {
    fn scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator>;
}

/// An in-memory relation of `[u64; 3]` rows, queryable as an [`AtomSource`].
///
/// Wraps `crate::sorted_vec_trie::join_scan` — see that function's doc
/// comment for the linear-filter-then-sort implementation (cheap and
/// correct for the small per-round relations a semi-naive fixpoint
/// produces).
pub struct SliceSource<'a> {
    pub rows: &'a [[u64; 3]],
}

impl<'a> SliceSource<'a> {
    pub fn new(rows: &'a [[u64; 3]]) -> Self {
        Self { rows }
    }
}

impl AtomSource for SliceSource<'_> {
    fn scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator> {
        crate::sorted_vec_trie::join_scan(self.rows, s, p, o, target_field)
    }
}

/// An [`AtomSource`] that always yields an empty (already-exhausted) scan —
/// a placeholder "no EDB" base source, useful when a fixpoint run has no
/// separate store-backed relation (e.g. pure in-memory unit tests where the
/// base facts are already folded into `Total` directly).
pub struct NullSource;

impl AtomSource for NullSource {
    fn scan(
        &self,
        _s: Option<u64>,
        _p: Option<u64>,
        _o: Option<u64>,
        _target_field: usize,
    ) -> Box<dyn TrieIterator> {
        Box::new(oxigraph_nova_core::EmptyTrieIter)
    }
}

/// An [`AtomSource`] that transparently unions two other sources — e.g. the
/// stable, LOUDS-backed base facts (EDB, via
/// [`crate::store_source::StoreAtomSource`]) with the in-memory closure
/// derived so far (IDB, via [`SliceSource`]) — without copying either side
/// into a combined `Vec` first.
///
/// This is what lets a semi-naive fixpoint round's "Total" atoms scan the
/// real store directly every round rather than re-materializing the base
/// relation into memory on every iteration: `primary` is queried once per
/// scan call exactly like any other `AtomSource`, and [`UnionTrieIter`]
/// merges its results with `secondary`'s on the fly, key by key.
pub struct CombinedSource<'a> {
    pub primary: &'a dyn AtomSource,
    pub secondary: &'a dyn AtomSource,
}

impl AtomSource for CombinedSource<'_> {
    fn scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator> {
        let a = self.primary.scan(s, p, o, target_field);
        let b = self.secondary.scan(s, p, o, target_field);
        UnionTrieIter::new(a, b)
    }
}

/// A [`TrieIterator`] that merges two other `TrieIterator`s into one sorted,
/// deduplicated key stream — the trie-level equivalent of a merge-union of
/// two sorted iterators, recursing through `open()` at every depth.
///
/// Both `a` and `b` are ordinary `TrieIterator`s (backend-agnostic — one may
/// be LOUDS-backed, the other a [`SortedVecTrie`](crate::SortedVecTrie)); the
/// union is computed purely through the trait's `key`/`seek`/`open`/`at_end`
/// contract, so it composes with any backend without either side knowing
/// about the other.
pub struct UnionTrieIter {
    a: Box<dyn TrieIterator>,
    b: Box<dyn TrieIterator>,
}

impl UnionTrieIter {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(a: Box<dyn TrieIterator>, b: Box<dyn TrieIterator>) -> Box<dyn TrieIterator> {
        Box::new(Self { a, b })
    }
}

impl TrieIterator for UnionTrieIter {
    fn key(&self) -> u64 {
        match (self.a.at_end(), self.b.at_end()) {
            (true, true) => 0, // precondition violated by caller; harmless default
            (true, false) => self.b.key(),
            (false, true) => self.a.key(),
            (false, false) => self.a.key().min(self.b.key()),
        }
    }

    fn seek(&mut self, target: u64) {
        if !self.a.at_end() {
            self.a.seek(target);
        }
        if !self.b.at_end() {
            self.b.seek(target);
        }
    }

    fn advance(&mut self) {
        if self.at_end() {
            return;
        }
        let next = self.key().saturating_add(1);
        self.seek(next);
    }

    fn open(&self) -> Box<dyn TrieIterator> {
        if self.at_end() {
            return Box::new(oxigraph_nova_core::EmptyTrieIter);
        }
        let cur = self.key();
        let a_matches = !self.a.at_end() && self.a.key() == cur;
        let b_matches = !self.b.at_end() && self.b.key() == cur;
        match (a_matches, b_matches) {
            (true, true) => UnionTrieIter::new(self.a.open(), self.b.open()),
            (true, false) => self.a.open(),
            (false, true) => self.b.open(),
            (false, false) => unreachable!("open() called with neither side at the current key"),
        }
    }

    fn at_end(&self) -> bool {
        self.a.at_end() && self.b.at_end()
    }

    fn remaining_count(&self) -> u64 {
        // A precise merged count would require deduplicating overlapping
        // keys between `a` and `b`, which isn't worth the O(n) cost here —
        // callers (VEO cardinality estimation) already treat u64::MAX as
        // "unknown, fall back to stable ordering".
        u64::MAX
    }
}

/// One fully-classified rule-body atom: its three fields, plus which
/// [`AtomSource`] answers scans for it.
///
/// The `source` field is what makes the join heterogeneous — two atoms in
/// the same [`leapfrog_join`] call can point at two entirely different
/// backing relations (e.g. one at `Total`, one at `Delta`).
pub struct Atom<'a> {
    pub s: AtomField,
    pub p: AtomField,
    pub o: AtomField,
    pub source: &'a dyn AtomSource,
}

impl Atom<'_> {
    fn is_active_for_var(&self, var: usize) -> bool {
        let hit = |f: &AtomField| matches!(f, AtomField::Var(v) if *v == var);
        hit(&self.s) || hit(&self.p) || hit(&self.o)
    }

    /// Resolve this atom's fields for a scan targeting `var`, given the
    /// current `bindings`. Mirrors `nova-query`'s
    /// `PatternSpec::resolve_for_var`.
    fn resolve_for_var(
        &self,
        var: usize,
        bindings: &[Option<u64>],
    ) -> (Option<u64>, Option<u64>, Option<u64>, usize) {
        let resolve = |f: &AtomField| -> Option<u64> {
            match f {
                AtomField::Const(id) => Some(*id),
                AtomField::Var(v) => bindings[*v],
            }
        };
        let target_field = match (&self.s, &self.p, &self.o) {
            (AtomField::Var(v), _, _) if *v == var => 0,
            (_, AtomField::Var(v), _) if *v == var => 1,
            (_, _, AtomField::Var(v)) if *v == var => 2,
            _ => unreachable!("atom must be active for var"),
        };
        (
            resolve(&self.s),
            resolve(&self.p),
            resolve(&self.o),
            target_field,
        )
    }
}

/// Leapfrog-synchronize a set of scans to their common minimum key.
///
/// Identical algorithm to `nova-query`'s `leapfrog_sync` — see
/// `crates/nova-query/src/lftj.rs` for the reference implementation this
/// mirrors.
fn leapfrog_sync(scans: &mut [Box<dyn TrieIterator>]) -> Option<u64> {
    if scans.is_empty() || scans.iter().any(|s| s.at_end()) {
        return None;
    }
    loop {
        let max_key = scans.iter().map(|s| s.key()).max()?;
        let mut all_at_max = true;
        for scan in scans.iter_mut() {
            if scan.key() < max_key {
                scan.seek(max_key);
                if scan.at_end() {
                    return None;
                }
                if scan.key() != max_key {
                    all_at_max = false;
                    break;
                }
            }
        }
        if all_at_max {
            return Some(max_key);
        }
    }
}

/// Recursive join step: bind `unbound[0]`, recurse over `unbound[1..]`.
fn join_step(
    atoms: &[Atom],
    unbound: &[usize],
    bindings: &mut Vec<Option<u64>>,
    results: &mut Vec<Vec<u64>>,
) {
    if unbound.is_empty() {
        results.push(bindings.iter().map(|b| b.expect("fully bound")).collect());
        return;
    }

    let var = unbound[0];
    let remaining = &unbound[1..];

    let active: Vec<&Atom> = atoms.iter().filter(|a| a.is_active_for_var(var)).collect();
    if active.is_empty() {
        // No atom constrains this variable — should not arise for
        // well-formed rule bodies, but recurse without binding rather than
        // panicking.
        join_step(atoms, remaining, bindings, results);
        return;
    }

    let mut scans: Vec<Box<dyn TrieIterator>> = Vec::with_capacity(active.len());
    for a in &active {
        let (s, p, o, target_field) = a.resolve_for_var(var, bindings);
        scans.push(a.source.scan(s, p, o, target_field));
    }
    if scans.iter().any(|s| s.at_end()) {
        return;
    }

    loop {
        match leapfrog_sync(&mut scans) {
            None => break,
            Some(val) => {
                bindings[var] = Some(val);
                join_step(atoms, remaining, bindings, results);
                bindings[var] = None;
                scans[0].advance();
                if scans[0].at_end() {
                    break;
                }
            }
        }
    }
}

/// Evaluate a rule body (a conjunction of [`Atom`]s, each possibly scanning
/// a different [`AtomSource`]) via leapfrog join.
///
/// Returns one `Vec<u64>` per matching binding, indexed by variable
/// (`result[i]` = the value bound to variable `i`), for variables
/// `0..num_vars`.
pub fn leapfrog_join(atoms: &[Atom], num_vars: usize) -> Vec<Vec<u64>> {
    let mut bindings: Vec<Option<u64>> = vec![None; num_vars];
    let unbound: Vec<usize> = (0..num_vars).collect();
    let mut results = Vec::new();
    join_step(atoms, &unbound, &mut bindings, &mut results);
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `?x sc ?z . ?z sc ?y` over a single relation {(1,sc,2), (2,sc,3)}
    /// (sc = predicate id 100) should join on `z` and yield exactly one
    /// binding: x=1, z=2, y=3.
    #[test]
    fn two_atom_join_single_source() {
        let rows: Vec<[u64; 3]> = vec![[1, 100, 2], [2, 100, 3]];
        let src = SliceSource::new(&rows);
        let atoms = vec![
            Atom {
                s: AtomField::Var(0),
                p: AtomField::Const(100),
                o: AtomField::Var(1),
                source: &src,
            },
            Atom {
                s: AtomField::Var(1),
                p: AtomField::Const(100),
                o: AtomField::Var(2),
                source: &src,
            },
        ];
        let mut results = leapfrog_join(&atoms, 3);
        results.sort();
        assert_eq!(results, vec![vec![1, 2, 3]]);
    }

    /// Same join, but atom 0 scans one relation (`total`) and atom 1 scans
    /// a *different* relation (`delta`) — proving the join is genuinely
    /// heterogeneous-source.
    #[test]
    fn two_atom_join_heterogeneous_sources() {
        let total: Vec<[u64; 3]> = vec![[1, 100, 2], [5, 100, 6]]; // only x-z edges
        let delta: Vec<[u64; 3]> = vec![[2, 100, 3]]; // only the new z-y edge
        let total_src = SliceSource::new(&total);
        let delta_src = SliceSource::new(&delta);
        let atoms = vec![
            Atom {
                s: AtomField::Var(0),
                p: AtomField::Const(100),
                o: AtomField::Var(1),
                source: &total_src,
            },
            Atom {
                s: AtomField::Var(1),
                p: AtomField::Const(100),
                o: AtomField::Var(2),
                source: &delta_src,
            },
        ];
        let results = leapfrog_join(&atoms, 3);
        assert_eq!(results, vec![vec![1, 2, 3]]);
    }

    #[test]
    fn no_match_yields_empty() {
        let rows: Vec<[u64; 3]> = vec![[1, 100, 2]];
        let src = SliceSource::new(&rows);
        let atoms = vec![
            Atom {
                s: AtomField::Var(0),
                p: AtomField::Const(100),
                o: AtomField::Var(1),
                source: &src,
            },
            Atom {
                s: AtomField::Var(1),
                p: AtomField::Const(100),
                o: AtomField::Var(2),
                source: &src,
            },
        ];
        let results = leapfrog_join(&atoms, 3);
        assert!(results.is_empty());
    }

    /// `CombinedSource` over two disjoint relations must behave like a
    /// single relation containing the union of both.
    #[test]
    fn combined_source_unions_disjoint_relations() {
        let a: Vec<[u64; 3]> = vec![[1, 100, 2]];
        let b: Vec<[u64; 3]> = vec![[2, 100, 3]];
        let src_a = SliceSource::new(&a);
        let src_b = SliceSource::new(&b);
        let combined = CombinedSource {
            primary: &src_a,
            secondary: &src_b,
        };
        let atoms = vec![
            Atom {
                s: AtomField::Var(0),
                p: AtomField::Const(100),
                o: AtomField::Var(1),
                source: &combined,
            },
            Atom {
                s: AtomField::Var(1),
                p: AtomField::Const(100),
                o: AtomField::Var(2),
                source: &combined,
            },
        ];
        let mut results = leapfrog_join(&atoms, 3);
        results.sort();
        assert_eq!(results, vec![vec![1, 2, 3]]);
    }

    /// `CombinedSource` must deduplicate a row present in *both* underlying
    /// sources (e.g. a fact that is both a base fact and was re-derived).
    #[test]
    fn combined_source_dedups_overlapping_rows() {
        let a: Vec<[u64; 3]> = vec![[1, 100, 2]];
        let b: Vec<[u64; 3]> = vec![[1, 100, 2]]; // same row in both sides
        let src_a = SliceSource::new(&a);
        let src_b = SliceSource::new(&b);
        let combined = CombinedSource {
            primary: &src_a,
            secondary: &src_b,
        };
        let mut scan = combined.scan(Some(1), Some(100), None, 2);
        assert_eq!(scan.key(), 2);
        scan.advance();
        assert!(scan.at_end(), "duplicate row must yield exactly one key");
    }
}
