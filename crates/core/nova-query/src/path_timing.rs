//! Backend-agnostic e2e path timing buckets for HTTP architecture validation.
//!
//! These counters are process-wide and always compiled in. They exist so a
//! LOUDS-vs-Ring comparison over the *same* HTTP endpoint can answer:
//!
//! - Is the prepared operator actually used? (Execution drops on Ring)
//! - Does decode/serialize dominate? (Decode/Serialize large on both)
//!
//! Buckets are intentionally **non-overlapping**:
//! - `Parse` — SPARQL text → algebra (server)
//! - `Execution` — operator walk / join only (no term decode)
//! - `Decode` — TermId → oxrdf Term + solution materialize
//! - `Serialize` — SPARQL Results JSON/XML/CSV body build (server)
//!
//! `PhysicalPrepare` is ring-only detail (prepare/cache miss cost) and is
//! kept on the ring `SPARQL_PATH` counters; it is *not* double-counted here
//! because Execution starts after prepare returns.

use std::sync::atomic::{AtomicU64, Ordering};

/// Named non-overlapping path timing buckets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathTimingBucket {
    Parse,
    Execution,
    Decode,
    Serialize,
}

/// Cumulative nanoseconds per bucket (process-wide).
static PARSE_NS: AtomicU64 = AtomicU64::new(0);
static EXECUTION_NS: AtomicU64 = AtomicU64::new(0);
static DECODE_NS: AtomicU64 = AtomicU64::new(0);
static SERIALIZE_NS: AtomicU64 = AtomicU64::new(0);

/// How many times each bucket was recorded (≈ request / emit batches).
static PARSE_N: AtomicU64 = AtomicU64::new(0);
static EXECUTION_N: AtomicU64 = AtomicU64::new(0);
static DECODE_N: AtomicU64 = AtomicU64::new(0);
static SERIALIZE_N: AtomicU64 = AtomicU64::new(0);

/// Snapshot of cumulative path timing.
#[derive(Clone, Copy, Debug, Default)]
pub struct PathTimingSnapshot {
    pub parse_ns: u64,
    pub execution_ns: u64,
    pub decode_ns: u64,
    pub serialize_ns: u64,
    pub parse_n: u64,
    pub execution_n: u64,
    pub decode_n: u64,
    pub serialize_n: u64,
}

impl PathTimingSnapshot {
    /// Sum of all buckets (excludes network / HTTP framing outside server).
    pub fn total_ns(&self) -> u64 {
        self.parse_ns
            .saturating_add(self.execution_ns)
            .saturating_add(self.decode_ns)
            .saturating_add(self.serialize_ns)
    }

    /// Per-request means (ns) using each bucket's sample count.
    /// Falls back to `n=1` if a bucket never fired (avoids div-by-zero).
    pub fn mean_ms(&self) -> PathTimingMeansMs {
        PathTimingMeansMs {
            parse_ms: mean_ms(self.parse_ns, self.parse_n),
            execution_ms: mean_ms(self.execution_ns, self.execution_n),
            decode_ms: mean_ms(self.decode_ns, self.decode_n),
            serialize_ms: mean_ms(self.serialize_ns, self.serialize_n),
            total_ms: mean_ms(self.total_ns(), self.parse_n.max(1)),
        }
    }
}

/// Mean wall times in milliseconds (per sample).
#[derive(Clone, Copy, Debug, Default)]
pub struct PathTimingMeansMs {
    pub parse_ms: f64,
    pub execution_ms: f64,
    pub decode_ms: f64,
    pub serialize_ms: f64,
    pub total_ms: f64,
}

#[inline]
fn mean_ms(ns: u64, n: u64) -> f64 {
    if n == 0 {
        0.0
    } else {
        (ns as f64 / n as f64) / 1e6
    }
}

/// Record nanoseconds into a named bucket **and** count one sample.
///
/// Use for once-per-request buckets (Parse, Execution, Serialize).
#[inline]
pub fn add_path_timing_ns(bucket: PathTimingBucket, ns: u64) {
    match bucket {
        PathTimingBucket::Parse => {
            PARSE_NS.fetch_add(ns, Ordering::Relaxed);
            PARSE_N.fetch_add(1, Ordering::Relaxed);
        }
        PathTimingBucket::Execution => {
            EXECUTION_NS.fetch_add(ns, Ordering::Relaxed);
            EXECUTION_N.fetch_add(1, Ordering::Relaxed);
        }
        PathTimingBucket::Decode => {
            DECODE_NS.fetch_add(ns, Ordering::Relaxed);
            DECODE_N.fetch_add(1, Ordering::Relaxed);
        }
        PathTimingBucket::Serialize => {
            SERIALIZE_NS.fetch_add(ns, Ordering::Relaxed);
            SERIALIZE_N.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Accumulate nanoseconds **without** bumping the sample count.
///
/// Use for per-row Decode inside an emit loop; the enclosing evaluate path
/// should call [`add_path_timing_ns`] once with the summed decode time, or
/// call [`bump_path_timing_sample`] after the walk so means stay per-request.
#[inline]
pub fn acc_path_timing_ns(bucket: PathTimingBucket, ns: u64) {
    match bucket {
        PathTimingBucket::Parse => {
            PARSE_NS.fetch_add(ns, Ordering::Relaxed);
        }
        PathTimingBucket::Execution => {
            EXECUTION_NS.fetch_add(ns, Ordering::Relaxed);
        }
        PathTimingBucket::Decode => {
            DECODE_NS.fetch_add(ns, Ordering::Relaxed);
        }
        PathTimingBucket::Serialize => {
            SERIALIZE_NS.fetch_add(ns, Ordering::Relaxed);
        }
    }
}

/// Bump sample count for a bucket without adding time (pair with [`acc_path_timing_ns`]).
#[inline]
pub fn bump_path_timing_sample(bucket: PathTimingBucket) {
    match bucket {
        PathTimingBucket::Parse => {
            PARSE_N.fetch_add(1, Ordering::Relaxed);
        }
        PathTimingBucket::Execution => {
            EXECUTION_N.fetch_add(1, Ordering::Relaxed);
        }
        PathTimingBucket::Decode => {
            DECODE_N.fetch_add(1, Ordering::Relaxed);
        }
        PathTimingBucket::Serialize => {
            SERIALIZE_N.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Snapshot cumulative counters.
pub fn path_timing_snapshot() -> PathTimingSnapshot {
    PathTimingSnapshot {
        parse_ns: PARSE_NS.load(Ordering::Relaxed),
        execution_ns: EXECUTION_NS.load(Ordering::Relaxed),
        decode_ns: DECODE_NS.load(Ordering::Relaxed),
        serialize_ns: SERIALIZE_NS.load(Ordering::Relaxed),
        parse_n: PARSE_N.load(Ordering::Relaxed),
        execution_n: EXECUTION_N.load(Ordering::Relaxed),
        decode_n: DECODE_N.load(Ordering::Relaxed),
        serialize_n: SERIALIZE_N.load(Ordering::Relaxed),
    }
}

/// Zero all path-timing counters (for harnesses / A-B experiments).
pub fn reset_path_timing() {
    PARSE_NS.store(0, Ordering::Relaxed);
    EXECUTION_NS.store(0, Ordering::Relaxed);
    DECODE_NS.store(0, Ordering::Relaxed);
    SERIALIZE_NS.store(0, Ordering::Relaxed);
    PARSE_N.store(0, Ordering::Relaxed);
    EXECUTION_N.store(0, Ordering::Relaxed);
    DECODE_N.store(0, Ordering::Relaxed);
    SERIALIZE_N.store(0, Ordering::Relaxed);
}
