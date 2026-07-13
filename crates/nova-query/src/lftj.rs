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
//!
//! ## Parallel root-level dispatch
//!
//! References: Wu & Suciu, "HoneyComb: A Parallel Worst-Case Optimal Join"
//! (PODS 2025, `/research/2502.06715v1.pdf`); Wei, Liu, Lin, "PJDL:
//! Parallelizing Leapfrog Triejoin via Incremental Trie Construction and
//! Dynamic Load Balancing" (DASFAA 2026, `/research/2026_DASFAA.pdf`).
//!
//! HoneyComb's own approach — hash-partitioning *every* query variable's
//! domain and physically reorganizing each relation into per-partition
//! contiguous arrays before joining — is deliberately not adopted here: it
//! requires materializing per-query copies of the relations, which is
//! incompatible with CompactLTJ's succinct, immutable, mmap'd LOUDS tries
//! (the entire point of which is to *avoid* per-query data copies). What is
//! adopted from both papers is the diagnosis that parallelizing only the
//! outermost join variable is skew-prone (HoneyComb §1–2 measures a >15×
//! task-runtime skew ratio from naively hash-partitioning just the first
//! variable), and the PJDL-style fix: enumerate the adaptively-chosen root
//! variable's full domain first with one sequential leapfrog pass
//! (`lftj_enumerate_level`), then hand each domain value's *independent*
//! subtree computation (a fresh `bindings` clone, no shared mutable state)
//! to rayon's work-stealing pool from `eval_bgp_lftj_cancellable` once the
//! domain is large enough (`should_parallelize_lftj_root`); below that
//! threshold, or on wasm32 (where rayon is unavailable), the same
//! enumerated matches are walked with a plain sequential loop, so no query
//! pays for an extra pass. Because each root value's subtree needs no
//! relation reorganization, this preserves CompactLTJ's space/zero-copy
//! properties untouched — only the join *evaluation*, never the storage
//! layout, is parallelized. A further, adaptive skew-mitigation layer
//! (splitting *below* the root for a single outsized subtree, the way
//! HoneyComb partitions every level) is deferred until profiling on a real
//! skewed dataset shows this single-level split is insufficient.
//!
//! ### Dispatch granularity: chunked, not one-task-per-value
//!
//! An earlier version of this dispatch handed each enumerated root value to
//! its own rayon task (`matches.par_iter().map(...)`). Benchmarked against
//! `bsbm_large`'s synthetic 1.25M-triple dataset, that granularity regressed
//! wall-clock time by 80-130% across *every* query shape (`star_class_region`,
//! `star_with_features`, `full_star`, `triangle`) relative to the plain
//! sequential loop, despite each shape's root domain comfortably exceeding
//! `PARALLEL_LFTJ_ROOT_THRESHOLD`. The initial hypothesis was lock contention
//! on `RingStore`'s internal lock (many rayon worker threads concurrently
//! calling `lftj_join_scan`/`lftj_real_count` through a shared `Mutex`), so
//! `RingStore` and `Dictionary`'s decode cache were migrated from `Mutex` to
//! `RwLock` (concurrent readers no longer serialize on the same lock). This
//! migration is real and worthwhile for concurrent-read scalability generally
//! — but re-benchmarking after it showed **the exact same 80-130% regression
//! magnitude**, falsifying the lock-contention hypothesis entirely.
//!
//! The actual root cause: for these query shapes, `matches.len()` (the
//! enumerated root domain) is typically in the hundreds to low thousands, and
//! each individual value's subtree recursion (`lftj_step` over `remaining`)
//! completes in well under a microsecond. At that grain, one rayon task per
//! value is dominated by task dispatch/steal overhead (work-stealing deque
//! push/pop, cross-core cache-line traffic, thread wake-up) rather than by
//! useful work. The fix actually implemented in `run_lftj_root` is **chunked
//! dispatch**: `matches` is partitioned into `~2 * rayon::current_num_threads()`
//! coarse-grained chunks via `par_chunks`, and each chunk runs a tight
//! sequential loop over its slice of root values in a single rayon task. This
//! bounds the number of dispatched tasks to a small constant regardless of
//! domain size while still spreading work across every core.
//!
//! Re-benchmarked against a true sequential baseline (obtained by temporarily
//! forcing `PARALLEL_LFTJ_ROOT_THRESHOLD = usize::MAX`, since Criterion's own
//! `change:` percentage compares against its last saved run, not a fixed
//! reference), chunked dispatch alone at the production threshold of 64
//! measured: `star_class_region` and `star_with_features` — no statistically
//! significant change; `full_star` — a small ~4% regression; `triangle`
//! (the highest join fan-out shape) — a ~13% **improvement**. This fixed the
//! regression but left only modest absolute speedup — residual contention
//! on the store `RwLock` (re-acquired for every `join_scan`/`decode`/
//! `estimate`) and on the shared dictionary decode-cache `Mutex` still
//! serialized the workers.
//!
//! ### Query-scoped snapshot + per-worker decode caches
//!
//! The residual contention is removed by a two-part snapshot path:
//!
//! 1. **Query-scoped lock-free snapshot** (`LftjSource::lftj_query_snapshot`
//!    → `RingLftjSnapshot`). Under a single store `RwLock` read guard, when
//!    the LSM delta is empty, the store freezes an `Arc`-shared view of the
//!    LOUDS graphs plus a `DictDecodeSnapshot` of the dictionary. After that
//!    one acquisition, every subsequent `join_scan` / `estimate_count` /
//!    `decode_term` call on the snapshot is lock-free (the succinct
//!    structures and the compacted Front-Coded tier are immutable).
//! 2. **Per-worker decode caches.** Sharing one snapshot across rayon
//!    workers would re-introduce contention on the snapshot's private
//!    decode-cache `Mutex`. `DictDecodeSnapshot::clone` (invoked once per
//!    rayon chunk via `LftjSnapshot::clone_for_worker`) therefore Arc-shares
//!    the delta maps / compacted tier / graph table (O(1) refcount bumps)
//!    and installs a **fresh empty** worker-sized LRU
//!    (`WORKER_DECODE_CACHE_CAPACITY = 8192`, not the live dictionary's
//!    100_000 — `lru::LruCache::new` pre-allocates a full-capacity
//!    `HashMap`). Sequential evaluation keeps the full 100_000 cache.
//!
//! When the delta is non-empty (or the backend does not specialize
//! `lftj_query_snapshot`), evaluation falls back to the locked path with
//! no behavior change. On the snapshot path, `eval_bgp_lftj_cancellable`
//! prefers the snapshot for the entire `run_lftj_root` call.
//!
//! Re-benchmarked on `bsbm_large` (chunked parallel *with* the snapshot +
//! per-worker caches, vs. the previous chunked-only baseline):
//!
//! | Query | Δ wall-clock | Δ throughput |
//! |---|---|---|
//! | `star_class_region` | −22% | +29% |
//! | `star_with_features` | −60% | +148% |
//! | `full_star` | −62% | +165% |
//! | `triangle` | −56% | — |
//!
//! A further, adaptive skew-mitigation layer (splitting *below* the root
//! for a single outsized subtree, the way HoneyComb partitions every
//! level) remains deferred until profiling on a real skewed dataset shows
//! this chunked single-level split is insufficient.

use crate::dataset::{Dataset, GraphSelector};
use crate::options::CancellationToken;
use crate::solution::{Solution, Solutions};
use oxigraph_nova_core::{GraphName, Variable};
use smallvec::SmallVec;
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Inline capacity for the hot per-`lftj_step` / `lftj_enumerate_level` buffers
/// (`active` patterns, open `scans`, VEO reorder). Typical BGP fan-out is well
/// under this; larger queries spill to the heap transparently via `SmallVec`.
const LFTJ_INLINE_CAP: usize = 8;

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
/// Sorting 3–6 elements is O(1) — the overhead is negligible. The result is a
/// stack-inline [`SmallVec`] so callers that only need a temporary reorder
/// (the common case of ≤ [`LFTJ_INLINE_CAP`] unbound variables) pay no heap
/// allocation for the sort buffer.
fn veo_sort<D: Dataset + ?Sized>(
    unbound: &[usize],
    specs: &[PatternSpec],
    bindings: &[Option<u64>],
    dataset: &D,
    graph: &GraphSelector,
) -> SmallVec<[usize; LFTJ_INLINE_CAP]> {
    struct Candidate {
        var_idx: usize,
        weight: u64,
    }

    let mut candidates: SmallVec<[Candidate; LFTJ_INLINE_CAP]> = unbound
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
fn lftj_step<D: Dataset + ?Sized>(
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
    // non-CLTJ backends without regressing CLTJ* behaviour. The sort buffer
    // is a stack-inline `SmallVec` so the common ≤8-var case never heap-
    // allocates just to reorder.
    let veo_buf: SmallVec<[usize; LFTJ_INLINE_CAP]>;
    let order: &[usize] = if dataset.supports_veo_estimates() && unbound.len() > 1 {
        veo_buf = veo_sort(unbound, specs, bindings, dataset, graph);
        &veo_buf
    } else {
        unbound
    };

    let var_idx = order[0];
    let remaining = &order[1..];

    // ── Find patterns active for this variable ──────────────────────────────
    let active: SmallVec<[&PatternSpec; LFTJ_INLINE_CAP]> = specs
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
    // Stack-inline for the common case of a few patterns active on one
    // variable (typical star / triangle fan-out ≪ LFTJ_INLINE_CAP).
    let mut scans: SmallVec<[Box<dyn oxigraph_nova_core::TrieIterator>; LFTJ_INLINE_CAP]> =
        SmallVec::with_capacity(active.len());
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

// ── Root-level enumeration (for parallel dispatch) ────────────────────────────

/// Threshold on the number of distinct root-variable values below which
/// parallel dispatch is not worth the rayon task-spawn overhead.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
const PARALLEL_LFTJ_ROOT_THRESHOLD: usize = 64;

/// Should the enumerated root variable's `n` matching values be joined in
/// parallel (one rayon task per value) rather than with a plain sequential
/// loop?
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
#[inline]
fn should_parallelize_lftj_root(n: usize) -> bool {
    n >= PARALLEL_LFTJ_ROOT_THRESHOLD
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[inline]
#[allow(dead_code)]
fn should_parallelize_lftj_root(_n: usize) -> bool {
    false
}

/// Enumerate every matching value of the adaptively-chosen next variable
/// without recursing into child levels — a single sequential leapfrog pass
/// over just that one variable.
///
/// Picks the variable exactly as `lftj_step` would (same `veo_sort` call),
/// opens one scan per pattern active for it, and leapfrogs through all
/// matching keys in ascending order, collecting them into a `Vec<u64>`
/// instead of recursing on each one. This is the same amount of top-level
/// leapfrog work `lftj_step`'s own loop would perform — the difference is
/// only that recursion into `remaining` is deferred to the caller, which can
/// then choose to run those per-value subtree computations sequentially or
/// fan them out across rayon's work-stealing pool.
///
/// Returns `(var_idx, remaining_unbound, matches)`, or `None` when a scan
/// could not be opened for the chosen variable (mirrors `lftj_step`'s
/// scan-unavailable case, which yields zero rows for the whole BGP).
fn lftj_enumerate_level<D: Dataset + ?Sized>(
    dataset: &D,
    specs: &[PatternSpec],
    graph: &GraphSelector,
    unbound: &[usize],
    bindings: &[Option<u64>],
) -> Option<(usize, Vec<usize>, Vec<u64>)> {
    let veo_buf: SmallVec<[usize; LFTJ_INLINE_CAP]>;
    let order: &[usize] = if dataset.supports_veo_estimates() && unbound.len() > 1 {
        veo_buf = veo_sort(unbound, specs, bindings, dataset, graph);
        &veo_buf
    } else {
        unbound
    };

    let var_idx = order[0];
    // `remaining` is returned to the caller and lives across the whole root
    // fan-out, so a heap `Vec` is appropriate here (not a per-step buffer).
    let remaining: Vec<usize> = order[1..].to_vec();

    let active: SmallVec<[&PatternSpec; LFTJ_INLINE_CAP]> = specs
        .iter()
        .filter(|sp| sp.is_active_for_var(var_idx))
        .collect();
    if active.is_empty() {
        // Every variable in `join_vars` comes from some pattern it is active
        // for, so this cannot arise for a well-formed BGP; handled
        // defensively, mirroring `lftj_step`'s own defensive branch.
        return None;
    }

    let mut scans: SmallVec<[Box<dyn oxigraph_nova_core::TrieIterator>; LFTJ_INLINE_CAP]> =
        SmallVec::with_capacity(active.len());
    for sp in &active {
        let (sv, pv, ov, target_field) = sp.resolve_for_var(var_idx, bindings);
        let scan = dataset.lftj_join_scan(sv, pv, ov, target_field, graph)?;
        scans.push(scan);
    }

    if scans.iter().any(|s| s.at_end()) {
        return Some((var_idx, remaining, Vec::new()));
    }

    let mut matches: Vec<u64> = Vec::new();

    loop {
        match leapfrog_sync(&mut scans) {
            None => break,
            Some(val) => {
                matches.push(val);
                scans[0].advance();
                if scans[0].at_end() {
                    break;
                }
            }
        }
    }

    Some((var_idx, remaining, matches))
}

/// Drive the top-level join: enumerate the adaptively-chosen root
/// variable's domain once (via [`lftj_enumerate_level`]), then either fan
/// the per-value subtree recursions out across rayon's work-stealing pool
/// (when the domain is large enough — [`should_parallelize_lftj_root`]) or
/// walk them with a plain sequential loop that exactly reproduces
/// `lftj_step`'s own top-level loop.
///
/// Row order is preserved either way: matches are enumerated in ascending
/// leapfrog order and, in the parallel path, collected back in that same
/// order (`rayon`'s `par_iter().map(...).collect()` preserves input order
/// regardless of which thread finishes first), so parallel and sequential
/// evaluation of the same query produce byte-identical `Solutions`.
#[allow(clippy::too_many_arguments)]
fn run_lftj_root<D: Dataset + ?Sized>(
    dataset: &D,
    specs: &[PatternSpec],
    join_vars: &Arc<[Variable]>,
    graph: &GraphSelector,
    unbound: &[usize],
    mut bindings: Vec<Option<u64>>,
    cancellation: Option<&CancellationToken>,
) -> (Solutions, bool) {
    if let Some(token) = cancellation
        && token.is_cancelled()
    {
        return (Vec::new(), true);
    }

    // Zero unbound variables (every pattern was fully ground and already
    // existence-checked in `build_spec`): delegate straight to `lftj_step`,
    // whose base case emits the single trivial solution.
    if unbound.is_empty() {
        let mut results: Solutions = Vec::new();
        let mut aborted = false;
        lftj_step(
            dataset,
            specs,
            join_vars,
            graph,
            unbound,
            &mut bindings,
            &mut results,
            cancellation,
            &mut aborted,
        );
        return (results, aborted);
    }

    let Some((var_idx, remaining, matches)) =
        lftj_enumerate_level(dataset, specs, graph, unbound, &bindings)
    else {
        return (Vec::new(), false);
    };

    if matches.is_empty() {
        return (Vec::new(), false);
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    if should_parallelize_lftj_root(matches.len()) {
        use rayon::prelude::*;

        // Chunked dispatch: benchmarked one-task-per-value dispatch
        // (`matches.par_iter().map(...)`) regressed wall-clock time 80-130%
        // across every bsbm_large query shape, even after eliminating lock
        // contention as a possible cause (RingStore's internal lock was
        // converted from Mutex to RwLock with no change in the regression's
        // magnitude). Root cause: each individual value's subtree here is
        // sub-microsecond, so with `matches.len()` in the hundreds to low
        // thousands, one rayon task per value is dominated by task
        // dispatch/steal overhead. Partitioning into ~2x-thread-count
        // coarse-grained chunks (each running a tight sequential loop)
        // bounds the task count to a small constant while still spreading
        // work across all cores.
        let num_threads = rayon::current_num_threads().max(1);
        let chunk_count = (num_threads * 2).min(matches.len()).max(1);
        let chunk_size = matches.len().div_ceil(chunk_count).max(1);

        let parts: Vec<(Solutions, bool)> = matches
            .par_chunks(chunk_size)
            .map(|chunk| {
                // Prefer a per-worker dataset clone with a fresh decode cache
                // (LftjSnapshotDataset / DictDecodeSnapshot). When the backend
                // doesn't specialize, share `&dataset` (no dyn cast — D may
                // be `?Sized`).
                let worker_owned = dataset.lftj_clone_for_worker();
                let mut sub_bindings = bindings.clone();
                let mut sub_results: Solutions = Vec::new();
                let mut sub_aborted = false;
                for &val in chunk {
                    if sub_aborted {
                        break;
                    }
                    sub_bindings[var_idx] = Some(val);
                    match worker_owned.as_ref() {
                        Some(worker) => lftj_step(
                            worker.as_ref(),
                            specs,
                            join_vars,
                            graph,
                            &remaining,
                            &mut sub_bindings,
                            &mut sub_results,
                            cancellation,
                            &mut sub_aborted,
                        ),
                        None => lftj_step(
                            dataset,
                            specs,
                            join_vars,
                            graph,
                            &remaining,
                            &mut sub_bindings,
                            &mut sub_results,
                            cancellation,
                            &mut sub_aborted,
                        ),
                    }
                    sub_bindings[var_idx] = None;
                }
                (sub_results, sub_aborted)
            })
            .collect();

        let mut results: Solutions = Vec::new();
        let mut aborted = false;
        for (sub_results, sub_aborted) in parts {
            results.extend(sub_results);
            aborted |= sub_aborted;
        }
        return (results, aborted);
    }

    // Sequential fallback (below threshold, or wasm32 where rayon is
    // unavailable): reproduces `lftj_step`'s own top-level loop exactly.
    let mut results: Solutions = Vec::new();
    let mut aborted = false;
    for &val in &matches {
        bindings[var_idx] = Some(val);
        lftj_step(
            dataset,
            specs,
            join_vars,
            graph,
            &remaining,
            &mut bindings,
            &mut results,
            cancellation,
            &mut aborted,
        );
        bindings[var_idx] = None;
        if aborted {
            break;
        }
    }
    (results, aborted)
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
    //
    // `run_lftj_root` enumerates the adaptively-chosen root variable's
    // domain once and, when that domain is large enough, hands each value's
    // independent subtree to rayon's work-stealing pool — see the module
    // doc's "Parallel root-level dispatch" section.
    //
    // Prefer a query-scoped lock-free snapshot when the backend provides one
    // (RingStore via `lftj_query_snapshot`): a single store-lock acquisition
    // freezes the Ring + dictionary state for the whole join, so parallel
    // workers never re-enter the store lock for join_scan/decode/estimate.
    // Pattern classification above still uses the live dataset (needs
    // intern of constants, which is also lock-free-friendly on the snapshot
    // but we already did it before snapshotting). Fall back to the live
    // dataset when no snapshot is available (non-CLTJ backends, tests).
    let bindings: Vec<Option<u64>> = vec![None; join_vars.len()];
    let unbound: Vec<usize> = (0..join_vars.len()).collect();

    let snapshot = dataset.lftj_query_snapshot();
    let (results, aborted) = if let Some(ref snap) = snapshot {
        run_lftj_root(
            snap.as_ref(),
            &specs,
            &join_vars,
            active_graph,
            &unbound,
            bindings,
            cancellation,
        )
    } else {
        run_lftj_root(
            dataset,
            &specs,
            &join_vars,
            active_graph,
            &unbound,
            bindings,
            cancellation,
        )
    };

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
        assert_eq!(order.as_slice(), &[0, 1]);
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
            order.as_slice(),
            &[1, 0],
            "VEO must prefer var 1 (real subtree size 2) over var 0 (real subtree size 10)"
        );
    }

    /// A dataset with a large, configurable number of distinct subject ids
    /// satisfying two patterns joined on a single variable (`?x`) — used to
    /// exercise `run_lftj_root`'s parallel dispatch path
    /// (`should_parallelize_lftj_root`'s threshold requires ≥64 matches).
    ///
    /// Both patterns are active for the same (only) join variable and target
    /// the subject field; `lftj_join_scan` returns the full `1..=n` id range
    /// for either pattern's (p, o) constant pair, so leapfrog intersection
    /// yields exactly `n` matches in ascending order.
    ///
    /// When `cancel_on_scan` is set, every `lftj_join_scan` call cancels
    /// `token` as a side effect — since `lftj_enumerate_level` (the single
    /// sequential pass that opens these scans) always runs to completion
    /// *before* `run_lftj_root` decides whether to fan out in parallel, this
    /// deterministically cancels the token before any parallel sub-task
    /// runs, without relying on a timing-sensitive race.
    struct ManyRootDataset {
        n: u64,
        cancel_on_scan: bool,
        token: CancellationToken,
    }

    impl DatasetLftjSource for ManyRootDataset {
        fn supports_lftj(&self) -> bool {
            true
        }
        fn lftj_has_delta(&self) -> bool {
            false
        }
        fn lftj_intern_term(&self, term: &Term, _: &GraphSelector) -> Option<u64> {
            // p1=100, o1=101, p2=200, o2=201 — anything else is not internable.
            if let Term::NamedNode(n) = term {
                match n.as_str() {
                    "http://ex/p1" => Some(100),
                    "http://ex/o1" => Some(101),
                    "http://ex/p2" => Some(200),
                    "http://ex/o2" => Some(201),
                    _ => None,
                }
            } else {
                None
            }
        }
        fn lftj_decode_term(&self, id: u64) -> Option<Term> {
            Some(Term::NamedNode(
                oxigraph_nova_core::NamedNode::new_unchecked(format!("http://ex/s{id}")),
            ))
        }
        fn lftj_join_scan(
            &self,
            _s: Option<u64>,
            _p: Option<u64>,
            _o: Option<u64>,
            _target_field: usize,
            _: &GraphSelector,
        ) -> Option<Box<dyn TrieIterator>> {
            if self.cancel_on_scan {
                self.token.cancel();
            }
            let vals: Vec<u64> = (1..=self.n).collect();
            Some(Box::new(VecTrieIter { vals, pos: 0 }))
        }
    }

    impl Dataset for ManyRootDataset {
        fn find_quads<'a>(&'a self, _: &QuadPattern) -> Result<QuadIter<'a>> {
            Ok(Box::new(std::iter::empty()))
        }
        fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
            Ok(Box::new(std::iter::empty()))
        }
    }

    fn many_root_patterns() -> Vec<TriplePattern> {
        let var_x = || TermPattern::Variable(spargebra::term::Variable::new_unchecked("x"));
        vec![
            TriplePattern {
                subject: var_x(),
                predicate: NamedNodePattern::NamedNode(
                    oxigraph_nova_core::NamedNode::new_unchecked("http://ex/p1"),
                ),
                object: TermPattern::NamedNode(oxigraph_nova_core::NamedNode::new_unchecked(
                    "http://ex/o1",
                )),
            },
            TriplePattern {
                subject: var_x(),
                predicate: NamedNodePattern::NamedNode(
                    oxigraph_nova_core::NamedNode::new_unchecked("http://ex/p2"),
                ),
                object: TermPattern::NamedNode(oxigraph_nova_core::NamedNode::new_unchecked(
                    "http://ex/o2",
                )),
            },
        ]
    }

    /// The parallel root-dispatch path (`should_parallelize_lftj_root`
    /// returns `true` once the enumerated domain reaches
    /// `PARALLEL_LFTJ_ROOT_THRESHOLD`) must produce exactly the same rows,
    /// in exactly the same order, as the sequential path below that
    /// threshold — matches are enumerated in ascending leapfrog order and
    /// `par_iter().map(...).collect()` preserves that order regardless of
    /// which rayon thread finishes first.
    #[test]
    fn parallel_root_dispatch_preserves_order_and_content() {
        let patterns = many_root_patterns();

        // Below threshold: sequential loop path.
        let small = ManyRootDataset {
            n: 10,
            cancel_on_scan: false,
            token: CancellationToken::default(),
        };
        let small_results = eval_bgp_lftj(&small, &patterns, &GraphSelector::Default)
            .expect("LFTJ should handle this BGP")
            .expect("no error expected");
        assert_eq!(small_results.len(), 10);

        // Above threshold (PARALLEL_LFTJ_ROOT_THRESHOLD == 64): parallel path.
        let big = ManyRootDataset {
            n: 200,
            cancel_on_scan: false,
            token: CancellationToken::default(),
        };
        assert!(should_parallelize_lftj_root(200));
        let big_results = eval_bgp_lftj(&big, &patterns, &GraphSelector::Default)
            .expect("LFTJ should handle this BGP")
            .expect("no error expected");
        assert_eq!(big_results.len(), 200);

        // Row order must be ascending by subject id (1..=200), matching the
        // sequential leapfrog enumeration order exactly.
        let var_x = spargebra::term::Variable::new_unchecked("x");
        for (i, sol) in big_results.iter().enumerate() {
            let expected = Term::NamedNode(oxigraph_nova_core::NamedNode::new_unchecked(format!(
                "http://ex/s{}",
                i + 1
            )));
            assert_eq!(sol.get(&var_x), Some(&expected));
        }
    }

    /// Cancellation observed during the single sequential enumeration pass
    /// (before parallel fan-out ever begins) must be honoured by every
    /// dispatched sub-task: the whole join aborts with zero rows and a
    /// `Cancelled` error, not a partial/successful result.
    #[test]
    fn parallel_root_dispatch_honours_cancellation_from_all_subtasks() {
        let patterns = many_root_patterns();
        let token = CancellationToken::default();
        let ds = ManyRootDataset {
            n: 200,
            cancel_on_scan: true,
            token: token.clone(),
        };
        assert!(!token.is_cancelled());
        let result =
            eval_bgp_lftj_cancellable(&ds, &patterns, &GraphSelector::Default, Some(&token));
        assert!(token.is_cancelled());
        match result {
            Some(Err(e)) => {
                assert!(
                    e.downcast_ref::<crate::options::EvalLimitError>().is_some(),
                    "expected a Cancelled error, got: {e:?}"
                );
            }
            other => panic!("expected Some(Err(Cancelled)), got {other:?}"),
        }
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
}
