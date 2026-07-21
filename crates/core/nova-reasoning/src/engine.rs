//! [`ReasoningEngine`] — the pluggable "compute the inferred-only closure of
//! a dataset" seam.
//!
//! Per the project's explicit design direction, the **default** path is an
//! in-memory fixpoint overlay, not persisted materialization —
//! [`ReasoningDataset`](crate::ReasoningDataset)
//! wraps a [`Dataset`] and keeps the engine's output as a plain in-memory
//! `Vec<Quad>`, never writing it back into the store. This trait exists so
//! an alternative engine — e.g. a vendored `oxreason` fork, or an adapter
//! over the external `reasonable` crate — can be swapped in later to
//! *materialize* inferred facts into an ordinary named graph instead,
//! without `ReasoningDataset` itself needing to change: it only ever
//! depends on this trait's `infer` method returning inferred quads, never
//! on *how* they were computed or where (if anywhere) they end up
//! persisted. There is no reserved `GraphId` for such a graph — see
//! `oxigraph_nova_core::dict`'s module doc comment — any named graph the
//! deployment chooses works identically.

//! [`LftjFixpointEngine`] is the default, always-available implementation:
//! it drives [`crate::fixpoint::closure_over_store`] directly over a
//! [`Dataset`]'s LFTJ capability methods (`lftj_intern_term`/
//! `lftj_join_scan`/`lftj_decode_term`). It only produces useful results
//! over an LFTJ-capable dataset (i.e. [`oxigraph_nova_query::StoreDataset`]
//! wrapping an `oxigraph_nova_engine_ring::LoudsStore`) — a plain
//! [`oxigraph_nova_query::InMemoryDataset`] has no LFTJ support and yields
//! an empty closure, by design (this engine's whole reason to exist is to
//! reuse the WCOJ machinery, not to duplicate a from-scratch triple-scan
//! reasoner).
//!
//! ## Rule coverage
//!
//! [`LftjFixpointEngine`] currently covers:
//!   - `rdfs:subClassOf` / `rdfs:subPropertyOf` transitivity
//!     (`scm-sco`/`scm-spo`, via [`crate::rule::Rule::transitive`]).
//!   - `rdf:type`/`subClassOf` propagation (`cax-sco`, via
//!     [`crate::rule::Rule::type_propagation`]).
//!   - Property hierarchy propagation (`prp-spo1`, via
//!     [`crate::rule::Rule::subproperty_propagation`]).
//!   - Property domain/range propagation (`prp-dom`/`prp-rng`, via
//!     [`crate::rule::Rule::domain`]/[`crate::rule::Rule::range`]).
//!   - Generic `owl:TransitiveProperty`/`owl:SymmetricProperty` (`prp-trp`/
//!     `prp-symp`, via [`crate::rule::Rule::transitive_property`]/
//!     [`crate::rule::Rule::symmetric_property`]) — a single generic rule
//!     per family (predicate itself a rule variable, following the same
//!     `WILDCARD`-predicate shape used in the reference `ReasonerRules.h`
//!     rule table this engine's coverage was checked against), with the set
//!     of properties it applies to discovered dynamically per `infer()`
//!     call and seeded from their own edges (see `infer`'s discovery loop).
//!   - `owl:equivalentClass`/`owl:equivalentProperty` (`cax-eqc`/`prp-eqp`,
//!     via [`crate::rule::Rule::equivalent_class_forward`]/`_backward` and
//!     [`crate::rule::Rule::equivalent_property_forward`]/`_backward`) —
//!     each expands to a mutual `subClassOf`/`subPropertyOf` pair.
//!   - `owl:inverseOf` (`prp-inv1`/`prp-inv2`, via
//!     [`crate::rule::Rule::inverse_forward`]/[`crate::rule::Rule::inverse_backward`]).
//!
//!   - `owl:sameAs` — via [`ReasoningEngine::same_as_tracker`]/
//!     [`crate::same_as::SameAsTracker`], **not** a materialized `eq-rep-*`
//!     closure: [`LftjFixpointEngine::same_as_tracker`] scans every
//!     `owl:sameAs` pair and builds a frozen union-find, which
//!     [`ReasoningDataset`](crate::ReasoningDataset) uses to canonicalize
//!     query terms and expand results back out at query time — see
//!     [`crate::same_as`]'s module doc comment for the full rationale.
//!   - OWL 2 RL consistency-checking rules `prp-asyp`/`prp-irp`/`cax-dw`
//!     (asymmetric/irreflexive property violations, disjoint-class
//!     violations) plus `eq-diff` (an `owl:sameAs` pair also asserted
//!     `owl:differentFrom`) — reported as [`Diagnostic::violation`]s via
//!     [`check_consistency`] and `infer`'s `eq-diff` check, never as derived
//!     triples (these are existential, one-shot checks, not recursive
//!     derivations, so they run directly over the closure/dataset rather
//!     than through [`crate::rule::RuleSet`]/[`crate::fixpoint`]).
//!
//! Arbitrary datatype-value clashes (e.g. two literals asserted `owl:sameAs`
//! whose XSD values are provably distinct) are not covered — this would
//! require a general XSD value-space comparison, which is out of scope for
//! this increment; the `eq-diff` check above covers the far more common
//! explicit-`owl:differentFrom`-vs-`owl:sameAs` clash.

//! ## Head-only constants and synthetic TermIds
//!
//! A rule's head can reference a predicate/class that is a compile-time
//! constant (e.g. `rdf:type` for `prp-dom`/`prp-rng`) but that never
//! actually appears as a *base fact* term anywhere in the dataset — e.g. a
//! dataset asserting `hasParent rdfs:domain Person` and `alice hasParent
//! bob`, with zero `rdf:type` triples anywhere. [`Dataset::lftj_intern_term`]
//! is a **read-only** dictionary lookup (never inserts), so such a
//! predicate cannot be interned to a real `TermId` — but the inference
//! ("alice rdf:type Person") is still correct and expected.
//!
//! [`TermResolver`] is the fix: every well-known predicate/class this engine
//! references is resolved once per `infer()` call via
//! [`TermResolver::resolve`], which falls back to minting a **synthetic**
//! id (from a reserved range far above any real `TermId`, see
//! [`SYNTHETIC_ID_BASE`]) when the real dictionary lookup fails. Every rule
//! that mentions that predicate/class — in its body *or* head — uses the
//! exact same resolved id throughout one `infer()` call, so derived facts
//! and body-atom matches stay internally consistent even though the id
//! doesn't correspond to any dictionary entry: a synthetic id can only ever
//! match rows that this same `infer()` call itself derives (or seeds) using
//! that id, never a real store row (which by construction never contains
//! an id from the reserved synthetic range).

use crate::join::{Atom, AtomField, AtomSource, SliceSource, UnionTrieIter, leapfrog_join};
use crate::rule::{Rule, RuleSet};
use crate::same_as::SameAsTracker;
use anyhow::Result;
use oxigraph_nova_core::{
    EmptyTrieIter, GraphName, NamedNode, NamedOrBlankNode, Quad, Term, TrieIterator,
};
use oxigraph_nova_query::{Dataset, GraphSelector, PatternTerm, QuadPattern};
use std::collections::HashMap;
use std::sync::Arc;

/// Distinguishes a merely-informational [`Diagnostic`] (e.g. "skipped a
/// malformed declaration triple") from an actual OWL 2 RL consistency
/// violation (e.g. `prp-asyp`/`prp-irp`/`cax-dw` firing) — see
/// [`Diagnostic::violation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Purely informational — never indicates the dataset is inconsistent.
    Info,
    /// An OWL 2 RL consistency-checking rule matched: the dataset is
    /// inconsistent per this diagnostic's rule.
    Violation,
}

/// One diagnostic emitted by a [`ReasoningEngine`] — e.g. a skipped
/// declaration triple, a derived row that failed to decode back into a
/// `Quad`, or (see [`Diagnostic::violation`]) an OWL 2 RL consistency-rule
/// match.
///
/// Materialization never *aborts* on a diagnostic — even a
/// [`Severity::Violation`] is purely reported, never treated as a hard
/// error — surfaced to callers via
/// [`ReasoningDataset::diagnostics`](crate::ReasoningDataset::diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub rule: String,
    pub message: String,
    pub severity: Severity,
}

impl Diagnostic {
    /// An informational diagnostic (the common case prior to consistency
    /// checking) — see [`Severity::Info`].
    pub fn new(rule: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            rule: rule.into(),
            message: message.into(),
            severity: Severity::Info,
        }
    }

    /// An OWL 2 RL consistency-rule violation — see [`Severity::Violation`].
    pub fn violation(rule: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            rule: rule.into(),
            message: message.into(),
            severity: Severity::Violation,
        }
    }

    /// `true` if this diagnostic reports an actual inconsistency
    /// ([`Severity::Violation`]) rather than an informational note.
    pub fn is_violation(&self) -> bool {
        self.severity == Severity::Violation
    }
}

/// Computes the inferred-only closure of a [`Dataset`]: every quad entailed
/// by the engine's rules but not already present as a base fact.
///
/// This is the hook point requested for pluggable reasoning backends (e.g.
/// an adapter over the external `reasonable` crate that performs full
/// materialization) — [`ReasoningDataset`](crate::ReasoningDataset) only
/// ever calls `infer` once (at construction) and holds the result as an
/// in-memory overlay; it has no opinion on how an implementor computes its
/// answer.
pub trait ReasoningEngine: Send + Sync {
    /// Compute every quad entailed by `dataset` that is not already one of
    /// its base facts, plus any diagnostics produced along the way.
    fn infer(&self, dataset: &dyn Dataset) -> Result<(Vec<Quad>, Vec<Diagnostic>)>;

    /// Build a frozen union-find over `dataset`'s `owl:sameAs` pairs — see
    /// [`crate::same_as`]'s module doc comment for the query-time
    /// canonicalization design this supports.
    ///
    /// Default: no `owl:sameAs` support at all (an empty tracker, under
    /// which every term canonicalizes to itself) — a `ReasoningEngine`
    /// implementor that has nothing to say about `owl:sameAs` need not
    /// override this.
    fn same_as_tracker(&self, dataset: &dyn Dataset) -> Result<SameAsTracker> {
        let _ = dataset;
        Ok(SameAsTracker::empty())
    }
}

/// An [`AtomSource`] adapter over a [`Dataset`]'s LFTJ capability methods,
/// scoped to a fixed list of concrete [`GraphSelector`]s (one per graph to
/// reason over) — the `Dataset`-level analog of
/// [`crate::store_source::StoreAtomSource`] (which wraps
/// `oxigraph_nova_core::LftjSource` directly, keyed on a raw `u8` graph id).
///
/// `Dataset::lftj_join_scan` only supports `GraphSelector::Default`/`Named`
/// (see its doc comment: `AnyNamed`/`Union` return `None`, since
/// `StoreDataset` cannot resolve those down to one `u8` graph id) — so this
/// cannot simply store a single `GraphSelector::Union` and delegate
/// directly. Instead it holds one concrete selector per graph this engine
/// reasons over (the default graph plus every named graph enumerated up
/// front) and unions their individual scans together via
/// [`UnionTrieIter`], mirroring how `nova-query`'s own BGP evaluator
/// handles `GraphSelector::Union` internally (`eval_bgp_lftj_multi_graph`).
struct DatasetAtomSource<'a> {
    dataset: &'a dyn Dataset,
    graphs: Vec<GraphSelector>,
}

impl AtomSource for DatasetAtomSource<'_> {
    fn scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator> {
        let mut acc: Box<dyn TrieIterator> = Box::new(EmptyTrieIter);
        for g in &self.graphs {
            if let Some(scan) = self.dataset.lftj_join_scan(s, p, o, target_field, g) {
                acc = UnionTrieIter::new(acc, scan);
            }
        }
        acc
    }
}

fn rdf_type() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
}

fn rdfs_sub_class_of() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#subClassOf")
}

fn rdfs_sub_property_of() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#subPropertyOf")
}

fn rdfs_domain() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#domain")
}

fn rdfs_range() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#range")
}

fn owl_transitive_property() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#TransitiveProperty")
}

fn owl_symmetric_property() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#SymmetricProperty")
}

fn owl_equivalent_class() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#equivalentClass")
}

fn owl_equivalent_property() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#equivalentProperty")
}

fn owl_inverse_of() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#inverseOf")
}

fn owl_same_as() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#sameAs")
}

fn owl_different_from() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#differentFrom")
}

fn owl_asymmetric_property() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#AsymmetricProperty")
}

fn owl_irreflexive_property() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#IrreflexiveProperty")
}

fn owl_disjoint_with() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#disjointWith")
}

/// Lower bound of the synthetic-TermId range — see this module's doc
/// comment ("Head-only constants and synthetic TermIds"). Chosen far above
/// [`oxigraph_nova_core::MAX_TERM_ID`] (≈2^40) so it can never collide with
/// a real dictionary-assigned id.
const SYNTHETIC_ID_BASE: u64 = 1u64 << 63;

/// Resolves well-known predicate/class [`Term`]s to `u64` ids for one
/// [`LftjFixpointEngine::infer`] call, falling back to a synthetic id (see
/// [`SYNTHETIC_ID_BASE`]) when the real dictionary has no entry for a term.
/// Also the inverse: decodes a row's ids back to [`Term`]s, checking the
/// synthetic table first.
///
/// Callers must resolve each well-known constant **exactly once** per
/// `infer()` call and reuse the returned id everywhere that constant is
/// needed (rule bodies, rule heads, seeding) — this is what keeps a
/// synthetic id internally consistent across every rule that references it
/// (see the module doc comment).
struct TermResolver<'a> {
    dataset: &'a dyn Dataset,
    graph: GraphSelector,
    synthetic: HashMap<u64, Term>,
    next_synthetic: u64,
}

impl<'a> TermResolver<'a> {
    fn new(dataset: &'a dyn Dataset, graph: GraphSelector) -> Self {
        Self {
            dataset,
            graph,
            synthetic: HashMap::new(),
            next_synthetic: SYNTHETIC_ID_BASE,
        }
    }

    /// Resolve `term` to an id: a real dictionary id if one exists, else a
    /// freshly-minted synthetic one. Always succeeds.
    fn resolve(&mut self, term: &Term) -> u64 {
        if let Some(id) = self.dataset.lftj_intern_term(term, &self.graph) {
            return id;
        }
        let id = self.next_synthetic;
        self.next_synthetic += 1;
        self.synthetic.insert(id, term.clone());
        id
    }

    /// Decode `id` back to a [`Term`], checking the synthetic table before
    /// falling back to the dataset's real dictionary.
    fn decode(&self, id: u64) -> Option<Term> {
        if let Some(t) = self.synthetic.get(&id) {
            return Some(t.clone());
        }
        self.dataset
            .lftj_decode_term(id)
            .map(std::sync::Arc::unwrap_or_clone)
    }
}

/// Decode a `[u64; 3]` TermId row back into a base-graph [`Quad`], returning
/// `None` if any id fails to decode or the subject/predicate position holds
/// a term shape that cannot appear there (e.g. a literal subject) — such a
/// row should never actually arise for the rules this engine runs, but this
/// guards against a corrupt/foreign TermId rather than panicking.
fn decode_quad(resolver: &TermResolver, row: [u64; 3]) -> Option<Quad> {
    let s = resolver.decode(row[0])?;
    let p = resolver.decode(row[1])?;
    let o = resolver.decode(row[2])?;
    let subject = match s {
        Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n),
        Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b),
        _ => return None,
    };
    let predicate = match p {
        Term::NamedNode(n) => n,
        _ => return None,
    };
    Some(Quad::new(subject, predicate, o, GraphName::DefaultGraph))
}

/// The default, always-available [`ReasoningEngine`]: drives
/// [`crate::fixpoint::closure_over_store`] directly over a [`Dataset`]'s
/// LFTJ join-scan capability. See this module's doc comment for rule
/// coverage and the LFTJ-capability requirement.
#[derive(Debug, Default, Clone, Copy)]
pub struct LftjFixpointEngine;

impl LftjFixpointEngine {
    pub fn new() -> Self {
        Self
    }
}

/// Collect every `owl:sameAs` pair from `dataset` (RDF-merge of every
/// graph, via `GraphSelector::Union`) and build a [`SameAsTracker`] from
/// them. Shared helper for [`ReasoningEngine::same_as_tracker`]'s
/// `LftjFixpointEngine` override — factored out so it can be called
/// regardless of `supports_lftj()` (unlike `infer`, `owl:sameAs` tracking
/// needs nothing beyond ordinary [`Dataset::find_quads`] — see
/// `crate::same_as`'s module doc comment).
fn build_same_as_tracker(dataset: &dyn Dataset) -> Result<SameAsTracker> {
    let pattern = QuadPattern {
        subject: PatternTerm::Variable,
        predicate: PatternTerm::bound(Term::NamedNode(owl_same_as())),
        object: PatternTerm::Variable,
        graph: GraphSelector::Union,
    };
    let mut pairs = Vec::new();
    for m in dataset.find_quads(&pattern)? {
        let m = m?;
        pairs.push((
            Arc::unwrap_or_clone(m.subject),
            Arc::unwrap_or_clone(m.object),
        ));
    }
    Ok(SameAsTracker::build(pairs))
}

/// Run the OWL 2 RL consistency-checking rules this engine covers —
/// `prp-asyp`, `prp-irp`, `cax-dw` — against `dataset`'s base facts plus
/// `closure` (this `infer()` call's already-computed inferred rows,
/// TermId-keyed), returning one [`Diagnostic::violation`] per match.
///
/// These are existential, one-shot checks (find *any* witness), not
/// recursive derivations, so they run directly via [`leapfrog_join`] over
/// [`Atom`]s pointed at a [`SliceSource`] wrapping `closure` — no
/// `RuleSet`/[`crate::fixpoint`] involved, matching this module's doc
/// comment. `resolver` must already have every predicate/class this
/// function references resolved (see `same_as_tracker`'s and `infer`'s
/// callers) — `check_consistency` itself only ever calls
/// `resolver.decode`, never `resolver.resolve`, so it cannot mint new
/// synthetic ids partway through.
fn check_consistency(
    resolver: &TermResolver,
    closure_rows: &[[u64; 3]],
    asymmetric_property_id: u64,
    irreflexive_property_id: u64,
    disjoint_with_id: u64,
    rdf_type_id: u64,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let src = SliceSource::new(closure_rows);

    // `prp-asyp`: ?p rdf:type owl:AsymmetricProperty ∧ ?x ?p ?y ∧ ?y ?p ?x.
    //
    // The join naturally matches both (x=a, y=b) and its swap (x=b, y=a)
    // for the same underlying violation (the two body atoms are
    // themselves symmetric in x/y) — dedup on the unordered pair via a
    // `seen` set (min/max of the two TermIds) so each violating pair is
    // reported exactly once.
    {
        let atoms = vec![
            Atom {
                s: AtomField::Var(0), // ?p
                p: AtomField::Const(rdf_type_id),
                o: AtomField::Const(asymmetric_property_id),
                source: &src,
            },
            Atom {
                s: AtomField::Var(1), // ?x
                p: AtomField::Var(0),
                o: AtomField::Var(2), // ?y
                source: &src,
            },
            Atom {
                s: AtomField::Var(2),
                p: AtomField::Var(0),
                o: AtomField::Var(1),
                source: &src,
            },
        ];
        let mut seen: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
        for binding in leapfrog_join(&atoms, 3) {
            let key = (binding[1].min(binding[2]), binding[1].max(binding[2]));
            if !seen.insert(key) {
                continue;
            }
            let p = resolver.decode(binding[0]);
            let x = resolver.decode(binding[1]);
            let y = resolver.decode(binding[2]);
            diagnostics.push(Diagnostic::violation(
                "prp-asyp",
                format!(
                    "asymmetric property violation: {p:?} relates {x:?} to {y:?} and {y:?} to {x:?}"
                ),
            ));
        }
    }

    // `prp-irp`: ?p rdf:type owl:IrreflexiveProperty ∧ ?x ?p ?x.
    //
    // The join engine's variable-binding order resolves an atom's fields
    // via the *first* matching position only (see `Atom::resolve_for_var`)
    // — so a single atom with the same variable in both the `s` and `o`
    // position (e.g. `Var(1) ?p Var(1)`) does not actually constrain those
    // two occurrences to be equal while the scan for that variable is
    // still in flight (the second occurrence is treated as still
    // unbound). To get a correctly-enforced self-loop check, use two
    // distinct variables for the edge atom's endpoints and filter for
    // equality on the materialized bindings instead.
    {
        let atoms = vec![
            Atom {
                s: AtomField::Var(0), // ?p
                p: AtomField::Const(rdf_type_id),
                o: AtomField::Const(irreflexive_property_id),
                source: &src,
            },
            Atom {
                s: AtomField::Var(1), // ?x
                p: AtomField::Var(0),
                o: AtomField::Var(2), // ?y
                source: &src,
            },
        ];
        for binding in leapfrog_join(&atoms, 3) {
            if binding[1] != binding[2] {
                continue;
            }
            let p = resolver.decode(binding[0]);
            let x = resolver.decode(binding[1]);
            diagnostics.push(Diagnostic::violation(
                "prp-irp",
                format!("irreflexive property violation: {p:?} relates {x:?} to itself"),
            ));
        }
    }

    // `cax-dw`: ?c1 owl:disjointWith ?c2 ∧ ?x rdf:type ?c1 ∧ ?x rdf:type ?c2.
    {
        let atoms = vec![
            Atom {
                s: AtomField::Var(0), // ?c1
                p: AtomField::Const(disjoint_with_id),
                o: AtomField::Var(1), // ?c2
                source: &src,
            },
            Atom {
                s: AtomField::Var(2), // ?x
                p: AtomField::Const(rdf_type_id),
                o: AtomField::Var(0),
                source: &src,
            },
            Atom {
                s: AtomField::Var(2),
                p: AtomField::Const(rdf_type_id),
                o: AtomField::Var(1),
                source: &src,
            },
        ];
        for binding in leapfrog_join(&atoms, 3) {
            let c1 = resolver.decode(binding[0]);
            let c2 = resolver.decode(binding[1]);
            let x = resolver.decode(binding[2]);
            diagnostics.push(Diagnostic::violation(
                "cax-dw",
                format!(
                    "disjoint-class violation: {x:?} is rdf:type both {c1:?} and {c2:?}, \
                     which are declared owl:disjointWith each other"
                ),
            ));
        }
    }

    diagnostics
}

impl ReasoningEngine for LftjFixpointEngine {
    fn same_as_tracker(&self, dataset: &dyn Dataset) -> Result<SameAsTracker> {
        build_same_as_tracker(dataset)
    }

    fn infer(&self, dataset: &dyn Dataset) -> Result<(Vec<Quad>, Vec<Diagnostic>)> {
        // Reason over the RDF merge of every graph in the dataset (no
        // dedicated TBox/ontology graph is reserved; see
        // `oxigraph_nova_core::dict`'s module doc comment). Treating the
        // whole dataset as one reasoning scope is a reasonable default for
        // a first increment, and every inferred quad is presented back as
        // a default-graph quad regardless of which graph fed it (matching
        // the Jena/GraphDB "inference is part of the default graph view"
        // convention documented for `ReasoningDataset`).
        //
        // `find_quads`/`lftj_intern_term` accept `GraphSelector::Union`
        // directly (nova-query's nested-loop/BGP-pattern paths both

        // understand it), but `Dataset::lftj_join_scan` does not (see
        // `DatasetAtomSource`'s doc comment) — so the concrete graph list
        // below is built once and threaded through `DatasetAtomSource`
        // separately from the `Union` selector used for interning/seeding.
        if !dataset.supports_lftj() {
            // Not an LFTJ-capable dataset (e.g. a plain InMemoryDataset) —
            // this engine has nothing else to offer; see the module doc
            // comment for why it doesn't fall back to a from-scratch scan.
            return Ok((Vec::new(), Vec::new()));
        }

        let graph = GraphSelector::Union;
        let mut graphs: Vec<GraphSelector> = vec![GraphSelector::Default];
        for g in dataset.named_graphs()? {
            graphs.push(GraphSelector::Named(g?));
        }

        let mut diagnostics: Vec<Diagnostic> = Vec::new();
        let mut resolver = TermResolver::new(dataset, graph.clone());

        // Every well-known predicate/class this engine's rules reference,
        // resolved exactly once (see `TermResolver`'s doc comment for why
        // that matters). `resolve` always succeeds — it mints a synthetic
        // id when the real dictionary has no entry — so every rule below
        // is always constructed; a rule whose predicates are all synthetic
        // simply never matches any real store row (see the module doc
        // comment).
        let rdf_type_id = resolver.resolve(&Term::NamedNode(rdf_type()));
        let sub_class_of_id = resolver.resolve(&Term::NamedNode(rdfs_sub_class_of()));
        let sub_property_of_id = resolver.resolve(&Term::NamedNode(rdfs_sub_property_of()));
        let domain_id = resolver.resolve(&Term::NamedNode(rdfs_domain()));
        let range_id = resolver.resolve(&Term::NamedNode(rdfs_range()));
        let transitive_property_id = resolver.resolve(&Term::NamedNode(owl_transitive_property()));
        let symmetric_property_id = resolver.resolve(&Term::NamedNode(owl_symmetric_property()));
        let equivalent_class_id = resolver.resolve(&Term::NamedNode(owl_equivalent_class()));
        let equivalent_property_id = resolver.resolve(&Term::NamedNode(owl_equivalent_property()));
        let inverse_of_id = resolver.resolve(&Term::NamedNode(owl_inverse_of()));
        let asymmetric_property_id = resolver.resolve(&Term::NamedNode(owl_asymmetric_property()));
        let irreflexive_property_id =
            resolver.resolve(&Term::NamedNode(owl_irreflexive_property()));
        let disjoint_with_id = resolver.resolve(&Term::NamedNode(owl_disjoint_with()));

        let rules = vec![
            Rule::transitive(sub_class_of_id),
            Rule::transitive(sub_property_of_id),
            Rule::type_propagation(rdf_type_id, sub_class_of_id),
            Rule::subproperty_propagation(sub_property_of_id),
            Rule::domain(domain_id, rdf_type_id),
            Rule::range(range_id, rdf_type_id),
            Rule::transitive_property(rdf_type_id, transitive_property_id),
            Rule::symmetric_property(rdf_type_id, symmetric_property_id),
            Rule::equivalent_class_forward(equivalent_class_id, sub_class_of_id),
            Rule::equivalent_class_backward(equivalent_class_id, sub_class_of_id),
            Rule::equivalent_property_forward(equivalent_property_id, sub_property_of_id),
            Rule::equivalent_property_backward(equivalent_property_id, sub_property_of_id),
            Rule::inverse_forward(inverse_of_id),
            Rule::inverse_backward(inverse_of_id),
        ];
        let rule_set = RuleSet::new(rules);

        // Seed the fixpoint from every quad using one of the "core" fixed
        // predicates — sufficient to bootstrap semi-naive evaluation's
        // first Delta for every rule above except `prp-trp`/`prp-symp`
        // (see `closure_over_store`'s doc comment for why a rule-relevant
        // seed, not a full store scan, is enough).
        let mut seed: Vec<[u64; 3]> = Vec::new();
        for pred in [
            rdf_type(),
            rdfs_sub_class_of(),
            rdfs_sub_property_of(),
            rdfs_domain(),
            rdfs_range(),
            owl_equivalent_class(),
            owl_equivalent_property(),
            owl_inverse_of(),
            owl_disjoint_with(),
        ] {
            let pattern = QuadPattern {
                subject: PatternTerm::Variable,
                predicate: PatternTerm::bound(Term::NamedNode(pred)),
                object: PatternTerm::Variable,
                graph: graph.clone(),
            };
            for m in dataset.find_quads(&pattern)? {
                let m = m?;
                let s = resolver.resolve(&m.subject);
                let p = resolver.resolve(&m.predicate);
                let o = resolver.resolve(&m.object);
                seed.push([s, p, o]);
            }
        }

        // `prp-trp`/`prp-symp`: every property declared `?p rdf:type
        // owl:TransitiveProperty`/`owl:SymmetricProperty` is data-dependent
        // — discover each declared property (the declaration triple itself
        // is already covered by the `rdf_type()` seeding loop above, since
        // it's just another `rdf:type` triple) and seed every edge using
        // that property as its predicate, which is what
        // `Rule::transitive_property`/`Rule::symmetric_property` actually
        // need in `Delta` to bootstrap their own closure.
        //
        // `owl:AsymmetricProperty`/`owl:IrreflexiveProperty` are included
        // here too, purely for seeding purposes: unlike `prp-trp`/
        // `prp-symp`, neither is an inference rule — they're consistency
        // checks handled entirely by `check_consistency` below — but that
        // function only ever reads from `closure_rows`
        // (`seed` ∪ rule-derived rows), so the edges of a property declared
        // with either class must still land in `seed` or `check_consistency`
        // has no rows to match against. No `Rule` is added to `rules` for
        // either class.
        for (decl_class, rule_name) in [
            (owl_transitive_property(), "prp-trp"),
            (owl_symmetric_property(), "prp-symp"),
            (owl_asymmetric_property(), "prp-asyp"),
            (owl_irreflexive_property(), "prp-irp"),
        ] {
            let pattern = QuadPattern {
                subject: PatternTerm::Variable,
                predicate: PatternTerm::bound(Term::NamedNode(rdf_type())),
                object: PatternTerm::bound(Term::NamedNode(decl_class.clone())),
                graph: graph.clone(),
            };
            for m in dataset.find_quads(&pattern)? {
                let m = m?;
                let Term::NamedNode(p_node) = m.subject.as_ref() else {
                    diagnostics.push(Diagnostic::new(
                        rule_name,
                        format!(
                            "{} declaration has a non-IRI subject ({:?}); skipped",
                            decl_class.as_str(),
                            m.subject
                        ),
                    ));
                    continue;
                };
                let p_id = resolver.resolve(m.subject.as_ref());
                let edge_pattern = QuadPattern {
                    subject: PatternTerm::Variable,
                    predicate: PatternTerm::bound(Term::NamedNode(p_node.clone())),
                    object: PatternTerm::Variable,
                    graph: graph.clone(),
                };
                for e in dataset.find_quads(&edge_pattern)? {
                    let e = e?;
                    let s = resolver.resolve(&e.subject);
                    let o = resolver.resolve(&e.object);
                    seed.push([s, p_id, o]);
                }
            }
        }

        let source = DatasetAtomSource { dataset, graphs };
        let closure = crate::fixpoint::closure_over_store(&rule_set, &source, &seed);

        diagnostics.extend(check_consistency(
            &resolver,
            &closure,
            asymmetric_property_id,
            irreflexive_property_id,
            disjoint_with_id,
            rdf_type_id,
        ));

        let same_as = build_same_as_tracker(dataset)?;
        if !same_as.is_empty() {
            let diff_pattern = QuadPattern {
                subject: PatternTerm::Variable,
                predicate: PatternTerm::bound(Term::NamedNode(owl_different_from())),
                object: PatternTerm::Variable,
                graph: GraphSelector::Union,
            };
            for m in dataset.find_quads(&diff_pattern)? {
                let m = m?;
                if same_as.canonicalize(&m.subject) == same_as.canonicalize(&m.object) {
                    diagnostics.push(Diagnostic::violation(
                        "eq-diff",
                        format!(
                            "{:?} and {:?} are owl:sameAs-equivalent but also asserted owl:differentFrom each other",
                            m.subject, m.object
                        ),
                    ));
                }
            }
        }

        let seed_set: std::collections::HashSet<[u64; 3]> = seed.iter().copied().collect();

        let mut inferred: Vec<Quad> = Vec::new();
        for row in closure {
            if seed_set.contains(&row) {
                // Already a base fact seeded from a core predicate — not a
                // new inference.
                continue;
            }
            let Some(quad) = decode_quad(&resolver, row) else {
                diagnostics.push(Diagnostic::new(
                    "decode",
                    format!("a derived row failed to decode back into a Quad: {row:?}"),
                ));
                continue;
            };

            // A rule can re-derive a triple that is itself a *base fact*
            // whose predicate isn't one of the "core" predicates seeded
            // above (e.g. `prp-spo1` re-deriving `alice hasParent bob`
            // when that exact triple was also separately asserted).
            // `seed_set` alone can't catch this, since `hasParent` isn't a
            // seeded predicate — so check directly against the dataset
            // (any graph, via `GraphSelector::Union`) before accepting a
            // row as a genuine inference. This check is what guarantees
            // `ReasoningDataset`'s documented "each quad appears exactly
            // once" contract.
            let pattern = QuadPattern {
                subject: PatternTerm::bound(Term::from(quad.subject.clone())),
                predicate: PatternTerm::bound(Term::NamedNode(quad.predicate.clone())), /* oxrdf::Quad */
                object: PatternTerm::bound(quad.object.clone()),
                graph: GraphSelector::Union,
            };
            if dataset.find_quads(&pattern)?.next().is_some() {
                continue;
            }

            inferred.push(quad);
        }

        Ok((inferred, diagnostics))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{BlankNode, GraphName as CoreGraphName, QuadStore};
    use oxigraph_nova_engine_ring::LoudsStore;
    use oxigraph_nova_query::StoreDataset;
    use std::sync::Arc;

    fn nn(s: &str) -> NamedNode {
        NamedNode::new(s).unwrap()
    }

    #[test]
    fn infers_subclass_transitivity_and_type_propagation() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        insert(
            nn("http://ex/Dog"),
            rdfs_sub_class_of(),
            Term::NamedNode(nn("http://ex/Mammal")),
        );
        insert(
            nn("http://ex/Mammal"),
            rdfs_sub_class_of(),
            Term::NamedNode(nn("http://ex/Animal")),
        );
        insert(
            nn("http://ex/fido"),
            rdf_type(),
            Term::NamedNode(nn("http://ex/Dog")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(diagnostics.is_empty());

        let has = |s: &str, p: NamedNode, o: &str| {
            inferred.iter().any(|q| {
                q.subject == NamedOrBlankNode::NamedNode(nn(s))
                    && q.predicate == p
                    && q.object == Term::NamedNode(nn(o))
            })
        };
        assert!(
            has("http://ex/Dog", rdfs_sub_class_of(), "http://ex/Animal"),
            "Dog subClassOf Animal must be derived transitively"
        );
        assert!(
            has("http://ex/fido", rdf_type(), "http://ex/Mammal"),
            "fido rdf:type Mammal must be derived via type propagation"
        );
        assert!(
            has("http://ex/fido", rdf_type(), "http://ex/Animal"),
            "fido rdf:type Animal must be derived transitively through the derived subclass edge"
        );
        // Base facts must not be re-emitted as "inferred".
        assert!(!has(
            "http://ex/Dog",
            rdfs_sub_class_of(),
            "http://ex/Mammal"
        ));
    }

    #[test]
    fn non_lftj_dataset_yields_empty_closure() {
        let dataset = oxigraph_nova_query::InMemoryDataset::new();
        let engine = LftjFixpointEngine::new();
        let (inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(inferred.is_empty());
        assert!(diagnostics.is_empty());
    }

    /// `prp-spo1`: `hasFather rdfs:subPropertyOf hasParent`, plus a
    /// `hasFather` edge unrelated to any other rule this engine covers, must
    /// propagate to a derived `hasParent` triple with the same subject/
    /// object — including when the `hasFather` edge is inserted in a
    /// *separate* graph than the `subPropertyOf` declaration (both are
    /// reasoned over via `GraphSelector::Union`).
    #[test]
    fn infers_subproperty_propagation() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let has_father = nn("http://ex/hasFather");
        let has_parent = nn("http://ex/hasParent");
        insert(
            has_father.clone(),
            rdfs_sub_property_of(),
            Term::NamedNode(has_parent.clone()),
        );
        insert(
            nn("http://ex/alice"),
            has_father.clone(),
            Term::NamedNode(nn("http://ex/bob")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(diagnostics.is_empty());

        let has = |s: &str, p: NamedNode, o: &str| {
            inferred.iter().any(|q| {
                q.subject == NamedOrBlankNode::NamedNode(nn(s))
                    && q.predicate == p
                    && q.object == Term::NamedNode(nn(o))
            })
        };
        assert!(
            has("http://ex/alice", has_parent, "http://ex/bob"),
            "alice hasParent bob must be derived via prp-spo1 property propagation"
        );
        // The base hasFather fact must not be re-emitted as "inferred".
        assert!(!has("http://ex/alice", has_father, "http://ex/bob"));
    }

    fn rdfs_domain() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#domain")
    }

    fn rdfs_range() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#range")
    }

    /// `prp-dom`/`prp-rng`: `hasParent rdfs:domain Person`,
    /// `hasParent rdfs:range Person`, `alice hasParent bob` must derive both
    /// `alice rdf:type Person` (domain) and `bob rdf:type Person` (range) —
    /// with **zero** `rdf:type` triples asserted anywhere in the dataset,
    /// proving the synthetic-TermId fix for head-only constants (see this
    /// module's doc comment): `rdf:type` cannot be interned via the
    /// read-only `lftj_intern_term`, since no base fact uses it, yet the
    /// domain/range rules must still fire.
    #[test]
    fn infers_domain_and_range() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let has_parent = nn("http://ex/hasParent");
        let person = nn("http://ex/Person");
        insert(
            has_parent.clone(),
            rdfs_domain(),
            Term::NamedNode(person.clone()),
        );
        insert(
            has_parent.clone(),
            rdfs_range(),
            Term::NamedNode(person.clone()),
        );
        insert(
            nn("http://ex/alice"),
            has_parent.clone(),
            Term::NamedNode(nn("http://ex/bob")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(diagnostics.is_empty());

        let has = |s: &str, p: NamedNode, o: &str| {
            inferred.iter().any(|q| {
                q.subject == NamedOrBlankNode::NamedNode(nn(s))
                    && q.predicate == p
                    && q.object == Term::NamedNode(nn(o))
            })
        };
        assert!(
            has("http://ex/alice", rdf_type(), "http://ex/Person"),
            "alice rdf:type Person must be derived via prp-dom domain propagation"
        );
        assert!(
            has("http://ex/bob", rdf_type(), "http://ex/Person"),
            "bob rdf:type Person must be derived via prp-rng range propagation"
        );
    }

    /// Regression test for the "duplicate inferred/base fact" bug: a rule
    /// can re-derive a triple that was *also* separately asserted as a base
    /// fact, even when that triple's predicate isn't one of the "core"
    /// seeded predicates — `seed_set` alone can't catch this (see
    /// `LftjFixpointEngine::infer`'s doc comment on the base-fact/dataset
    /// check). Here,
    /// `alice hasParent bob` is both asserted directly *and* re-derivable
    /// via `prp-spo1` from `hasFather subPropertyOf hasParent` +
    /// `alice hasFather bob` — it must not appear in `inferred`.
    #[test]
    fn does_not_report_a_base_fact_re_derived_by_a_rule_as_inferred() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let has_father = nn("http://ex/hasFather");
        let has_parent = nn("http://ex/hasParent");
        insert(
            has_father.clone(),
            rdfs_sub_property_of(),
            Term::NamedNode(has_parent.clone()),
        );
        insert(
            nn("http://ex/alice"),
            has_father,
            Term::NamedNode(nn("http://ex/bob")),
        );
        // Also assert the very triple prp-spo1 would derive.
        insert(
            nn("http://ex/alice"),
            has_parent.clone(),
            Term::NamedNode(nn("http://ex/bob")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(diagnostics.is_empty());

        let occurrences = inferred
            .iter()
            .filter(|q| {
                q.subject == NamedOrBlankNode::NamedNode(nn("http://ex/alice"))
                    && q.predicate == has_parent
                    && q.object == Term::NamedNode(nn("http://ex/bob"))
            })
            .count();
        assert_eq!(
            occurrences, 0,
            "a base fact re-derivable by a rule must not be reported as inferred, \
             even though its predicate (hasParent) isn't a core seeded predicate"
        );
    }

    fn owl_transitive_property() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#TransitiveProperty")
    }

    /// `prp-trp`: a user-declared `owl:TransitiveProperty` (`ancestorOf`,
    /// unrelated to the two hardcoded `subClassOf`/`subPropertyOf`
    /// transitivity predicates) must get its own dynamically-discovered
    /// transitivity rule: a→b→c chain of `ancestorOf` edges must derive the
    /// transitive a-ancestorOf-c edge.
    #[test]
    fn infers_generic_transitive_property() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let ancestor_of = nn("http://ex/ancestorOf");
        insert(
            ancestor_of.clone(),
            rdf_type(),
            Term::NamedNode(owl_transitive_property()),
        );
        insert(
            nn("http://ex/alice"),
            ancestor_of.clone(),
            Term::NamedNode(nn("http://ex/bob")),
        );
        insert(
            nn("http://ex/bob"),
            ancestor_of.clone(),
            Term::NamedNode(nn("http://ex/carol")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(diagnostics.is_empty());

        let has = |s: &str, p: NamedNode, o: &str| {
            inferred.iter().any(|q| {
                q.subject == NamedOrBlankNode::NamedNode(nn(s))
                    && q.predicate == p
                    && q.object == Term::NamedNode(nn(o))
            })
        };
        assert!(
            has("http://ex/alice", ancestor_of.clone(), "http://ex/carol"),
            "alice ancestorOf carol must be derived via prp-trp transitivity \
             over the dynamically-declared owl:TransitiveProperty"
        );
        // Base facts must not be re-emitted as "inferred".
        assert!(!has(
            "http://ex/alice",
            ancestor_of.clone(),
            "http://ex/bob"
        ));
        assert!(!has("http://ex/bob", ancestor_of, "http://ex/carol"));
    }

    fn owl_symmetric_property() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#SymmetricProperty")
    }

    /// `prp-symp`: a user-declared `owl:SymmetricProperty` (`knows`) must
    /// derive the reverse edge for every asserted `knows` triple.
    #[test]
    fn infers_generic_symmetric_property() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let knows = nn("http://ex/knows");
        insert(
            knows.clone(),
            rdf_type(),
            Term::NamedNode(owl_symmetric_property()),
        );
        insert(
            nn("http://ex/alice"),
            knows.clone(),
            Term::NamedNode(nn("http://ex/bob")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(diagnostics.is_empty());

        let has = |s: &str, p: NamedNode, o: &str| {
            inferred.iter().any(|q| {
                q.subject == NamedOrBlankNode::NamedNode(nn(s))
                    && q.predicate == p
                    && q.object == Term::NamedNode(nn(o))
            })
        };
        assert!(
            has("http://ex/bob", knows.clone(), "http://ex/alice"),
            "bob knows alice must be derived via prp-symp symmetry"
        );
        assert!(!has("http://ex/alice", knows, "http://ex/bob"));
    }

    /// `cax-eqc`: `A owl:equivalentClass B` must expand to *both*
    /// `A rdfs:subClassOf B` and `B rdfs:subClassOf A`.
    #[test]
    fn infers_equivalent_class_expands_both_directions() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        store
            .insert(&Quad::new(
                nn("http://ex/A"),
                owl_equivalent_class(),
                Term::NamedNode(nn("http://ex/B")),
                g,
            ))
            .unwrap();
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(diagnostics.is_empty());

        let has = |s: &str, o: &str| {
            inferred.iter().any(|q| {
                q.subject == NamedOrBlankNode::NamedNode(nn(s))
                    && q.predicate == rdfs_sub_class_of()
                    && q.object == Term::NamedNode(nn(o))
            })
        };
        assert!(has("http://ex/A", "http://ex/B"));
        assert!(has("http://ex/B", "http://ex/A"));
    }

    /// `prp-eqp`: `p1 owl:equivalentProperty p2` must expand to both
    /// `p1 rdfs:subPropertyOf p2` and `p2 rdfs:subPropertyOf p1`.
    #[test]
    fn infers_equivalent_property_expands_both_directions() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        store
            .insert(&Quad::new(
                nn("http://ex/p1"),
                owl_equivalent_property(),
                Term::NamedNode(nn("http://ex/p2")),
                g,
            ))
            .unwrap();
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(diagnostics.is_empty());

        let has = |s: &str, o: &str| {
            inferred.iter().any(|q| {
                q.subject == NamedOrBlankNode::NamedNode(nn(s))
                    && q.predicate == rdfs_sub_property_of()
                    && q.object == Term::NamedNode(nn(o))
            })
        };
        assert!(has("http://ex/p1", "http://ex/p2"));
        assert!(has("http://ex/p2", "http://ex/p1"));
    }

    /// `prp-inv1`/`prp-inv2`: `hasParent owl:inverseOf hasChild`,
    /// `alice hasParent bob` must derive `bob hasChild alice` (prp-inv1);
    /// separately, `carol hasChild dave` must derive `dave hasParent carol`
    /// (prp-inv2) — both directions active simultaneously without
    /// cross-contaminating each other's edges.
    #[test]
    fn infers_inverse_of_both_directions() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let has_parent = nn("http://ex/hasParent");
        let has_child = nn("http://ex/hasChild");
        insert(
            has_parent.clone(),
            owl_inverse_of(),
            Term::NamedNode(has_child.clone()),
        );
        insert(
            nn("http://ex/alice"),
            has_parent.clone(),
            Term::NamedNode(nn("http://ex/bob")),
        );
        insert(
            nn("http://ex/carol"),
            has_child.clone(),
            Term::NamedNode(nn("http://ex/dave")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(diagnostics.is_empty());

        let has = |s: &str, p: NamedNode, o: &str| {
            inferred.iter().any(|q| {
                q.subject == NamedOrBlankNode::NamedNode(nn(s))
                    && q.predicate == p
                    && q.object == Term::NamedNode(nn(o))
            })
        };
        assert!(
            has("http://ex/bob", has_child.clone(), "http://ex/alice"),
            "bob hasChild alice must be derived via prp-inv1"
        );
        assert!(
            has("http://ex/dave", has_parent, "http://ex/carol"),
            "dave hasParent carol must be derived via prp-inv2"
        );
    }

    /// A declaration triple with a non-IRI (blank node) subject — e.g.
    /// `_:bn rdf:type owl:TransitiveProperty` — cannot be used as a
    /// predicate (blank nodes are not valid RDF predicates), so it must be
    /// skipped with a [`Diagnostic`] rather than panicking or silently
    /// producing an empty closure with no explanation.
    #[test]
    fn diagnostic_emitted_for_non_iri_transitive_property_subject() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        store
            .insert(&Quad::new(
                BlankNode::new_unchecked("bn"),
                rdf_type(),
                Term::NamedNode(owl_transitive_property()),
                g,
            ))
            .unwrap();
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (_inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(
            diagnostics.iter().any(|d| d.rule == "prp-trp"),
            "expected a prp-trp diagnostic for the non-IRI declaration subject, got: {diagnostics:?}"
        );
    }

    /// `prp-asyp`: `hates rdf:type owl:AsymmetricProperty`, plus both
    /// `alice hates bob` and `bob hates alice` asserted, must produce a
    /// [`Diagnostic::violation`] with rule `"prp-asyp"`.
    #[test]
    fn detects_asymmetric_property_violation() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let hates = nn("http://ex/hates");
        insert(
            hates.clone(),
            rdf_type(),
            Term::NamedNode(owl_asymmetric_property()),
        );
        insert(
            nn("http://ex/alice"),
            hates.clone(),
            Term::NamedNode(nn("http://ex/bob")),
        );
        insert(
            nn("http://ex/bob"),
            hates,
            Term::NamedNode(nn("http://ex/alice")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (_inferred, diagnostics) = engine.infer(&dataset).unwrap();
        let violations: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.rule == "prp-asyp")
            .collect();
        assert_eq!(
            violations.len(),
            1,
            "expected exactly one prp-asyp violation, got: {diagnostics:?}"
        );
        assert!(violations[0].is_violation());
    }

    /// A dataset that respects an `owl:AsymmetricProperty` declaration
    /// (only one direction asserted) must produce no `prp-asyp` diagnostic.
    #[test]
    fn no_asymmetric_property_violation_when_only_one_direction_asserted() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let hates = nn("http://ex/hates");
        insert(
            hates.clone(),
            rdf_type(),
            Term::NamedNode(owl_asymmetric_property()),
        );
        insert(
            nn("http://ex/alice"),
            hates,
            Term::NamedNode(nn("http://ex/bob")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (_inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(!diagnostics.iter().any(|d| d.rule == "prp-asyp"));
    }

    /// `prp-irp`: `marriedTo rdf:type owl:IrreflexiveProperty`, plus
    /// `alice marriedTo alice`, must produce a [`Diagnostic::violation`]
    /// with rule `"prp-irp"`.
    #[test]
    fn detects_irreflexive_property_violation() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let married_to = nn("http://ex/marriedTo");
        insert(
            married_to.clone(),
            rdf_type(),
            Term::NamedNode(owl_irreflexive_property()),
        );
        insert(
            nn("http://ex/alice"),
            married_to,
            Term::NamedNode(nn("http://ex/alice")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (_inferred, diagnostics) = engine.infer(&dataset).unwrap();
        let violations: Vec<_> = diagnostics.iter().filter(|d| d.rule == "prp-irp").collect();
        assert_eq!(
            violations.len(),
            1,
            "expected exactly one prp-irp violation, got: {diagnostics:?}"
        );
        assert!(violations[0].is_violation());
    }

    /// A dataset that never relates an `owl:IrreflexiveProperty` to itself
    /// must produce no `prp-irp` diagnostic.
    #[test]
    fn no_irreflexive_property_violation_when_no_self_relation() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let married_to = nn("http://ex/marriedTo");
        insert(
            married_to.clone(),
            rdf_type(),
            Term::NamedNode(owl_irreflexive_property()),
        );
        insert(
            nn("http://ex/alice"),
            married_to,
            Term::NamedNode(nn("http://ex/bob")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (_inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(!diagnostics.iter().any(|d| d.rule == "prp-irp"));
    }

    /// `cax-dw`: `Cat owl:disjointWith Dog`, plus `felix rdf:type Cat` and
    /// `felix rdf:type Dog`, must produce a [`Diagnostic::violation`] with
    /// rule `"cax-dw"`.
    #[test]
    fn detects_disjoint_class_violation() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let cat = nn("http://ex/Cat");
        let dog = nn("http://ex/Dog");
        insert(
            cat.clone(),
            owl_disjoint_with(),
            Term::NamedNode(dog.clone()),
        );
        insert(nn("http://ex/felix"), rdf_type(), Term::NamedNode(cat));
        insert(nn("http://ex/felix"), rdf_type(), Term::NamedNode(dog));
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (_inferred, diagnostics) = engine.infer(&dataset).unwrap();
        let violations: Vec<_> = diagnostics.iter().filter(|d| d.rule == "cax-dw").collect();
        assert_eq!(
            violations.len(),
            1,
            "expected exactly one cax-dw violation, got: {diagnostics:?}"
        );
        assert!(violations[0].is_violation());
    }

    /// A dataset where an individual is only `rdf:type` one of two
    /// disjoint classes must produce no `cax-dw` diagnostic.
    #[test]
    fn no_disjoint_class_violation_when_only_one_class_asserted() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        let cat = nn("http://ex/Cat");
        let dog = nn("http://ex/Dog");
        insert(cat.clone(), owl_disjoint_with(), Term::NamedNode(dog));
        insert(nn("http://ex/felix"), rdf_type(), Term::NamedNode(cat));
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (_inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(!diagnostics.iter().any(|d| d.rule == "cax-dw"));
    }

    /// `eq-diff`: `alice owl:sameAs bob` plus `alice owl:differentFrom bob`
    /// must produce a [`Diagnostic::violation`] with rule `"eq-diff"`.
    #[test]
    fn detects_same_as_different_from_clash() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        insert(
            nn("http://ex/alice"),
            owl_same_as(),
            Term::NamedNode(nn("http://ex/bob")),
        );
        insert(
            nn("http://ex/alice"),
            owl_different_from(),
            Term::NamedNode(nn("http://ex/bob")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (_inferred, diagnostics) = engine.infer(&dataset).unwrap();
        let violations: Vec<_> = diagnostics.iter().filter(|d| d.rule == "eq-diff").collect();
        assert_eq!(
            violations.len(),
            1,
            "expected exactly one eq-diff violation, got: {diagnostics:?}"
        );
        assert!(violations[0].is_violation());
    }

    /// A dataset with `owl:sameAs`/`owl:differentFrom` asserted between
    /// terms that are *not* in the same equivalence class must produce no
    /// `eq-diff` diagnostic.
    #[test]
    fn no_same_as_different_from_clash_when_unrelated() {
        let store = LoudsStore::new();
        let g = CoreGraphName::DefaultGraph;
        let insert = |s: NamedNode, p: NamedNode, o: Term| {
            store.insert(&Quad::new(s, p, o, g.clone())).unwrap();
        };
        insert(
            nn("http://ex/alice"),
            owl_same_as(),
            Term::NamedNode(nn("http://ex/bob")),
        );
        insert(
            nn("http://ex/carol"),
            owl_different_from(),
            Term::NamedNode(nn("http://ex/dave")),
        );
        store.compact().unwrap();

        let dataset = StoreDataset::new(Arc::new(store));
        let engine = LftjFixpointEngine::new();
        let (_inferred, diagnostics) = engine.infer(&dataset).unwrap();
        assert!(!diagnostics.iter().any(|d| d.rule == "eq-diff"));
    }
}
