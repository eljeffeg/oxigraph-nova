//! OWL 2 RL reasoning for Oxigraph Nova.
//!
//! Primary execution engine: an **LFTJ-native semi-naive fixpoint driver**.
//! See `CLAUDE.md`'s Phase 3 design section for the full rationale — in
//! short, Nova's Leapfrog Triejoin (LFTJ) evaluator only operates over the
//! compacted LOUDS index, and a non-empty LSM delta disables it entirely
//! (`LftjSource::lftj_has_delta`). [`sorted_vec_trie::SortedVecTrie`] is the
//! fix: a transient, in-memory `TrieIterator` over one fixpoint round's
//! newly-derived facts, and [`join`] is the heterogeneous-source leapfrog
//! join helper that lets a rule body mix stable (LOUDS-backed, via
//! `AtomSource`) and transient (in-memory `SliceSource`) atom sources
//! within the same join. [`fixpoint`] drives semi-naive evaluation to
//! closure and [`rule`] defines the (currently hand-coded, eventually
//! rules-as-data) rule shape.

pub mod fixpoint;
pub mod join;
pub mod rule;
pub mod sorted_vec_trie;

pub use join::{Atom, AtomField, AtomSource, SliceSource, leapfrog_join};
pub use rule::Rule;
pub use sorted_vec_trie::SortedVecTrie;
