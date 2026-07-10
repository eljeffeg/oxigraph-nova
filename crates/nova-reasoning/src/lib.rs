//! OWL 2 RL reasoning for Oxigraph Nova.
//!
//! Primary execution engine: an **LFTJ-native semi-naive fixpoint driver**.
//! Nova's Leapfrog Triejoin (LFTJ) evaluator only operates over the
//! compacted LOUDS index, and a non-empty LSM delta disables it entirely
//! (`LftjSource::lftj_has_delta`). [`sorted_vec_trie::SortedVecTrie`] is the
//! fix: a transient, in-memory `TrieIterator` over one fixpoint round's
//! newly-derived facts, and [`join`] is the heterogeneous-source leapfrog
//! join helper that lets a rule body mix stable (LOUDS-backed, via
//! [`store_source::StoreAtomSource`]) and transient (in-memory
//! [`join::SliceSource`]) atom sources within the same join — composed via
//! [`join::CombinedSource`], which unions two `AtomSource`s without copying
//! either into a combined `Vec`. [`fixpoint`] drives semi-naive evaluation
//! to closure over a [`rule::RuleSet`] of rules-as-data (arbitrary N-atom
//! bodies, predicate-indexed dispatch so each round only runs rules whose
//! body could actually match the current delta).

pub mod engine;
pub mod fixpoint;
pub mod join;
pub mod reasoning_dataset;
pub mod rule;
pub mod same_as;
pub mod sorted_vec_trie;
pub mod store_source;

pub use engine::{Diagnostic, LftjFixpointEngine, ReasoningEngine, Severity};
pub use join::{
    Atom, AtomField, AtomSource, CombinedSource, NullSource, SliceSource, leapfrog_join,
};
pub use reasoning_dataset::ReasoningDataset;
pub use rule::{Rule, RuleAtom, RuleSet};
pub use same_as::SameAsTracker;
pub use sorted_vec_trie::SortedVecTrie;
pub use store_source::StoreAtomSource;
