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
use crate::solution::{Solution, Solutions};
use oxigraph_nova_core::{GraphName, Variable};
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

// ── Field specification ────────────────────────────────────────────────────────

/// How a single s/p/o field in a triple pattern is classified for LFTJ.
#[derive(Clone, Debug)]
enum FieldSpec {
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
struct PatternSpec {
    s: FieldSpec,
    p: FieldSpec,
    o: FieldSpec,
}

impl PatternSpec {
    /// Is this pattern active for `var_idx`?
    ///
    /// A pattern is active for `j` if exactly one of its fields is `JoinVar(j)`.
    fn is_active_for_var(&self, var_idx: usize) -> bool {
        let is_jv = |f: &FieldSpec| matches!(f, FieldSpec::JoinVar(i) if *i == var_idx);
        is_jv(&self.s) || is_jv(&self.p) || is_jv(&self.o)
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

    SpecResult::Ok(PatternSpec { s, p, o })
}

// ── Adaptive VEO sort ─────────────────────────────────────────────────────────

/// Sort `unbound` (var indices into `join_vars`) ascending by the minimum
/// **real bound-context subtree size** across all patterns that contain each
/// variable — CLTJ*'s adaptive VEO (§3.5).
///
/// For each candidate variable/pattern this calls
/// [`Dataset::lftj_real_count`], a zero-allocation probe that performs the
/// same LOUDS navigation as opening a real scan but never constructs a
/// `Box<dyn TrieIterator>`. This is the *actual* leaf-descendant count under
/// the current binding, matching the C++ reference's `subtree_size_fixed1/2`,
/// not a static dataset-wide vocabulary-size proxy.
///
/// Falls back to `dataset.lftj_estimate_count(...)` when `lftj_real_count`
/// returns `None` (non-CLTJ backends, or `AnyNamed`/`Union` graph selectors).
/// For non-CLTJ backends all estimates are `u64::MAX`, so the sort is stable
/// (preserves first-appearance order).
///
/// Sorting 3–6 elements is O(1) — the overhead is negligible.
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
                weight = weight.min(w);
            }
            Candidate {
                var_idx: vi,
                weight,
            }
        })
        .collect();

    candidates.sort_by_key(|c| c.weight);

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

    // ── Obtain one scan per active pattern ───────────────────────────────────
    let mut scans: Vec<Box<dyn oxigraph_nova_core::TrieIterator>> =
        Vec::with_capacity(active.len());
    for sp in &active {
        let (sv, pv, ov, target_field) = sp.resolve_for_var(var_idx, bindings);
        match dataset.lftj_join_scan(sv, pv, ov, target_field, graph) {
            Some(scan) => scans.push(scan),
            None => return, // scan not available → caller will use fallback
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

    // ── Run LFTJ with adaptive VEO ─────────────────────────────────────────
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
}
