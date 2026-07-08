//! Ring-accelerated property path evaluation.
//!
//! For simple-predicate paths (`PPE::NamedNode(p)`), the Ring index provides
//! O(degree) neighbor enumeration via [`Dataset::lftj_join_scan`], avoiding the
//! O(total-triples) full-scan that `find_quads` requires.
//!
//! ## Principled approach
//!
//! Reference: "BWT Indexes for Optimal Joins"
//! (OASIcs, Arroyuelo/Navarro et al.), §Property Paths / RPQs.
//!
//! For a path `p+` or `p*` over a Ring-indexed dataset:
//!
//! 1. Enumerate sources via `lftj_join_scan(None, Some(p_id), None, 0, ag)` —
//!    PSO ordering — iterating subjects with predicate `p` in O(|S_p|).
//! 2. For each source `s`, enumerate successors via
//!    `lftj_join_scan(Some(s), Some(p_id), None, 2, ag)` — SPO ordering —
//!    iterating objects in O(deg(s, p)).
//! 3. BFS from each source accumulates the full transitive closure.
//!
//! Total work is O(|closure|), which is ≪ O(n) when predicate `p` appears on
//! only a fraction of all graph triples.  The Ring's LOUDS tries provide O(1)
//! navigation per step and O(log ℓ) seek via exponential search, so each
//! adjacency query is considerably faster than a `find_quads` round-trip.

use crate::dataset::{Dataset, GraphSelector};
use crate::options::CancellationToken;
use anyhow::Result;
use oxrdf::{NamedNode, Term};
use spargebra::algebra::PropertyPathExpression as PPE;
use std::collections::{HashSet, VecDeque};

/// Returns `true` and short-circuits with a cancellation error if `cancellation`
/// is set and has been flipped. Shared by every BFS/traversal loop below.
fn is_cancelled(cancellation: Option<&CancellationToken>) -> bool {
    cancellation.is_some_and(|c| c.is_cancelled())
}

// ── Pair enumeration ──────────────────────────────────────────────────────────

/// Enumerate all `(subject, object)` pairs for a single named-node predicate,
/// using the Ring's LOUDS trie iterators instead of a full `find_quads` scan.
///
/// ## Access pattern
///
/// - `lftj_join_scan(None, Some(p_id), None, 0, ag)` — PSO ordering — yields
///   all subjects that have predicate `p` in O(|S_p|).
/// - For each subject `s`, `lftj_join_scan(Some(s), Some(p_id), None, 2, ag)` —
///   SPO ordering — yields all objects for `(s, p)` in O(deg(s, p)).
///
/// Returns `None` if LFTJ is unavailable (e.g. predicate not in dictionary),
/// so the caller can fall through to the generic `find_quads` path.
pub fn ring_pairs_for_pred<D: Dataset>(
    dataset: &D,
    pred: &NamedNode,
    ag: &GraphSelector,
) -> Option<Result<Vec<(Term, Term)>>> {
    let p_id = dataset.lftj_intern_term(&Term::NamedNode(pred.clone()), ag)?;

    // Subjects with predicate p — PSO depth-1 iterator within P=p_id.
    let mut s_iter = dataset.lftj_join_scan(None, Some(p_id), None, 0, ag)?;

    let mut pairs: Vec<(Term, Term)> = Vec::new();

    while !s_iter.at_end() {
        let s_id = s_iter.key();
        let s_term = match dataset.lftj_decode_term(s_id) {
            Some(t) => t,
            None => {
                s_iter.advance();
                continue;
            }
        };

        // Objects for (s, p) — SPO depth-2 iterator within S=s_id, P=p_id.
        let mut o_iter = dataset.lftj_join_scan(Some(s_id), Some(p_id), None, 2, ag)?;

        while !o_iter.at_end() {
            let o_id = o_iter.key();
            if let Some(o_term) = dataset.lftj_decode_term(o_id) {
                pairs.push((s_term.clone(), o_term));
            }
            o_iter.advance();
        }

        s_iter.advance();
    }

    Some(Ok(pairs))
}

// ── Node enumeration ──────────────────────────────────────────────────────────

/// Enumerate all subject IDs that have predicate `pred_id` in the active graph.
///
/// Uses `lftj_join_scan(None, Some(pred_id), None, 0, ag)` — PSO ordering —
/// O(|S_p|) rather than O(n total triples).
///
/// Returns `None` if LFTJ is unavailable.
pub fn ring_subjects_for_pred<D: Dataset>(
    dataset: &D,
    pred_id: u64,
    ag: &GraphSelector,
) -> Option<Result<Vec<u64>>> {
    let mut iter = dataset.lftj_join_scan(None, Some(pred_id), None, 0, ag)?;
    let mut ids: Vec<u64> = Vec::new();
    while !iter.at_end() {
        ids.push(iter.key());
        iter.advance();
    }
    Some(Ok(ids))
}

/// Enumerate all distinct node IDs (subjects ∪ objects) in the active graph.
///
/// Used by `ZeroOrMore` transitive closure to generate identity pairs `(x, x)`
/// for every graph node, including nodes not connected by the path predicate.
///
/// - Subjects: `lftj_join_scan(None, None, None, 0, ag)` → SPO depth-0, O(|S|).
/// - Objects:  `lftj_join_scan(None, None, None, 2, ag)` → OPS depth-0, O(|O|).
///
/// Returns `None` if LFTJ is unavailable.
pub fn ring_all_node_ids<D: Dataset>(dataset: &D, ag: &GraphSelector) -> Option<Result<Vec<u64>>> {
    let mut s_iter = dataset.lftj_join_scan(None, None, None, 0, ag)?;
    let mut o_iter = dataset.lftj_join_scan(None, None, None, 2, ag)?;

    let mut ids: HashSet<u64> = HashSet::new();
    while !s_iter.at_end() {
        ids.insert(s_iter.key());
        s_iter.advance();
    }
    while !o_iter.at_end() {
        ids.insert(o_iter.key());
        o_iter.advance();
    }
    Some(Ok(ids.into_iter().collect()))
}

// ── Transitive closure ────────────────────────────────────────────────────────

/// BFS transitive closure for a single predicate, using Ring TrieIterators for
/// lazy neighbor enumeration.
///
/// Each BFS step calls `lftj_join_scan(Some(curr_id), Some(pred_id), None, 2, ag)`
/// — SPO ordering — to enumerate successors in O(deg(curr, p)) rather than
/// scanning a pre-materialised adjacency HashMap.
///
/// ## Parameters
///
/// - `pred_id` — interned predicate ID.
/// - `start_ids` — BFS starting node IDs (subjects only for `+`, all nodes for `*`).
/// - `include_identity` — `true` for `ZeroOrMore`; emits `(x, x)` for every
///   start node even when no outgoing edge exists.
///
/// Returns `None` if Ring neighbor enumeration becomes unavailable mid-BFS
/// (caller falls back to the generic HashMap-BFS).
pub fn ring_bfs_transitive<D: Dataset>(
    dataset: &D,
    pred_id: u64,
    start_ids: &[u64],
    include_identity: bool,
    ag: &GraphSelector,
) -> Option<Result<Vec<(Term, Term)>>> {
    ring_bfs_transitive_cancellable(dataset, pred_id, start_ids, include_identity, ag, None)
}

/// [`ring_bfs_transitive`] with an optional [`CancellationToken`] checked
/// periodically during the BFS, so a long-running transitive closure over a
/// densely-connected predicate can be aborted promptly on client disconnect
/// or timeout.
pub fn ring_bfs_transitive_cancellable<D: Dataset>(
    dataset: &D,
    pred_id: u64,
    start_ids: &[u64],
    include_identity: bool,
    ag: &GraphSelector,
    cancellation: Option<&CancellationToken>,
) -> Option<Result<Vec<(Term, Term)>>> {
    let mut result: Vec<(Term, Term)> = Vec::new();
    // (start_id, reachable_id) deduplication across all BFS roots.
    let mut global_seen: HashSet<(u64, u64)> = HashSet::new();

    for &start_id in start_ids {
        if is_cancelled(cancellation) {
            return Some(Err(anyhow::Error::from(
                crate::options::EvalLimitError::Cancelled,
            )));
        }

        let start_term = match dataset.lftj_decode_term(start_id) {
            Some(t) => t,
            None => continue,
        };

        // Identity pair for ZeroOrMore (*).
        if include_identity && global_seen.insert((start_id, start_id)) {
            result.push((start_term.clone(), start_term.clone()));
        }

        // BFS: accumulate all nodes reachable from start_id via pred_id.
        //
        // `visited` prevents re-expanding a node that's already been processed
        // from this root (loop / cycle safety). We still check `global_seen` for
        // each (start, nbr) pair because a neighbor may be reachable from this
        // root without being newly visited (e.g. it was already visited before we
        // reach it via a different path through a cycle).
        let mut visited: HashSet<u64> = HashSet::new();
        visited.insert(start_id);
        let mut queue: VecDeque<u64> = VecDeque::new();
        queue.push_back(start_id);

        while let Some(curr_id) = queue.pop_front() {
            if is_cancelled(cancellation) {
                return Some(Err(anyhow::Error::from(
                    crate::options::EvalLimitError::Cancelled,
                )));
            }

            let mut nbr_iter = dataset.lftj_join_scan(Some(curr_id), Some(pred_id), None, 2, ag)?;

            while !nbr_iter.at_end() {
                let nbr_id = nbr_iter.key();

                // Only expand unseen nodes to avoid infinite BFS in cycles.
                if visited.insert(nbr_id) {
                    queue.push_back(nbr_id);
                }

                // Emit every (start, nbr) pair the first time we see it —
                // including (start, start) when a cycle brings us back (correct
                // per SPARQL ALP semantics for `+`).
                if global_seen.insert((start_id, nbr_id))
                    && let Some(nbr_term) = dataset.lftj_decode_term(nbr_id)
                {
                    result.push((start_term.clone(), nbr_term));
                }

                nbr_iter.advance();
            }
        }
    }

    Some(Ok(result))
}

// ── Endpoint-aware (bidirectional / backward) transitive closure ─────────────

/// Ring-accelerated, endpoint-aware transitive closure / reachability for
/// `p+` / `p*` paths where at least one endpoint of the path query is bound
/// to a concrete node.
///
/// Rather than materializing the full transitive closure (or even the full
/// set of BFS start nodes), this dispatches on which endpoint(s) are bound:
///
/// - **Both bound** (e.g. `ASK { :a p+ :b }`) — bidirectional meet-in-the-middle
///   BFS: expand the smaller of the forward frontier (from `source_id`, via
///   SPO) and backward frontier (from `target_id`, via predecessor lookups)
///   one layer at a time until they intersect. This bounds the work by the
///   size of the *shortest connecting path's neighborhood*, not the size of
///   the whole graph.
/// - **Only source bound** — ordinary forward BFS from a single root
///   (`ring_bfs_transitive` with a one-element start set), instead of
///   enumerating every node with predicate `p` first.
/// - **Only target bound** — backward BFS from the target, walking
///   predecessor edges (`lftj_join_scan(None, Some(pred_id), Some(curr), 0, ag)`,
///   OPS-style ordering) instead of enumerating all sources via
///   `ring_all_node_ids`/`ring_subjects_for_pred` and running forward BFS
///   from each one.
///
/// Returns `None` if Ring neighbor enumeration is unavailable (caller falls
/// back to the generic path). Returns `Some(Err(_))` if `source_id`/`target_id`
/// cannot be decoded back to a `Term` (should not normally happen since the
/// caller obtained the id via `lftj_intern_term`).
pub fn ring_bfs_transitive_bound<D: Dataset>(
    dataset: &D,
    pred_id: u64,
    source_id: Option<u64>,
    target_id: Option<u64>,
    include_identity: bool,
    ag: &GraphSelector,
) -> Option<Result<Vec<(Term, Term)>>> {
    ring_bfs_transitive_bound_cancellable(
        dataset,
        pred_id,
        source_id,
        target_id,
        include_identity,
        ag,
        None,
    )
}

/// [`ring_bfs_transitive_bound`] with an optional [`CancellationToken`]
/// threaded into every internal branch (bidirectional meet-in-the-middle,
/// forward, and backward BFS).
#[allow(clippy::too_many_arguments)]
pub fn ring_bfs_transitive_bound_cancellable<D: Dataset>(
    dataset: &D,
    pred_id: u64,
    source_id: Option<u64>,
    target_id: Option<u64>,
    include_identity: bool,
    ag: &GraphSelector,
    cancellation: Option<&CancellationToken>,
) -> Option<Result<Vec<(Term, Term)>>> {
    match (source_id, target_id) {
        (Some(s), Some(t)) => {
            match bidirectional_reachable(dataset, pred_id, s, t, ag, cancellation)? {
                Ok(true) => {
                    let st = dataset.lftj_decode_term(s)?;
                    let tt = dataset.lftj_decode_term(t)?;
                    Some(Ok(vec![(st, tt)]))
                }
                Ok(false) => {
                    if include_identity && s == t {
                        let st = dataset.lftj_decode_term(s)?;
                        Some(Ok(vec![(st.clone(), st)]))
                    } else {
                        Some(Ok(vec![]))
                    }
                }
                Err(e) => Some(Err(e)),
            }
        }
        (Some(s), None) => ring_bfs_transitive_cancellable(
            dataset,
            pred_id,
            &[s],
            include_identity,
            ag,
            cancellation,
        ),
        (None, Some(t)) => {
            ring_bfs_transitive_backward(dataset, pred_id, t, include_identity, ag, cancellation)
        }
        (None, None) => None,
    }
}

/// Backward BFS from a single bound target, walking predecessor edges.
///
/// Each step calls `lftj_join_scan(None, Some(pred_id), Some(curr_id), 0, ag)`
/// to enumerate subjects `s` such that `(s, pred_id, curr_id)` holds — the
/// dataset chooses an OPS-style ordering internally since `o` and `p` are
/// bound and `s` (target_field = 0) is being enumerated.
fn ring_bfs_transitive_backward<D: Dataset>(
    dataset: &D,
    pred_id: u64,
    target_id: u64,
    include_identity: bool,
    ag: &GraphSelector,
    cancellation: Option<&CancellationToken>,
) -> Option<Result<Vec<(Term, Term)>>> {
    let target_term = dataset.lftj_decode_term(target_id)?;
    let mut result: Vec<(Term, Term)> = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();
    seen.insert(target_id);

    if include_identity {
        result.push((target_term.clone(), target_term.clone()));
    }

    let mut queue: VecDeque<u64> = VecDeque::new();
    queue.push_back(target_id);

    while let Some(curr_id) = queue.pop_front() {
        if is_cancelled(cancellation) {
            return Some(Err(anyhow::Error::from(
                crate::options::EvalLimitError::Cancelled,
            )));
        }

        let mut pred_iter = dataset.lftj_join_scan(None, Some(pred_id), Some(curr_id), 0, ag)?;
        while !pred_iter.at_end() {
            let s_id = pred_iter.key();
            if seen.insert(s_id) {
                queue.push_back(s_id);
                if let Some(s_term) = dataset.lftj_decode_term(s_id) {
                    result.push((s_term, target_term.clone()));
                }
            }
            pred_iter.advance();
        }
    }

    Some(Ok(result))
}

/// Bidirectional meet-in-the-middle reachability check: is `target_id`
/// reachable from `source_id` via one-or-more `pred_id` edges?
///
/// Expands the forward frontier (successors, via SPO) and the backward
/// frontier (predecessors, via OPS-style lookups) one layer at a time,
/// stopping as soon as a node discovered on one side is already present in
/// the other side's visited set. Note that a direct edge `source -> target`
/// (or a self-loop `source -> source` when `source_id == target_id`) is
/// detected on the very first forward-layer expansion, since both visited
/// sets are seeded with their respective endpoint before any traversal.
///
/// This deliberately requires at least one traversed edge — the trivial
/// zero-length `source_id == target_id` case (relevant only to `*`, not `+`)
/// is handled by the caller via `include_identity`.
fn bidirectional_reachable<D: Dataset>(
    dataset: &D,
    pred_id: u64,
    source_id: u64,
    target_id: u64,
    ag: &GraphSelector,
    cancellation: Option<&CancellationToken>,
) -> Option<Result<bool>> {
    let mut fwd_visited: HashSet<u64> = HashSet::new();
    let mut bwd_visited: HashSet<u64> = HashSet::new();
    fwd_visited.insert(source_id);
    bwd_visited.insert(target_id);

    let mut fwd_frontier: VecDeque<u64> = VecDeque::new();
    fwd_frontier.push_back(source_id);
    let mut bwd_frontier: VecDeque<u64> = VecDeque::new();
    bwd_frontier.push_back(target_id);

    loop {
        if is_cancelled(cancellation) {
            return Some(Err(anyhow::Error::from(
                crate::options::EvalLimitError::Cancelled,
            )));
        }

        if fwd_frontier.is_empty() && bwd_frontier.is_empty() {
            return Some(Ok(false));
        }

        // Expand whichever frontier is smaller first — the classic
        // meet-in-the-middle heuristic that bounds work by the smaller side.
        let expand_fwd_first = fwd_frontier.len() <= bwd_frontier.len();

        for pass in 0..2 {
            let do_fwd = (pass == 0) == expand_fwd_first;
            if do_fwd {
                if fwd_frontier.is_empty() {
                    continue;
                }
                let layer: Vec<u64> = fwd_frontier.drain(..).collect();
                for curr in layer {
                    let mut nbr_iter =
                        dataset.lftj_join_scan(Some(curr), Some(pred_id), None, 2, ag)?;
                    while !nbr_iter.at_end() {
                        let nbr_id = nbr_iter.key();
                        if bwd_visited.contains(&nbr_id) {
                            return Some(Ok(true));
                        }
                        if fwd_visited.insert(nbr_id) {
                            fwd_frontier.push_back(nbr_id);
                        }
                        nbr_iter.advance();
                    }
                }
            } else {
                if bwd_frontier.is_empty() {
                    continue;
                }
                let layer: Vec<u64> = bwd_frontier.drain(..).collect();
                for curr in layer {
                    let mut pred_iter =
                        dataset.lftj_join_scan(None, Some(pred_id), Some(curr), 0, ag)?;
                    while !pred_iter.at_end() {
                        let s_id = pred_iter.key();
                        if fwd_visited.contains(&s_id) {
                            return Some(Ok(true));
                        }
                        if bwd_visited.insert(s_id) {
                            bwd_frontier.push_back(s_id);
                        }
                        pred_iter.advance();
                    }
                }
            }
        }
    }
}

// ── Product-automaton RPQ evaluation ─────────────────────────────────────────
//
// Composed path expressions (e.g. `(p/q)+`, `p|q`, `!(p|q)`) previously went
// through `path_pairs`, which recursively materializes a full
// `Vec<(Term, Term)>` *per operator* and joins/unions them — O(closure)
// *memory* at every level of the expression tree, not just once per query.
//
// Instead, we compile the `PropertyPathExpression` into a small Thompson-style
// NFA (states proportional to *expression size*, not data size) and run a
// single BFS over `(node_id, nfa_state)` product pairs, using the existing
// `lftj_join_scan` neighbor lookup at each step. This evaluates the entire
// RPQ in one traversal with no intermediate pair-set materialization — the
// only memory cost is the visited `(node, state)` set.

/// One transition edge in the compiled path-expression NFA.
///
/// `Fwd`/`Rev` walk a single named predicate forward/backward via Ring SPO/OPS
/// lookups; `NegFwd`/`NegRev` walk *any* predicate not in the given negated
/// set (SPARQL `!(p1|p2|...)` / `^(p1|p2|...)`); `Eps` is a state transition
/// with no edge traversal (used for `Sequence`/`Alternative`/Kleene-star
/// wiring, Thompson-construction style).
enum Edge {
    Fwd(NamedNode, usize),
    Rev(NamedNode, usize),
    NegFwd(Vec<NamedNode>, usize),
    NegRev(Vec<NamedNode>, usize),
    Eps(usize),
}

/// A small NFA compiled from a `PropertyPathExpression`, Thompson-construction
/// style: each `compile` call returns a fresh `(start, accept)` state pair
/// wired into `edges`.
struct Nfa {
    edges: Vec<Vec<Edge>>,
}

impl Nfa {
    fn new() -> Self {
        Self { edges: Vec::new() }
    }

    fn new_state(&mut self) -> usize {
        self.edges.push(Vec::new());
        self.edges.len() - 1
    }

    /// Compile `path` (already normalized via [`normalize_reverse`] so that
    /// `Reverse` only ever wraps a `NamedNode` or `NegatedPropertySet` leaf)
    /// into a fragment of this NFA, returning its `(start, accept)` states.
    fn compile(&mut self, path: &PPE) -> (usize, usize) {
        match path {
            PPE::NamedNode(p) => {
                let s0 = self.new_state();
                let s1 = self.new_state();
                self.edges[s0].push(Edge::Fwd(p.clone(), s1));
                (s0, s1)
            }
            PPE::Reverse(inner) => match inner.as_ref() {
                PPE::NamedNode(p) => {
                    let s0 = self.new_state();
                    let s1 = self.new_state();
                    self.edges[s0].push(Edge::Rev(p.clone(), s1));
                    (s0, s1)
                }
                PPE::NegatedPropertySet(v) => {
                    let s0 = self.new_state();
                    let s1 = self.new_state();
                    self.edges[s0].push(Edge::NegRev(v.clone(), s1));
                    (s0, s1)
                }
                _ => unreachable!("path must be pre-normalized via normalize_reverse"),
            },
            PPE::NegatedPropertySet(v) => {
                let s0 = self.new_state();
                let s1 = self.new_state();
                self.edges[s0].push(Edge::NegFwd(v.clone(), s1));
                (s0, s1)
            }
            PPE::Sequence(a, b) => {
                let (a0, a1) = self.compile(a);
                let (b0, b1) = self.compile(b);
                self.edges[a1].push(Edge::Eps(b0));
                (a0, b1)
            }
            PPE::Alternative(a, b) => {
                let s0 = self.new_state();
                let s1 = self.new_state();
                let (a0, a1) = self.compile(a);
                let (b0, b1) = self.compile(b);
                self.edges[s0].push(Edge::Eps(a0));
                self.edges[s0].push(Edge::Eps(b0));
                self.edges[a1].push(Edge::Eps(s1));
                self.edges[b1].push(Edge::Eps(s1));
                (s0, s1)
            }
            PPE::ZeroOrMore(inner) => {
                let s0 = self.new_state();
                let s1 = self.new_state();
                let (i0, i1) = self.compile(inner);
                self.edges[s0].push(Edge::Eps(i0));
                self.edges[s0].push(Edge::Eps(s1));
                self.edges[i1].push(Edge::Eps(i0));
                self.edges[i1].push(Edge::Eps(s1));
                (s0, s1)
            }
            PPE::OneOrMore(inner) => {
                let s0 = self.new_state();
                let s1 = self.new_state();
                let (i0, i1) = self.compile(inner);
                self.edges[s0].push(Edge::Eps(i0));
                self.edges[i1].push(Edge::Eps(i0));
                self.edges[i1].push(Edge::Eps(s1));
                (s0, s1)
            }
            PPE::ZeroOrOne(inner) => {
                let s0 = self.new_state();
                let s1 = self.new_state();
                let (i0, i1) = self.compile(inner);
                self.edges[s0].push(Edge::Eps(i0));
                self.edges[s0].push(Edge::Eps(s1));
                self.edges[i1].push(Edge::Eps(s1));
                (s0, s1)
            }
        }
    }
}

/// Push `Reverse` nodes down to the leaves of the expression tree, so that
/// `Reverse` only ever wraps a `NamedNode` or `NegatedPropertySet`.
///
/// `Reverse` is self-inverse and distributes over the path algebra as:
/// - `Reverse(Reverse(x)) = x`
/// - `Reverse(Sequence(a, b)) = Sequence(Reverse(b), Reverse(a))` (order flips)
/// - `Reverse(Alternative(a, b)) = Alternative(Reverse(a), Reverse(b))`
/// - `Reverse` commutes through `ZeroOrMore`/`OneOrMore`/`ZeroOrOne` unchanged.
fn normalize_reverse(path: &PPE) -> PPE {
    match path {
        PPE::Reverse(inner) => match inner.as_ref() {
            PPE::NamedNode(_) | PPE::NegatedPropertySet(_) => path.clone(),
            PPE::Reverse(inner2) => normalize_reverse(inner2),
            PPE::Sequence(a, b) => PPE::Sequence(
                Box::new(normalize_reverse(&PPE::Reverse(Box::new(
                    b.as_ref().clone(),
                )))),
                Box::new(normalize_reverse(&PPE::Reverse(Box::new(
                    a.as_ref().clone(),
                )))),
            ),
            PPE::Alternative(a, b) => PPE::Alternative(
                Box::new(normalize_reverse(&PPE::Reverse(Box::new(
                    a.as_ref().clone(),
                )))),
                Box::new(normalize_reverse(&PPE::Reverse(Box::new(
                    b.as_ref().clone(),
                )))),
            ),
            PPE::ZeroOrMore(a) => PPE::ZeroOrMore(Box::new(normalize_reverse(&PPE::Reverse(
                Box::new(a.as_ref().clone()),
            )))),
            PPE::OneOrMore(a) => PPE::OneOrMore(Box::new(normalize_reverse(&PPE::Reverse(
                Box::new(a.as_ref().clone()),
            )))),
            PPE::ZeroOrOne(a) => PPE::ZeroOrOne(Box::new(normalize_reverse(&PPE::Reverse(
                Box::new(a.as_ref().clone()),
            )))),
        },
        PPE::Sequence(a, b) => PPE::Sequence(
            Box::new(normalize_reverse(a)),
            Box::new(normalize_reverse(b)),
        ),
        PPE::Alternative(a, b) => PPE::Alternative(
            Box::new(normalize_reverse(a)),
            Box::new(normalize_reverse(b)),
        ),
        PPE::ZeroOrMore(a) => PPE::ZeroOrMore(Box::new(normalize_reverse(a))),
        PPE::OneOrMore(a) => PPE::OneOrMore(Box::new(normalize_reverse(a))),
        PPE::ZeroOrOne(a) => PPE::ZeroOrOne(Box::new(normalize_reverse(a))),
        PPE::NamedNode(_) | PPE::NegatedPropertySet(_) => path.clone(),
    }
}

/// Returns `true` for the simple-predicate path shapes already handled by
/// dedicated fast paths (`ring_pairs_for_pred` / `ring_bfs_transitive` /
/// `ring_bfs_transitive_bound`) — the product automaton should defer to
/// those rather than duplicating the work for these already-optimal cases.
fn is_simple_predicate_path(path: &PPE) -> bool {
    matches!(path, PPE::NamedNode(_))
        || matches!(path, PPE::OneOrMore(inner) if matches!(inner.as_ref(), PPE::NamedNode(_)))
        || matches!(path, PPE::ZeroOrMore(inner) if matches!(inner.as_ref(), PPE::NamedNode(_)))
}

/// Epsilon-closure of a set of NFA states: all states reachable via zero or
/// more `Edge::Eps` transitions.
fn epsilon_closure(nfa: &Nfa, states: &[usize]) -> HashSet<usize> {
    let mut closure: HashSet<usize> = states.iter().copied().collect();
    let mut stack: Vec<usize> = states.to_vec();
    while let Some(s) = stack.pop() {
        for edge in &nfa.edges[s] {
            if let Edge::Eps(t) = edge
                && closure.insert(*t)
            {
                stack.push(*t);
            }
        }
    }
    closure
}

/// BFS over `(node_id, nfa_state)` product pairs starting from `start_id` at
/// `start_state`'s epsilon-closure, returning the set of node ids reached in
/// `accept_state`.
///
/// Each non-epsilon edge is resolved via `lftj_join_scan`: `Fwd`/`Rev` scan a
/// single predicate's successors/predecessors (SPO / OPS-style, O(deg) per
/// step); `NegFwd`/`NegRev` first enumerate the node's distinct predicates
/// (`lftj_join_scan(Some(node), None, None, 1, ag)` — SPO depth-1 — or the
/// object-side equivalent) and skip those in the negated set.
///
/// Returns `None` if Ring neighbor enumeration is unavailable mid-BFS.
fn product_bfs<D: Dataset>(
    dataset: &D,
    nfa: &Nfa,
    start_state: usize,
    accept_state: usize,
    start_id: u64,
    ag: &GraphSelector,
    cancellation: Option<&CancellationToken>,
) -> Option<Result<HashSet<u64>>> {
    let mut visited: HashSet<(u64, usize)> = HashSet::new();
    let mut queue: VecDeque<(u64, usize)> = VecDeque::new();
    let mut accept_ids: HashSet<u64> = HashSet::new();

    let push = |node_id: u64,
                states: HashSet<usize>,
                visited: &mut HashSet<(u64, usize)>,
                queue: &mut VecDeque<(u64, usize)>,
                accept_ids: &mut HashSet<u64>| {
        for st in states {
            if visited.insert((node_id, st)) {
                queue.push_back((node_id, st));
                if st == accept_state {
                    accept_ids.insert(node_id);
                }
            }
        }
    };

    push(
        start_id,
        epsilon_closure(nfa, &[start_state]),
        &mut visited,
        &mut queue,
        &mut accept_ids,
    );

    while let Some((node_id, state)) = queue.pop_front() {
        if is_cancelled(cancellation) {
            return Some(Err(anyhow::Error::from(
                crate::options::EvalLimitError::Cancelled,
            )));
        }

        for edge in &nfa.edges[state] {
            match edge {
                Edge::Eps(_) => {} // handled via epsilon_closure
                Edge::Fwd(pred, next) => {
                    let Some(p_id) = dataset.lftj_intern_term(&Term::NamedNode(pred.clone()), ag)
                    else {
                        continue;
                    };
                    let mut it = dataset.lftj_join_scan(Some(node_id), Some(p_id), None, 2, ag)?;
                    let next_states = epsilon_closure(nfa, &[*next]);
                    while !it.at_end() {
                        push(
                            it.key(),
                            next_states.clone(),
                            &mut visited,
                            &mut queue,
                            &mut accept_ids,
                        );
                        it.advance();
                    }
                }
                Edge::Rev(pred, next) => {
                    let Some(p_id) = dataset.lftj_intern_term(&Term::NamedNode(pred.clone()), ag)
                    else {
                        continue;
                    };
                    let mut it = dataset.lftj_join_scan(None, Some(p_id), Some(node_id), 0, ag)?;
                    let next_states = epsilon_closure(nfa, &[*next]);
                    while !it.at_end() {
                        push(
                            it.key(),
                            next_states.clone(),
                            &mut visited,
                            &mut queue,
                            &mut accept_ids,
                        );
                        it.advance();
                    }
                }
                Edge::NegFwd(excluded, next) => {
                    let excluded_ids: HashSet<u64> = excluded
                        .iter()
                        .filter_map(|p| dataset.lftj_intern_term(&Term::NamedNode(p.clone()), ag))
                        .collect();
                    let mut pit = dataset.lftj_join_scan(Some(node_id), None, None, 1, ag)?;
                    let next_states = epsilon_closure(nfa, &[*next]);
                    while !pit.at_end() {
                        let p_id = pit.key();
                        if !excluded_ids.contains(&p_id) {
                            let mut oit =
                                dataset.lftj_join_scan(Some(node_id), Some(p_id), None, 2, ag)?;
                            while !oit.at_end() {
                                push(
                                    oit.key(),
                                    next_states.clone(),
                                    &mut visited,
                                    &mut queue,
                                    &mut accept_ids,
                                );
                                oit.advance();
                            }
                        }
                        pit.advance();
                    }
                }
                Edge::NegRev(excluded, next) => {
                    let excluded_ids: HashSet<u64> = excluded
                        .iter()
                        .filter_map(|p| dataset.lftj_intern_term(&Term::NamedNode(p.clone()), ag))
                        .collect();
                    let mut pit = dataset.lftj_join_scan(None, None, Some(node_id), 1, ag)?;
                    let next_states = epsilon_closure(nfa, &[*next]);
                    while !pit.at_end() {
                        let p_id = pit.key();
                        if !excluded_ids.contains(&p_id) {
                            let mut sit =
                                dataset.lftj_join_scan(None, Some(p_id), Some(node_id), 0, ag)?;
                            while !sit.at_end() {
                                push(
                                    sit.key(),
                                    next_states.clone(),
                                    &mut visited,
                                    &mut queue,
                                    &mut accept_ids,
                                );
                                sit.advance();
                            }
                        }
                        pit.advance();
                    }
                }
            }
        }
    }

    Some(Ok(accept_ids))
}

/// Evaluate an arbitrary (composed) property-path expression via the
/// product-automaton BFS, given optional bound source/target endpoints.
///
/// - **Source bound** (target bound or not): BFS forward from `source_id`
///   over the compiled automaton, filtering to `target_id` if also bound.
/// - **Target bound only**: BFS forward from `target_id` over the automaton
///   compiled from `normalize_reverse(Reverse(path))` — since
///   `L(Reverse(path)) = { (o, s) : (s, o) ∈ L(path) }`, walking the reversed
///   expression forward from a bound `o` yields exactly the set of `s` with
///   `(s, o) ∈ L(path)`.
/// - **Neither bound**: enumerates all graph nodes as BFS roots. This still
///   avoids the per-operator `Vec<(Term, Term)>` materialization that
///   `path_pairs` performs for composed expressions, even though total work
///   remains O(nodes × closure) in the worst case.
///
/// Returns `None` for the simple single-predicate path shapes already
/// handled by dedicated (cheaper) fast paths — see [`is_simple_predicate_path`].
pub fn ring_eval_rpq<D: Dataset>(
    dataset: &D,
    path: &PPE,
    source_id: Option<u64>,
    target_id: Option<u64>,
    ag: &GraphSelector,
) -> Option<Result<Vec<(Term, Term)>>> {
    ring_eval_rpq_cancellable(dataset, path, source_id, target_id, ag, None)
}

/// [`ring_eval_rpq`] with an optional [`CancellationToken`] checked
/// periodically during the product-automaton BFS (and once per candidate
/// root node in the unbound-endpoints case), so a runaway RPQ over a densely
/// connected graph can be aborted promptly.
#[allow(clippy::too_many_arguments)]
pub fn ring_eval_rpq_cancellable<D: Dataset>(
    dataset: &D,
    path: &PPE,
    source_id: Option<u64>,
    target_id: Option<u64>,
    ag: &GraphSelector,
    cancellation: Option<&CancellationToken>,
) -> Option<Result<Vec<(Term, Term)>>> {
    if is_simple_predicate_path(path) {
        return None;
    }

    match (source_id, target_id) {
        (Some(s), t) => {
            let normalized = normalize_reverse(path);
            let mut nfa = Nfa::new();
            let (start, accept) = nfa.compile(&normalized);
            let accept_ids = match product_bfs(dataset, &nfa, start, accept, s, ag, cancellation)? {
                Ok(ids) => ids,
                Err(e) => return Some(Err(e)),
            };
            let s_term = dataset.lftj_decode_term(s)?;
            let mut pairs = Vec::new();
            for id in accept_ids {
                if let Some(tid) = t
                    && id != tid
                {
                    continue;
                }
                if let Some(term) = dataset.lftj_decode_term(id) {
                    pairs.push((s_term.clone(), term));
                }
            }
            Some(Ok(pairs))
        }
        (None, Some(tid)) => {
            let reversed = normalize_reverse(&PPE::Reverse(Box::new(path.clone())));
            let mut nfa = Nfa::new();
            let (start, accept) = nfa.compile(&reversed);
            let accept_ids = match product_bfs(dataset, &nfa, start, accept, tid, ag, cancellation)?
            {
                Ok(ids) => ids,
                Err(e) => return Some(Err(e)),
            };
            let t_term = dataset.lftj_decode_term(tid)?;
            let mut pairs = Vec::new();
            for id in accept_ids {
                if let Some(term) = dataset.lftj_decode_term(id) {
                    pairs.push((term, t_term.clone()));
                }
            }
            Some(Ok(pairs))
        }
        (None, None) => {
            let normalized = normalize_reverse(path);
            let mut nfa = Nfa::new();
            let (start, accept) = nfa.compile(&normalized);
            let all_ids = match ring_all_node_ids(dataset, ag)? {
                Ok(ids) => ids,
                Err(e) => return Some(Err(e)),
            };
            let mut pairs = Vec::new();
            let mut seen: HashSet<(u64, u64)> = HashSet::new();
            for sid in all_ids {
                if is_cancelled(cancellation) {
                    return Some(Err(anyhow::Error::from(
                        crate::options::EvalLimitError::Cancelled,
                    )));
                }

                let accept_ids =
                    match product_bfs(dataset, &nfa, start, accept, sid, ag, cancellation)? {
                        Ok(ids) => ids,
                        Err(e) => return Some(Err(e)),
                    };
                if accept_ids.is_empty() {
                    continue;
                }
                let Some(s_term) = dataset.lftj_decode_term(sid) else {
                    continue;
                };
                for id in accept_ids {
                    if seen.insert((sid, id))
                        && let Some(term) = dataset.lftj_decode_term(id)
                    {
                        pairs.push((s_term.clone(), term));
                    }
                }
            }
            Some(Ok(pairs))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {

    use super::*;
    use crate::dataset::{GraphSelector, QuadIter, QuadPattern};
    use anyhow::Result;
    use oxigraph_nova_core::{GraphName, Term, TrieIterator};
    use oxrdf::NamedNode;

    // ── Stub Dataset for path unit tests ─────────────────────────────────────

    /// Minimal LFTJ-capable dataset for testing property path helpers.
    ///
    /// Stores triples as `[s_id, p_id, o_id]` and implements `lftj_join_scan`
    /// with a sorted `Vec` filter — semantically equivalent to what the Ring
    /// provides, without requiring the full LOUDS trie stack.
    struct PathStub {
        triples: Vec<[u64; 3]>,
        dict: Vec<(String, u64)>,
    }

    impl PathStub {
        fn new(triples: Vec<[u64; 3]>, dict: Vec<(&str, u64)>) -> Self {
            let mut t = triples;
            t.sort_unstable();
            t.dedup();
            Self {
                triples: t,
                dict: dict.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            }
        }

        fn id_of(&self, uri: &str) -> Option<u64> {
            self.dict.iter().find(|(k, _)| k == uri).map(|(_, v)| *v)
        }
    }

    // ── Trivial TrieIterator over sorted Vec<u64> ─────────────────────────────

    struct VecIter {
        vals: Vec<u64>,
        pos: usize,
    }

    impl TrieIterator for VecIter {
        fn key(&self) -> u64 {
            self.vals[self.pos]
        }
        fn seek(&mut self, t: u64) {
            while self.pos < self.vals.len() && self.vals[self.pos] < t {
                self.pos += 1;
            }
        }
        fn open(&self) -> Box<dyn TrieIterator> {
            Box::new(VecIter {
                vals: vec![],
                pos: 0,
            })
        }
        fn at_end(&self) -> bool {
            self.pos >= self.vals.len()
        }
    }

    impl Dataset for PathStub {
        fn find_quads<'a>(&'a self, _: &QuadPattern) -> Result<QuadIter<'a>> {
            Ok(Box::new(std::iter::empty()))
        }
        fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>> {
            Ok(Box::new(std::iter::empty()))
        }
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
            self.dict
                .iter()
                .find(|(_, v)| *v == id)
                .map(|(k, _)| Term::NamedNode(NamedNode::new_unchecked(k.clone())))
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
                .filter(|t| {
                    s.is_none_or(|sv| t[0] == sv)
                        && p.is_none_or(|pv| t[1] == pv)
                        && o.is_none_or(|ov| t[2] == ov)
                })
                .map(|t| t[target_field])
                .collect();
            vals.sort_unstable();
            vals.dedup();
            Some(Box::new(VecIter { vals, pos: 0 }))
        }
    }

    // ── Fixture ───────────────────────────────────────────────────────────────

    fn n(s: &str) -> NamedNode {
        NamedNode::new_unchecked(s)
    }

    fn t(s: &str) -> Term {
        Term::NamedNode(NamedNode::new_unchecked(s))
    }

    /// Graph: a→b→c→d (chain) with shortcut a→c, all via predicate p (id=20).
    fn chain_stub() -> PathStub {
        PathStub::new(
            vec![[1, 20, 2], [2, 20, 3], [3, 20, 4], [1, 20, 3]],
            vec![
                ("http://ex/a", 1),
                ("http://ex/b", 2),
                ("http://ex/c", 3),
                ("http://ex/d", 4),
                ("http://ex/p", 20),
            ],
        )
    }

    // ── ring_pairs_for_pred ───────────────────────────────────────────────────

    #[test]
    fn ring_pairs_for_pred_basic() {
        let ds = chain_stub();
        let pred = n("http://ex/p");
        let pairs = ring_pairs_for_pred(&ds, &pred, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // Direct edges: (a,b), (a,c), (b,c), (c,d)
        assert_eq!(pairs.len(), 4);
    }

    #[test]
    fn ring_pairs_for_pred_unknown_pred_returns_none() {
        let ds = chain_stub();
        let pred = n("http://ex/unknown");
        // lftj_intern_term returns None → ring_pairs_for_pred returns None (caller uses fallback)
        assert!(ring_pairs_for_pred(&ds, &pred, &GraphSelector::Default).is_none());
    }

    // ── ring_subjects_for_pred ────────────────────────────────────────────────

    #[test]
    fn ring_subjects_for_pred_basic() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let mut subjs = ring_subjects_for_pred(&ds, p_id, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        subjs.sort_unstable();
        // Subjects with predicate p: a(1), b(2), c(3)
        assert_eq!(subjs, vec![1, 2, 3]);
    }

    // ── ring_all_node_ids ─────────────────────────────────────────────────────

    #[test]
    fn ring_all_node_ids_basic() {
        let ds = chain_stub();
        let mut ids = ring_all_node_ids(&ds, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        ids.sort_unstable();
        // Subjects ∪ Objects: a(1), b(2), c(3), d(4)
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    // ── ring_bfs_transitive — OneOrMore (+) ───────────────────────────────────

    #[test]
    fn ring_bfs_one_or_more_chain() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let subjs = ring_subjects_for_pred(&ds, p_id, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        let pairs = ring_bfs_transitive(&ds, p_id, &subjs, false, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // Transitive closure of {a,b,c} via p+:
        // (a,b),(a,c),(a,d),(b,c),(b,d),(c,d) = 6
        assert_eq!(pairs.len(), 6);
    }

    #[test]
    fn ring_bfs_one_or_more_no_identity() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let subjs = ring_subjects_for_pred(&ds, p_id, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        let pairs = ring_bfs_transitive(&ds, p_id, &subjs, false, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // No identity pairs in OneOrMore
        let identity_count = pairs.iter().filter(|(s, o)| s == o).count();
        assert_eq!(identity_count, 0);
    }

    // ── ring_bfs_transitive — ZeroOrMore (*) ──────────────────────────────────

    #[test]
    fn ring_bfs_zero_or_more() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let all_ids = ring_all_node_ids(&ds, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        let pairs = ring_bfs_transitive(&ds, p_id, &all_ids, true, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // Identity pairs: (a,a),(b,b),(c,c),(d,d) = 4
        let identity_count = pairs.iter().filter(|(s, o)| s == o).count();
        assert_eq!(identity_count, 4);
        // Non-identity reachable: (a,b),(a,c),(a,d),(b,c),(b,d),(c,d) = 6
        assert_eq!(pairs.len(), 10);
    }

    // ── ring_bfs_transitive — cycles ──────────────────────────────────────────

    #[test]
    fn ring_bfs_cycle_two_nodes() {
        // Graph: a→b→a (2-cycle via predicate p=20)
        let ds = PathStub::new(
            vec![[1, 20, 2], [2, 20, 1]],
            vec![("http://ex/a", 1), ("http://ex/b", 2), ("http://ex/p", 20)],
        );
        let p_id = ds.id_of("http://ex/p").unwrap();
        let subjs = ring_subjects_for_pred(&ds, p_id, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        let pairs = ring_bfs_transitive(&ds, p_id, &subjs, false, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // (a→b), (a→b→a i.e. a,a), (b→a), (b→a→b i.e. b,b) = 4
        // Cycles: (a,a) and (b,b) are valid for + per SPARQL ALP semantics.
        assert_eq!(pairs.len(), 4);
    }

    #[test]
    fn ring_bfs_empty_graph() {
        let ds = PathStub::new(vec![], vec![("http://ex/p", 20)]);
        let p_id = 20u64;
        let subjs = ring_subjects_for_pred(&ds, p_id, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        assert!(subjs.is_empty());
        let pairs = ring_bfs_transitive(&ds, p_id, &subjs, false, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        assert!(pairs.is_empty());
    }

    // ── ring_bfs_transitive_bound — both endpoints bound ──────────────────────

    #[test]
    fn bound_both_reachable_direct_edge() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let a = ds.id_of("http://ex/a").unwrap();
        let b = ds.id_of("http://ex/b").unwrap();
        let pairs =
            ring_bfs_transitive_bound(&ds, p_id, Some(a), Some(b), false, &GraphSelector::Default)
                .unwrap()
                .unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, t("http://ex/a"));
        assert_eq!(pairs[0].1, t("http://ex/b"));
    }

    #[test]
    fn bound_both_reachable_multi_hop() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let a = ds.id_of("http://ex/a").unwrap();
        let d = ds.id_of("http://ex/d").unwrap();
        // a -> c -> d (2 hops) reachable via p+
        let pairs =
            ring_bfs_transitive_bound(&ds, p_id, Some(a), Some(d), false, &GraphSelector::Default)
                .unwrap()
                .unwrap();
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn bound_both_unreachable() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let d = ds.id_of("http://ex/d").unwrap();
        let a = ds.id_of("http://ex/a").unwrap();
        // d -> a is not reachable (chain only goes forward)
        let pairs =
            ring_bfs_transitive_bound(&ds, p_id, Some(d), Some(a), false, &GraphSelector::Default)
                .unwrap()
                .unwrap();
        assert!(pairs.is_empty());
    }

    #[test]
    fn bound_both_zero_or_more_identity() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let a = ds.id_of("http://ex/a").unwrap();
        // ZeroOrMore (include_identity=true): a to a with no edge traversal
        // should still succeed via the identity pair.
        let pairs =
            ring_bfs_transitive_bound(&ds, p_id, Some(a), Some(a), true, &GraphSelector::Default)
                .unwrap()
                .unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], (t("http://ex/a"), t("http://ex/a")));
    }

    #[test]
    fn bound_both_one_or_more_no_self_identity() {
        // Without a cycle, `a+` bound to itself should NOT match
        // (OneOrMore excludes the trivial zero-length path).
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let a = ds.id_of("http://ex/a").unwrap();
        let pairs =
            ring_bfs_transitive_bound(&ds, p_id, Some(a), Some(a), false, &GraphSelector::Default)
                .unwrap()
                .unwrap();
        assert!(pairs.is_empty());
    }

    #[test]
    fn bound_both_cycle_self_reachable() {
        // Graph: a→b→a (2-cycle). a+ bound to (a,a) should succeed since a
        // cycle brings us back to a via at least one edge.
        let ds = PathStub::new(
            vec![[1, 20, 2], [2, 20, 1]],
            vec![("http://ex/a", 1), ("http://ex/b", 2), ("http://ex/p", 20)],
        );
        let p_id = ds.id_of("http://ex/p").unwrap();
        let a = ds.id_of("http://ex/a").unwrap();
        let pairs =
            ring_bfs_transitive_bound(&ds, p_id, Some(a), Some(a), false, &GraphSelector::Default)
                .unwrap()
                .unwrap();
        assert_eq!(pairs.len(), 1);
    }

    // ── ring_bfs_transitive_bound — only target bound (backward BFS) ──────────

    #[test]
    fn bound_target_only_backward_bfs() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let d = ds.id_of("http://ex/d").unwrap();
        let mut pairs =
            ring_bfs_transitive_bound(&ds, p_id, None, Some(d), false, &GraphSelector::Default)
                .unwrap()
                .unwrap();
        pairs.sort_by_key(|(s, _)| s.to_string());
        // Predecessors of d via p+: a, b, c (a->c->d shortcut and a->b->c->d chain)
        let sources: Vec<String> = pairs.iter().map(|(s, _)| s.to_string()).collect();
        assert_eq!(sources.len(), 3);
        assert!(sources.contains(&n("http://ex/a").to_string()));
        assert!(sources.contains(&n("http://ex/b").to_string()));
        assert!(sources.contains(&n("http://ex/c").to_string()));
    }

    #[test]
    fn bound_target_only_zero_or_more_includes_identity() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let d = ds.id_of("http://ex/d").unwrap();
        let pairs =
            ring_bfs_transitive_bound(&ds, p_id, None, Some(d), true, &GraphSelector::Default)
                .unwrap()
                .unwrap();
        // Includes (d, d) plus a, b, c predecessors = 4
        assert_eq!(pairs.len(), 4);
        assert!(pairs.contains(&(t("http://ex/d"), t("http://ex/d"))));
    }

    // ── ring_bfs_transitive_bound — only source bound (forward BFS) ───────────

    #[test]
    fn bound_source_only_forward_bfs() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        let a = ds.id_of("http://ex/a").unwrap();
        let pairs =
            ring_bfs_transitive_bound(&ds, p_id, Some(a), None, false, &GraphSelector::Default)
                .unwrap()
                .unwrap();
        // a reaches b, c, d via p+
        assert_eq!(pairs.len(), 3);
    }

    // ── ring_bfs_transitive_bound — neither bound ──────────────────────────────

    #[test]
    fn bound_neither_returns_none() {
        let ds = chain_stub();
        let p_id = ds.id_of("http://ex/p").unwrap();
        assert!(
            ring_bfs_transitive_bound(&ds, p_id, None, None, false, &GraphSelector::Default)
                .is_none()
        );
    }

    // ── ring_eval_rpq — product-automaton RPQ evaluation ──────────────────────

    /// Two-predicate graph for Sequence/Alternative/Negated tests:
    /// a --p--> b --q--> c, and a --q--> d (so p/q from a reaches c;
    /// p|q from a reaches b and d; !(p) from a reaches d).
    fn two_pred_stub() -> PathStub {
        PathStub::new(
            vec![[1, 20, 2], [2, 21, 3], [1, 21, 4]],
            vec![
                ("http://ex/a", 1),
                ("http://ex/b", 2),
                ("http://ex/c", 3),
                ("http://ex/d", 4),
                ("http://ex/p", 20),
                ("http://ex/q", 21),
            ],
        )
    }

    fn seq_pq() -> PPE {
        PPE::Sequence(
            Box::new(PPE::NamedNode(n("http://ex/p"))),
            Box::new(PPE::NamedNode(n("http://ex/q"))),
        )
    }

    fn alt_pq() -> PPE {
        PPE::Alternative(
            Box::new(PPE::NamedNode(n("http://ex/p"))),
            Box::new(PPE::NamedNode(n("http://ex/q"))),
        )
    }

    #[test]
    fn rpq_sequence_bound_source() {
        let ds = two_pred_stub();
        let a = ds.id_of("http://ex/a").unwrap();
        let path = seq_pq();
        let pairs = ring_eval_rpq(&ds, &path, Some(a), None, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // a --p--> b --q--> c, so p/q from a reaches only c.
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], (t("http://ex/a"), t("http://ex/c")));
    }

    #[test]
    fn rpq_sequence_bound_target() {
        let ds = two_pred_stub();
        let c = ds.id_of("http://ex/c").unwrap();
        let path = seq_pq();
        let pairs = ring_eval_rpq(&ds, &path, None, Some(c), &GraphSelector::Default)
            .unwrap()
            .unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], (t("http://ex/a"), t("http://ex/c")));
    }

    #[test]
    fn rpq_sequence_bound_both_true() {
        let ds = two_pred_stub();
        let a = ds.id_of("http://ex/a").unwrap();
        let c = ds.id_of("http://ex/c").unwrap();
        let path = seq_pq();
        let pairs = ring_eval_rpq(&ds, &path, Some(a), Some(c), &GraphSelector::Default)
            .unwrap()
            .unwrap();
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn rpq_sequence_bound_both_false() {
        let ds = two_pred_stub();
        let a = ds.id_of("http://ex/a").unwrap();
        let d = ds.id_of("http://ex/d").unwrap();
        let path = seq_pq();
        let pairs = ring_eval_rpq(&ds, &path, Some(a), Some(d), &GraphSelector::Default)
            .unwrap()
            .unwrap();
        assert!(pairs.is_empty());
    }

    #[test]
    fn rpq_sequence_neither_bound() {
        let ds = two_pred_stub();
        let path = seq_pq();
        let pairs = ring_eval_rpq(&ds, &path, None, None, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], (t("http://ex/a"), t("http://ex/c")));
    }

    #[test]
    fn rpq_alternative_bound_source() {
        let ds = two_pred_stub();
        let a = ds.id_of("http://ex/a").unwrap();
        let path = alt_pq();
        let mut pairs = ring_eval_rpq(&ds, &path, Some(a), None, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        pairs.sort_by_key(|(_, o)| o.to_string());
        // a --p--> b, a --q--> d
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], (t("http://ex/a"), t("http://ex/b")));
        assert_eq!(pairs[1], (t("http://ex/a"), t("http://ex/d")));
    }

    #[test]
    fn rpq_negated_property_set_forward() {
        let ds = two_pred_stub();
        let a = ds.id_of("http://ex/a").unwrap();
        // !(p) from a: excludes predicate p, leaving only the q-edge a->d.
        let path = PPE::NegatedPropertySet(vec![n("http://ex/p")]);
        let pairs = ring_eval_rpq(&ds, &path, Some(a), None, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], (t("http://ex/a"), t("http://ex/d")));
    }

    #[test]
    fn rpq_negated_property_set_reverse() {
        let ds = two_pred_stub();
        let d = ds.id_of("http://ex/d").unwrap();
        // ^(!(p)) i.e. Reverse(NegatedPropertySet([p])) bound at target d:
        // walking backward over "any predicate except p" from d finds a
        // (since a --q--> d and q is not excluded).
        let path = PPE::Reverse(Box::new(PPE::NegatedPropertySet(vec![n("http://ex/p")])));
        let pairs = ring_eval_rpq(&ds, &path, Some(d), None, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], (t("http://ex/d"), t("http://ex/a")));
    }

    #[test]
    fn rpq_nested_kleene_star_over_sequence() {
        // Wrap Sequence(p,p) (two p-hops) inside OneOrMore to confirm nested
        // composition + Kleene star works through the product automaton
        // (chain_stub isn't a simple-predicate path once wrapped in
        // Sequence, so it goes through ring_eval_rpq).
        let ds = chain_stub();
        let p = n("http://ex/p");
        let two_hop = PPE::Sequence(
            Box::new(PPE::NamedNode(p.clone())),
            Box::new(PPE::NamedNode(p)),
        );
        let path = PPE::OneOrMore(Box::new(two_hop));
        let a = ds.id_of("http://ex/a").unwrap();
        let pairs = ring_eval_rpq(&ds, &path, Some(a), None, &GraphSelector::Default)
            .unwrap()
            .unwrap();
        // a--p->b--p->c (2 hops) = c; a--p->c--p->d (via shortcut a->c) = d.
        let mut targets: Vec<String> = pairs.iter().map(|(_, o)| o.to_string()).collect();
        targets.sort();
        assert!(targets.contains(&t("http://ex/c").to_string()));
        assert!(targets.contains(&t("http://ex/d").to_string()));
    }

    #[test]
    fn normalize_reverse_sequence_flips_order() {
        // ^(p/q) should be equivalent to ^q/^p.
        let path = PPE::Reverse(Box::new(seq_pq()));
        let normalized = normalize_reverse(&path);
        let expected = PPE::Sequence(
            Box::new(PPE::Reverse(Box::new(PPE::NamedNode(n("http://ex/q"))))),
            Box::new(PPE::Reverse(Box::new(PPE::NamedNode(n("http://ex/p"))))),
        );
        assert_eq!(format!("{normalized:?}"), format!("{expected:?}"));
    }

    #[test]
    fn is_simple_predicate_path_defers_to_dedicated_fast_paths() {
        assert!(is_simple_predicate_path(&PPE::NamedNode(n("http://ex/p"))));
        assert!(is_simple_predicate_path(&PPE::OneOrMore(Box::new(
            PPE::NamedNode(n("http://ex/p"))
        ))));
        assert!(is_simple_predicate_path(&PPE::ZeroOrMore(Box::new(
            PPE::NamedNode(n("http://ex/p"))
        ))));
        assert!(!is_simple_predicate_path(&seq_pq()));
        assert!(!is_simple_predicate_path(&alt_pq()));
    }

    #[test]
    fn ring_eval_rpq_returns_none_for_simple_predicate_path() {
        let ds = chain_stub();
        let path = PPE::NamedNode(n("http://ex/p"));
        assert!(ring_eval_rpq(&ds, &path, None, None, &GraphSelector::Default).is_none());
    }
}
