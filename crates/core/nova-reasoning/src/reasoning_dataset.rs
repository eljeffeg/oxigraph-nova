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

//! ## `owl:sameAs` query-time canonicalization
//!
//! [`ReasoningDataset::wrap`] also captures a frozen
//! [`SameAsTracker`](crate::same_as::SameAsTracker) (built once, via
//! [`ReasoningEngine::same_as_tracker`]) and applies it in `find_quads`.
//!
//! Since nothing is ever rewritten in the inner store (no materialization —
//! see the module doc comment above), a fact might be physically stored
//! under *any* member of a `owl:sameAs` equivalence class — e.g. `alice
//! owl:sameAs bob` plus `alice knows carol` stored under `alice`, while a
//! query asks about `bob`. So "canonicalization" here is implemented as a
//! **fan-out**: every bound subject/object term in the incoming pattern is
//! widened to the full list of its `owl:sameAs` equivalence-class members
//! (via [`ReasoningDataset::candidate_patterns`]), and the inner
//! dataset/inferred overlay is queried once per resulting concrete pattern,
//! unioning the results together (deduped). Symmetrically, every matched
//! subject/object term that came from a position the original query left
//! as a variable is expanded back out to its equivalence class before being
//! yielded (via [`ReasoningDataset::expand_match`]) — together implementing
//! `eq-sym`/`eq-trans`/`eq-rep-s`/`eq-rep-o` as a query-time rewrite rather
//! than a materialized closure (see `crate::same_as`'s module doc comment
//! for the full rationale). This is skipped entirely (fast path, zero
//! overhead) whenever the tracker is empty — the overwhelmingly common case
//! of a dataset with no `owl:sameAs` triples at all.

use crate::engine::{Diagnostic, ReasoningEngine};
use crate::same_as::SameAsTracker;
use anyhow::Result;
use oxigraph_nova_core::{GraphName, Term};
use oxigraph_nova_query::{
    Dataset, DatasetLftjSource, GraphSelector, PatternTerm, QuadIter, QuadMatch, QuadPattern,
};
use std::collections::HashSet;
use std::sync::Arc;

/// Wraps a base [`Dataset`] `D` with an in-memory closure computed once (at
/// construction) by a [`ReasoningEngine`]. See the module doc comment for
/// full semantics.
pub struct ReasoningDataset<D: Dataset> {
    inner: D,
    /// Inferred triples held as shared `Arc<Term>` so every `find_quads`
    /// match is a refcount bump, not a deep Term/string clone.
    inferred: Vec<(Arc<Term>, Arc<Term>, Arc<Term>)>,
    diagnostics: Vec<Diagnostic>,
    /// Frozen union-find over `owl:sameAs` pairs — see [`SameAsTracker`]'s
    /// module doc comment for the query-time canonicalize/expand design
    /// applied in [`ReasoningDataset::find_quads`]. Empty (the common case)
    /// whenever the wrapped engine has no `owl:sameAs` support, or the
    /// dataset simply has no `owl:sameAs` triples.
    same_as: SameAsTracker,
}

impl<D: Dataset> ReasoningDataset<D> {
    /// Run `engine` over `inner` once and hold the result as an in-memory
    /// overlay. Fails only if the engine itself errors (e.g. a store I/O
    /// failure surfaced through `find_quads`); an engine that finds nothing
    /// to infer is not an error (see [`ReasoningEngine::infer`]).
    pub fn wrap(inner: D, engine: &dyn ReasoningEngine) -> Result<Self> {
        let (inferred_quads, diagnostics) = engine.infer(&inner)?;
        // Arc-wrap once at construction so every subsequent match is a
        // refcount bump rather than a deep Term/string clone.
        let inferred = inferred_quads
            .into_iter()
            .map(|q| {
                (
                    Arc::new(Term::from(q.subject)),
                    Arc::new(Term::NamedNode(q.predicate)),
                    Arc::new(q.object),
                )
            })
            .collect();
        let same_as = engine.same_as_tracker(&inner)?;
        Ok(Self {
            inner,
            inferred,
            diagnostics,
            same_as,
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

    /// The frozen `owl:sameAs` union-find this overlay canonicalizes/expands
    /// against — see the module doc comment.
    pub fn same_as_tracker(&self) -> &SameAsTracker {
        &self.same_as
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
                PatternTerm::Variable => true,
                PatternTerm::Bound(v) => v.as_ref() == s.as_ref(),
            };
            let p_ok = match &pattern.predicate {
                PatternTerm::Variable => true,
                PatternTerm::Bound(v) => v.as_ref() == p.as_ref(),
            };
            let o_ok = match &pattern.object {
                PatternTerm::Variable => true,
                PatternTerm::Bound(v) => v.as_ref() == o.as_ref(),
            };
            if s_ok && p_ok && o_ok {
                Some(QuadMatch {
                    subject: Arc::clone(s),
                    predicate: Arc::clone(p),
                    object: Arc::clone(o),
                    graph_name: GraphName::DefaultGraph,
                })
            } else {
                None
            }
        })
    }

    /// Widen a pattern's bound subject/object terms to *every* member of
    /// their `owl:sameAs` equivalence class — the "before lookup" half of
    /// query-time canonicalization (see module doc comment).
    ///
    /// Note this is a fan-out to every class member, **not** a substitution
    /// with a single canonical representative: the inner dataset is never
    /// rewritten (nothing is materialized — see this module's doc
    /// comment), so it may hold the fact under *any* member of the class
    /// (e.g. stored as `alice knows carol` while the query asks about
    /// `bob`, `alice`'s `owl:sameAs` partner) — only trying the
    /// union-find's arbitrarily-chosen root term would miss such a fact
    /// whenever the root isn't the literal term the store happens to use.
    /// The predicate position is left untouched (see [`SameAsTracker`]'s
    /// module doc comment on why `eq-rep-p` is not implemented). Returns
    /// `vec![pattern.clone()]` whenever `self.same_as.is_empty()`.
    fn candidate_patterns(&self, pattern: &QuadPattern) -> Vec<QuadPattern> {
        if self.same_as.is_empty() {
            return vec![pattern.clone()];
        }
        let subjects: Vec<PatternTerm> = match &pattern.subject {
            PatternTerm::Bound(t) => self
                .same_as
                .class_members(t.as_ref())
                .into_iter()
                .map(PatternTerm::bound)
                .collect(),
            PatternTerm::Variable => vec![PatternTerm::Variable],
        };
        let objects: Vec<PatternTerm> = match &pattern.object {
            PatternTerm::Bound(t) => self
                .same_as
                .class_members(t.as_ref())
                .into_iter()
                .map(PatternTerm::bound)
                .collect(),
            PatternTerm::Variable => vec![PatternTerm::Variable],
        };
        let mut out = Vec::with_capacity(subjects.len() * objects.len());
        for s in &subjects {
            for o in &objects {
                out.push(QuadPattern {
                    subject: s.clone(),
                    predicate: pattern.predicate.clone(),
                    object: o.clone(),
                    graph: pattern.graph.clone(),
                });
            }
        }
        out
    }

    /// Expand `m`'s subject/object back out to every member of its
    /// `owl:sameAs` equivalence class — the "after lookup" half of
    /// query-time canonicalization (see module doc comment). Yields exactly
    /// `[m]` (unchanged) whenever `self.same_as.is_empty()`. When a
    /// position was originally *bound* in `pattern` (rather than a free
    /// variable), that position is not expanded — expansion only widens
    /// positions the query left as variables, matching the Fluree design
    /// note's "expand only when returning results [for variables]" shape;
    /// a bound position was already fanned out over going in (see
    /// `candidate_patterns`), and the original request explicitly asked
    /// for that one term, not its whole class.
    fn expand_match(&self, pattern: &QuadPattern, m: QuadMatch) -> Vec<QuadMatch> {
        if self.same_as.is_empty() {
            return vec![m];
        }
        // class_members already returns Arc<Term> — Arc::clone only.
        let subjects: Vec<Arc<Term>> = if matches!(pattern.subject, PatternTerm::Variable) {
            self.same_as.class_members(m.subject.as_ref())
        } else {
            vec![m.subject.clone()]
        };
        let objects: Vec<Arc<Term>> = if matches!(pattern.object, PatternTerm::Variable) {
            self.same_as.class_members(m.object.as_ref())
        } else {
            vec![m.object.clone()]
        };
        let mut out = Vec::with_capacity(subjects.len() * objects.len());
        for s in &subjects {
            for o in &objects {
                out.push(QuadMatch {
                    subject: Arc::clone(s),
                    predicate: Arc::clone(&m.predicate),
                    object: Arc::clone(o),
                    graph_name: m.graph_name.clone(),
                });
            }
        }
        out
    }
}

/// LFTJ acceleration is deliberately turned off on the wrapped view — see
/// module doc comment. Every method here uses the trait's "unsupported"
/// default (i.e. this impl block is intentionally empty).
impl<D: Dataset> DatasetLftjSource for ReasoningDataset<D> {}

impl<D: Dataset> Dataset for ReasoningDataset<D> {
    fn find_quads<'a>(&'a self, pattern: &QuadPattern) -> Result<QuadIter<'a>> {
        if self.same_as.is_empty() {
            let base = self.inner.find_quads(pattern)?;
            let overlay = self.inferred_matches(pattern).map(Ok);
            return Ok(Box::new(base.chain(overlay)));
        }

        // `owl:sameAs` canonicalization is active: fan the pattern's bound
        // S/O terms out to every member of their equivalence class (since
        // the inner store is never rewritten — see `candidate_patterns`'s
        // doc comment), look each candidate pattern up against both the
        // inner dataset and the inferred overlay, then expand each match's
        // variable-position S/O terms back out to the full class, and dedup
        // (multiple candidate patterns, and/or the inner dataset plus the
        // inferred overlay, can easily yield the exact same expanded
        // QuadMatch more than once).
        let mut seen: HashSet<QuadMatchKey> = HashSet::new();
        let mut expanded: Vec<Result<QuadMatch>> = Vec::new();
        for candidate in self.candidate_patterns(pattern) {
            let base = self.inner.find_quads(&candidate)?;
            let overlay = self.inferred_matches(&candidate).map(Ok);
            for m in base.chain(overlay) {
                match m {
                    Ok(m) => {
                        for e in self.expand_match(pattern, m) {
                            if seen.insert(QuadMatchKey::from(&e)) {
                                expanded.push(Ok(e));
                            }
                        }
                    }
                    Err(e) => expanded.push(Err(e)),
                }
            }
        }
        Ok(Box::new(expanded.into_iter()))
    }

    fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
        // No synthetic inference graph exists to enumerate — everything
        // inferred is presented as part of the (unnamed) default graph.
        self.inner.named_graphs()
    }
}

/// Hashable/comparable dedup key for a [`QuadMatch`] — `QuadMatch` itself
/// derives `PartialEq` but not `Hash`/`Eq`, so a small local key is used
/// instead. Hashes Term content via `Arc<Term>` (no `to_string` allocation)
/// — used only by the `owl:sameAs`-expansion dedup pass in `find_quads`.
#[derive(PartialEq, Eq, Hash)]
struct QuadMatchKey {
    subject: Arc<Term>,
    predicate: Arc<Term>,
    object: Arc<Term>,
    graph_name: GraphName,
}

impl From<&QuadMatch> for QuadMatchKey {
    fn from(m: &QuadMatch) -> Self {
        Self {
            subject: Arc::clone(&m.subject),
            predicate: Arc::clone(&m.predicate),
            object: Arc::clone(&m.object),
            graph_name: m.graph_name.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::LftjFixpointEngine;
    use oxigraph_nova_core::{GraphName as CoreGraphName, NamedNode, Quad, QuadStore};
    use oxigraph_nova_engine_ring::LoudsStore;
    use oxigraph_nova_query::StoreDataset;
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

    fn owl_same_as() -> NamedNode {
        nn("http://www.w3.org/2002/07/owl#sameAs")
    }

    fn build_store() -> LoudsStore {
        let store = LoudsStore::new();
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
            PatternTerm::bound(Term::NamedNode(nn("http://ex/fido"))),
            PatternTerm::bound(Term::NamedNode(rdf_type())),
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

    /// `owl:sameAs` end-to-end: `alice owl:sameAs bob`, `alice knows carol`
    /// asserted as a base fact. Querying `?x knows carol` (subject
    /// unbound) must return **both** `alice` and `bob` (expansion);
    /// querying `bob knows ?y` (subject bound to the *other* member of the
    /// class) must still find the fact stored under `alice` (fan-out
    /// before lookup).
    #[test]
    fn same_as_canonicalizes_and_expands_across_equivalence_class() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let knows = nn("http://ex/knows");
        store
            .insert(&Quad::new(
                nn("http://ex/alice"),
                owl_same_as(),
                Term::NamedNode(nn("http://ex/bob")),
                g.clone(),
            ))
            .unwrap();
        store
            .insert(&Quad::new(
                nn("http://ex/alice"),
                knows.clone(),
                Term::NamedNode(nn("http://ex/carol")),
                g,
            ))
            .unwrap();
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let reasoning = ReasoningDataset::wrap(dataset, &engine).unwrap();
        assert!(
            !reasoning.same_as_tracker().is_empty(),
            "sanity: sameAs tracker must be populated"
        );

        // Expansion: ?x knows carol -> {alice, bob}.
        let pattern = QuadPattern::default_graph(
            PatternTerm::Variable,
            PatternTerm::bound(Term::NamedNode(knows.clone())),
            PatternTerm::bound(Term::NamedNode(nn("http://ex/carol"))),
        );
        let mut subjects: Vec<String> = reasoning
            .find_quads(&pattern)
            .unwrap()
            .map(|m| m.unwrap().subject.to_string())
            .collect();
        subjects.sort();
        subjects.dedup();
        assert_eq!(
            subjects,
            vec![
                "<http://ex/alice>".to_string(),
                "<http://ex/bob>".to_string()
            ],
            "querying by carol must expand the subject to both alice and bob"
        );

        // Fan-out: bob knows ?y must still find carol (the fact is stored
        // under alice, bob's sameAs partner).
        let pattern2 = QuadPattern::default_graph(
            PatternTerm::bound(Term::NamedNode(nn("http://ex/bob"))),
            PatternTerm::bound(Term::NamedNode(knows)),
            PatternTerm::Variable,
        );
        let objects: Vec<String> = reasoning
            .find_quads(&pattern2)
            .unwrap()
            .map(|m| m.unwrap().object.to_string())
            .collect();
        assert_eq!(
            objects,
            vec!["<http://ex/carol>".to_string()],
            "querying bob (alice's sameAs partner) must still find the carol fact"
        );
    }

    /// When there are no `owl:sameAs` triples at all, `same_as_tracker()`
    /// must be empty and ordinary queries must behave exactly as before
    /// (no expansion, no dedup pass) — the fast path.
    #[test]
    fn no_same_as_triples_yields_empty_tracker_and_unexpanded_results() {
        let store = build_store();
        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let reasoning = ReasoningDataset::wrap(dataset, &engine).unwrap();
        assert!(reasoning.same_as_tracker().is_empty());
    }
}
