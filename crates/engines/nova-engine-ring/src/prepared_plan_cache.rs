//! product prepared physical-operator cache.
//!
//! Holds specialized BGP physical plans (two-hop, wedge, …) across evaluations
//! so repeated SPARQL/HTTP requests do not rebuild adjacency every `evaluate()`.
//!
//! ## Pipeline
//!
//! ```text
//! BGP → shape recognizer → physical operator → prepared operator → reusable exec
//! ```
//!
//! Two-hop and wedge are variants of one [`PhysicalOpKind`] /
//! [`PreparedPhysicalOp`] space. Future stars, chains, diamonds extend the same
//! key + enum without a second cache.
//!
//! ## Cache identity
//!
//! Key = (snapshot_version, graph_id, kind, adj_mode).
//! Store mutations / compact bump the snapshot version and invalidate.
//!
//! ## Concurrency
//!
//! Plans are not `Sync` (mutable hop cursors). The cache stores them behind
//! `Mutex` and hands out a short-lived guard. LRU capacity is small (default 32).

use crate::image::BraidedGraphImage;
use crate::product_path::{
    PredAdjacencyMode, SPARQL_PATH, TimingBucket, add_timing_ns, effective_pred_adjacency_mode,
    effective_prepared_plan_cache_enabled,
};
use crate::scan::{
    PredicateAdjacency, PreparedDirectedTriangleImpl, PreparedKChainImpl, PreparedSpExpansionImpl,
    PreparedSpObjectScanImpl, PreparedStarImpl, PreparedTwoHopImpl, PreparedWedgeImpl,
};
use oxigraph_nova_core::{
    PreparedDirectedTriangle, PreparedKChain, PreparedPhysicalOperator, PreparedSpExpansion,
    PreparedStar, PreparedTwoHop, PreparedWedge,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Default LRU capacity (bounded; not a general prepared-statement framework).
pub const PREPARED_PLAN_CACHE_CAP: usize = 32;

// ── Shape identity ───────────────────────────────────────────────────────────

/// Discriminator for specialized BGP physical operators.
///
/// Extending the product path for a new motif means adding a variant here and
/// a prepare branch — not a second cache type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PhysicalOpKind {
    /// Chain: `?a P1 ?b. ?b P2 ?c`.
    TwoHop {
        p1: u64,
        p2: u64,
    },
    /// Wedge / triangle: `?a P ?b. ?b P ?c. ?a P ?c`.
    Wedge {
        pred: u64,
    },
    /// SP-expansion / 2join: `?s P_filter O_filter . ?s P_expand ?o`.
    SpExpansion {
        p_filter: u64,
        o_filter: u64,
        p_expand: u64,
    },
    /// 3-hop chain: `?a P1 ?b . ?b P2 ?c . ?c P3 ?d`.
    KChain {
        p1: u64,
        p2: u64,
        p3: u64,
    },
    Star {
        p1: u64,
        p2: u64,
        p3: u64,
    },
    /// Directed 3-cycle: `?a P1 ?b . ?b P2 ?c . ?c P3 ?a`.
    DirectedTriangle {
        p1: u64,
        p2: u64,
        p3: u64,
    },
}

/// Concrete prepared bodies owned by the cache.
pub enum PreparedPhysicalOp {
    TwoHop(PreparedTwoHopImpl),
    Wedge(PreparedWedgeImpl),
    SpExpansion(PreparedSpExpansionImpl),
    KChain(PreparedKChainImpl),
    Star(PreparedStarImpl),
    DirectedTriangle(PreparedDirectedTriangleImpl),
}

impl PreparedPhysicalOperator for PreparedPhysicalOp {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        match self {
            PreparedPhysicalOp::TwoHop(p) => p.execute(emit),
            PreparedPhysicalOp::Wedge(p) => p.execute(emit),
            PreparedPhysicalOp::SpExpansion(p) => p.execute(emit),
            PreparedPhysicalOp::KChain(p) => p.execute(emit),
            PreparedPhysicalOp::Star(p) => p.execute(emit),
            PreparedPhysicalOp::DirectedTriangle(p) => p.execute(emit),
        }
    }
}

// ── Cache key ────────────────────────────────────────────────────────────────

/// Unified cache key for any prepared physical operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PhysicalOpPlanKey {
    /// RingStore compact generation / snapshot version.
    pub snapshot_version: u64,
    pub graph_id: u8,
    pub kind: PhysicalOpKind,
    /// Execution mode that affects the plan body (adjacency build mode).
    pub adj_mode: u8,
}

impl PhysicalOpPlanKey {
    #[inline]
    pub fn adj_mode_tag(mode: PredAdjacencyMode) -> u8 {
        match mode {
            PredAdjacencyMode::Off => 0,
            PredAdjacencyMode::Eager => 1,
            PredAdjacencyMode::Native => 2,
        }
    }

    #[inline]
    pub fn two_hop(snapshot_version: u64, graph_id: u8, p1: u64, p2: u64, adj_mode: u8) -> Self {
        Self {
            snapshot_version,
            graph_id,
            kind: PhysicalOpKind::TwoHop { p1, p2 },
            adj_mode,
        }
    }

    #[inline]
    pub fn wedge(snapshot_version: u64, graph_id: u8, pred: u64, adj_mode: u8) -> Self {
        Self {
            snapshot_version,
            graph_id,
            kind: PhysicalOpKind::Wedge { pred },
            adj_mode,
        }
    }

    #[inline]
    pub fn sp_expansion(
        snapshot_version: u64,
        graph_id: u8,
        p_filter: u64,
        o_filter: u64,
        p_expand: u64,
        adj_mode: u8,
    ) -> Self {
        Self {
            snapshot_version,
            graph_id,
            kind: PhysicalOpKind::SpExpansion {
                p_filter,
                o_filter,
                p_expand,
            },
            adj_mode,
        }
    }

    #[inline]
    pub fn k_chain(
        snapshot_version: u64,
        graph_id: u8,
        p1: u64,
        p2: u64,
        p3: u64,
        adj_mode: u8,
    ) -> Self {
        Self {
            snapshot_version,
            graph_id,
            kind: PhysicalOpKind::KChain { p1, p2, p3 },
            adj_mode,
        }
    }

    #[inline]
    pub fn star(
        snapshot_version: u64,
        graph_id: u8,
        p1: u64,
        p2: u64,
        p3: u64,
        adj_mode: u8,
    ) -> Self {
        Self {
            snapshot_version,
            graph_id,
            kind: PhysicalOpKind::Star { p1, p2, p3 },
            adj_mode,
        }
    }

    #[inline]
    pub fn directed_triangle(
        snapshot_version: u64,
        graph_id: u8,
        p1: u64,
        p2: u64,
        p3: u64,
        adj_mode: u8,
    ) -> Self {
        Self {
            snapshot_version,
            graph_id,
            kind: PhysicalOpKind::DirectedTriangle { p1, p2, p3 },
            adj_mode,
        }
    }
}

/// Historical two-hop key shape. Convertible to [`PhysicalOpPlanKey`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TwoHopPlanKey {
    pub snapshot_version: u64,
    pub graph_id: u8,
    pub p1: u64,
    pub p2: u64,
    pub adj_mode: u8,
}

impl TwoHopPlanKey {
    #[inline]
    pub fn adj_mode_tag(mode: PredAdjacencyMode) -> u8 {
        PhysicalOpPlanKey::adj_mode_tag(mode)
    }
}

impl From<TwoHopPlanKey> for PhysicalOpPlanKey {
    fn from(k: TwoHopPlanKey) -> Self {
        PhysicalOpPlanKey::two_hop(k.snapshot_version, k.graph_id, k.p1, k.p2, k.adj_mode)
    }
}

/// Historical wedge key shape. Convertible to [`PhysicalOpPlanKey`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WedgePlanKey {
    pub snapshot_version: u64,
    pub graph_id: u8,
    pub pred: u64,
    pub adj_mode: u8,
}

impl WedgePlanKey {
    #[inline]
    pub fn adj_mode_tag(mode: PredAdjacencyMode) -> u8 {
        PhysicalOpPlanKey::adj_mode_tag(mode)
    }
}

impl From<WedgePlanKey> for PhysicalOpPlanKey {
    fn from(k: WedgePlanKey) -> Self {
        PhysicalOpPlanKey::wedge(k.snapshot_version, k.graph_id, k.pred, k.adj_mode)
    }
}

// ── Unified LRU cache ────────────────────────────────────────────────────────

struct CacheEntry {
    plan: PreparedPhysicalOp,
}

/// Small mutex-guarded LRU of prepared physical operators.
pub struct PhysicalOpPreparedPlanCache {
    cap: usize,
    map: HashMap<PhysicalOpPlanKey, CacheEntry>,
    /// Most-recently-used at the back.
    order: Vec<PhysicalOpPlanKey>,
}

impl PhysicalOpPreparedPlanCache {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            map: HashMap::new(),
            order: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&mut self) {
        let n = self.map.len() as u64;
        self.map.clear();
        self.order.clear();
        if n > 0 {
            SPARQL_PATH
                .prepared_plan_cache_invalidate
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn touch(&mut self, key: &PhysicalOpPlanKey) {
        if let Some(i) = self.order.iter().position(|k| k == key) {
            let k = self.order.remove(i);
            self.order.push(k);
        }
    }

    fn evict_if_needed(&mut self) {
        while self.map.len() > self.cap {
            if let Some(old) = self.order.first().copied() {
                self.order.remove(0);
                self.map.remove(&old);
            } else {
                break;
            }
        }
    }

    /// Take plan out for exclusive execute; caller must [`put_back`].
    pub fn take(&mut self, key: &PhysicalOpPlanKey) -> Option<PreparedPhysicalOp> {
        if !self.map.contains_key(key) {
            return None;
        }
        let entry = self.map.remove(key)?;
        if let Some(i) = self.order.iter().position(|k| k == key) {
            self.order.remove(i);
        }
        Some(entry.plan)
    }

    /// Return a plan after execute (does not bump insert counter).
    pub fn put_back(&mut self, key: PhysicalOpPlanKey, plan: PreparedPhysicalOp) {
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.map.entry(key) {
            e.insert(CacheEntry { plan });
            self.touch(&key);
            return;
        }
        self.map.insert(key, CacheEntry { plan });
        self.order.push(key);
        self.evict_if_needed();
    }
}

/// name for the unified cache (same type).
pub type TwoHopPreparedPlanCache = PhysicalOpPreparedPlanCache;
/// name for the unified cache (same type).
pub type WedgePreparedPlanCache = PhysicalOpPreparedPlanCache;

// ── Guard ────────────────────────────────────────────────────────────────────

/// Guard that owns a prepared physical op for one execute, then returns it.
pub struct CachedPhysicalOpGuard {
    key: PhysicalOpPlanKey,
    plan: Option<PreparedPhysicalOp>,
    cache: Arc<Mutex<PhysicalOpPreparedPlanCache>>,
    return_to_cache: bool,
}

impl Drop for CachedPhysicalOpGuard {
    fn drop(&mut self) {
        if let Some(plan) = self.plan.take()
            && self.return_to_cache
        {
            self.cache.lock().put_back(self.key, plan);
        }
    }
}

impl PreparedPhysicalOperator for CachedPhysicalOpGuard {
    fn execute(&mut self, emit: &mut dyn FnMut(&[u64]) -> Result<(), ()>) -> Result<u64, ()> {
        let plan = self.plan.as_mut().expect("plan present during execute");
        plan.execute(emit)
    }
}

/// Historical alias.
pub type CachedTwoHopGuard = CachedPhysicalOpGuard;
/// Historical alias.
pub type CachedWedgeGuard = CachedPhysicalOpGuard;

// ── get_or_prepare ───────────────────────────────────────────────────────────

fn cache_lookup_or_prepare(
    cache: &Arc<Mutex<PhysicalOpPreparedPlanCache>>,
    key: PhysicalOpPlanKey,
    prepare: impl FnOnce() -> Option<PreparedPhysicalOp>,
) -> Option<Box<dyn PreparedPhysicalOperator>> {
    let cache_on = effective_prepared_plan_cache_enabled();
    if cache_on {
        let mut g = cache.lock();
        if let Some(plan) = g.take(&key) {
            SPARQL_PATH
                .prepared_plan_cache_hit
                .fetch_add(1, Ordering::Relaxed);
            drop(g);
            return Some(Box::new(CachedPhysicalOpGuard {
                key,
                plan: Some(plan),
                cache: Arc::clone(cache),
                return_to_cache: true,
            }));
        }
        SPARQL_PATH
            .prepared_plan_cache_miss
            .fetch_add(1, Ordering::Relaxed);
        drop(g);
    } else {
        // Rung A: cache forced off — still count misses for diagnostics.
        SPARQL_PATH
            .prepared_plan_cache_miss
            .fetch_add(1, Ordering::Relaxed);
    }

    let t0 = std::time::Instant::now();
    let plan = prepare()?;
    add_timing_ns(
        TimingBucket::PhysicalPrepare,
        t0.elapsed().as_nanos() as u64,
    );

    if cache_on {
        SPARQL_PATH
            .prepared_plan_cache_insert
            .fetch_add(1, Ordering::Relaxed);
        return Some(Box::new(CachedPhysicalOpGuard {
            key,
            plan: Some(plan),
            cache: Arc::clone(cache),
            return_to_cache: true,
        }));
    }

    Some(Box::new(plan))
}

/// Resolve or prepare a two-hop plan, optionally via the product cache.
pub fn get_or_prepare_two_hop(
    cache: &Arc<Mutex<PhysicalOpPreparedPlanCache>>,
    snapshot_version: u64,
    graph_id: u8,
    img: Arc<BraidedGraphImage>,
    p1: u64,
    p2: u64,
) -> Option<Box<dyn PreparedTwoHop>> {
    let mode = effective_pred_adjacency_mode();
    let key = PhysicalOpPlanKey::two_hop(
        snapshot_version,
        graph_id,
        p1,
        p2,
        PhysicalOpPlanKey::adj_mode_tag(mode),
    );
    cache_lookup_or_prepare(cache, key, || {
        PreparedTwoHopImpl::prepare(img, p1, p2).map(PreparedPhysicalOp::TwoHop)
    })
}

/// Resolve or prepare a wedge plan, optionally via the product cache.
pub fn get_or_prepare_wedge(
    cache: &Arc<Mutex<PhysicalOpPreparedPlanCache>>,
    snapshot_version: u64,
    graph_id: u8,
    img: Arc<BraidedGraphImage>,
    predicate: u64,
) -> Option<Box<dyn PreparedWedge>> {
    let mode = effective_pred_adjacency_mode();
    let key = PhysicalOpPlanKey::wedge(
        snapshot_version,
        graph_id,
        predicate,
        PhysicalOpPlanKey::adj_mode_tag(mode),
    );
    cache_lookup_or_prepare(cache, key, || {
        PreparedWedgeImpl::prepare(img, predicate).map(PreparedPhysicalOp::Wedge)
    })
}

/// Resolve or prepare an SP-expansion / 2join plan via the product cache.
///
/// `expand_adj` is the optional shared adjacency for `p_expand` (from the
/// store-level SpAdjCache). When `None`, prepare builds adj once on cold miss.
pub fn get_or_prepare_sp_expansion(
    cache: &Arc<Mutex<PhysicalOpPreparedPlanCache>>,
    snapshot_version: u64,
    graph_id: u8,
    img: Arc<BraidedGraphImage>,
    p_filter: u64,
    o_filter: u64,
    p_expand: u64,
    expand_adj: Option<Arc<PredicateAdjacency>>,
) -> Option<Box<dyn PreparedSpExpansion>> {
    let mode = effective_pred_adjacency_mode();
    let key = PhysicalOpPlanKey::sp_expansion(
        snapshot_version,
        graph_id,
        p_filter,
        o_filter,
        p_expand,
        PhysicalOpPlanKey::adj_mode_tag(mode),
    );
    // Prefer shared adj from SpAdjCache; on cold miss also seed it via build.
    let adj = expand_adj.or_else(|| PreparedSpObjectScanImpl::build_shared_adj(&img, p_expand));
    cache_lookup_or_prepare(cache, key, || {
        PreparedSpExpansionImpl::prepare(img, p_filter, o_filter, p_expand, adj)
            .map(PreparedPhysicalOp::SpExpansion)
    })
}

/// Resolve or prepare a k-chain (k=3) plan via the product cache.
pub fn get_or_prepare_k_chain(
    cache: &Arc<Mutex<PhysicalOpPreparedPlanCache>>,
    snapshot_version: u64,
    graph_id: u8,
    img: Arc<BraidedGraphImage>,
    p1: u64,
    p2: u64,
    p3: u64,
) -> Option<Box<dyn PreparedKChain>> {
    let mode = effective_pred_adjacency_mode();
    let key = PhysicalOpPlanKey::k_chain(
        snapshot_version,
        graph_id,
        p1,
        p2,
        p3,
        PhysicalOpPlanKey::adj_mode_tag(mode),
    );
    cache_lookup_or_prepare(cache, key, || {
        PreparedKChainImpl::prepare(img, p1, p2, p3).map(PreparedPhysicalOp::KChain)
    })
}

/// Resolve or prepare a subject-star (k=3) plan via the product cache.
pub fn get_or_prepare_star(
    cache: &Arc<Mutex<PhysicalOpPreparedPlanCache>>,
    snapshot_version: u64,
    graph_id: u8,
    img: Arc<BraidedGraphImage>,
    p1: u64,
    p2: u64,
    p3: u64,
) -> Option<Box<dyn PreparedStar>> {
    let mode = effective_pred_adjacency_mode();
    let key = PhysicalOpPlanKey::star(
        snapshot_version,
        graph_id,
        p1,
        p2,
        p3,
        PhysicalOpPlanKey::adj_mode_tag(mode),
    );
    cache_lookup_or_prepare(cache, key, || {
        PreparedStarImpl::prepare(img, p1, p2, p3).map(PreparedPhysicalOp::Star)
    })
}

/// Resolve or prepare a directed-triangle plan via the product cache.
pub fn get_or_prepare_directed_triangle(
    cache: &Arc<Mutex<PhysicalOpPreparedPlanCache>>,
    snapshot_version: u64,
    graph_id: u8,
    img: Arc<BraidedGraphImage>,
    p1: u64,
    p2: u64,
    p3: u64,
) -> Option<Box<dyn PreparedDirectedTriangle>> {
    let mode = effective_pred_adjacency_mode();
    let key = PhysicalOpPlanKey::directed_triangle(
        snapshot_version,
        graph_id,
        p1,
        p2,
        p3,
        PhysicalOpPlanKey::adj_mode_tag(mode),
    );
    cache_lookup_or_prepare(cache, key, || {
        PreparedDirectedTriangleImpl::prepare(img, p1, p2, p3)
            .map(PreparedPhysicalOp::DirectedTriangle)
    })
}
