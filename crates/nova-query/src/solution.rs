//! SPARQL solution mappings — the proper typed binding layer.
//!
//! A [`Solution`] is a partial mapping from [`Variable`] to [`Term`],
//! matching the SPARQL 1.1 § 18.1.7 "solution mapping" specification.
//! Using `oxrdf::Variable` as the key (not `String`) keeps us aligned with
//! the spargebra algebra types from the start, avoiding a stringly-typed
//! intermediary that would force refactoring once real evaluation starts.
//!
//! The evaluator builds `Vec<Solution>` (a "solution sequence") for SELECT,
//! a single `bool` for ASK, and `Vec<Triple>` for CONSTRUCT.
//!
//! The WCOJ evaluator can emit solutions lazily; `Solution` stays the same,
//! with `Solutions` as a type alias that can be changed to a streaming iterator
//! if needed in the future.

// Re-export so calling code can write `oxigraph_nova_query::Variable`.
pub use oxrdf::Variable as SparqlVariable;
use oxrdf::{BlankNode, Term, Variable};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// A single SPARQL solution mapping: a set of (variable → RDF term) bindings.
///
/// Variables that are not bound in this solution are simply absent from the
/// map — unbound is not the same as bound-to-null.
///
/// Also carries a `bnode_cache`: a per-solution-row cache used to implement
/// the SPARQL 1.1 `BNODE(strExpr)` function correctly (§17.4.2.7). Per spec,
/// repeated calls to `BNODE(strExpr)` with the same string value *within the
/// same solution* must return the *same* blank node, while different
/// solutions (even with the same string value) must get *fresh*, distinct
/// blank nodes. The cache is shared (via `Arc<Mutex<_>>` — `Arc` rather than
/// `Rc` so `Solution` is `Send` and can cross thread boundaries, e.g. into the
/// HTTP server's background serialization thread; the lock is never
/// contended since a solution row is only ever touched by one thread at a
/// time) across clones that represent the *same* logical row being
/// incrementally built up (e.g. BGP nested-loop extension,
/// `Extend`/`Filter`/`Project` passthroughs).
///
/// The cache is wrapped in a [`OnceLock`] so the `Arc<Mutex<HashMap<_>>>`
/// heap allocation is deferred until the first actual `BNODE(strExpr)` call
/// via [`Self::bnode_for`] — even though a fresh row's `OnceLock` starts
/// uninitialized, `bnode_for` can still lazily populate it through a shared
/// `&self` reference. `BNODE(strExpr)` is rare in practice, and
/// `Solution::new()` is called once per *result row* (e.g. 900K times for a
/// large LFTJ join), so this avoids one heap allocation per row for the
/// overwhelming majority of rows that never call `BNODE()`, at the cost of
/// a branch on every `bnode_for` call to check initialization.
#[derive(Debug, Clone, Default)]
pub struct Solution {
    bindings: HashMap<Variable, Term>,
    bnode_cache: OnceLock<Arc<Mutex<HashMap<String, BlankNode>>>>,
}

impl PartialEq for Solution {
    /// Two solutions are equal iff their bindings match; the internal
    /// `BNODE()` cache is bookkeeping, not part of a solution's identity.
    fn eq(&self, other: &Self) -> bool {
        self.bindings == other.bindings
    }
}

impl Solution {
    /// Create an empty solution (no variables bound), with no `BNODE()`
    /// cache allocated yet — i.e. a genuinely *new* solution row. The cache
    /// is lazily created on first use by [`Self::bnode_for`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `var` to `term`, overwriting any previous binding.
    pub fn insert(&mut self, var: Variable, term: Term) {
        self.bindings.insert(var, term);
    }

    /// Look up the binding for `var`. Returns `None` if unbound.
    pub fn get(&self, var: &Variable) -> Option<&Term> {
        self.bindings.get(var)
    }

    /// Returns `true` if `var` is bound in this solution.
    pub fn contains(&self, var: &Variable) -> bool {
        self.bindings.contains_key(var)
    }

    /// Number of bound variables.
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Iterate over all (variable, term) pairs in this solution.
    pub fn iter(&self) -> impl Iterator<Item = (&Variable, &Term)> {
        self.bindings.iter()
    }

    /// Get or create the blank node associated with `key` (the string value
    /// of `BNODE(strExpr)`'s argument) *within this solution row*. Because
    /// `bnode_cache` is an `Arc<Mutex<_>>` (behind a lazily-initialized
    /// `OnceLock`), clones produced while extending or filtering the *same*
    /// row (e.g. `sol.clone()` inside BGP nested-loop join, or `&mut sol` in
    /// `Extend`) share the same underlying cache once initialized, so
    /// repeated `BNODE(?x)` calls with the same value within one row's
    /// evaluation return the same blank node. A genuinely fresh row created
    /// via `Solution::new()` has no cache allocated yet — the first call
    /// here lazily creates one, so different rows never collide even if the
    /// argument value repeats, and rows that never call `BNODE()` never pay
    /// for the allocation at all.
    pub fn bnode_for(&self, key: &str) -> BlankNode {
        let cache = self
            .bnode_cache
            .get_or_init(|| Arc::new(Mutex::new(HashMap::new())));
        let mut cache = cache.lock().unwrap();
        cache.entry(key.to_string()).or_default().clone()
    }

    /// Try to merge `other` into this solution (SPARQL *compatible* merge).
    ///
    /// Two solutions are compatible if every variable they share is bound to
    /// the same value (SPARQL 1.1 § 18.1.8).  Returns `None` when they
    /// conflict on at least one shared variable, `Some(merged)` otherwise.
    ///
    /// This is the core operation for implementing hash-join and nested-loop
    /// join in the iterator evaluator.
    pub fn merge_compatible(&self, other: &Solution) -> Option<Solution> {
        // Check for conflicts on shared variables first.
        for (var, term) in &other.bindings {
            if let Some(existing) = self.bindings.get(var)
                && existing != term
            {
                return None;
            }
        }
        // No conflicts — build the union.
        let mut merged = self.clone();
        for (var, term) in &other.bindings {
            merged.bindings.insert(var.clone(), term.clone());
        }
        Some(merged)
    }

    /// Project this solution to the given set of variables (for SELECT list).
    ///
    /// Variables absent from `vars` are dropped; variables in `vars` but
    /// unbound in this solution remain absent (per SPARQL semantics — they
    /// must not appear as explicit `NULL`s at this layer).
    ///
    /// The `BNODE()` cache is preserved (shared, if already initialized)
    /// across the projection since this still represents the *same*
    /// solution row, just with a narrower set of visible variables.
    pub fn project<'a>(&self, vars: impl IntoIterator<Item = &'a Variable>) -> Solution {
        let mut projected = Solution {
            bindings: HashMap::new(),
            bnode_cache: self.bnode_cache.clone(),
        };
        for var in vars {
            if let Some(term) = self.bindings.get(var) {
                projected.insert(var.clone(), term.clone());
            }
        }
        projected
    }
}

/// A complete result table: a sequence of solution mappings.
/// Currently implemented as `Vec<Solution>`; the type alias allows
/// switching to a lazy iterator in the future if needed.
pub type Solutions = Vec<Solution>;

// ── Conversions ─────────────────────────────────────────────────────────────

impl FromIterator<(Variable, Term)> for Solution {
    fn from_iter<I: IntoIterator<Item = (Variable, Term)>>(iter: I) -> Self {
        let mut s = Solution::new();
        for (var, term) in iter {
            s.insert(var, term);
        }
        s
    }
}

impl IntoIterator for Solution {
    type Item = (Variable, Term);
    type IntoIter = std::collections::hash_map::IntoIter<Variable, Term>;

    fn into_iter(self) -> Self::IntoIter {
        self.bindings.into_iter()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::{Literal, NamedNode};

    fn var(name: &str) -> Variable {
        Variable::new_unchecked(name)
    }
    fn iri(s: &str) -> Term {
        Term::NamedNode(NamedNode::new_unchecked(s))
    }
    fn lit(s: &str) -> Term {
        Term::Literal(Literal::new_simple_literal(s))
    }

    #[test]
    fn merge_compatible_no_overlap() {
        let mut a = Solution::new();
        a.insert(var("x"), iri("http://ex/A"));

        let mut b = Solution::new();
        b.insert(var("y"), lit("hello"));

        let merged = a.merge_compatible(&b).expect("should merge");
        assert_eq!(merged.get(&var("x")), Some(&iri("http://ex/A")));
        assert_eq!(merged.get(&var("y")), Some(&lit("hello")));
    }

    #[test]
    fn merge_compatible_same_value() {
        let mut a = Solution::new();
        a.insert(var("x"), iri("http://ex/A"));

        let mut b = Solution::new();
        b.insert(var("x"), iri("http://ex/A")); // same binding

        assert!(a.merge_compatible(&b).is_some());
    }

    #[test]
    fn merge_incompatible_conflict() {
        let mut a = Solution::new();
        a.insert(var("x"), iri("http://ex/A"));

        let mut b = Solution::new();
        b.insert(var("x"), iri("http://ex/B")); // conflict

        assert!(a.merge_compatible(&b).is_none());
    }

    #[test]
    fn project_subset() {
        let mut s = Solution::new();
        s.insert(var("x"), iri("http://ex/A"));
        s.insert(var("y"), lit("hello"));
        s.insert(var("z"), lit("world"));

        let projected = s.project([&var("x"), &var("z")]);
        assert!(projected.contains(&var("x")));
        assert!(!projected.contains(&var("y")));
        assert!(projected.contains(&var("z")));
    }

    #[test]
    fn from_iterator() {
        let s: Solution = vec![(var("a"), iri("http://ex/1")), (var("b"), lit("two"))]
            .into_iter()
            .collect();
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn bnode_for_same_key_same_row() {
        let s = Solution::new();
        let b1 = s.bnode_for("BAZ");
        let b2 = s.bnode_for("BAZ");
        assert_eq!(b1, b2, "same key within one row must return same bnode");
    }

    #[test]
    fn bnode_for_different_rows_fresh() {
        let s1 = Solution::new();
        let s2 = Solution::new();
        let b1 = s1.bnode_for("BAZ");
        let b2 = s2.bnode_for("BAZ");
        assert_ne!(
            b1, b2,
            "different rows must get fresh bnodes even for same key"
        );
    }

    #[test]
    fn bnode_for_shared_across_clone_after_init() {
        // Once a row's bnode cache has been lazily initialized by a first
        // `bnode_for` call, clones taken *after* that point share the same
        // underlying `Arc<Mutex<HashMap<_>>>` (the `OnceLock` itself is
        // cloned as already-initialized, so `Clone` copies the `Arc`
        // pointer, not its contents). This matches every real evaluator
        // code path (`Filter`/`OrderBy`/`Extend`), which always calls
        // `bnode_for` on the same solution reference throughout one row's
        // evaluation rather than branching into independent clones first.
        let s = Solution::new();
        let _ = s.bnode_for("BAZ"); // force lazy init
        let cloned = s.clone();
        let b1 = s.bnode_for("BAZ");
        let b2 = cloned.bnode_for("BAZ");
        assert_eq!(
            b1, b2,
            "clones taken after cache init share the bnode cache (Arc<Mutex<_>>)"
        );
    }

    #[test]
    fn bnode_for_pre_init_clones_are_independent() {
        // A clone taken *before* the first `bnode_for` call has no cache to
        // share yet — the `OnceLock` is still uninitialized at clone time,
        // so `Clone` copies that uninitialized state rather than an `Arc`
        // pointer. Each side then lazily initializes its own independent
        // cache on first use, so they must NOT share bnode assignments, even
        // for the same key. This never happens on any real evaluator path
        // (which always calls `bnode_for` on one solution reference
        // throughout a row's evaluation before branching), but pinning the
        // behavior here guards against silently changing this nuance.
        let s = Solution::new();
        let cloned = s.clone(); // taken before any bnode_for call
        let b1 = s.bnode_for("BAZ");
        let b2 = cloned.bnode_for("BAZ");
        assert_ne!(
            b1, b2,
            "clones taken before cache init do not share a bnode cache"
        );
    }
}
