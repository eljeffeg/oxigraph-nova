//! SPARQL product-path wire: env flags + path counters.
//!
//! Env (pilot defaults **on** unless noted):
//! - `NOVA_RING_MMAP=0|1` — materialize `NOVARNG1` after compact/bulk_load
//! - `NOVA_RING_D2=0|1` — allow D2 triangle iterator when pattern matches
//! - `NOVA_RING_COUNTERS=1` — sparse path-counter logs (first open + every 1M opens).
//!   Counters always accumulate. **Never** enable verbose-per-open logging:
//!   path_2hop opens millions of scans and I/O will dominate latency.
//! - `NOVA_RING_VEO_OLD_HEURISTIC=1` — A/B only: MiddleRuns VEO uses row-span
//!   heuristic (pre-exact-run fix). Default **off** (exact distinct-run with budget).
//! - `NOVA_RING_LASTCOL_POLICY` — LastCol open policy (see [`lastcol_scan_policy`]).
//!   Product default is **MappedRdi** for enumerate-all opens.
//! - `NOVA_RING_D1_TINY_MERGE` — D1 tiny-range merge threshold T (see
//!   [`d1_tiny_merge_threshold`]). Default **0** (braided D1 only on generic
//!   multi_subject path). Prepared wedge closes use
//!   [`wedge_left_once_threshold`] (default **4**) independently.
//! - `NOVA_RING_D1_TINY_STRATEGY` — algorithm when T fires (see
//!   [`d1_tiny_strategy`]). Default **merge** (buffered dual materialize).
//!   Experimental: `nested` / `probe` / `fixed` / `fused` (max-len≤4 kernels).
//! - `NOVA_RING_WEDGE_LEFT_ONCE` — PreparedWedge SP-D1 close tiny-merge threshold
//!   (see [`wedge_left_once_threshold`]). Default **4** (enabled on product
//!   triangle). Does **not** change generic [`BraidedD1ObjectScan`] / global T=0.

//! `NOVA_RING_D1_ASYM` — large-range asymmetric D1 kernel (see
//!   [`d1_asym_mode`]). Default **off** (braided residual). Experimental A/B
//!   only; does not change product defaults.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Process-wide SPARQL/LFTJ path counters for RingStore.
#[derive(Default)]
pub struct SparqlPathCounters {
    pub join_scan_open: AtomicU64,
    pub path_mapped_rdi: AtomicU64,
    /// Mapped RNV open (AlwaysMappedRnv policy / forced seek-successor open).
    pub path_mapped_rnv: AtomicU64,
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
    // ── D1 construction breakdown (product wedge / multi_subject) ────────
    /// Times a D1 (or prepared D1) open was attempted.
    pub d1_open_calls: AtomicU64,
    /// Cumulative ns for full D1 open (remap + range_sp + first next).
    pub d1_open_ns: AtomicU64,
    /// Cumulative ns spent densifying subject/pred TermIds on D1 open.
    pub d1_remap_ns: AtomicU64,
    /// Cumulative ns deriving SP (or S) ranges on D1 open.
    pub d1_range_sp_ns: AtomicU64,
    /// Cumulative ns for first `intersection_next_value2` on open.
    pub d1_first_next_ns: AtomicU64,
    /// Times left SP(a,P) was reused across a right subject under same a.
    pub d1_left_range_reuse: AtomicU64,
    /// Times a prepared fixed-P context was built (once per wedge query).
    pub d1_pred_prepare: AtomicU64,
    // ── two-hop path (prepared SP scanners / range reuse) ──────────────
    /// Times a prepared SP→O scanner was built (once per hop predicate).
    pub k9_sp_prepare: AtomicU64,
    /// Times `reset_to_subject` was called on a prepared SP scanner.
    pub k9_sp_reset: AtomicU64,
    /// Times reset found an empty SP range.
    pub k9_sp_empty_range: AtomicU64,
    /// Cumulative ns spent densifying subjects on SP reset.
    pub k9_sp_remap_ns: AtomicU64,
    /// Cumulative ns deriving SP ranges on reset.
    pub k9_sp_range_ns: AtomicU64,
    /// Cumulative ns rebinding the LastCol cursor after range derivation.
    pub k9_sp_cursor_ns: AtomicU64,
    /// Objects emitted by prepared SP scanners (iteration work).
    pub k9_sp_values_emitted: AtomicU64,
    /// Times a PreparedTwoHop plan was built.
    pub k9_two_hop_prepare: AtomicU64,
    /// Times PreparedTwoHop::execute ran.
    pub k9_two_hop_execute: AtomicU64,
    /// Result triples emitted by PreparedTwoHop.
    pub k9_two_hop_rows: AtomicU64,
    /// Second-hop (P2) resets / range lookups (`k9_second_hop_resets`).
    pub k9_p2_range_lookups: AtomicU64,
    /// Unique middle-node b subjects on hop2 (`k9_second_hop_unique_subjects`).
    pub k9_p2_unique_b: AtomicU64,
    /// Actual `range_sp` derivations on hop2 (misses; `k9_second_hop_range_derivations`).
    pub k9_p2_range_derivations: AtomicU64,
    /// Hop2 range-cache hits (`k9_second_hop_range_reuse_hits`).
    pub k9_p2_range_reuse_hits: AtomicU64,
    // ── hit/miss rebind + walk split ──────────────────────────────
    /// Cumulative ns for cheap in-place RDI bounds reset on hop2 cache hits.
    pub k9_p2_hit_rebind_ns: AtomicU64,
    /// Cumulative ns for fresh LastCol open on hop2 cache misses.
    pub k9_p2_miss_rebind_ns: AtomicU64,
    /// Cumulative ns walking/emitting hop2 values after a cache-hit rebind.
    pub k9_p2_hit_cursor_ns: AtomicU64,
    /// Cumulative ns walking/emitting hop2 values after a cache-miss rebind.
    pub k9_p2_miss_cursor_ns: AtomicU64,
    /// Objects walked from hop2 opens that hit the middle-b range cache.
    pub k9_p2_values_from_hits: AtomicU64,
    /// Objects walked from hop2 opens that missed (first-touch range_sp).
    pub k9_p2_values_from_misses: AtomicU64,
    // ── predicate-scoped adjacency ───────────────────────────────────
    /// Times a predicate adjacency table was built (query-local prepare).
    pub k9_adj_prepare: AtomicU64,
    /// Cumulative ns building the adjacency table.
    pub k9_adj_prepare_ns: AtomicU64,
    /// Cumulative ns in two-hop execute body after adjacency is ready.
    pub k9_adj_execute_ns: AtomicU64,
    /// Subjects with a non-empty SP range in the adjacency table.
    pub k9_adj_ranges_present: AtomicU64,
    /// Bytes allocated for the adjacency range table (approx).
    pub k9_adj_bytes: AtomicU64,
    /// Hop2 opens served by direct adjacency lookup (no range_sp).
    pub k9_adj_direct_hits: AtomicU64,
    /// Build mode tag: 0=off/R1, 1=eager range_sp, 2=native sequential.
    pub k9_adj_mode: AtomicU64,
    // ── product prepared-plan cache (two-hop) ─────────────────────────
    /// Cache lookup hit (reused PreparedTwoHop + adjacency).
    pub prepared_plan_cache_hit: AtomicU64,
    /// Cache lookup miss (will prepare + insert).
    pub prepared_plan_cache_miss: AtomicU64,
    /// New plan inserted into the cache.
    pub prepared_plan_cache_insert: AtomicU64,
    /// Cache cleared (store mutation / compact / disable).
    pub prepared_plan_cache_invalidate: AtomicU64,
    // ── end-to-end timing buckets (ns, cumulative process-wide) ────────
    /// SPARQL parse + algebra setup (HTTP / evaluator entry).
    pub query_parse_plan_ns: AtomicU64,
    /// Physical prepare (PreparedTwoHop::prepare including adj build).
    pub physical_prepare_ns: AtomicU64,
    /// Operator execute body (two-hop walk; excludes serialize).
    pub execution_ns: AtomicU64,
    /// Dictionary decode + solution materialize (id→Term + Solution push).
    pub decode_materialize_ns: AtomicU64,
    /// HTTP result serialization.
    pub serialization_ns: AtomicU64,
}

impl SparqlPathCounters {
    pub fn reset(&self) {
        for a in [
            &self.join_scan_open,
            &self.path_mapped_rdi,
            &self.path_mapped_rnv,
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
            &self.d1_open_calls,
            &self.d1_open_ns,
            &self.d1_remap_ns,
            &self.d1_range_sp_ns,
            &self.d1_first_next_ns,
            &self.d1_left_range_reuse,
            &self.d1_pred_prepare,
            &self.k9_sp_prepare,
            &self.k9_sp_reset,
            &self.k9_sp_empty_range,
            &self.k9_sp_remap_ns,
            &self.k9_sp_range_ns,
            &self.k9_sp_cursor_ns,
            &self.k9_sp_values_emitted,
            &self.k9_two_hop_prepare,
            &self.k9_two_hop_execute,
            &self.k9_two_hop_rows,
            &self.k9_p2_range_lookups,
            &self.k9_p2_unique_b,
            &self.k9_p2_range_derivations,
            &self.k9_p2_range_reuse_hits,
            &self.k9_p2_hit_rebind_ns,
            &self.k9_p2_miss_rebind_ns,
            &self.k9_p2_hit_cursor_ns,
            &self.k9_p2_miss_cursor_ns,
            &self.k9_p2_values_from_hits,
            &self.k9_p2_values_from_misses,
            &self.k9_adj_prepare,
            &self.k9_adj_prepare_ns,
            &self.k9_adj_execute_ns,
            &self.k9_adj_ranges_present,
            &self.k9_adj_bytes,
            &self.k9_adj_direct_hits,
            &self.k9_adj_mode,
            &self.prepared_plan_cache_hit,
            &self.prepared_plan_cache_miss,
            &self.prepared_plan_cache_insert,
            &self.prepared_plan_cache_invalidate,
            &self.query_parse_plan_ns,
            &self.physical_prepare_ns,
            &self.execution_ns,
            &self.decode_materialize_ns,
            &self.serialization_ns,
        ] {
            a.store(0, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> SparqlPathSnapshot {
        SparqlPathSnapshot {
            join_scan_open: self.join_scan_open.load(Ordering::Relaxed),
            path_mapped_rdi: self.path_mapped_rdi.load(Ordering::Relaxed),
            path_mapped_rnv: self.path_mapped_rnv.load(Ordering::Relaxed),
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
            d1_open_calls: self.d1_open_calls.load(Ordering::Relaxed),
            d1_open_ns: self.d1_open_ns.load(Ordering::Relaxed),
            d1_remap_ns: self.d1_remap_ns.load(Ordering::Relaxed),
            d1_range_sp_ns: self.d1_range_sp_ns.load(Ordering::Relaxed),
            d1_first_next_ns: self.d1_first_next_ns.load(Ordering::Relaxed),
            d1_left_range_reuse: self.d1_left_range_reuse.load(Ordering::Relaxed),
            d1_pred_prepare: self.d1_pred_prepare.load(Ordering::Relaxed),
            k9_sp_prepare: self.k9_sp_prepare.load(Ordering::Relaxed),
            k9_sp_reset: self.k9_sp_reset.load(Ordering::Relaxed),
            k9_sp_empty_range: self.k9_sp_empty_range.load(Ordering::Relaxed),
            k9_sp_remap_ns: self.k9_sp_remap_ns.load(Ordering::Relaxed),
            k9_sp_range_ns: self.k9_sp_range_ns.load(Ordering::Relaxed),
            k9_sp_cursor_ns: self.k9_sp_cursor_ns.load(Ordering::Relaxed),
            k9_sp_values_emitted: self.k9_sp_values_emitted.load(Ordering::Relaxed),
            k9_two_hop_prepare: self.k9_two_hop_prepare.load(Ordering::Relaxed),
            k9_two_hop_execute: self.k9_two_hop_execute.load(Ordering::Relaxed),
            k9_two_hop_rows: self.k9_two_hop_rows.load(Ordering::Relaxed),
            k9_p2_range_lookups: self.k9_p2_range_lookups.load(Ordering::Relaxed),
            k9_p2_unique_b: self.k9_p2_unique_b.load(Ordering::Relaxed),
            k9_p2_range_derivations: self.k9_p2_range_derivations.load(Ordering::Relaxed),
            k9_p2_range_reuse_hits: self.k9_p2_range_reuse_hits.load(Ordering::Relaxed),
            k9_p2_hit_rebind_ns: self.k9_p2_hit_rebind_ns.load(Ordering::Relaxed),
            k9_p2_miss_rebind_ns: self.k9_p2_miss_rebind_ns.load(Ordering::Relaxed),
            k9_p2_hit_cursor_ns: self.k9_p2_hit_cursor_ns.load(Ordering::Relaxed),
            k9_p2_miss_cursor_ns: self.k9_p2_miss_cursor_ns.load(Ordering::Relaxed),
            k9_p2_values_from_hits: self.k9_p2_values_from_hits.load(Ordering::Relaxed),
            k9_p2_values_from_misses: self.k9_p2_values_from_misses.load(Ordering::Relaxed),
            k9_adj_prepare: self.k9_adj_prepare.load(Ordering::Relaxed),
            k9_adj_prepare_ns: self.k9_adj_prepare_ns.load(Ordering::Relaxed),
            k9_adj_execute_ns: self.k9_adj_execute_ns.load(Ordering::Relaxed),
            k9_adj_ranges_present: self.k9_adj_ranges_present.load(Ordering::Relaxed),
            k9_adj_bytes: self.k9_adj_bytes.load(Ordering::Relaxed),
            k9_adj_direct_hits: self.k9_adj_direct_hits.load(Ordering::Relaxed),
            k9_adj_mode: self.k9_adj_mode.load(Ordering::Relaxed),
            prepared_plan_cache_hit: self.prepared_plan_cache_hit.load(Ordering::Relaxed),
            prepared_plan_cache_miss: self.prepared_plan_cache_miss.load(Ordering::Relaxed),
            prepared_plan_cache_insert: self.prepared_plan_cache_insert.load(Ordering::Relaxed),
            prepared_plan_cache_invalidate: self
                .prepared_plan_cache_invalidate
                .load(Ordering::Relaxed),
            query_parse_plan_ns: self.query_parse_plan_ns.load(Ordering::Relaxed),
            physical_prepare_ns: self.physical_prepare_ns.load(Ordering::Relaxed),
            execution_ns: self.execution_ns.load(Ordering::Relaxed),
            decode_materialize_ns: self.decode_materialize_ns.load(Ordering::Relaxed),
            serialization_ns: self.serialization_ns.load(Ordering::Relaxed),
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct SparqlPathSnapshot {
    pub join_scan_open: u64,
    pub path_mapped_rdi: u64,
    pub path_mapped_rnv: u64,
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
    // D1 construction breakdown
    pub d1_open_calls: u64,
    pub d1_open_ns: u64,
    pub d1_remap_ns: u64,
    pub d1_range_sp_ns: u64,
    pub d1_first_next_ns: u64,
    pub d1_left_range_reuse: u64,
    pub d1_pred_prepare: u64,
    // two-hop
    pub k9_sp_prepare: u64,
    pub k9_sp_reset: u64,
    pub k9_sp_empty_range: u64,
    pub k9_sp_remap_ns: u64,
    pub k9_sp_range_ns: u64,
    pub k9_sp_cursor_ns: u64,
    pub k9_sp_values_emitted: u64,
    pub k9_two_hop_prepare: u64,
    pub k9_two_hop_execute: u64,
    pub k9_two_hop_rows: u64,
    pub k9_p2_range_lookups: u64,
    pub k9_p2_unique_b: u64,
    pub k9_p2_range_derivations: u64,
    pub k9_p2_range_reuse_hits: u64,
    // hit/miss split
    pub k9_p2_hit_rebind_ns: u64,
    pub k9_p2_miss_rebind_ns: u64,
    pub k9_p2_hit_cursor_ns: u64,
    pub k9_p2_miss_cursor_ns: u64,
    pub k9_p2_values_from_hits: u64,
    pub k9_p2_values_from_misses: u64,
    // adjacency
    pub k9_adj_prepare: u64,
    pub k9_adj_prepare_ns: u64,
    pub k9_adj_execute_ns: u64,
    pub k9_adj_ranges_present: u64,
    pub k9_adj_bytes: u64,
    pub k9_adj_direct_hits: u64,
    pub k9_adj_mode: u64,
    // prepared-plan cache
    pub prepared_plan_cache_hit: u64,
    pub prepared_plan_cache_miss: u64,
    pub prepared_plan_cache_insert: u64,
    pub prepared_plan_cache_invalidate: u64,
    // e2e timing buckets
    pub query_parse_plan_ns: u64,
    pub physical_prepare_ns: u64,
    pub execution_ns: u64,
    pub decode_materialize_ns: u64,
    pub serialization_ns: u64,
}

/// Global counters for the Ring SPARQL product path.
pub static SPARQL_PATH: SparqlPathCounters = SparqlPathCounters {
    join_scan_open: AtomicU64::new(0),
    path_mapped_rdi: AtomicU64::new(0),
    path_mapped_rnv: AtomicU64::new(0),
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
    d1_open_calls: AtomicU64::new(0),
    d1_open_ns: AtomicU64::new(0),
    d1_remap_ns: AtomicU64::new(0),
    d1_range_sp_ns: AtomicU64::new(0),
    d1_first_next_ns: AtomicU64::new(0),
    d1_left_range_reuse: AtomicU64::new(0),
    d1_pred_prepare: AtomicU64::new(0),
    k9_sp_prepare: AtomicU64::new(0),
    k9_sp_reset: AtomicU64::new(0),
    k9_sp_empty_range: AtomicU64::new(0),
    k9_sp_remap_ns: AtomicU64::new(0),
    k9_sp_range_ns: AtomicU64::new(0),
    k9_sp_cursor_ns: AtomicU64::new(0),
    k9_sp_values_emitted: AtomicU64::new(0),
    k9_two_hop_prepare: AtomicU64::new(0),
    k9_two_hop_execute: AtomicU64::new(0),
    k9_two_hop_rows: AtomicU64::new(0),
    k9_p2_range_lookups: AtomicU64::new(0),
    k9_p2_unique_b: AtomicU64::new(0),
    k9_p2_range_derivations: AtomicU64::new(0),
    k9_p2_range_reuse_hits: AtomicU64::new(0),
    k9_p2_hit_rebind_ns: AtomicU64::new(0),
    k9_p2_miss_rebind_ns: AtomicU64::new(0),
    k9_p2_hit_cursor_ns: AtomicU64::new(0),
    k9_p2_miss_cursor_ns: AtomicU64::new(0),
    k9_p2_values_from_hits: AtomicU64::new(0),
    k9_p2_values_from_misses: AtomicU64::new(0),
    k9_adj_prepare: AtomicU64::new(0),
    k9_adj_prepare_ns: AtomicU64::new(0),
    k9_adj_execute_ns: AtomicU64::new(0),
    k9_adj_ranges_present: AtomicU64::new(0),
    k9_adj_bytes: AtomicU64::new(0),
    k9_adj_direct_hits: AtomicU64::new(0),
    k9_adj_mode: AtomicU64::new(0),
    prepared_plan_cache_hit: AtomicU64::new(0),
    prepared_plan_cache_miss: AtomicU64::new(0),
    prepared_plan_cache_insert: AtomicU64::new(0),
    prepared_plan_cache_invalidate: AtomicU64::new(0),
    query_parse_plan_ns: AtomicU64::new(0),
    physical_prepare_ns: AtomicU64::new(0),
    execution_ns: AtomicU64::new(0),
    decode_materialize_ns: AtomicU64::new(0),
    serialization_ns: AtomicU64::new(0),
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

/// `NOVA_RING_MMAP` — default **on** for server / compact path.
#[inline]
pub fn ring_mmap_enabled() -> bool {
    env_flag_default_on("NOVA_RING_MMAP")
}

/// When true, keep heap CyclicRing after NOVARNG1 mmap materialize (dual residency).
/// Default **off**: product path drops heap after successful mmap.
/// Escape: `NOVA_RING_KEEP_HEAP=1` or tests via `materialize_mapped_ex(..., true)`.
pub fn ring_keep_heap() -> bool {
    match std::env::var("NOVA_RING_KEEP_HEAP") {
        Ok(v) => {
            let t = v.trim();
            !(t.is_empty()
                || t == "0"
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("off"))
        }
        Err(_) => false,
    }
}

/// `NOVA_RING_D2` — default **on**; D2 requires mmap image.
#[inline]
pub fn ring_d2_enabled() -> bool {
    env_flag_default_on("NOVA_RING_D2")
}

/// Optional verbose counter logging (`NOVA_RING_COUNTERS=1`).
///
/// Cached after the first read so hot kernels only pay a single atomic load
/// (not an env lookup) when deciding whether to bump `SPARQL_PATH` counters.
/// Default is **off** — production path pays nothing beyond that load when
/// callers gate counter writes behind this flag.
#[inline]
pub fn ring_counters_log_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    // 0 = unset, 1 = off, 2 = on
    static CACHED: AtomicU8 = AtomicU8::new(0);
    match CACHED.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = match std::env::var("NOVA_RING_COUNTERS") {
                Ok(v) => {
                    let v = v.trim();
                    v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
                }
                Err(_) => false,
            };
            CACHED.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// Bump a path counter only when [`ring_counters_log_enabled`] is on.
///
/// Prefer this (or a local `let ctr = ring_counters_log_enabled()` + `if ctr`)
/// over unconditional `SPARQL_PATH.*.fetch_add` in per-row / per-hop loops.
#[inline]
pub fn path_ctr_add(counter: &std::sync::atomic::AtomicU64, delta: u64) {
    if ring_counters_log_enabled() {
        counter.fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
    }
}


/// A/B: force pre-fix MiddleRuns VEO heuristic (row-span). Default **off**.
///
/// A/B only: Ring old VEO vs corrected VEO vs LOUDS.
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

// ── LastCol operation semantics ─────────────────────────────────────────────

/// What the consumer intends to do with a LastCol cursor.
///
/// Select the physical LastCol implementation from **operation semantics**,
/// not from range length alone. `join_scan` opens are enumerate-all; leapfrog
/// seeks inside an already-open MappedRdi cursor may switch to mapped RNV
/// (see `BraidedMappedLastColScan::seek`). Membership / singleton is not a
/// LastCol open (handled as `DenseScanKind::Singleton`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastColOperation {
    /// Walk every distinct value in the range (star scan, path outer, BGP unbound).
    /// Product default physical path: **MappedRdi**.
    EnumerateDistinct,
    /// Guided successor probes (leapfrog / large-gap seek). Prefer mapped RNV.
    SeekSuccessor,
    /// Point presence / singleton lookup (not a LastCol open today).
    Membership,
}

/// Product LastCol open policy for `join_scan` (enumerate-all opens).
///
/// ## Product default
///
/// `AlwaysMappedRdi` — enumerate-all LastCol opens use mapped RDI when mmap is
/// present. Range-length → heap RNV (`ShortHeap`) is **experimental only**
/// (env / harness override), not the product default.
///
/// Operation → preferred physical path (documented contract):
/// | operation            | open policy                         |
/// |----------------------|-------------------------------------|
/// | EnumerateDistinct    | MappedRdi (product default)         |
/// | SeekSuccessor        | mapped RNV (inside open cursor / AlwaysMappedRnv override) |
/// | Membership           | Singleton / direct access           |
/// | ShortHeap experimental | env `thresh:N` / override only    |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastColScanPolicy {
    /// Experimental: heap RNV when `range.len() <= thresh`, else mapped RDI.
    /// Not the product default; keep for A/B and corpus threshold tests.
    ShortHeap { thresh: u32 },
    /// Always mapped RDI when mmap is present (product default).
    AlwaysMappedRdi,
    /// Always mapped RNV successor stream (no RDI open, no heap materialize).
    AlwaysMappedRnv,
    /// Always heap RNV (`BraidedStreamingScan`), even with mmap.
    AlwaysHeapRnv,
}

/// Historical short-range heap-RNV threshold (demoted from product default).
pub const LASTCOL_SHORT_HEAP_DEFAULT: u32 = 16;

/// Map an operation to the preferred open policy (semantic dispatch table).
///
/// Used by docs/harnesses; `join_scan` always opens under
/// [`effective_lastcol_scan_policy`], which defaults to the EnumerateDistinct
/// row (`AlwaysMappedRdi`).
#[inline]
pub fn lastcol_policy_for_operation(op: LastColOperation) -> LastColScanPolicy {
    match op {
        LastColOperation::EnumerateDistinct => LastColScanPolicy::AlwaysMappedRdi,
        LastColOperation::SeekSuccessor => LastColScanPolicy::AlwaysMappedRnv,
        // Membership is not opened as LastCol; Singleton path handles it.
        // If forced through LastCol, MappedRdi is a correct (if heavy) fallback.
        LastColOperation::Membership => LastColScanPolicy::AlwaysMappedRdi,
    }
}

/// Resolve LastCol open policy from env.
///
/// `NOVA_RING_LASTCOL_POLICY`:
/// - unset / `auto` / `semantic` — **product default: AlwaysMappedRdi**
///   (enumerate-all semantics; no range-length heap divert)
/// - `mapped_rdi` / `rdi` — always mapped RDI
/// - `mapped_rnv` / `rnv` — always mapped RNV
/// - `heap` / `heap_rnv` — always heap RNV
/// - `short` / `short16` — experimental ShortHeap with
///   [`LASTCOL_SHORT_HEAP_DEFAULT`] (or `NOVA_RING_SHORT_HEAP_RNV`)
/// - `thresh:N` or bare integer `N` — experimental ShortHeap with threshold N
///   (`0` ≡ never divert when mmap present; same wall path as mapped_rdi)
///
/// `NOVA_RING_SHORT_HEAP_RNV=N` sets the short-heap threshold only when a
/// ShortHeap mode is selected (ignored for auto/mapped_* / heap).
#[inline]
pub fn lastcol_scan_policy() -> LastColScanPolicy {
    if let Ok(raw) = std::env::var("NOVA_RING_LASTCOL_POLICY") {
        let v = raw.trim();
        if v.is_empty()
            || v.eq_ignore_ascii_case("auto")
            || v.eq_ignore_ascii_case("semantic")
            || v.eq_ignore_ascii_case("default")
        {
            return LastColScanPolicy::AlwaysMappedRdi;
        }
        if v.eq_ignore_ascii_case("mapped_rdi") || v.eq_ignore_ascii_case("rdi") {
            return LastColScanPolicy::AlwaysMappedRdi;
        }
        if v.eq_ignore_ascii_case("mapped_rnv") || v.eq_ignore_ascii_case("rnv") {
            return LastColScanPolicy::AlwaysMappedRnv;
        }
        if v.eq_ignore_ascii_case("heap") || v.eq_ignore_ascii_case("heap_rnv") {
            return LastColScanPolicy::AlwaysHeapRnv;
        }
        if v.eq_ignore_ascii_case("short")
            || v.eq_ignore_ascii_case("short16")
            || v.eq_ignore_ascii_case("short_heap")
        {
            return LastColScanPolicy::ShortHeap {
                thresh: short_heap_rnv_threshold_raw(),
            };
        }
        if let Some(rest) = v
            .strip_prefix("thresh:")
            .or_else(|| v.strip_prefix("THRESH:"))
        {
            let n: u32 = rest.trim().parse().unwrap_or(LASTCOL_SHORT_HEAP_DEFAULT);
            return LastColScanPolicy::ShortHeap { thresh: n };
        }
        if let Ok(n) = v.parse::<u32>() {
            return LastColScanPolicy::ShortHeap { thresh: n };
        }
    }
    // Product default = enumerate-all → MappedRdi.
    LastColScanPolicy::AlwaysMappedRdi
}

#[inline]
fn short_heap_rnv_threshold_raw() -> u32 {
    match std::env::var("NOVA_RING_SHORT_HEAP_RNV") {
        Ok(v) => {
            let v = v.trim();
            if v.is_empty() {
                LASTCOL_SHORT_HEAP_DEFAULT
            } else {
                v.parse().unwrap_or(LASTCOL_SHORT_HEAP_DEFAULT)
            }
        }
        Err(_) => LASTCOL_SHORT_HEAP_DEFAULT,
    }
}

/// Process-wide override for harness A/B (takes precedence over env until cleared).
static LASTCOL_POLICY_OVERRIDE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

/// Encode policy into a u64 tag for the atomic override.
#[inline]
fn encode_policy(p: LastColScanPolicy) -> u64 {
    match p {
        LastColScanPolicy::AlwaysMappedRdi => 1,
        LastColScanPolicy::AlwaysMappedRnv => 2,
        LastColScanPolicy::AlwaysHeapRnv => 3,
        LastColScanPolicy::ShortHeap { thresh } => 0x10 | (u64::from(thresh) << 8),
    }
}

#[inline]
fn decode_policy(tag: u64) -> Option<LastColScanPolicy> {
    if tag == u64::MAX {
        return None;
    }
    match tag & 0xff {
        1 => Some(LastColScanPolicy::AlwaysMappedRdi),
        2 => Some(LastColScanPolicy::AlwaysMappedRnv),
        3 => Some(LastColScanPolicy::AlwaysHeapRnv),
        t if t == 0x10 || (t & 0x0f) == 0 => {
            let thresh = (tag >> 8) as u32;
            Some(LastColScanPolicy::ShortHeap { thresh })
        }
        _ => None,
    }
}

/// Effective LastCol policy (override if set, else env).
#[inline]
pub fn effective_lastcol_scan_policy() -> LastColScanPolicy {
    let tag = LASTCOL_POLICY_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
    decode_policy(tag).unwrap_or_else(lastcol_scan_policy)
}

/// Harness-only: force a LastCol policy for subsequent join_scan opens.
pub fn set_lastcol_scan_policy_override(p: Option<LastColScanPolicy>) {
    match p {
        None => LASTCOL_POLICY_OVERRIDE.store(u64::MAX, std::sync::atomic::Ordering::Relaxed),
        Some(pol) => {
            LASTCOL_POLICY_OVERRIDE.store(encode_policy(pol), std::sync::atomic::Ordering::Relaxed)
        }
    }
}

// ── predicate adjacency mode ────────────────────────────────────────────

/// How PreparedTwoHop builds the hop2 subject→SP-range table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredAdjacencyMode {
    /// R1/R2 only: execution-local cache, first-touch `range_sp` (baseline A).
    Off,
    /// Eager: for each subject under P2 (distinct S on `range_p`), call `range_sp`.
    Eager,
    /// Native sequential: one pass over `A_s` lead partitions, binary-search P
    /// inside each non-empty subject run (no per-subject `range_s` indirection;
    /// still O(#subjects_with_edges) middle probes, but sequential).
    Native,
}

/// `NOVA_RING_K9_ADJ`:
/// unset / `1` / `auto` / `native` — **Native** ( product attempt)
/// - `eager` — Eager range_sp table
/// - `0` / `off` / `r1` — Off (R1/R2 baseline)
#[inline]
pub fn pred_adjacency_mode() -> PredAdjacencyMode {
    if let Ok(raw) = std::env::var("NOVA_RING_K9_ADJ") {
        let v = raw.trim();
        if v == "0"
            || v.eq_ignore_ascii_case("off")
            || v.eq_ignore_ascii_case("false")
            || v.eq_ignore_ascii_case("r1")
            || v.eq_ignore_ascii_case("r1r2")
        {
            return PredAdjacencyMode::Off;
        }
        if v.eq_ignore_ascii_case("eager") || v.eq_ignore_ascii_case("b") {
            return PredAdjacencyMode::Eager;
        }
        if v.is_empty()
            || v == "1"
            || v.eq_ignore_ascii_case("true")
            || v.eq_ignore_ascii_case("on")
            || v.eq_ignore_ascii_case("auto")
            || v.eq_ignore_ascii_case("native")
            || v.eq_ignore_ascii_case("d")
        {
            return PredAdjacencyMode::Native;
        }
    }
    // product default: native sequential adjacency.
    PredAdjacencyMode::Native
}

static ADJ_MODE_OVERRIDE: AtomicU64 = AtomicU64::new(u64::MAX);

#[inline]
pub fn effective_pred_adjacency_mode() -> PredAdjacencyMode {
    match ADJ_MODE_OVERRIDE.load(Ordering::Relaxed) {
        0 => PredAdjacencyMode::Off,
        1 => PredAdjacencyMode::Eager,
        2 => PredAdjacencyMode::Native,
        _ => pred_adjacency_mode(),
    }
}

/// Harness-only: force adjacency mode (`None` clears override).
pub fn set_pred_adjacency_mode_override(m: Option<PredAdjacencyMode>) {
    let tag = match m {
        None => u64::MAX,
        Some(PredAdjacencyMode::Off) => 0,
        Some(PredAdjacencyMode::Eager) => 1,
        Some(PredAdjacencyMode::Native) => 2,
    };
    ADJ_MODE_OVERRIDE.store(tag, Ordering::Relaxed);
}

// ──: D1 tiny-range merge threshold ─────────────────────────────

/// Max SP range length eligible for the D1 tiny-merge path (matches C1 batch cap).
pub const D1_TINY_MERGE_MAX: u32 = 64;

/// Product default: braided D1 only (tiny-merge off until validated).
pub const D1_TINY_MERGE_DEFAULT: u32 = 0;

/// `NOVA_RING_D1_TINY_MERGE`:
/// - unset / `0` / `off` / `false` / `braid` — **0** (braided D1 only; product default)
/// - `N` (1..=64) — when `max(|SP_a|, |SP_b|) ≤ N`, materialize sorted distinct
///   O lists via wavelet access and merge (fixed stack, no heap)
/// - `on` / `auto` / `true` — enable at T=16 (sweep midpoint)
///
/// Harness override via [`set_d1_tiny_merge_threshold_override`] wins over env.
#[inline]
pub fn d1_tiny_merge_threshold() -> u32 {
    if let Ok(raw) = std::env::var("NOVA_RING_D1_TINY_MERGE") {
        let v = raw.trim();
        if v.is_empty()
            || v == "0"
            || v.eq_ignore_ascii_case("off")
            || v.eq_ignore_ascii_case("false")
            || v.eq_ignore_ascii_case("no")
            || v.eq_ignore_ascii_case("braid")
            || v.eq_ignore_ascii_case("braided")
        {
            return 0;
        }
        if v.eq_ignore_ascii_case("on")
            || v.eq_ignore_ascii_case("true")
            || v.eq_ignore_ascii_case("auto")
            || v.eq_ignore_ascii_case("yes")
        {
            return 16;
        }
        if let Ok(n) = v.parse::<u32>() {
            return n.min(D1_TINY_MERGE_MAX);
        }
    }
    D1_TINY_MERGE_DEFAULT
}

/// `u64::MAX` = no override (read env); else threshold 0..=D1_TINY_MERGE_MAX.
static D1_TINY_MERGE_OVERRIDE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Effective tiny-merge threshold (override > env > default 0).
#[inline]
pub fn effective_d1_tiny_merge_threshold() -> u32 {
    let tag = D1_TINY_MERGE_OVERRIDE.load(Ordering::Relaxed);
    if tag == u64::MAX {
        d1_tiny_merge_threshold()
    } else {
        (tag as u32).min(D1_TINY_MERGE_MAX)
    }
}

/// Harness-only: force D1 tiny-merge threshold (`None` clears → env/default).
///
/// `Some(0)` forces braided D1. `Some(T)` with T∈1..=64 enables merge when both
/// SP ranges have `len ≤ T`.
pub fn set_d1_tiny_merge_threshold_override(t: Option<u32>) {
    match t {
        None => D1_TINY_MERGE_OVERRIDE.store(u64::MAX, Ordering::Relaxed),
        Some(n) => {
            D1_TINY_MERGE_OVERRIDE.store(u64::from(n.min(D1_TINY_MERGE_MAX)), Ordering::Relaxed)
        }
    }
}

// ──: D1 tiny intersection strategy ─────────────────────────────

/// Fixed-size kernel cap: nested/probe/fixed only dispatch when
/// `max(|L|, |R|) ≤ D1_TINY_FIXED_MAX` (corpus is uniformly 3×3).
pub const D1_TINY_FIXED_MAX: u32 = 4;

/// Algorithm used when the tiny threshold fires (T > 0 and both SP lens ≤ T).
///
/// Product default remains **Merge** behind T=0. Strategies other than Merge
/// are experimental probes for the open/construct bottleneck.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum D1TinyStrategy {
    /// Decode both sides sorted+distinct, two-pointer merge into buffer.
    Merge = 0,
    /// Decode both sides raw (no sort/dedup), nested compare, sort intersection.
    Nested = 1,
    /// Decode smaller side raw; probe each distinct value in the other via rank.
    Probe = 2,
    /// Specialized fixed-size kernels for max_len ≤ [`D1_TINY_FIXED_MAX`].
    /// Falls back to Nested when either side exceeds the fixed cap under T.
    Fixed = 3,
    /// Fused dual-range wavelet decode: one level walk for both SP ranges,
    /// DataLine/superblock loaded once per group, no per-row `get`/`rank`.
    Fused = 4,
}

impl D1TinyStrategy {
    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Nested => "nested",
            Self::Probe => "probe",
            Self::Fixed => "fixed",
            Self::Fused => "fused",
        }
    }

    #[inline]
    pub fn from_tag(tag: u64) -> Option<Self> {
        match tag {
            0 => Some(Self::Merge),
            1 => Some(Self::Nested),
            2 => Some(Self::Probe),
            3 => Some(Self::Fixed),
            4 => Some(Self::Fused),
            _ => None,
        }
    }
}

/// `NOVA_RING_D1_TINY_STRATEGY`:
/// - unset / `merge` / `buf` — **Merge** (dual materialize + two-pointer)
/// - `nested` / `cross` — Nested linear compare
/// - `probe` / `seek` — Decode-one + rank-probe other
/// - `fixed` / `k4` — Fixed-size kernels (max_len ≤ 4)
/// - `fused` / `dual` — Fused dual-range wavelet decode (shared level walk)
///
/// Only applies when T > 0. Product default T=0 keeps braided D1 regardless.
#[inline]
pub fn d1_tiny_strategy() -> D1TinyStrategy {
    if let Ok(raw) = std::env::var("NOVA_RING_D1_TINY_STRATEGY") {
        let v = raw.trim();
        if v.is_empty()
            || v.eq_ignore_ascii_case("merge")
            || v.eq_ignore_ascii_case("buf")
            || v.eq_ignore_ascii_case("buffer")
            || v.eq_ignore_ascii_case("default")
        {
            return D1TinyStrategy::Merge;
        }
        if v.eq_ignore_ascii_case("nested")
            || v.eq_ignore_ascii_case("cross")
            || v.eq_ignore_ascii_case("linear")
        {
            return D1TinyStrategy::Nested;
        }
        if v.eq_ignore_ascii_case("probe")
            || v.eq_ignore_ascii_case("seek")
            || v.eq_ignore_ascii_case("rank")
        {
            return D1TinyStrategy::Probe;
        }
        if v.eq_ignore_ascii_case("fixed")
            || v.eq_ignore_ascii_case("k4")
            || v.eq_ignore_ascii_case("kernel")
        {
            return D1TinyStrategy::Fixed;
        }
        if v.eq_ignore_ascii_case("fused")
            || v.eq_ignore_ascii_case("dual")
            || v.eq_ignore_ascii_case("batch")
        {
            return D1TinyStrategy::Fused;
        }
    }
    D1TinyStrategy::Merge
}

/// `u64::MAX` = no override; else strategy tag 0..=4.
static D1_TINY_STRATEGY_OVERRIDE: AtomicU64 = AtomicU64::new(u64::MAX);

// ── D1 Fused dual-range decode diagnostics ───────────────────────────────────

/// Fused opens that produced a TinyMerge mode (success path).
pub static D1_FUSED_OPENS: AtomicU64 = AtomicU64::new(0);
/// Source positions decoded across both ranges (sum of lens).
pub static D1_FUSED_SOURCE_POS: AtomicU64 = AtomicU64::new(0);
/// Wavelet levels visited (opens × n_levels).
pub static D1_FUSED_LEVELS: AtomicU64 = AtomicU64::new(0);
/// Physical DataLine loads performed by fused decode.
pub static D1_FUSED_DATALINE_LOADS: AtomicU64 = AtomicU64::new(0);
/// Physical Superblock loads performed by fused decode.
pub static D1_FUSED_SUPERBLOCK_LOADS: AtomicU64 = AtomicU64::new(0);
/// Times fused declined and fell back (caller keeps braided / other strat).
pub static D1_FUSED_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Snapshot of D1 fused dual-range decode counters.
#[derive(Clone, Copy, Debug, Default)]
pub struct D1FusedCounters {
    pub opens: u64,
    pub source_pos: u64,
    pub levels: u64,
    pub dataline_loads: u64,
    pub superblock_loads: u64,
    pub fallbacks: u64,
    /// Prepared-wedge: left SP range fused-decoded once per outer `a`.
    pub left_decodes: u64,
    /// Prepared-wedge: right SP range fused-decoded per `(a,b)` close.
    pub right_decodes: u64,
    /// Prepared-wedge: times a cached left buffer was reused for a right open.
    pub left_reuse: u64,
}

/// Load fused decode counters (relaxed).
#[inline]
pub fn d1_fused_counters() -> D1FusedCounters {
    D1FusedCounters {
        opens: D1_FUSED_OPENS.load(Ordering::Relaxed),
        source_pos: D1_FUSED_SOURCE_POS.load(Ordering::Relaxed),
        levels: D1_FUSED_LEVELS.load(Ordering::Relaxed),
        dataline_loads: D1_FUSED_DATALINE_LOADS.load(Ordering::Relaxed),
        superblock_loads: D1_FUSED_SUPERBLOCK_LOADS.load(Ordering::Relaxed),
        fallbacks: D1_FUSED_FALLBACKS.load(Ordering::Relaxed),
        left_decodes: D1_FUSED_LEFT_DECODES.load(Ordering::Relaxed),
        right_decodes: D1_FUSED_RIGHT_DECODES.load(Ordering::Relaxed),
        left_reuse: D1_FUSED_LEFT_REUSE.load(Ordering::Relaxed),
    }
}

/// Reset fused decode counters (harness-only).
#[inline]
pub fn reset_d1_fused_counters() {
    D1_FUSED_OPENS.store(0, Ordering::Relaxed);
    D1_FUSED_SOURCE_POS.store(0, Ordering::Relaxed);
    D1_FUSED_LEVELS.store(0, Ordering::Relaxed);
    D1_FUSED_DATALINE_LOADS.store(0, Ordering::Relaxed);
    D1_FUSED_SUPERBLOCK_LOADS.store(0, Ordering::Relaxed);
    D1_FUSED_FALLBACKS.store(0, Ordering::Relaxed);
    D1_FUSED_LEFT_DECODES.store(0, Ordering::Relaxed);
    D1_FUSED_RIGHT_DECODES.store(0, Ordering::Relaxed);
    D1_FUSED_LEFT_REUSE.store(0, Ordering::Relaxed);
}

// ── Prepared-wedge left-once fused decode counters ───────────────────────────

/// Left SP fused single-range decodes (once per outer `a` when Fused+T fires).
pub static D1_FUSED_LEFT_DECODES: AtomicU64 = AtomicU64::new(0);
/// Right SP fused single-range decodes (per `(a,b)` close under left reuse).
pub static D1_FUSED_RIGHT_DECODES: AtomicU64 = AtomicU64::new(0);
/// Times a prepared-wedge close reused a cached left symbol buffer.
pub static D1_FUSED_LEFT_REUSE: AtomicU64 = AtomicU64::new(0);

/// Effective tiny strategy (override > env > Merge).
#[inline]
pub fn effective_d1_tiny_strategy() -> D1TinyStrategy {
    let tag = D1_TINY_STRATEGY_OVERRIDE.load(Ordering::Relaxed);
    D1TinyStrategy::from_tag(tag).unwrap_or_else(d1_tiny_strategy)
}

/// Harness-only: force D1 tiny strategy (`None` clears → env/default Merge).
pub fn set_d1_tiny_strategy_override(s: Option<D1TinyStrategy>) {
    match s {
        None => D1_TINY_STRATEGY_OVERRIDE.store(u64::MAX, Ordering::Relaxed),
        Some(st) => D1_TINY_STRATEGY_OVERRIDE.store(st as u64, Ordering::Relaxed),
    }
}

// ── PreparedWedge left-once fused enable (product, operator-scoped) ──────────
//
// Validation (2026-07-19): fused-left-once is profitable on PreparedWedge when
// left_len ≤ 4 ∧ right_len ≤ 4 and the left buffer is retained for the outer
// group. Generic BraidedD1ObjectScan stays braided (global T default 0).

/// Product default for PreparedWedge fused left-once: T=4.
pub const WEDGE_LEFT_ONCE_DEFAULT: u32 = 4;

/// Cap for wedge left-once (same stack budget as D1 tiny merge).
pub const WEDGE_LEFT_ONCE_MAX: u32 = D1_TINY_MERGE_MAX;

/// `NOVA_RING_WEDGE_LEFT_ONCE`:
/// - unset — **4** (product default: fused left-once for |SP| ≤ 4)
/// - `0` / `off` / `false` / `braid` — disabled (braided D1 on wedge closes)
/// - `N` (1..=64) — enable when both SP ranges have `len ≤ N`
/// - `on` / `auto` / `true` — enable at default 4
///
/// Independent of [`d1_tiny_merge_threshold`] (global generic D1 stays T=0).
#[inline]
pub fn wedge_left_once_threshold() -> u32 {
    if let Ok(raw) = std::env::var("NOVA_RING_WEDGE_LEFT_ONCE") {
        let v = raw.trim();
        if v.is_empty() {
            return WEDGE_LEFT_ONCE_DEFAULT;
        }
        if v == "0"
            || v.eq_ignore_ascii_case("off")
            || v.eq_ignore_ascii_case("false")
            || v.eq_ignore_ascii_case("no")
            || v.eq_ignore_ascii_case("braid")
            || v.eq_ignore_ascii_case("braided")
        {
            return 0;
        }
        if v.eq_ignore_ascii_case("on")
            || v.eq_ignore_ascii_case("true")
            || v.eq_ignore_ascii_case("auto")
            || v.eq_ignore_ascii_case("yes")
        {
            return WEDGE_LEFT_ONCE_DEFAULT;
        }
        if let Ok(n) = v.parse::<u32>() {
            return n.min(WEDGE_LEFT_ONCE_MAX);
        }
    }
    WEDGE_LEFT_ONCE_DEFAULT
}

/// `u64::MAX` = no override (read env/default); else threshold 0..=MAX.
static WEDGE_LEFT_ONCE_OVERRIDE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Effective PreparedWedge left-once threshold (override > env > default 4).
#[inline]
pub fn effective_wedge_left_once_threshold() -> u32 {
    let tag = WEDGE_LEFT_ONCE_OVERRIDE.load(Ordering::Relaxed);
    if tag == u64::MAX {
        wedge_left_once_threshold()
    } else {
        (tag as u32).min(WEDGE_LEFT_ONCE_MAX)
    }
}

/// Harness-only: force PreparedWedge left-once threshold (`None` clears).
///
/// `Some(0)` forces braided D1 on wedge closes. Does not change generic D1 T.
pub fn set_wedge_left_once_threshold_override(t: Option<u32>) {
    match t {
        None => WEDGE_LEFT_ONCE_OVERRIDE.store(u64::MAX, Ordering::Relaxed),
        Some(n) => {
            WEDGE_LEFT_ONCE_OVERRIDE.store(u64::from(n.min(WEDGE_LEFT_ONCE_MAX)), Ordering::Relaxed)
        }
    }
}

// ── asymmetric large-range D1 kernel (product-off gate) ────────────────
//
// Measurement (2026-07-19): 77.3% of large closes have one side ≤4; dominant
// cell L=17–32 × R≤4. Kernel: decode shorter, probe/gallop longer. Product
// default remains **Off** (braided residual) until A/B acceptance.

/// Short-side max length for the asymmetric selector (`min ≤ SHORT`).
pub const D1_ASYM_SHORT_MAX: u32 = 4;

/// Long-side min length for the asymmetric selector (`max ≥ LONG_MIN`).
pub const D1_ASYM_LONG_MIN: u32 = 5;

/// Hard cap on the long side (stack / probe budget). Beyond this → braided.
pub const D1_ASYM_LONG_MAX: u32 = D1_TINY_MERGE_MAX;

/// asymmetric large-range D1 strategy.
///
/// Product default: [`D1AsymMode::Off`]. Enable only via env/harness override.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum D1AsymMode {
    /// Product default: large residual stays braided.
    Off = 0,
    /// Decode short side; rank-probe each distinct symbol in the long range.
    Probe = 1,
    /// Decode short side sorted; monotonic RNV (next-value) over the long range.
    Gallop = 2,
}

impl D1AsymMode {
    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Probe => "probe",
            Self::Gallop => "gallop",
        }
    }

    #[inline]
    pub fn from_tag(tag: u64) -> Option<Self> {
        match tag {
            0 => Some(Self::Off),
            1 => Some(Self::Probe),
            2 => Some(Self::Gallop),
            _ => None,
        }
    }
}

/// `NOVA_RING_D1_ASYM`:
/// - unset / `0` / `off` / `false` / `braid` — **Off** (product default)
/// - `probe` / `rank` / `1` / `on` — decode-short + rank-probe long
/// - `gallop` / `rnv` / `2` — decode-short + monotonic RNV on long
///
/// Selector (when mode ≠ Off): `min(L,R) ≤ 4` ∧ `max(L,R) ∈ [5, 64]`.
/// Otherwise braided. Independent of tiny-merge T and wedge left-once.
#[inline]
pub fn d1_asym_mode() -> D1AsymMode {
    if let Ok(raw) = std::env::var("NOVA_RING_D1_ASYM") {
        let v = raw.trim();
        if v.is_empty()
            || v == "0"
            || v.eq_ignore_ascii_case("off")
            || v.eq_ignore_ascii_case("false")
            || v.eq_ignore_ascii_case("no")
            || v.eq_ignore_ascii_case("braid")
            || v.eq_ignore_ascii_case("braided")
        {
            return D1AsymMode::Off;
        }
        if v.eq_ignore_ascii_case("probe")
            || v.eq_ignore_ascii_case("rank")
            || v.eq_ignore_ascii_case("seek")
            || v == "1"
            || v.eq_ignore_ascii_case("on")
            || v.eq_ignore_ascii_case("true")
            || v.eq_ignore_ascii_case("yes")
            || v.eq_ignore_ascii_case("auto")
        {
            return D1AsymMode::Probe;
        }
        if v.eq_ignore_ascii_case("gallop")
            || v.eq_ignore_ascii_case("rnv")
            || v.eq_ignore_ascii_case("next")
            || v == "2"
        {
            return D1AsymMode::Gallop;
        }
    }
    D1AsymMode::Off
}

/// `u64::MAX` = no override; else mode tag 0..=2.
static D1_ASYM_MODE_OVERRIDE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Effective asymmetric D1 mode (override > env > Off).
#[inline]
pub fn effective_d1_asym_mode() -> D1AsymMode {
    let tag = D1_ASYM_MODE_OVERRIDE.load(Ordering::Relaxed);
    D1AsymMode::from_tag(tag).unwrap_or_else(d1_asym_mode)
}

/// Harness-only: force asymmetric D1 mode (`None` clears → env/default Off).
pub fn set_d1_asym_mode_override(m: Option<D1AsymMode>) {
    match m {
        None => D1_ASYM_MODE_OVERRIDE.store(u64::MAX, Ordering::Relaxed),
        Some(mode) => D1_ASYM_MODE_OVERRIDE.store(mode as u64, Ordering::Relaxed),
    }
}

// ── asymmetric D1 diagnostics ──────────────────────────────────────────

/// Shape bins for short×long reporting (max side when min ≤ 4).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum D1AsymShapeBin {
    /// max in 5..=8
    Max5_8 = 0,
    /// max in 9..=16
    Max9_16 = 1,
    /// max in 17..=32
    Max17_32 = 2,
    /// max in 33..=64 (and any other eligible)
    Max33p = 3,
}

impl D1AsymShapeBin {
    #[inline]
    pub fn from_max_len(max_len: u32) -> Self {
        match max_len {
            0..=8 => Self::Max5_8,
            9..=16 => Self::Max9_16,
            17..=32 => Self::Max17_32,
            _ => Self::Max33p,
        }
    }
}

/// Opens that matched the asym shape selector (min≤4 ∧ max∈[5,64]).
pub static D1_ASYM_ELIGIBLE: AtomicU64 = AtomicU64::new(0);
/// Opens that successfully built TinyMerge via the asym kernel.
pub static D1_ASYM_ACTIVATED: AtomicU64 = AtomicU64::new(0);
/// Eligible opens that declined (decode/probe fail → braided).
pub static D1_ASYM_FALLBACKS: AtomicU64 = AtomicU64::new(0);
/// Distinct short-side symbols decoded (sum over activated opens).
pub static D1_ASYM_SHORT_SYMBOLS: AtomicU64 = AtomicU64::new(0);
/// Long-side probe/RNV attempts (per distinct short symbol).
pub static D1_ASYM_LONG_PROBES: AtomicU64 = AtomicU64::new(0);
/// Long-side probe hits (symbol present).
pub static D1_ASYM_PROBE_HITS: AtomicU64 = AtomicU64::new(0);
/// Activated opens whose intersection was empty.
pub static D1_ASYM_EMPTY: AtomicU64 = AtomicU64::new(0);
/// Cumulative open construct ns on activated asym path.
pub static D1_ASYM_OPEN_NS: AtomicU64 = AtomicU64::new(0);
/// Activated count by max-len shape bin (4 bins).
pub static D1_ASYM_SHAPE_BIN: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Snapshot of asymmetric D1 counters.
#[derive(Clone, Copy, Debug, Default)]
pub struct D1AsymCounters {
    pub eligible: u64,
    pub activated: u64,
    pub fallbacks: u64,
    pub short_symbols: u64,
    pub long_probes: u64,
    pub probe_hits: u64,
    pub empty: u64,
    pub open_ns: u64,
    /// Activated by shape bin: [5_8, 9_16, 17_32, 33p].
    pub shape_bins: [u64; 4],
}

/// Load asymmetric D1 counters (relaxed).
#[inline]
pub fn d1_asym_counters() -> D1AsymCounters {
    D1AsymCounters {
        eligible: D1_ASYM_ELIGIBLE.load(Ordering::Relaxed),
        activated: D1_ASYM_ACTIVATED.load(Ordering::Relaxed),
        fallbacks: D1_ASYM_FALLBACKS.load(Ordering::Relaxed),
        short_symbols: D1_ASYM_SHORT_SYMBOLS.load(Ordering::Relaxed),
        long_probes: D1_ASYM_LONG_PROBES.load(Ordering::Relaxed),
        probe_hits: D1_ASYM_PROBE_HITS.load(Ordering::Relaxed),
        empty: D1_ASYM_EMPTY.load(Ordering::Relaxed),
        open_ns: D1_ASYM_OPEN_NS.load(Ordering::Relaxed),
        shape_bins: [
            D1_ASYM_SHAPE_BIN[0].load(Ordering::Relaxed),
            D1_ASYM_SHAPE_BIN[1].load(Ordering::Relaxed),
            D1_ASYM_SHAPE_BIN[2].load(Ordering::Relaxed),
            D1_ASYM_SHAPE_BIN[3].load(Ordering::Relaxed),
        ],
    }
}

/// Reset asymmetric D1 counters (harness-only).
#[inline]
pub fn reset_d1_asym_counters() {
    D1_ASYM_ELIGIBLE.store(0, Ordering::Relaxed);
    D1_ASYM_ACTIVATED.store(0, Ordering::Relaxed);
    D1_ASYM_FALLBACKS.store(0, Ordering::Relaxed);
    D1_ASYM_SHORT_SYMBOLS.store(0, Ordering::Relaxed);
    D1_ASYM_LONG_PROBES.store(0, Ordering::Relaxed);
    D1_ASYM_PROBE_HITS.store(0, Ordering::Relaxed);
    D1_ASYM_EMPTY.store(0, Ordering::Relaxed);
    D1_ASYM_OPEN_NS.store(0, Ordering::Relaxed);
    for b in &D1_ASYM_SHAPE_BIN {
        b.store(0, Ordering::Relaxed);
    }
}

// ── product prepared-plan cache enable ───────────────────────────────────

/// `NOVA_RING_PREPARED_PLAN_CACHE`:
/// unset / `1` / `on` / `true` — **enabled** ( product default)
/// - `0` / `off` / `false` — disabled (rung A baseline: prepare every request)
#[inline]
pub fn prepared_plan_cache_enabled() -> bool {
    match std::env::var("NOVA_RING_PREPARED_PLAN_CACHE") {
        Ok(v) => {
            let v = v.trim();
            !(v == "0"
                || v.eq_ignore_ascii_case("off")
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no"))
        }
        Err(_) => true,
    }
}

/// Process-wide override: `None` = env/default, `Some(true/false)` forces.
static PREPARED_PLAN_CACHE_OVERRIDE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Effective prepared-plan cache enable (override > env > default on).
#[inline]
pub fn effective_prepared_plan_cache_enabled() -> bool {
    match PREPARED_PLAN_CACHE_OVERRIDE.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => prepared_plan_cache_enabled(),
    }
}

/// Harness/product control: force prepared-plan cache on/off (`None` clears).
pub fn set_prepared_plan_cache_override(on: Option<bool>) {
    let tag = match on {
        None => u64::MAX,
        Some(false) => 0,
        Some(true) => 1,
    };
    PREPARED_PLAN_CACHE_OVERRIDE.store(tag, Ordering::Relaxed);
}

/// Record cumulative nanoseconds into a named e2e timing bucket.
#[inline]
pub fn add_timing_ns(bucket: TimingBucket, ns: u64) {
    let a = match bucket {
        TimingBucket::QueryParsePlan => &SPARQL_PATH.query_parse_plan_ns,
        TimingBucket::PhysicalPrepare => &SPARQL_PATH.physical_prepare_ns,
        TimingBucket::Execution => &SPARQL_PATH.execution_ns,
        TimingBucket::DecodeMaterialize => &SPARQL_PATH.decode_materialize_ns,
        TimingBucket::Serialization => &SPARQL_PATH.serialization_ns,
    };
    a.fetch_add(ns, Ordering::Relaxed);
}

/// Named end-to-end timing buckets for accounting of the HTTP path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimingBucket {
    QueryParsePlan,
    PhysicalPrepare,
    Execution,
    DecodeMaterialize,
    Serialization,
}

/// Max distinct runs walked in one exact MiddleRuns VEO probe before falling
/// back to the row-span heuristic. Bounds planning cost on high-cardinality middles.
pub const VEO_MIDDLE_EXACT_RUN_BUDGET: u64 = 4_096;

/// Temp dir for per-graph `NOVARNG1` images (process-scoped).
/// Directory for NOVARNG1 mmap image files written during compact/bulk_load.
///
/// Override with `NOVA_RING_IMAGE_DIR` (harness disk footprint measurement).
/// Default: `{temp_dir}/nova-ring-{pid}`.
pub fn ring_image_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("NOVA_RING_IMAGE_DIR") {
        let p = std::path::PathBuf::from(dir);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
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
