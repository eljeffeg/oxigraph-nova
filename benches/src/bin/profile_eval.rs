//! Ad-hoc profiling tool: loads the 500K-entity benchmark dataset into a
//! LoudsStore and times pure in-process query evaluation (no HTTP, no JSON
//! serialization) for the exact same queries used by the external HTTP
//! benchmark (`benches/external/run_comparison.sh`), read directly from
//! `dataset.queries.json` to guarantee byte-identical SPARQL text. Used to
//! isolate whether the bottleneck is LFTJ evaluation itself or the
//! HTTP/serialization layer around it.
//!
//! Usage:
//!   cargo run -p oxigraph-nova-bench --release --bin profile_eval -- \
//!       /tmp/oxigraph-nova-bench/dataset.nt /tmp/oxigraph-nova-bench/dataset.queries.json
//!
//! Pass `--count-allocs` (anywhere in the argument list) to additionally wrap
//! the global allocator with an atomic counting shim and print an exact
//! allocation-count/byte-total breakdown per query, instead of relying on
//! noisy wall-clock timing to detect small per-row allocation changes —
//! wall-clock A/B alone cannot reliably distinguish small per-row
//! allocation-count differences from ordinary run-to-run noise.

use oxigraph_nova_core::{GraphName, Quad};
use oxigraph_nova_query::{Dataset, Evaluator, QueryResult, StoreDataset};
use oxigraph_nova_storage_ring::LoudsStore;
use oxttl::NTriplesParser;
use serde::Deserialize;
use spargebra::SparqlParser;
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// A `GlobalAlloc` shim that delegates to `System` but records alloc/dealloc
/// counts and byte totals in atomics. Always installed as the process's
/// global allocator (a `#[global_allocator]` can't be swapped at runtime),
/// but the counters are cheap (a handful of atomic adds per call) and only
/// read/reset when `--count-allocs` is passed, so this has no effect on
/// normal wall-clock timing runs.
struct CountingAllocator;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static DEALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static DEALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static REALLOC_COUNT: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        DEALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        REALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        // Account for realloc as a net delta so ALLOC_BYTES/DEALLOC_BYTES
        // still roughly track live bytes; the count is tracked separately
        // since a realloc is neither a fresh alloc nor a plain dealloc.
        if new_size > layout.size() {
            ALLOC_BYTES.fetch_add((new_size - layout.size()) as u64, Ordering::Relaxed);
        } else if new_size < layout.size() {
            DEALLOC_BYTES.fetch_add((layout.size() - new_size) as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Snapshot of the counting allocator's atomics at a point in time.
#[derive(Clone, Copy, Debug, Default)]
struct AllocStats {
    allocs: u64,
    alloc_bytes: u64,
    deallocs: u64,
    dealloc_bytes: u64,
    reallocs: u64,
}

fn alloc_stats_snapshot() -> AllocStats {
    AllocStats {
        allocs: ALLOC_COUNT.load(Ordering::Relaxed),
        alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
        deallocs: DEALLOC_COUNT.load(Ordering::Relaxed),
        dealloc_bytes: DEALLOC_BYTES.load(Ordering::Relaxed),
        reallocs: REALLOC_COUNT.load(Ordering::Relaxed),
    }
}

impl std::ops::Sub for AllocStats {
    type Output = AllocStats;
    fn sub(self, rhs: AllocStats) -> AllocStats {
        AllocStats {
            allocs: self.allocs.saturating_sub(rhs.allocs),
            alloc_bytes: self.alloc_bytes.saturating_sub(rhs.alloc_bytes),
            deallocs: self.deallocs.saturating_sub(rhs.deallocs),
            dealloc_bytes: self.dealloc_bytes.saturating_sub(rhs.dealloc_bytes),
            reallocs: self.reallocs.saturating_sub(rhs.reallocs),
        }
    }
}

#[derive(Deserialize)]
struct QueryDef {
    name: String,
    sparql: String,
}

fn run_query<D: Dataset>(ds: &D, name: &str, sparql: &str, iters: usize, count_allocs: bool) {
    let q = SparqlParser::new().parse_query(sparql).unwrap();
    // warmup
    for _ in 0..2 {
        let ev = Evaluator::new(ds);
        let _ = ev.evaluate(&q).unwrap();
    }
    let mut times = Vec::with_capacity(iters);
    let mut n_results = 0usize;
    // Only meaningful when count_allocs is set; tracks per-iteration alloc
    // counts so we can report a mean alongside the wall-clock numbers.
    let mut alloc_counts = Vec::with_capacity(iters);
    let mut alloc_byte_totals = Vec::with_capacity(iters);
    for i in 0..iters {
        let ev = Evaluator::new(ds);
        let before = count_allocs.then(alloc_stats_snapshot);
        let t0 = Instant::now();
        let result = ev.evaluate(&q).unwrap();
        let elapsed = t0.elapsed();
        if let Some(before) = before {
            let after = alloc_stats_snapshot();
            let delta = after - before;
            alloc_counts.push(delta.allocs);
            alloc_byte_totals.push(delta.alloc_bytes);
            eprintln!(
                "  [{name}] iter={i} alloc_bytes={} dealloc_bytes={} leaked_delta={}",
                delta.alloc_bytes,
                delta.dealloc_bytes,
                delta.alloc_bytes as i64 - delta.dealloc_bytes as i64
            );
        }
        n_results = match result {
            QueryResult::Solutions { stream, .. } => stream.count(),
            QueryResult::Boolean(b) => b as usize,
            QueryResult::Triples(stream) => stream.count(),
        };
        times.push(elapsed.as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    let p50 = times[times.len() / 2];
    println!(
        "[{name}] rows={n_results} iters={iters} mean={mean:.2}ms p50={p50:.2}ms min={:.2}ms max={:.2}ms",
        times[0],
        times[times.len() - 1]
    );
    if count_allocs {
        let mean_allocs = alloc_counts.iter().sum::<u64>() as f64 / alloc_counts.len() as f64;
        let mean_bytes =
            alloc_byte_totals.iter().sum::<u64>() as f64 / alloc_byte_totals.len() as f64;
        let per_row_allocs = if n_results > 0 {
            mean_allocs / n_results as f64
        } else {
            0.0
        };
        println!(
            "  [{name}] allocs: mean={mean_allocs:.0} ({per_row_allocs:.2}/row) bytes={mean_bytes:.0} min_allocs={} max_allocs={}",
            alloc_counts.iter().min().unwrap(),
            alloc_counts.iter().max().unwrap(),
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let count_allocs = args.iter().any(|a| a == "--count-allocs");
    let positional: Vec<&String> = args
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let path = positional
        .first()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "/tmp/oxigraph-nova-bench/dataset.nt".to_string());
    let queries_path = positional
        .get(1)
        .map(|s| s.to_string())
        .unwrap_or_else(|| "/tmp/oxigraph-nova-bench/dataset.queries.json".to_string());

    eprintln!("[profile_eval] Loading {path} ...");
    let t0 = Instant::now();
    let file = File::open(&path).expect("open dataset");
    let reader = BufReader::new(file);
    let mut parsed = 0usize;
    let quads = NTriplesParser::new().for_reader(reader).map(|r| {
        let t = r.expect("parse error");
        parsed += 1;
        if parsed.is_multiple_of(1_000_000) {
            eprintln!("[profile_eval]   ... {parsed} triples parsed");
        }
        Quad::new(t.subject, t.predicate, t.object, GraphName::DefaultGraph)
    });
    let store = LoudsStore::new();
    let count = store.bulk_load(quads).expect("bulk_load failed");
    eprintln!(
        "[profile_eval] Loaded + compacted {count} triples in {:.2}s.",
        t0.elapsed().as_secs_f64()
    );

    let store = Arc::new(store);
    let ds = StoreDataset::new(Arc::clone(&store));

    let queries_json = std::fs::read_to_string(&queries_path).expect("read queries.json");
    let queries: Vec<QueryDef> = serde_json::from_str(&queries_json).expect("parse queries.json");

    if count_allocs {
        println!(
            "\n=== Pure in-process evaluation (no HTTP, no JSON serialization) — counting-allocator mode ===\n"
        );
    } else {
        println!("\n=== Pure in-process evaluation (no HTTP, no JSON serialization) ===\n");
    }

    // `--rss-loop <query_name> <n>`: repeatedly evaluate a single named query
    // `n` times, printing this process's own RSS (via `ps -o rss=`, matching
    // the same metric external benchmark scripts poll via `vmmap -summary`)
    // after every iteration. Used to distinguish a genuine Rust-level leak
    // (RSS growing without bound even though the counting allocator shows
    // every iteration alloc/dealloc-ing an identical number of bytes) from
    // ordinary libsystem_malloc page retention (RSS grows but the counting
    // allocator already proves nothing is actually leaked at the Rust level).
    if let Some(pos) = args.iter().position(|a| a == "--rss-loop") {
        let query_name = args.get(pos + 1).expect("--rss-loop needs <query_name>");
        let n: usize = args
            .get(pos + 2)
            .expect("--rss-loop needs <n>")
            .parse()
            .expect("n must be a number");
        let qd = queries
            .iter()
            .find(|q| &q.name == query_name)
            .unwrap_or_else(|| panic!("no such query: {query_name}"));
        let q = SparqlParser::new().parse_query(&qd.sparql).unwrap();
        let pid = std::process::id();
        println!("[rss-loop] pid={pid} query={query_name} n={n}");
        for i in 0..n {
            let ev = Evaluator::new(&ds);
            let result = ev.evaluate(&q).unwrap();
            let rows = match result {
                QueryResult::Solutions { stream, .. } => stream.count(),
                QueryResult::Boolean(b) => b as usize,
                QueryResult::Triples(stream) => stream.count(),
            };
            let _ = rows;

            let rss_kb = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            println!(
                "[rss-loop] iter={i} rows={rows} rss_mb={:.1}",
                rss_kb as f64 / 1024.0
            );
        }
        return;
    }

    for qd in &queries {
        // Fewer iterations for expensive queries to keep total runtime reasonable.
        let iters = match qd.name.as_str() {
            "scan" | "path_2hop" | "triangle" => 5,
            _ => 10,
        };
        run_query(&ds, &qd.name, &qd.sparql, iters, count_allocs);
    }
}
