//! [`ReasoningDataset`] — an in-memory inferred-facts overlay over any
//! [`Dataset`], computed via a pluggable [`ReasoningEngine`].
//!
//! Per the project's explicit design direction: reasoning defaults to an
//! **in-memory overlay** (this type), not persisted materialization into
//! the store at all. A fuller, eventually-consistent, background-worker-
//! driven materialization design remains a possible *future*, opt-in path
//! built on the same [`ReasoningEngine`] trait — writing its output into an
//! ordinary named graph (there is no reserved `GraphId` for this; any named
//! graph, chosen by the deployment, works identically — see
//! `oxigraph_nova_core::dict`'s module doc comment). This decorator is the
//! simpler "reason once, hold the result in memory, present it as part of
//! the default graph" mechanism.

//! ## Semantics
//!
//! - Inference is computed **eagerly**, once, at [`ReasoningDataset::wrap`]
//!   time — there is no background worker and no generation counter here.
//!   Callers that mutate the wrapped store after wrapping must construct a
//!   new `ReasoningDataset` to see updated inferences (a `reason_now()`-style
//!   refresh method is a natural, low-risk follow-up).
//! - `find_quads` over [`GraphSelector::Default`] or [`GraphSelector::Union`]
//!   transparently unions the inner dataset's matches with the inferred
//!   overlay's matches, presenting every inferred quad as a
//!   default-graph quad — matching the Jena/GraphDB convention that
//!   inference is part of the default graph view. `GraphSelector::Named`,
//!   `AnyNamed`, and `UnionOf` queries are **not** widened: the inference
//!   overlay is invisible to `GRAPH ?g { }` and friends, and
//!   [`ReasoningDataset::named_graphs`] never mentions a synthetic
//!   inference graph — nothing is persisted, so there is no such graph to
//!   mention.

//! - **LFTJ acceleration is intentionally disabled** on the wrapped view
//!   (`supports_lftj` always returns `false`), so the SPARQL evaluator
//!   always uses its nested-loop fallback against `find_quads` — which is
//!   what correctly unions the overlay in. Restoring LFTJ acceleration
//!   would require a `TrieIterator` over the (small, already TermId-keyed)
//!   inferred overlay unioned with the inner dataset's trie, mirroring
//!   [`crate::join::CombinedSource`]/[`crate::join::UnionTrieIter`] — a
//!   reasonable follow-up once this decorator's basic correctness is
//!   proven out, not required for the initial hook point.

use crate::engine::{Diagnostic, ReasoningEngine};
use anyhow::Result;
use oxigraph_nova_core::{GraphName, Term};
use oxigraph_nova_query::{
    Dataset, DatasetLftjSource, GraphSelector, QuadIter, QuadMatch, QuadPattern,
};

/// Wraps a base [`Dataset`] `D` with an in-memory closure computed once (at
/// construction) by a [`ReasoningEngine`]. See the module doc comment for
/// full semantics.
pub struct ReasoningDataset<D: Dataset> {
    inner: D,
    inferred: Vec<(Term, Term, Term)>,
    diagnostics: Vec<Diagnostic>,
}

impl<D: Dataset> ReasoningDataset<D> {
    /// Run `engine` over `inner` once and hold the result as an in-memory
    /// overlay. Fails only if the engine itself errors (e.g. a store I/O
    /// failure surfaced through `find_quads`); an engine that finds nothing
    /// to infer is not an error (see [`ReasoningEngine::infer`]).
    pub fn wrap(inner: D, engine: &dyn ReasoningEngine) -> Result<Self> {
        let (inferred_quads, diagnostics) = engine.infer(&inner)?;
        let inferred = inferred_quads
            .into_iter()
            .map(|q| {
                (
                    Term::from(q.subject),
                    Term::NamedNode(q.predicate),
                    q.object,
                )
            })
            .collect();
        Ok(Self {
            inner,
            inferred,
            diagnostics,
        })
    }

    /// Diagnostics collected the one time this overlay's [`ReasoningEngine`]
    /// ran — never populated/refreshed afterward (see module doc comment).
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The inner, un-widened dataset this decorator wraps.
    pub fn inner(&self) -> &D {
        &self.inner
    }

    /// Number of distinct inferred triples held in this overlay.
    pub fn inferred_len(&self) -> usize {
        self.inferred.len()
    }

    fn inferred_matches<'a>(
        &'a self,
        pattern: &QuadPattern,
    ) -> impl Iterator<Item = QuadMatch> + 'a {
        let widen = matches!(pattern.graph, GraphSelector::Default | GraphSelector::Union);
        let pattern = pattern.clone();
        self.inferred.iter().filter_map(move |(s, p, o)| {
            if !widen {
                return None;
            }

            let s_ok = match &pattern.subject {
                oxigraph_nova_query::PatternTerm::Variable => true,
                oxigraph_nova_query::PatternTerm::Bound(v) => v == s,
            };
            let p_ok = match &pattern.predicate {
                oxigraph_nova_query::PatternTerm::Variable => true,
                oxigraph_nova_query::PatternTerm::Bound(v) => v == p,
            };
            let o_ok = match &pattern.object {
                oxigraph_nova_query::PatternTerm::Variable => true,
                oxigraph_nova_query::PatternTerm::Bound(v) => v == o,
            };
            if s_ok && p_ok && o_ok {
                Some(QuadMatch {
                    subject: s.clone(),
                    predicate: p.clone(),
                    object: o.clone(),
                    graph_name: GraphName::DefaultGraph,
                })
            } else {
                None
            }
        })
    }
}

/// LFTJ acceleration is deliberately turned off on the wrapped view — see
/// module doc comment. Every method here uses the trait's "unsupported"
/// default (i.e. this impl block is intentionally empty).
impl<D: Dataset> DatasetLftjSource for ReasoningDataset<D> {}

impl<D: Dataset> Dataset for ReasoningDataset<D> {
    fn find_quads<'a>(&'a self, pattern: &QuadPattern) -> Result<QuadIter<'a>> {
        let base = self.inner.find_quads(pattern)?;
        let overlay = self.inferred_matches(pattern).map(Ok);
        Ok(Box::new(base.chain(overlay)))
    }

    fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
        // No synthetic inference graph exists to enumerate — everything
        // inferred is presented as part of the (unnamed) default graph.
        self.inner.named_graphs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::LftjFixpointEngine;
    use oxigraph_nova_core::{GraphName as CoreGraphName, NamedNode, Quad, QuadStore};
    use oxigraph_nova_query::{PatternTerm, StoreDataset};
    use oxigraph_nova_storage_ring::RingStore;
    use std::sync::Arc;

    fn nn(s: &str) -> NamedNode {
        NamedNode::new(s).unwrap()
    }

    fn rdf_type() -> NamedNode {
        nn("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
    }

    fn rdfs_sub_class_of() -> NamedNode {
        nn("http://www.w3.org/2000/01/rdf-schema#subClassOf")
    }

    fn build_store() -> RingStore {
        let store = RingStore::new();
        let g = CoreGraphName::DefaultGraph;
        store
            .insert(&Quad::new(
                nn("http://ex/Dog"),
                rdfs_sub_class_of(),
                Term::NamedNode(nn("http://ex/Mammal")),
                g.clone(),
            ))
            .unwrap();
        store
            .insert(&Quad::new(
                nn("http://ex/Mammal"),
                rdfs_sub_class_of(),
                Term::NamedNode(nn("http://ex/Animal")),
                g.clone(),
            ))
            .unwrap();
        store
            .insert(&Quad::new(
                nn("http://ex/fido"),
                rdf_type(),
                Term::NamedNode(nn("http://ex/Dog")),
                g,
            ))
            .unwrap();
        store.compact().unwrap();
        store
    }

    #[test]
    fn default_graph_query_sees_inferred_and_base_facts() {
        let store = build_store();
        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let reasoning = ReasoningDataset::wrap(dataset, &engine).unwrap();
        assert!(reasoning.diagnostics().is_empty());
        assert!(reasoning.inferred_len() >= 2);

        let pattern = QuadPattern::default_graph(
            PatternTerm::Bound(Term::NamedNode(nn("http://ex/fido"))),
            PatternTerm::Bound(Term::NamedNode(rdf_type())),
            PatternTerm::Variable,
        );
        let mut objects: Vec<String> = reasoning
            .find_quads(&pattern)
            .unwrap()
            .map(|m| m.unwrap().object.to_string())
            .collect();
        objects.sort();
        assert_eq!(
            objects,
            vec![
                "<http://ex/Animal>".to_string(),
                "<http://ex/Dog>".to_string(),
                "<http://ex/Mammal>".to_string(),
            ],
            "fido must appear as rdf:type Dog (base), Mammal + Animal (inferred)"
        );
    }

    #[test]
    fn named_graph_query_does_not_see_inferred_facts() {
        let store = build_store();
        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let reasoning = ReasoningDataset::wrap(dataset, &engine).unwrap();

        let pattern = QuadPattern::all_in(GraphSelector::Named(CoreGraphName::NamedNode(nn(
            "http://ex/some-other-graph",
        ))));
        let results: Vec<_> = reasoning
            .find_quads(&pattern)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn lftj_is_disabled_on_the_wrapped_view() {
        let store = build_store();
        let dataset = StoreDataset::new(Arc::new(store));
        assert!(
            dataset.supports_lftj(),
            "sanity: inner store is LFTJ-capable"
        );
        let engine = LftjFixpointEngine::new();
        let reasoning = ReasoningDataset::wrap(dataset, &engine).unwrap();
        assert!(!reasoning.supports_lftj());
    }
}
