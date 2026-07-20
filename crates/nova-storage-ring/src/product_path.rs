//! E5.11 → SPARQL product wire (W0): flags + path counters.
//!
//! Env (pilot defaults **on** unless noted):
//! - `NOVA_RING_MMAP=0|1` — materialize `NOVARNG1` after compact/bulk_load
//! - `NOVA_RING_D2=0|1` — allow D2 triangle iterator when pattern matches
//! - `NOVA_RING_COUNTERS=1` — sparse path-counter logs (first open + every 1M opens).
//!   Counters always accumulate. **Never** enable verbose-per-open logging:
//!   path_2hop opens millions of scans and I/O will dominate latency.
//! - `NOVA_RING_VEO_OLD_HEURISTIC=1` — A/B only: MiddleRuns VEO uses row-span
//!   heuristic (pre-exact-run fix). Default **off** (exact distinct-run with budget).
//! - `NOVA_RING_KEEP_HEAP=1` — keep heap after mmap (default **off**; Phase 1A).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Process-wide SPARQL/LFTJ path counters for RingStore.
#[derive(Default)]
pub struct SparqlPathCounters {
    pub join_scan_open: AtomicU64,
    pub path_mapped_rdi: AtomicU64,
    pub path_heap_rnv: AtomicU64,
    pub path_middle_runs: AtomicU64,
    pub path_singleton: AtomicU64,
    pub d2_calls: AtomicU64,
    pub d2_hits: AtomicU64,
    pub decode_calls: AtomicU64,
    pub mmap_materialize_ok: AtomicU64,
    pub mmap_materialize_fail: AtomicU64,
    /// VEO estimate probes (every `lftj_real_count` / estimate path).
    pub veo_estimate_calls: AtomicU64,
    /// MiddleRuns used exact distinct-run walk (within budget).
    pub veo_middle_exact: AtomicU64,
    /// MiddleRuns fell back to row-span/vocab heuristic (budget or old-heuristic flag).
    pub veo_middle_fallback: AtomicU64,
    /// Cumulative nanoseconds spent in estimate_join_count (planning only).
    pub veo_plan_ns: AtomicU64,
}

impl SparqlPathCounters {
    pub fn reset(&self) {
        for a in [
            &self.join_scan_open,
            &self.path_mapped_rdi,
            &self.path_heap_rnv,
            &self.path_middle_runs,
            &self.path_singleton,
            &self.d2_calls,
            &self.d2_hits,
            &self.decode_calls,
            &self.mmap_materialize_ok,
            &self.mmap_materialize_fail,
            &self.veo_estimate_calls,
            &self.veo_middle_exact,
            &self.veo_middle_fallback,
            &self.veo_plan_ns,
        ] {
            a.store(0, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> SparqlPathSnapshot {
        SparqlPathSnapshot {
            join_scan_open: self.join_scan_open.load(Ordering::Relaxed),
            path_mapped_rdi: self.path_mapped_rdi.load(Ordering::Relaxed),
            path_heap_rnv: self.path_heap_rnv.load(Ordering::Relaxed),
            path_middle_runs: self.path_middle_runs.load(Ordering::Relaxed),
            path_singleton: self.path_singleton.load(Ordering::Relaxed),
            d2_calls: self.d2_calls.load(Ordering::Relaxed),
            d2_hits: self.d2_hits.load(Ordering::Relaxed),
            decode_calls: self.decode_calls.load(Ordering::Relaxed),
            mmap_materialize_ok: self.mmap_materialize_ok.load(Ordering::Relaxed),
            mmap_materialize_fail: self.mmap_materialize_fail.load(Ordering::Relaxed),
            veo_estimate_calls: self.veo_estimate_calls.load(Ordering::Relaxed),
            veo_middle_exact: self.veo_middle_exact.load(Ordering::Relaxed),
            veo_middle_fallback: self.veo_middle_fallback.load(Ordering::Relaxed),
            veo_plan_ns: self.veo_plan_ns.load(Ordering::Relaxed),
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct SparqlPathSnapshot {
    pub join_scan_open: u64,
    pub path_mapped_rdi: u64,
    pub path_heap_rnv: u64,
    pub path_middle_runs: u64,
    pub path_singleton: u64,
    pub d2_calls: u64,
    pub d2_hits: u64,
    pub decode_calls: u64,
    pub mmap_materialize_ok: u64,
    pub mmap_materialize_fail: u64,
    pub veo_estimate_calls: u64,
    pub veo_middle_exact: u64,
    pub veo_middle_fallback: u64,
    pub veo_plan_ns: u64,
}

/// Global counters for the Ring SPARQL product path.
pub static SPARQL_PATH: SparqlPathCounters = SparqlPathCounters {
    join_scan_open: AtomicU64::new(0),
    path_mapped_rdi: AtomicU64::new(0),
    path_heap_rnv: AtomicU64::new(0),
    path_middle_runs: AtomicU64::new(0),
    path_singleton: AtomicU64::new(0),
    d2_calls: AtomicU64::new(0),
    d2_hits: AtomicU64::new(0),
    decode_calls: AtomicU64::new(0),
    mmap_materialize_ok: AtomicU64::new(0),
    mmap_materialize_fail: AtomicU64::new(0),
    veo_estimate_calls: AtomicU64::new(0),
    veo_middle_exact: AtomicU64::new(0),
    veo_middle_fallback: AtomicU64::new(0),
    veo_plan_ns: AtomicU64::new(0),
};

static MMAP_FAIL_LOGGED: AtomicBool = AtomicBool::new(false);

#[inline]
fn env_flag_default_on(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let v = v.trim();
            !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
        }
        Err(_) => true,
    }
}

/// `NOVA_RING_MMAP` — default **on** for pilot server / compact path.
#[inline]
pub fn ring_mmap_enabled() -> bool {
    env_flag_default_on("NOVA_RING_MMAP")
}

/// `NOVA_RING_D2` — default **on**; D2 requires mmap image.
#[inline]
pub fn ring_d2_enabled() -> bool {
    env_flag_default_on("NOVA_RING_D2")
}

/// `NOVA_RING_KEEP_HEAP` — default **off**. When on, `materialize_mapped` retains
/// the heap CyclicRing alongside mmap (differentials / A/B). Product default
/// drops heap after successful mmap (Phase 1A single residency).
#[inline]
pub fn ring_keep_heap() -> bool {
    match std::env::var("NOVA_RING_KEEP_HEAP") {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
        }
        Err(_) => false,
    }
}

/// Optional verbose counter logging.
#[inline]
pub fn ring_counters_log_enabled() -> bool {
    match std::env::var("NOVA_RING_COUNTERS") {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
        }
        Err(_) => false,
    }
}

/// A/B: force pre-fix MiddleRuns VEO heuristic (row-span). Default **off**.
///
/// Use only for Phase-1 comparison: Ring old VEO vs corrected VEO vs LOUDS.
#[inline]
pub fn ring_veo_old_heuristic() -> bool {
    match std::env::var("NOVA_RING_VEO_OLD_HEURISTIC") {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
        }
        Err(_) => false,
    }
}

/// Max distinct runs walked in one exact MiddleRuns VEO probe before falling
/// back to the row-span heuristic. Bounds planning cost on high-cardinality middles.
pub const VEO_MIDDLE_EXACT_RUN_BUDGET: u64 = 4_096;

/// Temp dir for per-graph `NOVARNG1` images (process-scoped).
pub fn ring_image_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("nova-ring-{}", std::process::id()))
}

/// Log mmap materialize failure at most once per process.
pub fn log_mmap_fail_once(err: &dyn std::fmt::Display) {
    if !MMAP_FAIL_LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("nova-ring: NOVARNG1 mmap materialize failed (heap fallback): {err}");
    }
    SPARQL_PATH
        .mmap_materialize_fail
        .fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn bump_mmap_ok() {
    SPARQL_PATH
        .mmap_materialize_ok
        .fetch_add(1, Ordering::Relaxed);
}
