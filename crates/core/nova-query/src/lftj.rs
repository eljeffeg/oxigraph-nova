//! Leapfrog Triejoin (LFTJ) evaluator for BGP evaluation.
//!
//! Reference: T.L. Veldhuizen, "Leapfrog Triejoin: A Simple, Worst-Case Optimal
//! Join Algorithm", ICDT 2014.
//!
//! ## CLTJ* Adaptive Variable Elimination Order (VEO)
//!
//! Reference: Arroyuelo, Navarro, Gómez-Brandón et al.
//! "CompactLTJ: Space and Time Efficient Leapfrog Triejoin on Graph Databases"
//! VLDB Journal 2025, §3.5 "Adaptive Variable Elimination Order".
//!
//! Instead of a static first-appearance variable order, CLTJ* dynamically
//! re-sorts the remaining unbound variables at every recursive depth using
//! estimated leaf-descendant counts:
//!
//! ```text
//!     wj = min over patterns containing j of subtree_size(bound_ctx, j)
//! ```
//!
//! The variable with the smallest `wj` is iterated next.  This avoids exploring
//! large subtrees when a more selective variable is available.  In the paper
//! (§5.2, Table 1) adaptive VEO achieves almost an order of magnitude lower
//! average query time vs. static VEO on Wikidata.
//!
//! ## Real bound-context subtree sizes via a zero-allocation probe
//!
//! The C++ reference's `subtree_size_fixed1/2` (`ltj_iterator_basic.hpp`)
//! computes the *actual* leaf-descendant count under the currently bound trie
//! position — not a static, dataset-wide vocabulary-size heuristic.
//!
//! Nova's `veo_sort` mirrors this via [`Dataset::lftj_real_count`], which
//! performs the same LOUDS navigation as opening a real scan (`seek`/`open`)
//! but returns just a `u64` count, **without ever allocating a
//! `Box<dyn TrieIterator>`**. This is cheap enough to call for *every*
//! candidate variable/pattern at *every* recursion depth — only the winning
//! candidate then gets a real `lftj_join_scan(...)` call, made once by
//! `lftj_step`.
//!
//! When `lftj_real_count` returns `None` (non-CLTJ backends, or `AnyNamed`/
//! `Union` graph selectors), `veo_sort` falls back to the coarser
//! `dataset.lftj_estimate_count(...)` dataset-level heuristic.
//!
//! ## Implementation details
//!
//! | Aspect | Implementation |
//! |---|---|
//! | Variable ordering | Adaptive per depth |
//! | Bindings storage | `Vec<Option<u64>>` indexed by var_idx |
//! | `PatternSpec` method | `is_active_for_var(var_idx)` |
//! | `lftj_step` signature | `unbound: &[usize]` |
//! | Estimate method | real `Dataset::lftj_real_count(...)` (no allocation), falling back to `dataset.lftj_estimate_count(...)` |
//!
//! ## Algorithm sketch
//!
//! For a BGP with N triple patterns and k join variables:
//!
//! 1. Collect join variables (all variables; first-appearance order assigns
//!    stable `var_idx` indices 0..k).
//! 2. For each pattern, classify each field (s/p/o) as either:
//!    - `Const(id)` — a constant RDF term interned to its TermId
//!    - `JoinVar(var_idx)` — a variable at its stable index
//! 3. Recurse with `lftj_step(unbound = 0..k)`:
//!    a. Sort `unbound` by `wj` via `dataset.lftj_real_count(...)` (falling
//!    back to `dataset.lftj_estimate_count(...)`).
//!    b. Pick `j* = unbound[0]` (minimum wj).
//!    c. Open one scan per active pattern for `j*`.
//!    d. Leapfrog-sync to find common key.
//!    e. Bind `bindings[j*] = val`, recurse with `unbound[1..]`, advance.
//! 4. At `unbound.is_empty()`, emit a solution from `bindings`.

use crate::dataset::{Dataset, GraphSelector};
use crate::options::CancellationToken;
use crate::shapes::{KChainPlan, ShapePlan, SpExpansionPlan, StarPlan, TwoHopPlan, WedgePlan};
use crate::solution::{Solution, Solutions};
use oxigraph_nova_core::{GraphName, Term, Variable};
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// K9 path_2hop shape counters
static TWO_HOP_SHAPE_SEEN: AtomicU64 = AtomicU64::new(0);
static TWO_HOP_SELECTED: AtomicU64 = AtomicU64::new(0);
static TWO_HOP_FALLBACK: AtomicU64 = AtomicU64::new(0);
static SP_EXPANSION_SHAPE_SEEN: AtomicU64 = AtomicU64::new(0);
static SP_EXPANSION_SELECTED: AtomicU64 = AtomicU64::new(0);
static SP_EXPANSION_FALLBACK: AtomicU64 = AtomicU64::new(0);
// K7.1 / K11 fixed-P triangle wedge
static WEDGE_SHAPE_SEEN: AtomicU64 = AtomicU64::new(0);
static WEDGE_SELECTED: AtomicU64 = AtomicU64::new(0);
static WEDGE_FALLBACK: AtomicU64 = AtomicU64::new(0);
// K10 path_3hop (k=3 chain)
static K_CHAIN_SHAPE_SEEN: AtomicU64 = AtomicU64::new(0);
static K_CHAIN_SELECTED: AtomicU64 = AtomicU64::new(0);
static K_CHAIN_FALLBACK: AtomicU64 = AtomicU64::new(0);
// Subject-star (k=3)
static STAR_SHAPE_SEEN: AtomicU64 = AtomicU64::new(0);
static STAR_SELECTED: AtomicU64 = AtomicU64::new(0);
static STAR_FALLBACK: AtomicU64 = AtomicU64::new(0);

// ── LFTJ vs nested-loop fallback counters (process-wide) ──────────────────────
//
// Global, process-wide counters tracking how often BGP evaluation took the
// Leapfrog Triejoin fast path vs. falling back to nested-loop join.
// Incremented once per `eval_bgp` call in `evaluator.rs` (not
// per-triple-pattern), at the same point that decides which path to take;
// read by `nova-server`'s `/metrics` endpoint.
static LFTJ_USED: AtomicU64 = AtomicU64::new(0);
static LFTJ_FALLBACK: AtomicU64 = AtomicU64::new(0);

/// Record that a BGP evaluation took the LFTJ fast path.
pub fn record_lftj_used() {
    LFTJ_USED.fetch_add(1, Ordering::Relaxed);
}

/// Record that a BGP evaluation fell back to nested-loop join (LFTJ was
/// gated off — unsupported dataset, non-empty delta, blank nodes, etc.).
pub fn record_lftj_fallback() {
    LFTJ_FALLBACK.fetch_add(1, Ordering::Relaxed);
}

/// Total number of BGP evaluations that used the LFTJ fast path since
/// process start.
pub fn lftj_used_total() -> u64 {
    LFTJ_USED.load(Ordering::Relaxed)
}

/// Total number of BGP evaluations that fell back to nested-loop join since
/// process start.
pub fn lftj_fallback_total() -> u64 {
    LFTJ_FALLBACK.load(Ordering::Relaxed)
}

// ── W4b multi-subject object collapse diagnostics (process-wide) ─────────────
//
// These counters answer "why did D2 never fire on triangle?" without requiring
// a full SPARQL hang. Reset via [`reset_collapse_counters`].

static TRIANGLE_SHAPE_SEEN: AtomicU64 = AtomicU64::new(0);
static COLLAPSE_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static COLLAPSE_REJECT_PATTERN: AtomicU64 = AtomicU64::new(0);
static COLLAPSE_REJECT_BINDING_STATE: AtomicU64 = AtomicU64::new(0);
static COLLAPSE_REJECT_COLUMN: AtomicU64 = AtomicU64::new(0);
static COLLAPSE_REJECT_PRED: AtomicU64 = AtomicU64::new(0);
static COLLAPSE_REJECT_SUBJECTS: AtomicU64 = AtomicU64::new(0);
static COLLAPSE_DATASET_NONE: AtomicU64 = AtomicU64::new(0);
static COLLAPSE_SUCCESS: AtomicU64 = AtomicU64::new(0);
static LFTJ_DEPTH_ENTER: AtomicU64 = AtomicU64::new(0);
static LFTJ_DEPTH_LEAF: AtomicU64 = AtomicU64::new(0);
static LFTJ_SCAN_OPEN: AtomicU64 = AtomicU64::new(0);

/// Snapshot of W4b collapse / LFTJ depth / shape-walk counters for diagnostics.
#[derive(Clone, Debug, Default)]
pub struct CollapseCounterSnapshot {
    pub triangle_shape_seen: u64,
    pub collapse_attempts: u64,
    pub collapse_reject_pattern: u64,
    pub collapse_reject_binding_state: u64,
    pub collapse_reject_column: u64,
    pub collapse_reject_pred: u64,
    pub collapse_reject_subjects: u64,
    pub collapse_dataset_none: u64,
    pub collapse_success: u64,
    pub lftj_depth_enter: u64,
    pub lftj_depth_leaf: u64,
    pub lftj_scan_open: u64,
    /// KChain (k=3) specialized walk entered (catalog hit).
    pub k_chain_shape_seen: u64,
    /// KChain prepared body used (multi-var emit SELECTED path).
    pub k_chain_selected: u64,
    /// KChain nested `join_scan` fallback taken.
    pub k_chain_fallback: u64,
    /// Star (k=3) specialized walk entered (catalog hit).
    pub star_shape_seen: u64,
    /// Star prepared body used (multi-var emit SELECTED path).
    pub star_selected: u64,
    /// Star nested `join_scan` fallback taken.
    pub star_fallback: u64,
}

/// Zero all collapse / depth / shape-walk counters (harness use).
pub fn reset_collapse_counters() {
    for a in [
        &TRIANGLE_SHAPE_SEEN,
        &COLLAPSE_ATTEMPTS,
        &COLLAPSE_REJECT_PATTERN,
        &COLLAPSE_REJECT_BINDING_STATE,
        &COLLAPSE_REJECT_COLUMN,
        &COLLAPSE_REJECT_PRED,
        &COLLAPSE_REJECT_SUBJECTS,
        &COLLAPSE_DATASET_NONE,
        &COLLAPSE_SUCCESS,
        &LFTJ_DEPTH_ENTER,
        &LFTJ_DEPTH_LEAF,
        &LFTJ_SCAN_OPEN,
        &K_CHAIN_SHAPE_SEEN,
        &K_CHAIN_SELECTED,
        &K_CHAIN_FALLBACK,
        &STAR_SHAPE_SEEN,
        &STAR_SELECTED,
        &STAR_FALLBACK,
    ] {
        a.store(0, Ordering::Relaxed);
    }
}

/// Read collapse / depth / shape-walk counters.
pub fn collapse_counters_snapshot() -> CollapseCounterSnapshot {
    CollapseCounterSnapshot {
        triangle_shape_seen: TRIANGLE_SHAPE_SEEN.load(Ordering::Relaxed),
        collapse_attempts: COLLAPSE_ATTEMPTS.load(Ordering::Relaxed),
        collapse_reject_pattern: COLLAPSE_REJECT_PATTERN.load(Ordering::Relaxed),
        collapse_reject_binding_state: COLLAPSE_REJECT_BINDING_STATE.load(Ordering::Relaxed),
        collapse_reject_column: COLLAPSE_REJECT_COLUMN.load(Ordering::Relaxed),
        collapse_reject_pred: COLLAPSE_REJECT_PRED.load(Ordering::Relaxed),
        collapse_reject_subjects: COLLAPSE_REJECT_SUBJECTS.load(Ordering::Relaxed),
        collapse_dataset_none: COLLAPSE_DATASET_NONE.load(Ordering::Relaxed),
        collapse_success: COLLAPSE_SUCCESS.load(Ordering::Relaxed),
        lftj_depth_enter: LFTJ_DEPTH_ENTER.load(Ordering::Relaxed),
        lftj_depth_leaf: LFTJ_DEPTH_LEAF.load(Ordering::Relaxed),
        lftj_scan_open: LFTJ_SCAN_OPEN.load(Ordering::Relaxed),
        k_chain_shape_seen: K_CHAIN_SHAPE_SEEN.load(Ordering::Relaxed),
        k_chain_selected: K_CHAIN_SELECTED.load(Ordering::Relaxed),
        k_chain_fallback: K_CHAIN_FALLBACK.load(Ordering::Relaxed),
        star_shape_seen: STAR_SHAPE_SEEN.load(Ordering::Relaxed),
        star_selected: STAR_SELECTED.load(Ordering::Relaxed),
        star_fallback: STAR_FALLBACK.load(Ordering::Relaxed),
    }
}

// ── Field specification ────────────────────────────────────────────────────────

/// How a single s/p/o field in a triple pattern is classified for LFTJ.
///
/// `pub(crate)` so the shape catalog (`shapes/`) can pattern-match fields
/// without re-classifying patterns.
#[derive(Clone, Debug)]
pub(crate) enum FieldSpec {
    /// A constant term, already interned to its TermId.
    Const(u64),
    /// A join variable at the given stable index in the `join_vars` array.
    ///
    /// The index is fixed at classification time (first-appearance order) and
    /// does **not** correspond to recursion depth; the adaptive VEO determines
    /// the actual iteration order at runtime.
    JoinVar(usize),
}

/// The fully-classified version of one triple pattern.
///
/// `pub(crate)` for the shape catalog recognizers.
#[derive(Clone, Debug)]
pub(crate) struct PatternSpec {
    pub(crate) s: FieldSpec,
    pub(crate) p: FieldSpec,
    pub(crate) o: FieldSpec,
}

impl PatternSpec {
    /// Is this pattern active for `var_idx`?
    ///
    /// A pattern is active for `j` if exactly one of its fields is `JoinVar(j)`.
    fn is_active_for_var(&self, var_idx: usize) -> bool {
        let is_jv = |f: &FieldSpec| matches!(f, FieldSpec::JoinVar(i) if *i == var_idx);
        is_jv(&self.s) || is_jv(&self.p) || is_jv(&self.o)
    }

    /// Is every field of this pattern a constant (zero join variables)?
    ///
    /// A fully-ground pattern is never `is_active_for_var` for *any* variable,
    /// so `lftj_step`'s leapfrog recursion never opens a scan for it. Callers
    /// must independently verify existence for any fully-ground pattern (see
    /// `build_spec`) before treating the overall BGP as satisfied.
    fn is_fully_ground(&self) -> bool {
        matches!(self.s, FieldSpec::Const(_))
            && matches!(self.p, FieldSpec::Const(_))
            && matches!(self.o, FieldSpec::Const(_))
    }

    /// Resolve this pattern's fields for a scan targeting `var_idx`.
    ///
    /// `bindings[i] = Some(val)` for bound variables; `None` for unbound.
    ///
    /// Returns `(s_opt, p_opt, o_opt, target_field)`:
    /// - `Some(id)` for Const and already-bound JoinVar fields.
    /// - `None` for the target JoinVar and any unbound future variables.
    /// - `target_field`: 0=s, 1=p, 2=o — identifies which `None` is the seek target.
    fn resolve_for_var(
        &self,
        var_idx: usize,
        bindings: &[Option<u64>],
    ) -> (Option<u64>, Option<u64>, Option<u64>, usize) {
        let resolve_field = |f: &FieldSpec| -> Option<u64> {
            match f {
                FieldSpec::Const(id) => Some(*id),
                FieldSpec::JoinVar(i) => bindings[*i], // Some if bound, None if unbound
            }
        };

        let target_field = match (&self.s, &self.p, &self.o) {
            (FieldSpec::JoinVar(i), _, _) if *i == var_idx => 0,
            (_, FieldSpec::JoinVar(i), _) if *i == var_idx => 1,
            (_, _, FieldSpec::JoinVar(i)) if *i == var_idx => 2,
            _ => unreachable!("pattern must be active for var_idx"),
        };

        (
            resolve_field(&self.s),
            resolve_field(&self.p),
            resolve_field(&self.o),
            target_field,
        )
    }
}

// ── Variable collection ────────────────────────────────────────────────────────

/// Collect join variables from BGP patterns in order of first appearance.
///
/// The order establishes stable `var_idx` indices (0, 1, 2, …) used in
/// `JoinVar(var_idx)` throughout.  The actual iteration order is determined
/// dynamically by `veo_sort` in `lftj_step`, not by this function.
fn collect_join_vars(patterns: &[TriplePattern]) -> Vec<Variable> {
    let mut vars: Vec<Variable> = Vec::new();
    let mut seen: std::collections::HashMap<String, ()> = std::collections::HashMap::new();

    for tp in patterns {
        for var in extract_vars_from_pattern(tp) {
            if seen.insert(var.as_str().to_string(), ()).is_none() {
                vars.push(var);
            }
        }
    }
    vars
}

fn extract_vars_from_pattern(tp: &TriplePattern) -> Vec<Variable> {
    let mut out = Vec::new();
    if let TermPattern::Variable(v) = &tp.subject {
        out.push(v.clone());
    }
    if let NamedNodePattern::Variable(v) = &tp.predicate {
        out.push(v.clone());
    }
    if let TermPattern::Variable(v) = &tp.object {
        out.push(v.clone());
    }
    out
}

// ── Fallback detection ────────────────────────────────────────────────────────

fn pattern_has_blank_nodes(tp: &TriplePattern) -> bool {
    matches!(&tp.subject, TermPattern::BlankNode(_))
        || matches!(&tp.object, TermPattern::BlankNode(_))
}

// ── Pattern spec building ─────────────────────────────────────────────────────

enum SpecResult {
    Ok(PatternSpec),
    EmptyResult,
    Fallback,
}

fn classify_term_field<D: Dataset>(
    field: &TermPattern,
    join_vars: &[Variable],
    dataset: &D,
    graph: &GraphSelector,
) -> Result<FieldSpec, bool> {
    match field {
        TermPattern::Variable(v) => {
            let idx = join_vars
                .iter()
                .position(|jv| jv == v)
                .expect("variable must appear in join_vars");
            Ok(FieldSpec::JoinVar(idx))
        }
        TermPattern::NamedNode(n) => {
            let term = oxigraph_nova_core::Term::NamedNode(n.clone());
            match dataset.lftj_intern_term(&term, graph) {
                Some(id) => Ok(FieldSpec::Const(id)),
                None => Err(true),
            }
        }
        TermPattern::Literal(l) => {
            let term = oxigraph_nova_core::Term::Literal(l.clone());
            match dataset.lftj_intern_term(&term, graph) {
                Some(id) => Ok(FieldSpec::Const(id)),
                None => Err(true),
            }
        }
        TermPattern::BlankNode(_) => Err(false),
        // Quoted-triple patterns cause graceful fallback to the nested-loop
        // evaluator which handles structural binding via bind_triple_pattern().
        TermPattern::Triple(_) => Err(false),
    }
}

fn classify_nn_field<D: Dataset>(
    field: &NamedNodePattern,
    join_vars: &[Variable],
    dataset: &D,
    graph: &GraphSelector,
) -> Result<FieldSpec, bool> {
    match field {
        NamedNodePattern::Variable(v) => {
            let idx = join_vars
                .iter()
                .position(|jv| jv == v)
                .expect("variable must appear in join_vars");
            Ok(FieldSpec::JoinVar(idx))
        }
        NamedNodePattern::NamedNode(n) => {
            let term = oxigraph_nova_core::Term::NamedNode(n.clone());
            match dataset.lftj_intern_term(&term, graph) {
                Some(id) => Ok(FieldSpec::Const(id)),
                None => Err(true),
            }
        }
    }
}

/// Convert a fully-classified (`Const`-only) `TermPattern` back to a concrete
/// `Term`. Only ever called after `classify_term_field` has already
/// classified the same field as `Ok(FieldSpec::Const(_))`, so `Variable`,
/// `BlankNode` and `Triple` (which produce `JoinVar`/`Err`, never `Const`)
/// cannot occur here.
fn ground_term_pattern(tp: &TermPattern) -> oxigraph_nova_core::Term {
    match tp {
        TermPattern::NamedNode(n) => oxigraph_nova_core::Term::NamedNode(n.clone()),
        TermPattern::Literal(l) => oxigraph_nova_core::Term::Literal(l.clone()),
        TermPattern::Variable(_) | TermPattern::BlankNode(_) | TermPattern::Triple(_) => {
            unreachable!("ground_term_pattern called on a non-constant field")
        }
    }
}

/// Same as [`ground_term_pattern`] for the predicate position.
fn ground_nn_pattern(nn: &NamedNodePattern) -> oxigraph_nova_core::Term {
    match nn {
        NamedNodePattern::NamedNode(n) => oxigraph_nova_core::Term::NamedNode(n.clone()),
        NamedNodePattern::Variable(_) => {
            unreachable!("ground_nn_pattern called on a non-constant field")
        }
    }
}

/// Resolve a `GraphSelector` to a concrete `GraphName` for
/// `Dataset::contains_quad`. Only `Default`/`Named` selectors reach
/// `build_spec` (`AnyNamed`/`Union` are handled by `eval_bgp_lftj_multi_graph`
/// before `build_spec` is ever called per-graph, and `UnionOf` triggers an
/// immediate nested-loop fallback) — `None` here is defensive only.
fn graph_name_for_selector(graph: &GraphSelector) -> Option<GraphName> {
    match graph {
        GraphSelector::Default => Some(GraphName::DefaultGraph),
        GraphSelector::Named(g) => Some(g.clone()),
        GraphSelector::AnyNamed | GraphSelector::Union | GraphSelector::UnionOf(_) => None,
    }
}

fn build_spec<D: Dataset>(
    tp: &TriplePattern,
    join_vars: &[Variable],
    dataset: &D,
    graph: &GraphSelector,
) -> SpecResult {
    if pattern_has_blank_nodes(tp) {
        return SpecResult::Fallback;
    }

    let s = match classify_term_field(&tp.subject, join_vars, dataset, graph) {
        Ok(f) => f,
        Err(true) => return SpecResult::EmptyResult,
        Err(false) => return SpecResult::Fallback,
    };
    let p = match classify_nn_field(&tp.predicate, join_vars, dataset, graph) {
        Ok(f) => f,
        Err(true) => return SpecResult::EmptyResult,
        Err(false) => return SpecResult::Fallback,
    };
    let o = match classify_term_field(&tp.object, join_vars, dataset, graph) {
        Ok(f) => f,
        Err(true) => return SpecResult::EmptyResult,
        Err(false) => return SpecResult::Fallback,
    };

    let spec = PatternSpec { s, p, o };

    // ── Fully-ground pattern: verify existence directly ─────────────────────
    //
    // A pattern with zero variables (all of s/p/o are `Const`) is never
    // `is_active_for_var` for *any* join variable, so `lftj_step`'s leapfrog
    // recursion never opens a scan for it -- the pattern would otherwise
    // silently contribute nothing and the BGP would be treated as trivially
    // satisfied by `lftj_step`'s base case. Each field being individually
    // interned (checked above via `lftj_intern_term`) only means the *term*
    // exists in the dictionary -- NOT that this exact (s, p, o) triple was
    // ever asserted together. Verify that directly, once, via the
    // always-correct `Dataset::contains_quad` existence check (same
    // nested-loop-backed path used elsewhere in the evaluator).
    if spec.is_fully_ground() {
        let Some(graph_name) = graph_name_for_selector(graph) else {
            return SpecResult::Fallback;
        };
        let s_term = ground_term_pattern(&tp.subject);
        let p_term = ground_nn_pattern(&tp.predicate);
        let o_term = ground_term_pattern(&tp.object);
        match dataset.contains_quad(&s_term, &p_term, &o_term, &graph_name) {
            Ok(true) => {}
            Ok(false) => return SpecResult::EmptyResult,
            Err(_) => return SpecResult::Fallback,
        }
    }

    SpecResult::Ok(spec)
}

// ── Adaptive VEO sort ─────────────────────────────────────────────────────────

/// Sort `unbound` (var indices into `join_vars`) ascending by a **constraint-
/// aware** bound-context cost — CLTJ*'s adaptive VEO (§3.5) with a product-path
/// correction for multi-constant patterns.
///
/// For each candidate variable/pattern this calls
/// [`Dataset::lftj_real_count`], a zero-allocation probe that performs the
/// same LOUDS navigation as opening a real scan but never constructs a
/// `Box<dyn TrieIterator>`.
///
/// ## Why not raw min(distinct)?
///
/// Raw min distinct under each open can pick a catastrophically wrong outer
/// variable when backends return *accurate* counts. Classic BSBM 2join:
///
/// ```text
///   ?s P31 class0 . ?s P131 ?region
///   distinct(?region | P131) = 500
///   distinct(?s | P31, class0) = 1000
/// ```
///
/// Min-count VEO binds `?region` first → ~50k subject probes under each region
/// then class0 filter (~50× slower). Binding `?s` first is O(class0 subjects).
/// LOUDS's coarse vocab heuristic accidentally preferred `?s`; Ring's exact
/// MiddleRuns/LastCol estimates exposed the bug (Phase J1 census, 2026-07).
///
/// **Fix:** scale each pattern's count by `1/(n_bound+1)²` where `n_bound` is
/// the number of already-bound/const fields in the probe (not the target).
/// Multi-constant patterns (P31+class0) beat single-constant ones (P131 alone)
/// even when raw distinct is slightly larger. Falls back to first-appearance
/// order on ties (stable).
///
/// Falls back to `dataset.lftj_estimate_count(...)` when `lftj_real_count`
/// returns `None`. For non-CLTJ backends all estimates are `u64::MAX`, so the
/// sort is stable (preserves first-appearance order).
fn veo_sort<D: Dataset>(
    unbound: &[usize],
    specs: &[PatternSpec],
    bindings: &[Option<u64>],
    dataset: &D,
    graph: &GraphSelector,
) -> Vec<usize> {
    struct Candidate {
        var_idx: usize,
        weight: u64,
    }

    let mut candidates: Vec<Candidate> = unbound
        .iter()
        .map(|&vi| {
            let mut weight = u64::MAX;
            for sp in specs.iter().filter(|sp| sp.is_active_for_var(vi)) {
                let (sv, pv, ov, tf) = sp.resolve_for_var(vi, bindings);
                let w = dataset
                    .lftj_real_count(sv, pv, ov, tf, graph)
                    .unwrap_or_else(|| dataset.lftj_estimate_count(sv, pv, ov, tf, graph));
                // n_bound = consts + already-bound join vars in this probe
                // (target field is None and does not count).
                let n_bound = (sv.is_some() as u64) + (pv.is_some() as u64) + (ov.is_some() as u64);
                let denom = (n_bound + 1).saturating_mul(n_bound + 1).max(1);
                // ceil(w / denom) without overflow for huge w.
                let effective = w / denom + u64::from(w % denom != 0);
                weight = weight.min(effective.max(1));
            }
            Candidate {
                var_idx: vi,
                weight,
            }
        })
        .collect();

    // Ascending weight; stable on ties → first-appearance order preserved.
    candidates.sort_by(|a, b| {
        a.weight
            .cmp(&b.weight)
            .then_with(|| a.var_idx.cmp(&b.var_idx))
    });

    candidates.into_iter().map(|c| c.var_idx).collect()
}

// ── Leapfrog synchronization ──────────────────────────────────────────────────

/// Seek all scans to a common minimum key using the leapfrog protocol.
///
/// Returns `Some(key)` when all scans agree on the same key, or `None` if
/// any scan is exhausted before agreement is reached.
fn leapfrog_sync(scans: &mut [Box<dyn oxigraph_nova_core::TrieIterator>]) -> Option<u64> {
    if scans.is_empty() {
        return None;
    }
    if scans.iter().any(|s| s.at_end()) {
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
        // else: loop again with the updated max key
    }
}

// ── Recursive LFTJ step (adaptive VEO) ────────────────────────────────────────

/// W4b: if every resolved scan is "bound subject → target object" (optionally
/// with the same bound predicate), ask the dataset for a multi-subject object
/// intersection iterator (Ring D1/D2). Returns `None` to keep ordinary leapfrog.
fn try_collapse_subject_object_intersect<D: Dataset>(
    dataset: &D,
    graph: &GraphSelector,
    resolved: &[(Option<u64>, Option<u64>, Option<u64>, usize)],
) -> Option<Box<dyn oxigraph_nova_core::TrieIterator>> {
    if resolved.len() < 2 {
        COLLAPSE_REJECT_PATTERN.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    COLLAPSE_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    if resolved.len() >= 3 {
        // Triangle-closing shape candidate (≥3 active object-target scans).
        TRIANGLE_SHAPE_SEEN.fetch_add(1, Ordering::Relaxed);
    }
    // All must target object (field 2), have bound subject, unbound object.
    let mut subjects: Vec<u64> = Vec::with_capacity(resolved.len());
    let mut pred: Option<Option<u64>> = None;
    for &(s, p, o, tf) in resolved {
        if tf != 2 {
            COLLAPSE_REJECT_COLUMN.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        if s.is_none() || o.is_some() {
            COLLAPSE_REJECT_BINDING_STATE.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        subjects.push(s.unwrap());
        match pred {
            None => pred = Some(p),
            Some(prev) if prev != p => {
                COLLAPSE_REJECT_PRED.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            Some(_) => {}
        }
    }
    // Distinct subjects required (duplicate S would be a no-op / degenerate).
    subjects.sort_unstable();
    subjects.dedup();
    if subjects.len() < 2 {
        COLLAPSE_REJECT_SUBJECTS.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let predicate = pred.flatten();
    match dataset.lftj_multi_subject_object_intersect(&subjects, predicate, graph) {
        Some(it) => {
            COLLAPSE_SUCCESS.fetch_add(1, Ordering::Relaxed);
            Some(it)
        }
        None => {
            COLLAPSE_DATASET_NONE.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// Recursive inner loop of LFTJ with adaptive VEO.
///
/// ## Parameters
///
/// - `unbound` — var indices (into `join_vars`) yet to be bound, in the caller's
///   suggested order.  This function re-sorts them via `veo_sort` before iterating.
/// - `bindings` — indexed by `var_idx`; `Some(val)` = bound, `None` = unbound.
/// - `cancellation` — checked once per recursive call (cheap relaxed atomic
///   load); when set, `*aborted` is flipped to `true` and the whole subtree
///   unwinds immediately without emitting further solutions. Checking once
///   per call (rather than per leapfrog-loop iteration) keeps the overhead
///   proportional to the number of *bound* intermediate rows, not to every
///   scan advance, so this does not regress the hot leapfrog-sync path.
/// - `aborted` — set to `true` the moment cancellation is observed; every
///   recursive call and loop iteration checks it first so cancellation
///   propagates back up to `eval_bgp_lftj` promptly.
#[allow(clippy::too_many_arguments)]
fn lftj_step<D: Dataset>(
    dataset: &D,
    specs: &[PatternSpec],
    join_vars: &Arc<[Variable]>,
    graph: &GraphSelector,
    unbound: &[usize],
    bindings: &mut Vec<Option<u64>>,
    results: &mut Solutions,
    cancellation: Option<&CancellationToken>,
    aborted: &mut bool,
) {
    if *aborted {
        return;
    }
    if let Some(token) = cancellation
        && token.is_cancelled()
    {
        *aborted = true;
        return;
    }

    // Base case: all variables bound → emit a solution.
    //
    // Builds the row via `Solution::positional`, sharing `join_vars`'s
    // `Arc<[Variable]>` header (built once per query in `eval_bgp_lftj`)
    // across every emitted row instead of cloning each bound `Variable`
    // individually — see `solution.rs`'s module docs for the full
    // allocation-reduction rationale (measured via `profile_eval
    // --count-allocs`).
    if unbound.is_empty() {
        LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
        let values: Vec<Option<oxigraph_nova_core::Term>> = bindings
            .iter()
            .map(|binding| {
                binding
                    .as_ref()
                    .and_then(|&id| dataset.lftj_decode_term(id))
            })
            .collect();
        results.push(Solution::positional(Arc::clone(join_vars), values));
        return;
    }

    LFTJ_DEPTH_ENTER.fetch_add(1, Ordering::Relaxed);

    // ── Adaptive VEO: re-sort remaining unbound variables by min count ──────
    //
    // Only allocate + sort when the backend provides meaningful (non-u64::MAX)
    // estimates.  Backends that return u64::MAX for all estimates (MemoryStore,
    // StubDataset in tests) cannot benefit from VEO reordering — the stable
    // sort leaves the first-appearance order unchanged — but they do pay the
    // heap-allocation + closure-per-call cost on every recursive invocation.
    //
    // Gate on `dataset.supports_veo_estimates()` to skip the overhead for
    // non-CLTJ backends without regressing CLTJ* behaviour.
    let veo_buf: Vec<usize>;
    let order: &[usize] = if dataset.supports_veo_estimates() && unbound.len() > 1 {
        veo_buf = veo_sort(unbound, specs, bindings, dataset, graph);
        &veo_buf
    } else {
        unbound
    };

    let var_idx = order[0];
    let remaining = &order[1..];

    // ── Find patterns active for this variable ──────────────────────────────
    let active: Vec<&PatternSpec> = specs
        .iter()
        .filter(|sp| sp.is_active_for_var(var_idx))
        .collect();

    if active.is_empty() {
        // No pattern constrains this variable (degenerate; should not arise in
        // a well-formed BGP but handled defensively).
        lftj_step(
            dataset,
            specs,
            join_vars,
            graph,
            remaining,
            bindings,
            results,
            cancellation,
            aborted,
        );
        return;
    }

    // ── Obtain scans (W4b: collapse multi-subject → object to D1/D2) ────────
    //
    // When ≥2 active patterns all target the object field under distinct bound
    // subjects (triangle closing edge / product G3 shape), prefer one braided
    // intersection iterator over N independent leapfrog scans.
    let resolved: Vec<(Option<u64>, Option<u64>, Option<u64>, usize)> = active
        .iter()
        .map(|sp| sp.resolve_for_var(var_idx, bindings))
        .collect();

    let mut scans: Vec<Box<dyn oxigraph_nova_core::TrieIterator>> =
        Vec::with_capacity(active.len());

    let collapsed = try_collapse_subject_object_intersect(dataset, graph, &resolved);
    if let Some(scan) = collapsed {
        LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
        scans.push(scan);
    } else {
        for &(sv, pv, ov, target_field) in &resolved {
            match dataset.lftj_join_scan(sv, pv, ov, target_field, graph) {
                Some(scan) => {
                    LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
                    scans.push(scan);
                }
                None => return, // scan not available → caller will use fallback
            }
        }
    }

    if scans.iter().any(|s| s.at_end()) {
        return;
    }

    // ── Leapfrog loop ────────────────────────────────────────────────────────
    loop {
        if *aborted {
            break;
        }
        match leapfrog_sync(&mut scans) {
            None => break, // one or more scans exhausted
            Some(val) => {
                bindings[var_idx] = Some(val);
                lftj_step(
                    dataset,
                    specs,
                    join_vars,
                    graph,
                    remaining,
                    bindings,
                    results,
                    cancellation,
                    aborted,
                );
                bindings[var_idx] = None;

                if *aborted {
                    break;
                }

                // Advance the first scan past `val`; leapfrog_sync will re-sync.
                scans[0].advance();
                if scans[0].at_end() {
                    break;
                }
            }
        }
    }
}

/// Field helpers for the shape catalog (`shapes/`).
#[inline]
pub(crate) fn field_as_join(f: &FieldSpec) -> Option<usize> {
    match f {
        FieldSpec::JoinVar(i) => Some(*i),
        _ => None,
    }
}

#[inline]
pub(crate) fn field_as_const(f: &FieldSpec) -> Option<u64> {
    match f {
        FieldSpec::Const(v) => Some(*v),
        _ => None,
    }
}

// ── Shape physical strategies ─────────────────────────────────────────────────
//
// Recognition lives in `shapes/`. Walkers call `lftj_prepare_shape` with a
// `PhysicalShape` derived from the plan; nested-scan fallbacks remain when the
// engine returns None.

#[inline]
fn emit_sp_expansion_solution<D: Dataset>(
    dataset: &D,
    join_vars: &Arc<[Variable]>,
    s_idx: usize,
    o_idx: usize,
    s: u64,
    o: u64,
    // Optional pre-decoded subject term (avoids re-decode per object row).
    s_term: &Option<Term>,
    results: &mut Solutions,
) {
    let mut values: Vec<Option<Term>> = vec![None; join_vars.len()];
    values[s_idx] = s_term.clone().or_else(|| dataset.lftj_decode_term(s));
    values[o_idx] = dataset.lftj_decode_term(o);
    results.push(Solution::positional(Arc::clone(join_vars), values));
}

/// Physical 2join: subjects under (P_filter, O_filter), then prepared SP→O reset.
fn eval_sp_expansion_walk<D: Dataset>(
    dataset: &D,
    join_vars: &Arc<[Variable]>,
    graph: &GraphSelector,
    plan: &SpExpansionPlan,
    cancellation: Option<&CancellationToken>,
) -> anyhow::Result<Solutions> {
    SP_EXPANSION_SHAPE_SEEN.fetch_add(1, Ordering::Relaxed);

    let mut results: Solutions = Vec::new();
    let mut step: u64 = 0;

    // Prefer dense prepared SP-expansion (Ring): outer subjects cached,
    // expand stays dense — no per-subject external↔dense remap.
    if let Some(mut prepared) = dataset.lftj_prepare_shape(plan.to_physical(), graph) {
        SP_EXPANSION_SELECTED.fetch_add(1, Ordering::Relaxed);
        let emit_result = prepared.execute(&mut |ids| {
            // SP-expansion emits [s, o]
            debug_assert!(ids.len() >= 2);
            let s = ids[0];
            let o = ids[1];
            if let Some(tok) = cancellation
                && step.is_multiple_of(4096)
                && tok.is_cancelled()
            {
                    return Err(());
            }
            step += 1;
            LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
            let s_term = dataset.lftj_decode_term(s);
            emit_sp_expansion_solution(
                dataset,
                join_vars,
                plan.s_idx,
                plan.o_idx,
                s,
                o,
                &s_term,
                &mut results,
            );
            Ok(())
        });
        return match emit_result {
            Ok(_) => Ok(results),
            Err(()) => Err(anyhow::Error::from(
                crate::options::EvalLimitError::Cancelled,
            )),
        };
    }

    // Fallback: prepared SP→O scanner + outer join_scan (external keys).
    let mut prepared = dataset.lftj_prepare_sp_object_scan(plan.p_expand, graph);
    if prepared.is_some() {
        SP_EXPANSION_SELECTED.fetch_add(1, Ordering::Relaxed);
    } else {
        SP_EXPANSION_FALLBACK.fetch_add(1, Ordering::Relaxed);
    }

    // Outer: subjects of pattern A (bound P+O → target S).
    let mut s_scan =
        match dataset.lftj_join_scan(None, Some(plan.p_filter), Some(plan.o_filter), 0, graph) {
            Some(s) => s,
            None => return Ok(results),
        };
    LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);

    while !s_scan.at_end() {
        if let Some(tok) = cancellation
            && step.is_multiple_of(4096)
            && tok.is_cancelled()
        {
                return Err(anyhow::Error::from(
                    crate::options::EvalLimitError::Cancelled,
                ));
        }
        step += 1;
        let s = s_scan.key();
        // Decode subject once per s (shared across all objects of this s).
        let s_term = dataset.lftj_decode_term(s);

        if let Some(ref mut hop) = prepared {
            if hop.reset_to_subject(s) {
                while !hop.at_end() {
                    let o = hop.key();
                    LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
                    emit_sp_expansion_solution(
                        dataset,
                        join_vars,
                        plan.s_idx,
                        plan.o_idx,
                        s,
                        o,
                        &s_term,
                        &mut results,
                    );
                    hop.advance();
                }
            }
        } else {
            // Nested join_scan fallback (still fixed order, no VEO thrash).
            if let Some(mut o_scan) =
                dataset.lftj_join_scan(Some(s), Some(plan.p_expand), None, 2, graph)
            {
                LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
                while !o_scan.at_end() {
                    let o = o_scan.key();
                    LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
                    emit_sp_expansion_solution(
                        dataset,
                        join_vars,
                        plan.s_idx,
                        plan.o_idx,
                        s,
                        o,
                        &s_term,
                        &mut results,
                    );
                    o_scan.advance();
                }
            }
        }
        s_scan.advance();
    }

    Ok(results)
}

/// Emit one decoded solution from three bound TermIds (two-hop path).
///
/// Returns nanoseconds spent in decode+materialize for path timing.
#[inline]
fn emit_two_hop_solution<D: Dataset>(
    dataset: &D,
    join_vars: &Arc<[Variable]>,
    a_idx: usize,
    b_idx: usize,
    c_idx: usize,
    a: u64,
    b: u64,
    c: u64,
    results: &mut Solutions,
) -> u64 {
    let t_dec = std::time::Instant::now();
    let mut values: Vec<Option<oxigraph_nova_core::Term>> = vec![None; join_vars.len()];
    values[a_idx] = dataset.lftj_decode_term(a);
    values[b_idx] = dataset.lftj_decode_term(b);
    values[c_idx] = dataset.lftj_decode_term(c);
    if values[a_idx].is_none() || values[b_idx].is_none() || values[c_idx].is_none() {
        return t_dec.elapsed().as_nanos() as u64;
    }
    results.push(Solution::positional(Arc::clone(join_vars), values));
    let ns = t_dec.elapsed().as_nanos() as u64;
    crate::path_timing::add_path_timing_ns(crate::path_timing::PathTimingBucket::Decode, ns);
    ns
}

/// K9 physical two-hop: prepared scanners when available, else nested join_scan.
///
/// Always returns `Some` once the shape is recognized (specialized nested
/// fallback still beats generic VEO LFTJ on this shape).
///
/// Path timing: Execution = operator walk only; Decode = term materialize.
/// The two buckets are non-overlapping (decode ns is subtracted from wall).
fn eval_two_hop_walk<D: Dataset>(
    dataset: &D,
    join_vars: &Arc<[Variable]>,
    graph: &GraphSelector,
    plan: &TwoHopPlan,
    cancellation: Option<&CancellationToken>,
    id_only: bool,
) -> anyhow::Result<Solutions> {
    TWO_HOP_SHAPE_SEEN.fetch_add(1, Ordering::Relaxed);

    let empty_vals: Vec<Option<Term>> = vec![None; join_vars.len()];
    let mut results: Solutions = Vec::new();
    let mut step: u64 = 0;
    let mut decode_ns: u64 = 0;

    // Prefer prepared two-hop body (K9.1 + K9.2 resettable SP scanners).
    if let Some(mut prepared) = dataset.lftj_prepare_shape(plan.to_physical(), graph) {
        TWO_HOP_SELECTED.fetch_add(1, Ordering::Relaxed);
        let t_wall = std::time::Instant::now();
        let emit_result = prepared.execute(&mut |ids| {
            debug_assert!(ids.len() >= 3);
            let a = ids[0];
            let b = ids[1];
            let c = ids[2];
            if let Some(tok) = cancellation
                && step.is_multiple_of(4096)
                && tok.is_cancelled()
            {
                    return Err(());
            }
            step += 1;
            LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
            if id_only {
                results.push(Solution::positional(
                    Arc::clone(join_vars),
                    empty_vals.clone(),
                ));
            } else {
                decode_ns = decode_ns.saturating_add(emit_two_hop_solution(
                    dataset,
                    join_vars,
                    plan.a,
                    plan.b,
                    plan.c,
                    a,
                    b,
                    c,
                    &mut results,
                ));
            }
            Ok(())
        });
        match emit_result {
            Ok(_) => {
                let wall_ns = t_wall.elapsed().as_nanos() as u64;
                let exec_ns = wall_ns.saturating_sub(decode_ns);
                crate::path_timing::add_path_timing_ns(
                    crate::path_timing::PathTimingBucket::Execution,
                    exec_ns,
                );
                return Ok(results);
            }
            Err(()) => {
                return Err(anyhow::Error::from(
                    crate::options::EvalLimitError::Cancelled,
                ));
            }
        }
    }

    // Specialized nested join_scan fallback (fixed a→b→c order, no VEO).
    TWO_HOP_FALLBACK.fetch_add(1, Ordering::Relaxed);
    let mut a_scan = match dataset.lftj_join_scan(None, Some(plan.p1), None, 0, graph) {
        Some(s) => s,
        None => return Ok(results),
    };
    LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
    let t_wall = std::time::Instant::now();

    while !a_scan.at_end() {
        if let Some(tok) = cancellation
            && step.is_multiple_of(4096)
            && tok.is_cancelled()
        {
                return Err(anyhow::Error::from(
                    crate::options::EvalLimitError::Cancelled,
                ));
        }
        step += 1;
        let a = a_scan.key();
        let mut b_scan = match dataset.lftj_join_scan(Some(a), Some(plan.p1), None, 2, graph) {
            Some(s) => s,
            None => {
                a_scan.advance();
                continue;
            }
        };
        LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
        while !b_scan.at_end() {
            step += 1;
            let b = b_scan.key();
            if let Some(mut c_scan) = dataset.lftj_join_scan(Some(b), Some(plan.p2), None, 2, graph)
            {
                LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
                while !c_scan.at_end() {
                    let c = c_scan.key();
                    LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
                    if id_only {
                        results.push(Solution::positional(
                            Arc::clone(join_vars),
                            empty_vals.clone(),
                        ));
                    } else {
                        decode_ns = decode_ns.saturating_add(emit_two_hop_solution(
                            dataset,
                            join_vars,
                            plan.a,
                            plan.b,
                            plan.c,
                            a,
                            b,
                            c,
                            &mut results,
                        ));
                    }
                    c_scan.advance();
                }
            }
            b_scan.advance();
        }
        a_scan.advance();
    }

    let wall_ns = t_wall.elapsed().as_nanos() as u64;
    let exec_ns = wall_ns.saturating_sub(decode_ns);
    crate::path_timing::add_path_timing_ns(
        crate::path_timing::PathTimingBucket::Execution,
        exec_ns,
    );

    Ok(results)
}

/// K7.1 fixed-P triangle: prepared wedge when available, else D1-style nested walk.
///
/// Plan orientation is a→b, b→c, a→c under one predicate. Nested fallback:
/// enumerate `(a,b)` via join_scan, close `c` with
/// `lftj_multi_subject_object_intersect([a,b], Some(P))` (Ring D1), else a
/// third join_scan on `(a,P,?c)` filtered by membership in `b`'s objects.
fn eval_wedge_walk<D: Dataset>(
    dataset: &D,
    join_vars: &Arc<[Variable]>,
    graph: &GraphSelector,
    plan: &WedgePlan,
    cancellation: Option<&CancellationToken>,
) -> anyhow::Result<Solutions> {
    WEDGE_SHAPE_SEEN.fetch_add(1, Ordering::Relaxed);
    // Same observability bucket as in-step collapse (≥3 object targets).
    TRIANGLE_SHAPE_SEEN.fetch_add(1, Ordering::Relaxed);

    let mut results: Solutions = Vec::new();
    let mut step: u64 = 0;

    // Prefer prepared wedge body when the engine implements it.
    if let Some(mut prepared) = dataset.lftj_prepare_shape(plan.to_physical(), graph) {
        WEDGE_SELECTED.fetch_add(1, Ordering::Relaxed);
        let emit_result = prepared.execute(&mut |ids| {
            debug_assert!(ids.len() >= 3);
            let a = ids[0];
            let b = ids[1];
            let c = ids[2];
            if let Some(tok) = cancellation
                && step.is_multiple_of(4096)
                && tok.is_cancelled()
            {
                    return Err(());
            }
            step += 1;
            LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
            let _ = emit_two_hop_solution(
                dataset,
                join_vars,
                plan.a,
                plan.b,
                plan.c,
                a,
                b,
                c,
                &mut results,
            );
            Ok(())
        });
        return match emit_result {
            Ok(_) => Ok(results),
            Err(()) => Err(anyhow::Error::from(
                crate::options::EvalLimitError::Cancelled,
            )),
        };
    }

    // Nested D1-style walk (fixed a→b outer, close via multi-subject ∩).
    WEDGE_FALLBACK.fetch_add(1, Ordering::Relaxed);
    let p = plan.predicate;

    let mut a_scan = match dataset.lftj_join_scan(None, Some(p), None, 0, graph) {
        Some(s) => s,
        None => return Ok(results),
    };
    LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);

    while !a_scan.at_end() {
        if let Some(tok) = cancellation
            && step.is_multiple_of(4096)
            && tok.is_cancelled()
        {
                return Err(anyhow::Error::from(
                    crate::options::EvalLimitError::Cancelled,
                ));
        }
        step += 1;
        let a = a_scan.key();
        let mut b_scan = match dataset.lftj_join_scan(Some(a), Some(p), None, 2, graph) {
            Some(s) => s,
            None => {
                a_scan.advance();
                continue;
            }
        };
        LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
        while !b_scan.at_end() {
            step += 1;
            let b = b_scan.key();
            // Prefer braided multi-subject object ∩ (Ring D1) for closing edge.
            if let Some(mut c_scan) =
                dataset.lftj_multi_subject_object_intersect(&[a, b], Some(p), graph)
            {
                LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
                while !c_scan.at_end() {
                    let c = c_scan.key();
                    // Closing edge a→c and b→c both required; D1 already ∩'s
                    // objects of a and b under P. Skip a==b==c degenerate if any.
                    if c != a && c != b {
                        LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
                        let _ = emit_two_hop_solution(
                            dataset,
                            join_vars,
                            plan.a,
                            plan.b,
                            plan.c,
                            a,
                            b,
                            c,
                            &mut results,
                        );
                    }
                    c_scan.advance();
                }
            } else if let Some(mut c_scan) =
                dataset.lftj_join_scan(Some(a), Some(p), None, 2, graph)
            {
                // Last-resort: objects of a under P, probe (b,P,c) existence via
                // a second scan would be O(|Na|*|Nb|); instead materialize b's
                // objects once and filter.
                LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
                let mut b_objs: Vec<u64> = Vec::new();
                if let Some(mut bo) = dataset.lftj_join_scan(Some(b), Some(p), None, 2, graph) {
                    LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
                    while !bo.at_end() {
                        b_objs.push(bo.key());
                        bo.advance();
                    }
                }
                b_objs.sort_unstable();
                while !c_scan.at_end() {
                    let c = c_scan.key();
                    if c != a && c != b && b_objs.binary_search(&c).is_ok() {
                        LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
                        let _ = emit_two_hop_solution(
                            dataset,
                            join_vars,
                            plan.a,
                            plan.b,
                            plan.c,
                            a,
                            b,
                            c,
                            &mut results,
                        );
                    }
                    c_scan.advance();
                }
            }
            b_scan.advance();
        }
        a_scan.advance();
    }

    Ok(results)
}

/// Emit one decoded solution from four bound TermIds (3-hop chain).
#[inline]
#[allow(clippy::too_many_arguments)]
fn emit_k_chain_solution<D: Dataset>(
    dataset: &D,
    join_vars: &Arc<[Variable]>,
    a_idx: usize,
    b_idx: usize,
    c_idx: usize,
    d_idx: usize,
    a: u64,
    b: u64,
    c: u64,
    d: u64,
    results: &mut Solutions,
) {
    let mut values: Vec<Option<oxigraph_nova_core::Term>> = vec![None; join_vars.len()];
    values[a_idx] = dataset.lftj_decode_term(a);
    values[b_idx] = dataset.lftj_decode_term(b);
    values[c_idx] = dataset.lftj_decode_term(c);
    values[d_idx] = dataset.lftj_decode_term(d);
    if values[a_idx].is_none()
        || values[b_idx].is_none()
        || values[c_idx].is_none()
        || values[d_idx].is_none()
    {
        return;
    }
    results.push(Solution::positional(Arc::clone(join_vars), values));
}

/// K10 physical 3-hop chain: prepared body when available, else nested join_scan.
///
/// Prepared ops emit `[a,b,c,d]` via the multi-var slice emit API. When prepare
/// returns `None`, nested join_scan under (p1,p2,p3) still beats generic VEO.
fn eval_k_chain_walk<D: Dataset>(
    dataset: &D,
    join_vars: &Arc<[Variable]>,
    graph: &GraphSelector,
    plan: &KChainPlan,
    cancellation: Option<&CancellationToken>,
) -> anyhow::Result<Solutions> {
    K_CHAIN_SHAPE_SEEN.fetch_add(1, Ordering::Relaxed);

    let mut results: Solutions = Vec::new();
    let mut step: u64 = 0;

    if let Some(mut prepared) = dataset.lftj_prepare_shape(plan.to_physical(), graph) {
        K_CHAIN_SELECTED.fetch_add(1, Ordering::Relaxed);
        let emit_result = prepared.execute(&mut |ids| {
            debug_assert!(ids.len() >= 4, "KChain emit arity");
            let a = ids[0];
            let b = ids[1];
            let c = ids[2];
            let d = ids[3];
            if let Some(tok) = cancellation
                && step.is_multiple_of(4096)
                && tok.is_cancelled()
            {
                    return Err(());
            }
            step += 1;
            LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
            emit_k_chain_solution(
                dataset,
                join_vars,
                plan.a,
                plan.b,
                plan.c,
                plan.d,
                a,
                b,
                c,
                d,
                &mut results,
            );
            Ok(())
        });
        return match emit_result {
            Ok(_) => Ok(results),
            Err(()) => Err(anyhow::Error::from(
                crate::options::EvalLimitError::Cancelled,
            )),
        };
    }

    K_CHAIN_FALLBACK.fetch_add(1, Ordering::Relaxed);

    // Nested join_scan: a under P1 (target S) → b under (a,P1) O →
    // c under (b,P2) O → d under (c,P3) O.
    let mut a_scan = match dataset.lftj_join_scan(None, Some(plan.p1), None, 0, graph) {
        Some(s) => s,
        None => return Ok(results),
    };
    LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);

    while !a_scan.at_end() {
        if let Some(tok) = cancellation
            && step.is_multiple_of(4096)
            && tok.is_cancelled()
        {
                return Err(anyhow::Error::from(
                    crate::options::EvalLimitError::Cancelled,
                ));
        }
        step += 1;
        let a = a_scan.key();
        let mut b_scan = match dataset.lftj_join_scan(Some(a), Some(plan.p1), None, 2, graph) {
            Some(s) => s,
            None => {
                a_scan.advance();
                continue;
            }
        };
        LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
        while !b_scan.at_end() {
            step += 1;
            let b = b_scan.key();
            let mut c_scan = match dataset.lftj_join_scan(Some(b), Some(plan.p2), None, 2, graph) {
                Some(s) => s,
                None => {
                    b_scan.advance();
                    continue;
                }
            };
            LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
            while !c_scan.at_end() {
                step += 1;
                let c = c_scan.key();
                if let Some(mut d_scan) =
                    dataset.lftj_join_scan(Some(c), Some(plan.p3), None, 2, graph)
                {
                    LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
                    while !d_scan.at_end() {
                        let d = d_scan.key();
                        LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
                        emit_k_chain_solution(
                            dataset,
                            join_vars,
                            plan.a,
                            plan.b,
                            plan.c,
                            plan.d,
                            a,
                            b,
                            c,
                            d,
                            &mut results,
                        );
                        d_scan.advance();
                    }
                }
                c_scan.advance();
            }
            b_scan.advance();
        }
        a_scan.advance();
    }

    Ok(results)
}

/// Emit one decoded solution from four bound TermIds (subject-star k=3).
#[inline]
#[allow(clippy::too_many_arguments)]
fn emit_star_solution<D: Dataset>(
    dataset: &D,
    join_vars: &Arc<[Variable]>,
    s_idx: usize,
    o1_idx: usize,
    o2_idx: usize,
    o3_idx: usize,
    s: u64,
    o1: u64,
    o2: u64,
    o3: u64,
    results: &mut Solutions,
) {
    let mut values: Vec<Option<oxigraph_nova_core::Term>> = vec![None; join_vars.len()];
    values[s_idx] = dataset.lftj_decode_term(s);
    values[o1_idx] = dataset.lftj_decode_term(o1);
    values[o2_idx] = dataset.lftj_decode_term(o2);
    values[o3_idx] = dataset.lftj_decode_term(o3);
    if values[s_idx].is_none()
        || values[o1_idx].is_none()
        || values[o2_idx].is_none()
        || values[o3_idx].is_none()
    {
        return;
    }
    results.push(Solution::positional(Arc::clone(join_vars), values));
}

/// Subject-star (k=3): prepared body when available, else nested join_scan.
///
/// Prepared ops emit `[s, o1, o2, o3]` via the multi-var slice emit API. When
/// prepare returns `None`, nested join_scan under (p1,p2,p3) still beats
/// generic VEO: outer subjects under p1, then Cartesian objects under each arm.
fn eval_star_walk<D: Dataset>(
    dataset: &D,
    join_vars: &Arc<[Variable]>,
    graph: &GraphSelector,
    plan: &StarPlan,
    cancellation: Option<&CancellationToken>,
) -> anyhow::Result<Solutions> {
    STAR_SHAPE_SEEN.fetch_add(1, Ordering::Relaxed);

    let mut results: Solutions = Vec::new();
    let mut step: u64 = 0;

    if let Some(mut prepared) = dataset.lftj_prepare_shape(plan.to_physical(), graph) {
        STAR_SELECTED.fetch_add(1, Ordering::Relaxed);
        let emit_result = prepared.execute(&mut |ids| {
            debug_assert!(ids.len() >= 4, "Star emit arity");
            let s = ids[0];
            let o1 = ids[1];
            let o2 = ids[2];
            let o3 = ids[3];
            if let Some(tok) = cancellation
                && step.is_multiple_of(4096)
                && tok.is_cancelled()
            {
                    return Err(());
            }
            step += 1;
            LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
            emit_star_solution(
                dataset,
                join_vars,
                plan.s,
                plan.o1,
                plan.o2,
                plan.o3,
                s,
                o1,
                o2,
                o3,
                &mut results,
            );
            Ok(())
        });
        return match emit_result {
            Ok(_) => Ok(results),
            Err(()) => Err(anyhow::Error::from(
                crate::options::EvalLimitError::Cancelled,
            )),
        };
    }

    STAR_FALLBACK.fetch_add(1, Ordering::Relaxed);

    // Nested join_scan: subjects under P1 → objects under (s,P1), (s,P2), (s,P3).
    // Cartesian product of the three object arms for each subject that has all
    // three predicates present (empty arm → skip subject).
    let mut s_scan = match dataset.lftj_join_scan(None, Some(plan.p1), None, 0, graph) {
        Some(s) => s,
        None => return Ok(results),
    };
    LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);

    while !s_scan.at_end() {
        if let Some(tok) = cancellation
            && step.is_multiple_of(4096)
            && tok.is_cancelled()
        {
                return Err(anyhow::Error::from(
                    crate::options::EvalLimitError::Cancelled,
                ));
        }
        step += 1;
        let s = s_scan.key();

        // Materialize objects under each arm; skip if any arm is empty.
        let mut o1s: Vec<u64> = Vec::new();
        if let Some(mut sc) = dataset.lftj_join_scan(Some(s), Some(plan.p1), None, 2, graph) {
            LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
            while !sc.at_end() {
                o1s.push(sc.key());
                sc.advance();
            }
        }
        if o1s.is_empty() {
            s_scan.advance();
            continue;
        }

        let mut o2s: Vec<u64> = Vec::new();
        if let Some(mut sc) = dataset.lftj_join_scan(Some(s), Some(plan.p2), None, 2, graph) {
            LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
            while !sc.at_end() {
                o2s.push(sc.key());
                sc.advance();
            }
        }
        if o2s.is_empty() {
            s_scan.advance();
            continue;
        }

        let mut o3s: Vec<u64> = Vec::new();
        if let Some(mut sc) = dataset.lftj_join_scan(Some(s), Some(plan.p3), None, 2, graph) {
            LFTJ_SCAN_OPEN.fetch_add(1, Ordering::Relaxed);
            while !sc.at_end() {
                o3s.push(sc.key());
                sc.advance();
            }
        }
        if o3s.is_empty() {
            s_scan.advance();
            continue;
        }

        for &o1 in &o1s {
            for &o2 in &o2s {
                for &o3 in &o3s {
                    LFTJ_DEPTH_LEAF.fetch_add(1, Ordering::Relaxed);
                    emit_star_solution(
                        dataset,
                        join_vars,
                        plan.s,
                        plan.o1,
                        plan.o2,
                        plan.o3,
                        s,
                        o1,
                        o2,
                        o3,
                        &mut results,
                    );
                }
            }
        }
        s_scan.advance();
    }

    Ok(results)
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Try to evaluate a BGP using Leapfrog Triejoin with adaptive VEO.
///
/// Returns:
/// - `None` — LFTJ is not applicable; caller should fall back to nested-loop.
/// - `Some(Ok(solutions))` — LFTJ succeeded.
/// - `Some(Err(e))` — unrecoverable error (reserved for future I/O errors).
///
/// ## Fallback conditions
///
/// LFTJ is skipped and `None` is returned when:
/// - The dataset does not support LFTJ (`supports_lftj() == false`).
/// - The delta is non-empty — Ring is stale and may be missing recent writes.
/// - The graph selector is `AnyNamed` or `Union` (multi-graph iteration).
/// - A pattern contains blank nodes (not internable).
/// - A constant term in a pattern is not in the dictionary (returns empty directly).
///
/// ## K7.1 wedge fast path
///
/// When the BGP is the fixed-P undirected triangle
/// (`?a P ?b . ?b P ?c . ?a P ?c`), the shape catalog selects
/// [`ShapePlan::Wedge`] and evaluation uses a dedicated D1-walk plan
/// (outer a→b, close via `lftj_multi_subject_object_intersect`) instead of
/// generic 3-var LFTJ. In-step multi-subject collapse still applies to other
/// BGPs; counters: `triangle_shape_seen` on [`collapse_counters_snapshot`].
pub fn eval_bgp_lftj<D: Dataset>(
    dataset: &D,
    patterns: &[TriplePattern],
    active_graph: &GraphSelector,
) -> Option<anyhow::Result<Solutions>> {
    eval_bgp_lftj_cancellable(dataset, patterns, active_graph, None)
}

/// Same as [`eval_bgp_lftj`] but with an optional cancellation token checked
/// periodically in the leapfrog recursion — see `lftj_step`'s doc comment.
pub fn eval_bgp_lftj_cancellable<D: Dataset>(
    dataset: &D,
    patterns: &[TriplePattern],
    active_graph: &GraphSelector,
    cancellation: Option<&CancellationToken>,
) -> Option<anyhow::Result<Solutions>> {
    // ── Gate conditions ────────────────────────────────────────────────────────
    if !dataset.supports_lftj() {
        return None;
    }
    if dataset.lftj_has_delta() {
        return None; // stale Ring — fall back so we don't miss delta rows
    }
    match active_graph {
        GraphSelector::Default | GraphSelector::Named(_) => {}
        GraphSelector::AnyNamed => {
            return eval_bgp_lftj_multi_graph(dataset, patterns, false, cancellation);
        }
        GraphSelector::Union => {
            return eval_bgp_lftj_multi_graph(dataset, patterns, true, cancellation);
        }
        // A specific (FROM-clause-derived) set of graphs merged into "the"
        // default graph. LFTJ's single-graph-id fast path doesn't model an
        // arbitrary multi-graph merge, so fall back to nested-loop
        // evaluation, which already understands `GraphSelector::UnionOf` via
        // `graph_matches()` in dataset.rs.
        GraphSelector::UnionOf(_) => return None,
    }

    // ── Empty BGP ─────────────────────────────────────────────────────────────
    if patterns.is_empty() {
        return Some(Ok(vec![Solution::new()]));
    }

    // ── Collect join variables (assigns stable var_idx indices) ───────────────
    //
    // Wrapped in an `Arc<[Variable]>` immediately: this is the query-wide
    // header shared (via cheap `Arc::clone`) across every emitted row's
    // `Solution::positional(...)` — see `solution.rs`'s module docs.
    let join_vars: Arc<[Variable]> = Arc::from(collect_join_vars(patterns));

    // ── Classify all patterns ──────────────────────────────────────────────────
    let mut specs: Vec<PatternSpec> = Vec::with_capacity(patterns.len());
    for tp in patterns {
        match build_spec(tp, &join_vars, dataset, active_graph) {
            SpecResult::Ok(spec) => specs.push(spec),
            SpecResult::EmptyResult => return Some(Ok(vec![])),
            SpecResult::Fallback => return None,
        }
    }

    // ── Shape catalog dispatch ───────────────────────────────────────────────
    //
    // Recognizers are pure (planner-side). On a hit we always take the shape's
    // specialized walk. Walkers call `lftj_prepare_shape` with a PhysicalShape
    // (via plan constants / to_physical); when prepare returns None they keep
    // nested-scan fallbacks (still better than generic VEO on these shapes).
    // Miss → generic path below.
    if let Some(plan) = crate::shapes::recognize_shape(&specs, join_vars.len()) {
        return Some(match plan {
            ShapePlan::SpExpansion(sp_exp) => {
                eval_sp_expansion_walk(dataset, &join_vars, active_graph, &sp_exp, cancellation)
            }
            ShapePlan::TwoHop(two_hop) => eval_two_hop_walk(
                dataset,
                &join_vars,
                active_graph,
                &two_hop,
                cancellation,
                false,
            ),
            ShapePlan::Wedge(wedge) => {
                eval_wedge_walk(dataset, &join_vars, active_graph, &wedge, cancellation)
            }
            ShapePlan::KChain(k_chain) => {
                eval_k_chain_walk(dataset, &join_vars, active_graph, &k_chain, cancellation)
            }
            ShapePlan::Star(star) => {
                eval_star_walk(dataset, &join_vars, active_graph, &star, cancellation)
            }
        });
    }

    // ── Generic adaptive-VEO LFTJ ────────────────────────────────────────────
    //
    // `bindings[var_idx]` = Some(val) once that variable is bound; None until then.
    // `unbound` starts as all var indices 0..k; VEO re-sorts at each depth.
    let mut bindings: Vec<Option<u64>> = vec![None; join_vars.len()];

    let unbound: Vec<usize> = (0..join_vars.len()).collect();
    let mut results: Solutions = Vec::new();
    let mut aborted = false;

    lftj_step(
        dataset,
        &specs,
        &join_vars,
        active_graph,
        &unbound,
        &mut bindings,
        &mut results,
        cancellation,
        &mut aborted,
    );

    if aborted {
        return Some(Err(anyhow::Error::from(
            crate::options::EvalLimitError::Cancelled,
        )));
    }

    Some(Ok(results))
}

// ── Multi-graph LFTJ helper ────────────────────────────────────────────────────

/// Run LFTJ across all known named graphs (and optionally the default graph).
///
/// Called when the graph selector is `AnyNamed` or `Union`.  Iterates every
/// graph returned by `dataset.named_graphs()` and concatenates the per-graph
/// LFTJ results.
///
/// Returns `None` if LFTJ is not applicable for *any* individual graph (the
/// caller then falls through to nested-loop).
fn eval_bgp_lftj_multi_graph<D: Dataset>(
    dataset: &D,
    patterns: &[TriplePattern],
    include_default: bool,
    cancellation: Option<&CancellationToken>,
) -> Option<anyhow::Result<Solutions>> {
    if !dataset.supports_lftj() || dataset.lftj_has_delta() {
        return None;
    }
    let named_graphs: Vec<GraphName> = match dataset.named_graphs() {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(_) => return None,
    };
    let mut all_results: Solutions = Vec::new();

    if include_default {
        match eval_bgp_lftj_cancellable(dataset, patterns, &GraphSelector::Default, cancellation) {
            Some(Ok(sols)) => all_results.extend(sols),
            Some(Err(e)) => return Some(Err(e)),
            None => return None, // LFTJ not applicable for default graph → fallback
        }
    }

    for g in named_graphs {
        if matches!(g, GraphName::DefaultGraph) {
            continue;
        }
        let sel = GraphSelector::Named(g);
        match eval_bgp_lftj_cancellable(dataset, patterns, &sel, cancellation) {
            Some(Ok(sols)) => all_results.extend(sols),
            Some(Err(e)) => return Some(Err(e)),
            None => return None, // LFTJ not applicable for this graph → fallback
        }
    }

    Some(Ok(all_results))
}

// ── Unit tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::DatasetLftjSource;
    use oxigraph_nova_core::{Term, TrieIterator};

    // ── Stub dataset for unit-testing LFTJ logic ──────────────────────────────

    /// A minimal stub: two triples (a,b,c) and (a,b,d) stored as sorted ids.
    #[allow(dead_code)]
    struct StubDataset {
        triples: Vec<[u64; 3]>,
        dict: Vec<(String, u64)>,
    }

    impl StubDataset {
        #[allow(dead_code)]
        fn new() -> Self {
            Self {
                triples: vec![[1, 2, 3], [1, 2, 4]],
                dict: vec![
                    ("http://ex/a".into(), 1),
                    ("http://ex/b".into(), 2),
                    ("http://ex/c".into(), 3),
                    ("http://ex/d".into(), 4),
                ],
            }
        }

        #[allow(dead_code)]
        fn id_of(&self, uri: &str) -> Option<u64> {
            self.dict.iter().find(|(k, _)| k == uri).map(|(_, v)| *v)
        }
    }

    use crate::dataset::{GraphSelector, QuadIter, QuadPattern};
    use anyhow::Result;
    use oxigraph_nova_core::GraphName;

    impl DatasetLftjSource for StubDataset {
        fn supports_lftj(&self) -> bool {
            true
        }
        fn lftj_has_delta(&self) -> bool {
            false
        }

        fn lftj_intern_term(&self, term: &Term, _: &GraphSelector) -> Option<u64> {
            if let Term::NamedNode(n) = term {
                self.id_of(n.as_str())
            } else {
                None
            }
        }

        fn lftj_decode_term(&self, id: u64) -> Option<Term> {
            self.dict.iter().find(|(_, v)| *v == id).map(|(k, _)| {
                Term::NamedNode(oxigraph_nova_core::NamedNode::new_unchecked(k.clone()))
            })
        }

        fn lftj_join_scan(
            &self,
            s: Option<u64>,
            _p: Option<u64>,
            _o: Option<u64>,
            target_field: usize,
            _: &GraphSelector,
        ) -> Option<Box<dyn TrieIterator>> {
            let mut vals: Vec<u64> = self
                .triples
                .iter()
                .filter(|t| s.is_none_or(|sv| t[0] == sv))
                .map(|t| t[target_field])
                .collect();
            vals.sort_unstable();
            vals.dedup();
            Some(Box::new(VecTrieIter { vals, pos: 0 }))
        }
        // lftj_estimate_count and lftj_real_count both use their default
        // (u64::MAX / None) — VEO sort is stable for equal estimates, so
        // first-appearance order is preserved in tests.
    }

    impl Dataset for StubDataset {
        fn find_quads<'a>(&'a self, _: &QuadPattern) -> Result<QuadIter<'a>> {
            Ok(Box::new(std::iter::empty()))
        }
        fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
            Ok(Box::new(std::iter::empty()))
        }
    }

    struct VecTrieIter {
        vals: Vec<u64>,
        pos: usize,
    }

    impl TrieIterator for VecTrieIter {
        fn key(&self) -> u64 {
            self.vals[self.pos]
        }
        fn seek(&mut self, target: u64) {
            while self.pos < self.vals.len() && self.vals[self.pos] < target {
                self.pos += 1;
            }
        }
        fn open(&self) -> Box<dyn TrieIterator> {
            Box::new(VecTrieIter {
                vals: vec![],
                pos: 0,
            })
        }
        fn at_end(&self) -> bool {
            self.pos >= self.vals.len()
        }
    }

    #[test]
    fn leapfrog_sync_basic() {
        let mut scans: Vec<Box<dyn TrieIterator>> = vec![
            Box::new(VecTrieIter {
                vals: vec![1, 3, 5, 7],
                pos: 0,
            }),
            Box::new(VecTrieIter {
                vals: vec![2, 3, 6, 7],
                pos: 0,
            }),
            Box::new(VecTrieIter {
                vals: vec![3, 4, 7, 9],
                pos: 0,
            }),
        ];
        assert_eq!(leapfrog_sync(&mut scans), Some(3));
    }

    #[test]
    fn leapfrog_sync_exhausted() {
        let mut scans: Vec<Box<dyn TrieIterator>> = vec![
            Box::new(VecTrieIter {
                vals: vec![1, 2],
                pos: 0,
            }),
            Box::new(VecTrieIter {
                vals: vec![3, 4],
                pos: 0,
            }),
        ];
        assert_eq!(leapfrog_sync(&mut scans), None);
    }

    #[test]
    fn leapfrog_sync_empty_scan() {
        let mut scans: Vec<Box<dyn TrieIterator>> = vec![Box::new(VecTrieIter {
            vals: vec![],
            pos: 0,
        })];
        assert_eq!(leapfrog_sync(&mut scans), None);
    }

    /// Verify VEO sort is stable (preserves first-appearance) for equal estimates.
    ///
    /// `StubDataset` doesn't override `lftj_real_count` (inherits the trait
    /// default `None`), so `veo_sort` falls back to `lftj_estimate_count`,
    /// which also defaults to `u64::MAX` — all estimates equal, so `veo_sort`
    /// must not reorder the input.
    #[test]
    fn veo_sort_stable_equal_estimates() {
        let ds = StubDataset::new();
        let graph = GraphSelector::Default;
        // Two simple one-field specs: var 0 = S field, var 1 = O field.
        let specs = vec![
            PatternSpec {
                s: FieldSpec::JoinVar(0),
                p: FieldSpec::Const(2),
                o: FieldSpec::Const(3),
            },
            PatternSpec {
                s: FieldSpec::Const(1),
                p: FieldSpec::Const(2),
                o: FieldSpec::JoinVar(1),
            },
        ];
        let bindings = vec![None, None];
        let unbound = vec![0usize, 1usize];
        let order = veo_sort(&unbound, &specs, &bindings, &ds, &graph);
        // All estimates are u64::MAX (equal) → stable → original order preserved.
        assert_eq!(order, vec![0, 1]);
    }

    /// Verify VEO sort prefers the variable with the smaller **real
    /// bound-context subtree size** (via `Dataset::lftj_real_count`), not just
    /// a static estimate — demonstrating the actual behavioural effect of the
    /// real-subtree-size logic (not merely a plumbing change).
    ///
    /// `RemainingCountDataset` returns a real count that differs per target
    /// field: var 0 (S field) has a large remaining count (10), var 1 (O
    /// field) has a small one (2). VEO must pick var 1 first regardless of
    /// first-appearance order.
    #[test]
    fn veo_sort_prefers_smaller_real_subtree_size() {
        struct RemainingCountDataset;
        impl DatasetLftjSource for RemainingCountDataset {
            fn supports_lftj(&self) -> bool {
                true
            }
            fn lftj_has_delta(&self) -> bool {
                false
            }
            fn lftj_intern_term(&self, _: &Term, _: &GraphSelector) -> Option<u64> {
                None
            }
            fn lftj_decode_term(&self, _: u64) -> Option<Term> {
                None
            }
            fn lftj_real_count(
                &self,
                _s: Option<u64>,
                _p: Option<u64>,
                _o: Option<u64>,
                target_field: usize,
                _: &GraphSelector,
            ) -> Option<u64> {
                // target_field 0 (var 0's S field) → large remaining count.
                // target_field 2 (var 1's O field) → small remaining count.
                if target_field == 0 { Some(10) } else { Some(2) }
            }
        }
        impl Dataset for RemainingCountDataset {
            fn find_quads<'a>(&'a self, _: &QuadPattern) -> Result<QuadIter<'a>> {
                Ok(Box::new(std::iter::empty()))
            }
            fn named_graphs<'a>(
                &'a self,
            ) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
                Ok(Box::new(std::iter::empty()))
            }
        }

        let ds = RemainingCountDataset;
        let graph = GraphSelector::Default;
        // var 0 appears first (S field, large subtree), var 1 second (O field,
        // small subtree) — first-appearance order would pick var 0 first, but
        // real subtree size must override that and pick var 1 first.
        let specs = vec![
            PatternSpec {
                s: FieldSpec::JoinVar(0),
                p: FieldSpec::Const(2),
                o: FieldSpec::Const(3),
            },
            PatternSpec {
                s: FieldSpec::Const(1),
                p: FieldSpec::Const(2),
                o: FieldSpec::JoinVar(1),
            },
        ];
        let bindings = vec![None, None];
        let unbound = vec![0usize, 1usize];
        let order = veo_sort(&unbound, &specs, &bindings, &ds, &graph);
        assert_eq!(
            order,
            vec![1, 0],
            "VEO must prefer var 1 (real subtree size 2) over var 0 (real subtree size 10)"
        );
    }

    /// Regression test: a fully-ground triple pattern (a BGP
    /// with zero join variables -- every field a constant) must be verified
    /// against the store, not unconditionally treated as satisfied.
    #[test]
    fn fully_ground_pattern_checks_existence_not_just_term_interning() {
        /// A dataset whose constants are all individually "internable" (as
        /// long as they appear in `KNOWN_TERMS`), but whose `contains_quad`
        /// only returns `true` for one specific asserted triple -- this
        /// isolates whether `eval_bgp_lftj` actually checks existence for a
        /// fully-ground pattern, or merely checks that each term individually
        /// exists somewhere in the dictionary.
        struct ExistenceCheckDataset;

        const KNOWN_TERMS: &[&str] = &[
            "http://ex/a",
            "http://ex/p",
            "http://ex/q",
            "http://ex/o1",
            "http://ex/o2",
        ];

        fn term_id(uri: &str) -> Option<u64> {
            KNOWN_TERMS
                .iter()
                .position(|t| *t == uri)
                .map(|i| i as u64 + 1)
        }

        impl DatasetLftjSource for ExistenceCheckDataset {
            fn supports_lftj(&self) -> bool {
                true
            }
            fn lftj_has_delta(&self) -> bool {
                false
            }
            fn lftj_intern_term(&self, term: &Term, _: &GraphSelector) -> Option<u64> {
                if let Term::NamedNode(n) = term {
                    term_id(n.as_str())
                } else {
                    None
                }
            }
            fn lftj_decode_term(&self, _id: u64) -> Option<Term> {
                None
            }
        }

        impl Dataset for ExistenceCheckDataset {
            fn find_quads<'a>(&'a self, _: &QuadPattern) -> Result<QuadIter<'a>> {
                Ok(Box::new(std::iter::empty()))
            }
            fn named_graphs<'a>(
                &'a self,
            ) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
                Ok(Box::new(std::iter::empty()))
            }
            // Only `<http://ex/a> <http://ex/p> <http://ex/o1>` is actually
            // asserted -- every other combination of KNOWN_TERMS is not,
            // even though each individual term is internable.
            fn contains_quad(&self, s: &Term, p: &Term, o: &Term, g: &GraphName) -> Result<bool> {
                if !matches!(g, GraphName::DefaultGraph) {
                    return Ok(false);
                }
                let is = |t: &Term, uri: &str| matches!(t, Term::NamedNode(n) if n.as_str() == uri);
                Ok(is(s, "http://ex/a") && is(p, "http://ex/p") && is(o, "http://ex/o1"))
            }
        }

        let ds = ExistenceCheckDataset;

        let pattern = |o: &str| TriplePattern {
            subject: TermPattern::NamedNode(oxigraph_nova_core::NamedNode::new_unchecked(
                "http://ex/a",
            )),
            predicate: NamedNodePattern::NamedNode(oxigraph_nova_core::NamedNode::new_unchecked(
                "http://ex/p",
            )),
            object: TermPattern::NamedNode(oxigraph_nova_core::NamedNode::new_unchecked(o)),
        };

        // The asserted triple: must be found (one solution).
        let asserted = eval_bgp_lftj(&ds, &[pattern("http://ex/o1")], &GraphSelector::Default)
            .expect("LFTJ should handle a fully-ground pattern, not fall back")
            .expect("no error expected");
        assert_eq!(
            asserted.len(),
            1,
            "the asserted fully-ground triple must yield exactly one solution"
        );

        // Same subject/predicate, but a *different* (also-internable) object
        // that was never asserted together with them -- must be empty, not a
        // spurious match.
        let not_asserted = eval_bgp_lftj(&ds, &[pattern("http://ex/o2")], &GraphSelector::Default)
            .expect("LFTJ should handle a fully-ground pattern, not fall back")
            .expect("no error expected");
        assert!(
            not_asserted.is_empty(),
            "a fully-ground triple that was never asserted must yield zero solutions"
        );
    }

    // ── KChain (k=3) execution tests ──────────────────────────────────────────
    //
    // Recognizer unit tests live in `shapes/`. These exercise the full path:
    // classify BGP → catalog hit → `eval_k_chain_walk` nested join_scan →
    // positional emit. The stub is predicate-aware (unlike `StubDataset`).

    /// Predicate-aware triple store for shape walk execution tests.
    struct ChainDataset {
        /// Sorted (s, p, o) triples.
        triples: Vec<[u64; 3]>,
        dict: Vec<(String, u64)>,
    }

    impl ChainDataset {
        /// Data:
        ///   a --p1--> b --p2--> c --p3--> d     (one full 3-hop chain)
        ///   a --p1--> b2                        (dead-end under p1)
        ///   x --p9--> y                         (unrelated edge, different P)
        fn with_chain() -> Self {
            Self {
                triples: vec![
                    [1, 10, 2], // a p1 b
                    [1, 10, 5], // a p1 b2 (dead-end)
                    [2, 11, 3], // b p2 c
                    [3, 12, 4], // c p3 d
                    [6, 99, 7], // x p9 y (noise)
                ],
                dict: vec![
                    ("http://ex/a".into(), 1),
                    ("http://ex/b".into(), 2),
                    ("http://ex/c".into(), 3),
                    ("http://ex/d".into(), 4),
                    ("http://ex/b2".into(), 5),
                    ("http://ex/x".into(), 6),
                    ("http://ex/y".into(), 7),
                    ("http://ex/p1".into(), 10),
                    ("http://ex/p2".into(), 11),
                    ("http://ex/p3".into(), 12),
                    ("http://ex/p9".into(), 99),
                ],
            }
        }

        fn id_of(&self, uri: &str) -> Option<u64> {
            self.dict.iter().find(|(k, _)| k == uri).map(|(_, v)| *v)
        }
    }

    impl DatasetLftjSource for ChainDataset {
        fn supports_lftj(&self) -> bool {
            true
        }
        fn lftj_has_delta(&self) -> bool {
            false
        }
        fn lftj_intern_term(&self, term: &Term, _: &GraphSelector) -> Option<u64> {
            if let Term::NamedNode(n) = term {
                self.id_of(n.as_str())
            } else {
                None
            }
        }
        fn lftj_decode_term(&self, id: u64) -> Option<Term> {
            self.dict.iter().find(|(_, v)| *v == id).map(|(k, _)| {
                Term::NamedNode(oxigraph_nova_core::NamedNode::new_unchecked(k.clone()))
            })
        }
        fn lftj_join_scan(
            &self,
            s: Option<u64>,
            p: Option<u64>,
            o: Option<u64>,
            target_field: usize,
            _: &GraphSelector,
        ) -> Option<Box<dyn TrieIterator>> {
            let mut vals: Vec<u64> = self
                .triples
                .iter()
                .filter(|t| s.is_none_or(|sv| t[0] == sv))
                .filter(|t| p.is_none_or(|pv| t[1] == pv))
                .filter(|t| o.is_none_or(|ov| t[2] == ov))
                .map(|t| t[target_field])
                .collect();
            vals.sort_unstable();
            vals.dedup();
            Some(Box::new(VecTrieIter { vals, pos: 0 }))
        }
    }

    impl Dataset for ChainDataset {
        fn find_quads<'a>(&'a self, _: &QuadPattern) -> Result<QuadIter<'a>> {
            Ok(Box::new(std::iter::empty()))
        }
        fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
            Ok(Box::new(std::iter::empty()))
        }
    }

    fn var(name: &str) -> Variable {
        Variable::new_unchecked(name)
    }

    fn tp(s: TermPattern, p: NamedNodePattern, o: TermPattern) -> TriplePattern {
        TriplePattern {
            subject: s,
            predicate: p,
            object: o,
        }
    }

    fn jv(name: &str) -> TermPattern {
        TermPattern::Variable(var(name))
    }

    fn nn_const(uri: &str) -> NamedNodePattern {
        NamedNodePattern::NamedNode(oxigraph_nova_core::NamedNode::new_unchecked(uri))
    }

    fn uri_of(t: &Term) -> &str {
        match t {
            Term::NamedNode(n) => n.as_str(),
            _ => panic!("expected NamedNode"),
        }
    }

    /// `?a p1 ?b . ?b p2 ?c . ?c p3 ?d` must emit exactly one chain solution
    /// and bump the KChain SEEN/FALLBACK counters (`ChainDataset` has no prepare).
    ///
    /// Counter checks use before/after *positive deltas* only: process-wide
    /// atomics race under `cargo test` parallelism, so equality on the
    /// complementary counter (SELECTED) is not reliable here.
    #[test]
    fn k_chain_walk_emits_single_path() {
        let ds = ChainDataset::with_chain();
        let patterns = [
            tp(jv("a"), nn_const("http://ex/p1"), jv("b")),
            tp(jv("b"), nn_const("http://ex/p2"), jv("c")),
            tp(jv("c"), nn_const("http://ex/p3"), jv("d")),
        ];
        let before = collapse_counters_snapshot();
        let sols = eval_bgp_lftj(&ds, &patterns, &GraphSelector::Default)
            .expect("LFTJ applicable")
            .expect("no error");
        assert_eq!(sols.len(), 1, "exactly one 3-hop path a→b→c→d");
        let s = &sols[0];
        assert_eq!(uri_of(s.get(&var("a")).unwrap()), "http://ex/a");
        assert_eq!(uri_of(s.get(&var("b")).unwrap()), "http://ex/b");
        assert_eq!(uri_of(s.get(&var("c")).unwrap()), "http://ex/c");
        assert_eq!(uri_of(s.get(&var("d")).unwrap()), "http://ex/d");

        let after = collapse_counters_snapshot();
        assert!(
            after.k_chain_shape_seen > before.k_chain_shape_seen,
            "walker must bump SEEN"
        );
        assert!(
            after.k_chain_fallback > before.k_chain_fallback,
            "ChainDataset has no prepare → nested FALLBACK"
        );
    }

    /// Pattern order hop3, hop1, hop2 still orients a→b→c→d correctly.
    #[test]
    fn k_chain_walk_permuted_pattern_order() {
        let ds = ChainDataset::with_chain();
        let patterns = [
            tp(jv("c"), nn_const("http://ex/p3"), jv("d")),
            tp(jv("a"), nn_const("http://ex/p1"), jv("b")),
            tp(jv("b"), nn_const("http://ex/p2"), jv("c")),
        ];
        let before = collapse_counters_snapshot().k_chain_shape_seen;
        let sols = eval_bgp_lftj(&ds, &patterns, &GraphSelector::Default)
            .expect("LFTJ applicable")
            .expect("no error");
        assert_eq!(sols.len(), 1);
        let s = &sols[0];
        assert_eq!(uri_of(s.get(&var("a")).unwrap()), "http://ex/a");
        assert_eq!(uri_of(s.get(&var("b")).unwrap()), "http://ex/b");
        assert_eq!(uri_of(s.get(&var("c")).unwrap()), "http://ex/c");
        assert_eq!(uri_of(s.get(&var("d")).unwrap()), "http://ex/d");
        assert!(collapse_counters_snapshot().k_chain_shape_seen > before);
    }

    /// Dead-end under p1 (a→b2) must not produce a spurious solution; empty
    /// when no full path exists for the requested predicates.
    #[test]
    fn k_chain_walk_no_match_returns_empty() {
        let ds = ChainDataset::with_chain();
        // p9 has no 3-hop continuation.
        let patterns = [
            tp(jv("a"), nn_const("http://ex/p9"), jv("b")),
            tp(jv("b"), nn_const("http://ex/p2"), jv("c")),
            tp(jv("c"), nn_const("http://ex/p3"), jv("d")),
        ];
        let before = collapse_counters_snapshot().k_chain_shape_seen;
        let sols = eval_bgp_lftj(&ds, &patterns, &GraphSelector::Default)
            .expect("LFTJ applicable (shape still recognized)")
            .expect("no error");
        assert!(
            sols.is_empty(),
            "no complete 3-hop under p9/p2/p3 — got {} sols",
            sols.len()
        );
        // Shape is still recognized (catalog hit) even when the walk yields 0.
        assert!(collapse_counters_snapshot().k_chain_shape_seen > before);
    }

    /// Subject-star BGP on the chain fixture must hit the Star walker (not
    /// KChain) and yield empty — no subject fans out under p1/p2/p3.
    #[test]
    fn k_chain_star_bgp_yields_empty_not_chain() {
        let ds = ChainDataset::with_chain();
        // ?s p1 ?a . ?s p2 ?b . ?s p3 ?c  — star, not chain
        let patterns = [
            tp(jv("s"), nn_const("http://ex/p1"), jv("a")),
            tp(jv("s"), nn_const("http://ex/p2"), jv("b")),
            tp(jv("s"), nn_const("http://ex/p3"), jv("c")),
        ];
        let before = collapse_counters_snapshot();
        let sols = eval_bgp_lftj(&ds, &patterns, &GraphSelector::Default)
            .expect("LFTJ applicable (Star shape)")
            .expect("no error");
        assert!(
            sols.is_empty(),
            "star on chain fixture must be empty (got {})",
            sols.len()
        );
        let after = collapse_counters_snapshot();
        assert!(
            after.star_shape_seen > before.star_shape_seen,
            "must take Star walker, not KChain"
        );
    }

    /// Prepared multi-var emit path: SELECTED↑ and the same chain row.
    ///
    /// Only asserts a positive SELECTED delta (not FALLBACK equality): other
    /// k_chain tests running in parallel may bump FALLBACK on the shared
    /// process-wide atomics.
    #[test]
    fn k_chain_selected_prepared_body() {
        /// Wraps [`ChainDataset`] and returns a canned KChain prepared op.
        struct PreparedChainDataset {
            inner: ChainDataset,
        }

        struct FakeKChainOp {
            row: [u64; 4],
        }

        impl oxigraph_nova_core::PreparedPhysicalOperator for FakeKChainOp {
            fn execute(
                &mut self,
                emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>,
            ) -> Result<u64, ()> {
                emit(&self.row)?;
                Ok(1)
            }
        }

        impl DatasetLftjSource for PreparedChainDataset {
            fn supports_lftj(&self) -> bool {
                true
            }
            fn lftj_has_delta(&self) -> bool {
                false
            }
            fn lftj_intern_term(&self, term: &Term, g: &GraphSelector) -> Option<u64> {
                self.inner.lftj_intern_term(term, g)
            }
            fn lftj_decode_term(&self, id: u64) -> Option<Term> {
                self.inner.lftj_decode_term(id)
            }
            fn lftj_join_scan(
                &self,
                s: Option<u64>,
                p: Option<u64>,
                o: Option<u64>,
                target_field: usize,
                g: &GraphSelector,
            ) -> Option<Box<dyn TrieIterator>> {
                self.inner.lftj_join_scan(s, p, o, target_field, g)
            }
            fn lftj_prepare_shape(
                &self,
                shape: oxigraph_nova_core::PhysicalShape,
                _: &GraphSelector,
            ) -> Option<Box<dyn oxigraph_nova_core::PreparedPhysicalOperator>> {
                match shape {
                    oxigraph_nova_core::PhysicalShape::KChain { .. } => {
                        // a=1, b=2, c=3, d=4 — same as ChainDataset fixture path
                        Some(Box::new(FakeKChainOp { row: [1, 2, 3, 4] }))
                    }
                    _ => None,
                }
            }
        }

        impl Dataset for PreparedChainDataset {
            fn find_quads<'a>(&'a self, p: &QuadPattern) -> Result<QuadIter<'a>> {
                self.inner.find_quads(p)
            }
            fn named_graphs<'a>(
                &'a self,
            ) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
                self.inner.named_graphs()
            }
        }

        let ds = PreparedChainDataset {
            inner: ChainDataset::with_chain(),
        };
        let patterns = [
            tp(jv("a"), nn_const("http://ex/p1"), jv("b")),
            tp(jv("b"), nn_const("http://ex/p2"), jv("c")),
            tp(jv("c"), nn_const("http://ex/p3"), jv("d")),
        ];
        let before = collapse_counters_snapshot();
        let sols = eval_bgp_lftj(&ds, &patterns, &GraphSelector::Default)
            .expect("LFTJ applicable")
            .expect("no error");
        assert_eq!(sols.len(), 1);
        let s = &sols[0];
        assert_eq!(uri_of(s.get(&var("a")).unwrap()), "http://ex/a");
        assert_eq!(uri_of(s.get(&var("b")).unwrap()), "http://ex/b");
        assert_eq!(uri_of(s.get(&var("c")).unwrap()), "http://ex/c");
        assert_eq!(uri_of(s.get(&var("d")).unwrap()), "http://ex/d");

        let after = collapse_counters_snapshot();
        assert!(after.k_chain_shape_seen > before.k_chain_shape_seen);
        assert!(
            after.k_chain_selected > before.k_chain_selected,
            "prepared body must bump SELECTED"
        );
    }

    // ── Star (k=3) execution tests ────────────────────────────────────────────

    /// Subject-star fixture for Star walk tests.
    struct StarDataset {
        triples: Vec<[u64; 3]>,
        dict: Vec<(String, u64)>,
    }

    impl StarDataset {
        /// Data:
        ///   s --p1--> o1, s --p2--> o2, s --p3--> o3   (one full star)
        ///   s --p1--> o1b                               (extra arm under p1 → 2 sols)
        ///   t --p1--> o1                                (incomplete star, missing p2/p3)
        ///   x --p9--> y                                 (noise)
        fn with_star() -> Self {
            Self {
                triples: vec![
                    [1, 10, 2], // s p1 o1
                    [1, 10, 5], // s p1 o1b
                    [1, 11, 3], // s p2 o2
                    [1, 12, 4], // s p3 o3
                    [6, 10, 2], // t p1 o1 (incomplete)
                    [7, 99, 8], // x p9 y
                ],
                dict: vec![
                    ("http://ex/s".into(), 1),
                    ("http://ex/o1".into(), 2),
                    ("http://ex/o2".into(), 3),
                    ("http://ex/o3".into(), 4),
                    ("http://ex/o1b".into(), 5),
                    ("http://ex/t".into(), 6),
                    ("http://ex/x".into(), 7),
                    ("http://ex/y".into(), 8),
                    ("http://ex/p1".into(), 10),
                    ("http://ex/p2".into(), 11),
                    ("http://ex/p3".into(), 12),
                    ("http://ex/p9".into(), 99),
                ],
            }
        }

        fn id_of(&self, uri: &str) -> Option<u64> {
            self.dict.iter().find(|(k, _)| k == uri).map(|(_, v)| *v)
        }
    }

    impl DatasetLftjSource for StarDataset {
        fn supports_lftj(&self) -> bool {
            true
        }
        fn lftj_has_delta(&self) -> bool {
            false
        }
        fn lftj_intern_term(&self, term: &Term, _: &GraphSelector) -> Option<u64> {
            if let Term::NamedNode(n) = term {
                self.id_of(n.as_str())
            } else {
                None
            }
        }
        fn lftj_decode_term(&self, id: u64) -> Option<Term> {
            self.dict.iter().find(|(_, v)| *v == id).map(|(k, _)| {
                Term::NamedNode(oxigraph_nova_core::NamedNode::new_unchecked(k.clone()))
            })
        }
        fn lftj_join_scan(
            &self,
            s: Option<u64>,
            p: Option<u64>,
            o: Option<u64>,
            target_field: usize,
            _: &GraphSelector,
        ) -> Option<Box<dyn TrieIterator>> {
            let mut vals: Vec<u64> = self
                .triples
                .iter()
                .filter(|t| s.is_none_or(|sv| t[0] == sv))
                .filter(|t| p.is_none_or(|pv| t[1] == pv))
                .filter(|t| o.is_none_or(|ov| t[2] == ov))
                .map(|t| t[target_field])
                .collect();
            vals.sort_unstable();
            vals.dedup();
            Some(Box::new(VecTrieIter { vals, pos: 0 }))
        }
    }

    impl Dataset for StarDataset {
        fn find_quads<'a>(&'a self, _: &QuadPattern) -> Result<QuadIter<'a>> {
            Ok(Box::new(std::iter::empty()))
        }
        fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
            Ok(Box::new(std::iter::empty()))
        }
    }

    /// `?s p1 ?o1 . ?s p2 ?o2 . ?s p3 ?o3` emits two solutions (o1 and o1b)
    /// and bumps Star SEEN/FALLBACK.
    #[test]
    fn star_walk_emits_cartesian_arms() {
        let ds = StarDataset::with_star();
        let patterns = [
            tp(jv("s"), nn_const("http://ex/p1"), jv("o1")),
            tp(jv("s"), nn_const("http://ex/p2"), jv("o2")),
            tp(jv("s"), nn_const("http://ex/p3"), jv("o3")),
        ];
        let before = collapse_counters_snapshot();
        let sols = eval_bgp_lftj(&ds, &patterns, &GraphSelector::Default)
            .expect("LFTJ applicable")
            .expect("no error");
        assert_eq!(sols.len(), 2, "two objects under p1 × one each under p2/p3");
        // Both solutions share s/o2/o3; o1 differs.
        for sol in &sols {
            assert_eq!(uri_of(sol.get(&var("s")).unwrap()), "http://ex/s");
            assert_eq!(uri_of(sol.get(&var("o2")).unwrap()), "http://ex/o2");
            assert_eq!(uri_of(sol.get(&var("o3")).unwrap()), "http://ex/o3");
        }
        let o1s: Vec<&str> = sols
            .iter()
            .map(|s| uri_of(s.get(&var("o1")).unwrap()))
            .collect();
        assert!(o1s.contains(&"http://ex/o1"));
        assert!(o1s.contains(&"http://ex/o1b"));

        let after = collapse_counters_snapshot();
        assert!(after.star_shape_seen > before.star_shape_seen);
        assert!(
            after.star_fallback > before.star_fallback,
            "StarDataset has no prepare → nested FALLBACK"
        );
    }

    /// Pattern order arm3, arm1, arm2 still binds correctly.
    #[test]
    fn star_walk_permuted_pattern_order() {
        let ds = StarDataset::with_star();
        let patterns = [
            tp(jv("s"), nn_const("http://ex/p3"), jv("o3")),
            tp(jv("s"), nn_const("http://ex/p1"), jv("o1")),
            tp(jv("s"), nn_const("http://ex/p2"), jv("o2")),
        ];
        let before = collapse_counters_snapshot().star_shape_seen;
        let sols = eval_bgp_lftj(&ds, &patterns, &GraphSelector::Default)
            .expect("LFTJ applicable")
            .expect("no error");
        assert_eq!(sols.len(), 2);
        assert!(collapse_counters_snapshot().star_shape_seen > before);
    }

    /// Incomplete subjects (missing an arm) must not emit.
    #[test]
    fn star_walk_incomplete_subject_skipped() {
        let ds = StarDataset::with_star();
        // Query under p9/p2/p3 — no subject has all three.
        let patterns = [
            tp(jv("s"), nn_const("http://ex/p9"), jv("o1")),
            tp(jv("s"), nn_const("http://ex/p2"), jv("o2")),
            tp(jv("s"), nn_const("http://ex/p3"), jv("o3")),
        ];
        let before = collapse_counters_snapshot().star_shape_seen;
        let sols = eval_bgp_lftj(&ds, &patterns, &GraphSelector::Default)
            .expect("LFTJ applicable")
            .expect("no error");
        assert!(sols.is_empty());
        assert!(collapse_counters_snapshot().star_shape_seen > before);
    }

    /// Prepared multi-var emit path: SELECTED↑ and one canned star row.
    #[test]
    fn star_selected_prepared_body() {
        struct PreparedStarDataset {
            inner: StarDataset,
        }

        struct FakeStarOp {
            row: [u64; 4],
        }

        impl oxigraph_nova_core::PreparedPhysicalOperator for FakeStarOp {
            fn execute(
                &mut self,
                emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>,
            ) -> Result<u64, ()> {
                emit(&self.row)?;
                Ok(1)
            }
        }

        impl DatasetLftjSource for PreparedStarDataset {
            fn supports_lftj(&self) -> bool {
                true
            }
            fn lftj_has_delta(&self) -> bool {
                false
            }
            fn lftj_intern_term(&self, term: &Term, g: &GraphSelector) -> Option<u64> {
                self.inner.lftj_intern_term(term, g)
            }
            fn lftj_decode_term(&self, id: u64) -> Option<Term> {
                self.inner.lftj_decode_term(id)
            }
            fn lftj_join_scan(
                &self,
                s: Option<u64>,
                p: Option<u64>,
                o: Option<u64>,
                target_field: usize,
                g: &GraphSelector,
            ) -> Option<Box<dyn TrieIterator>> {
                self.inner.lftj_join_scan(s, p, o, target_field, g)
            }
            fn lftj_prepare_shape(
                &self,
                shape: oxigraph_nova_core::PhysicalShape,
                _: &GraphSelector,
            ) -> Option<Box<dyn oxigraph_nova_core::PreparedPhysicalOperator>> {
                match shape {
                    oxigraph_nova_core::PhysicalShape::Star { .. } => {
                        // s=1, o1=2, o2=3, o3=4
                        Some(Box::new(FakeStarOp { row: [1, 2, 3, 4] }))
                    }
                    _ => None,
                }
            }
        }

        impl Dataset for PreparedStarDataset {
            fn find_quads<'a>(&'a self, p: &QuadPattern) -> Result<QuadIter<'a>> {
                self.inner.find_quads(p)
            }
            fn named_graphs<'a>(
                &'a self,
            ) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
                self.inner.named_graphs()
            }
        }

        let ds = PreparedStarDataset {
            inner: StarDataset::with_star(),
        };
        let patterns = [
            tp(jv("s"), nn_const("http://ex/p1"), jv("o1")),
            tp(jv("s"), nn_const("http://ex/p2"), jv("o2")),
            tp(jv("s"), nn_const("http://ex/p3"), jv("o3")),
        ];
        let before = collapse_counters_snapshot();
        let sols = eval_bgp_lftj(&ds, &patterns, &GraphSelector::Default)
            .expect("LFTJ applicable")
            .expect("no error");
        assert_eq!(sols.len(), 1);
        let s = &sols[0];
        assert_eq!(uri_of(s.get(&var("s")).unwrap()), "http://ex/s");
        assert_eq!(uri_of(s.get(&var("o1")).unwrap()), "http://ex/o1");
        assert_eq!(uri_of(s.get(&var("o2")).unwrap()), "http://ex/o2");
        assert_eq!(uri_of(s.get(&var("o3")).unwrap()), "http://ex/o3");

        let after = collapse_counters_snapshot();
        assert!(after.star_shape_seen > before.star_shape_seen);
        assert!(
            after.star_selected > before.star_selected,
            "prepared body must bump SELECTED"
        );
    }
}
