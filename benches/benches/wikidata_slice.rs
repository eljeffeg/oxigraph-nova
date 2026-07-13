//! Benchmarks comparing two storage backends on a synthetic Wikidata-style dataset.
//!
//! # What is being compared?
//!
//! Two ways of storing and querying knowledge-graph triples:
//!
//! - **RingStore + LFTJ** — the new backend: data is stored in sorted arrays and
//!   queries use a "Leapfrog TrieJoin" (LFTJ) algorithm that seeks through sorted
//!   data rather than scanning everything.
//!
//! - **MemoryStore** — a simple baseline: data lives in a flat list (`Vec<Quad>`)
//!   and joins work by nested loops.
//!
//! # Test dataset
//!
//! We generate N fake "Wikidata-style" entities, each with 5 triples:
//!
//! ```text
//! Entity Q{i}  is-a          class{i % 10}      (10 classes total)
//! Entity Q{i}  located-in    region{i % 100}    (100 regions total)
//! Entity Q{i}  related-to    Q{(i+1) % N}       (3 outgoing links per entity)
//! Entity Q{i}  related-to    Q{(i+2) % N}
//! Entity Q{i}  related-to    Q{(i+3) % N}
//! ```
//!
//! Total: 5 × N triples. The `related-to` links form a dense directed graph
//! with ~3 × N triangles (cycles of length 3).
//!
//! # Benchmark groups
//!
//! | Group | What it measures | Expected result rows |
//! |---|---|---|
//! | `ingest` | How fast triples can be inserted into the delta write buffer | — |
//! | `compact_build_index` | How fast RingStore builds 6 LOUDS height-3 tries from the delta buffer — the one-time cost after bulk loading that enables O(log ℓ) LFTJ seeks | — |
//! | `query/scan` | Simple lookup: all entities and their class | N |
//! | `query/2join` | Two-condition filter: class-0 entities with their region | N/10 |
//! | `query/star` | Three properties of every entity at once | 3×N |
//! | `query/path` | Two-hop traversal: friend-of-a-friend | 9×N |
//! | `query/triangle` | Cyclic 3-way join: find all triangles | ≈3×N |
//!
//! # Why the triangle benchmark matters
//!
//! Triangle detection — "find A→B→C→A" — is a classic hard case for query
//! engines because every candidate must be checked against *all three* edges.
//!
//! - **MemoryStore** checks each candidate with a linear scan: O(N²) total work.
//! - **RingStore + LFTJ** seeks to the right position in sorted arrays: O(N · log N).
//!
//! At N = 10,000 that is a theoretical ~770× speed advantage for LFTJ.
//! The benchmark makes this gap visible and measurable.
//!
//! # Dataset sizes used
//!
//! RingStore is fast enough to benchmark at N = 10,000 (50,000 triples).
//! MemoryStore's quadratic scaling makes N = 10,000 impractical for multi-join
//! queries (each iteration would take > 2 seconds), so it is capped at N = 1,000
//! for query benchmarks. The single data point is enough to confirm the trend.
//!
//! # Running
//!
//! ```bash
//! # Run all benchmarks (HTML report written to target/criterion/)
//! cargo bench -p oxigraph-nova-bench
//!
//! # Run triangle comparison only
//! cargo bench -p oxigraph-nova-bench -- query/triangle
//!
//! # Print memory footprint estimates
//! cargo run -p oxigraph-nova-bench --example memory_report --release
//! ```

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxigraph_nova_core::{GraphName, NamedNode, Quad, QuadStore, Subject, Term};
use oxigraph_nova_query::{Dataset, Evaluator, QueryResult, StoreDataset};
use oxigraph_nova_storage_memory::MemoryStore;
use oxigraph_nova_storage_ring::RingStore;
use spargebra::SparqlParser;
use std::hint::black_box;
use std::sync::Arc;

// ── Namespace constants ───────────────────────────────────────────────────────

const NS_ENTITY: &str = "https://www.wikidata.org/entity/";
const P_P31: &str = "https://www.wikidata.org/prop/direct/P31"; // instance-of
const P_P131: &str = "https://www.wikidata.org/prop/direct/P131"; // located-in
const P_REL: &str = "https://www.wikidata.org/prop/direct/related"; // synthetic relation

/// SPARQL PREFIX block shared by all query strings.
/// Expands: wdt:P31 → P_P31, wdt:related → P_REL, wd:class0 → NS_ENTITY+"class0", etc.
const PREFIXES: &str = concat!(
    "PREFIX wd:  <https://www.wikidata.org/entity/>\n",
    "PREFIX wdt: <https://www.wikidata.org/prop/direct/>\n",
);

// ── Synthetic data generation ─────────────────────────────────────────────────

/// Build the IRI for entity i: `wd:Q{i}`.
#[inline]
fn entity_iri(i: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS_ENTITY}Q{i}"))
}

/// Build the IRI for class j: `wd:class{j}`.
#[inline]
fn class_iri(j: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS_ENTITY}class{j}"))
}

/// Build the IRI for region k: `wd:region{k}`.
#[inline]
fn region_iri(k: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS_ENTITY}region{k}"))
}

/// Generate 5×N Wikidata-style triples for N entities in the default graph.
///
/// Each entity gets:
/// - `is-a` → one of 10 classes
/// - `located-in` → one of 100 regions
/// - `related-to` → three neighbors (offsets +1, +2, +3 mod N)
///
/// The `related-to` links produce ~3×N directed triangles overall.
pub fn generate_quads(n: usize) -> Vec<Quad> {
    assert!(
        n >= 4,
        "need at least 4 entities to avoid duplicate related edges"
    );

    let p31 = NamedNode::new_unchecked(P_P31);
    let p131 = NamedNode::new_unchecked(P_P131);
    let prel = NamedNode::new_unchecked(P_REL);
    let dg = GraphName::DefaultGraph;

    let mut quads = Vec::with_capacity(n * 5);
    for i in 0..n {
        let subj = Subject::NamedNode(entity_iri(i));

        quads.push(Quad::new(
            subj.clone(),
            p31.clone(),
            Term::NamedNode(class_iri(i % 10)),
            dg.clone(),
        ));
        quads.push(Quad::new(
            subj.clone(),
            p131.clone(),
            Term::NamedNode(region_iri(i % 100)),
            dg.clone(),
        ));
        quads.push(Quad::new(
            subj.clone(),
            prel.clone(),
            Term::NamedNode(entity_iri((i + 1) % n)),
            dg.clone(),
        ));
        quads.push(Quad::new(
            subj.clone(),
            prel.clone(),
            Term::NamedNode(entity_iri((i + 2) % n)),
            dg.clone(),
        ));
        quads.push(Quad::new(
            subj.clone(),
            prel.clone(),
            Term::NamedNode(entity_iri((i + 3) % n)),
            dg.clone(),
        ));
    }
    quads
}

// ── Store construction helpers ────────────────────────────────────────────────

/// Insert `quads` into a fresh `RingStore`, then compact.
///
/// Compact builds **6 LOUDS height-3 tries** (one per SPO ordering: SPO, SOP, PSO,
/// POS, OPS, OSP) from the mutable delta BTreeMap.  After compaction, the Ring
/// index is immutable and every LFTJ seek is O(1) navigation + O(log ℓ) leap search.
fn build_ring_store(quads: &[Quad]) -> Arc<RingStore> {
    let store = Arc::new(RingStore::new());
    for q in quads {
        store.insert(q).unwrap();
    }
    store.compact().unwrap();
    store
}

/// Insert `quads` into a fresh `MemoryStore`.
/// MemoryStore stores triples in a flat list; all queries use nested-loop evaluation.
fn build_memory_store(quads: &[Quad]) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::new());
    for q in quads {
        store.insert(q).unwrap();
    }
    store
}

// ── Query execution helper ────────────────────────────────────────────────────

/// Parse and run a SPARQL SELECT query, returning the total number of result rows.
///
/// Fully materializes all results so the entire query pipeline is timed —
/// not just the setup. Panics on parse or evaluation errors so failures are
/// immediately visible in benchmark output.
fn count_solutions<D: Dataset>(dataset: &D, sparql: &str) -> usize {
    let q = SparqlParser::new().parse_query(sparql).unwrap();
    let mut ev = Evaluator::new(dataset);
    match ev.evaluate(&q).unwrap() {
        QueryResult::Solutions { stream, .. } => stream.count(),
        QueryResult::Boolean(b) => b as usize,
        QueryResult::Triples(stream) => stream.count(),
    }
}

// ── Benchmark 1: Ingest throughput ───────────────────────────────────────────
//
// How fast can each backend insert triples from scratch?
//
// RingStore appends into a write buffer (O(log N) dictionary lookup + BTreeMap
// insert), so it scales well even at N = 50,000.
//
// MemoryStore does a full linear duplicate scan on every insert — O(N²) total.
// At N = 1,000 it is already ~27× slower than RingStore; at N = 10,000 a single
// benchmark iteration takes ~2.8 s (completely impractical for Criterion).
// MemoryStore is therefore only measured at N = 1,000. The scaling gap is already
// obvious from that one data point.

fn bench_ingest(c: &mut Criterion) {
    let mut group = c.benchmark_group("ingest");

    // RingStore scales well — benchmark all three sizes.
    for &n in &[1_000usize, 10_000, 50_000] {
        let quads = generate_quads(n);
        group.throughput(Throughput::Elements(quads.len() as u64));
        group.bench_with_input(BenchmarkId::new("ring", n), &quads, |b, quads| {
            b.iter(|| {
                let store = RingStore::new();
                for q in quads {
                    store.insert(q).unwrap();
                }
                black_box(store)
            });
        });
    }

    // MemoryStore: only N = 1,000 is practical due to O(N²) duplicate scanning.
    {
        let n = 1_000usize;
        let quads = generate_quads(n);
        group.throughput(Throughput::Elements(quads.len() as u64));
        group.bench_with_input(BenchmarkId::new("memory", n), &quads, |b, quads| {
            b.iter(|| {
                let store = MemoryStore::new();
                for q in quads {
                    store.insert(q).unwrap();
                }
                black_box(store)
            });
        });
    }

    group.finish();
}

// ── Benchmark 2: Index build (compact) ───────────────────────────────────────
//
// RingStore writes accumulate in a mutable BTreeMap delta buffer first.
// `compact()` is the "flush" step — like an LSM-tree compaction — that:
//   1. Sorts and deduplicates all triples
//   2. Builds compact local-ID vocabularies (S, P, O)
//   3. Constructs 6 LOUDS height-3 tries (one per SPO ordering)
//
// After compact() runs, the index is immutable and LFTJ navigation is O(1)
// per trie level (child/degree/access via LOUDS rank/select) and O(log ℓ)
// per seek (exponential search in the flat label array L).
//
// This benchmark times ONLY the compact() step; insertion into the delta
// buffer is setup work done outside the timed region.

fn bench_compact(c: &mut Criterion) {
    let mut group = c.benchmark_group("compact_build_index");

    for &n in &[1_000usize, 10_000, 50_000] {
        let quads = generate_quads(n);
        group.throughput(Throughput::Elements(quads.len() as u64));

        group.bench_with_input(BenchmarkId::new("ring", n), &quads, |b, quads| {
            b.iter_batched(
                || {
                    // Setup (not timed): insert all triples into a fresh store.
                    let store = RingStore::new();
                    for q in quads {
                        store.insert(q).unwrap();
                    }
                    store
                },
                |store| {
                    // Timed: sort + deduplicate + build 6 LOUDS height-3 tries.
                    store.compact().unwrap();
                    black_box(store)
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

// ── Query benchmark sizes ─────────────────────────────────────────────────────
//
// RingStore + LFTJ is O(N · log N) on join queries, so N = 10,000 runs quickly
// and gives a clear signal.
//
// MemoryStore uses nested-loop evaluation; multi-join queries are O(N²). At
// N = 10,000 each star/path/triangle iteration exceeds 2 s. We cap MemoryStore
// at N = 1,000 for all query benchmarks — the quadratic scaling is already
// visible from the single data point.
//
// Datasets are built once outside the timing loop; the timed region is query
// evaluation only. `count_solutions` forces full result materialization.

/// RingStore query benchmark: 10,000 entities → 50,000 triples.
const RING_QUERY_N: usize = 10_000;

/// MemoryStore query benchmark: 1,000 entities → 5,000 triples.
/// Capped here due to O(N²) nested-loop join cost (see note above).
const MEM_QUERY_N: usize = 1_000;

// ── Benchmark 3: Simple predicate scan ───────────────────────────────────────
//
// Fetch every entity along with its class — one result row per entity.
// This is the simplest possible query: a single triple pattern with no joins.
// Both backends should handle this efficiently; any gap shows raw iteration cost.

fn bench_query_scan(c: &mut Criterion) {
    let ring_quads = generate_quads(RING_QUERY_N);
    let ring_ds = StoreDataset::new(build_ring_store(&ring_quads));
    let mem_quads = generate_quads(MEM_QUERY_N);
    let mem_ds = StoreDataset::new(build_memory_store(&mem_quads));

    // Expected: N rows (one instance-of triple per entity).
    let sparql = format!("{PREFIXES}SELECT * WHERE {{ ?s wdt:P31 ?o }}");

    let mut group = c.benchmark_group("query/scan");
    group.throughput(Throughput::Elements(RING_QUERY_N as u64));

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ring_ds, &sparql)))
    });
    group.bench_function("memory", |b| {
        b.iter(|| black_box(count_solutions(&mem_ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 4: Two-condition join ──────────────────────────────────────────
//
// Find all entities that belong to class-0, then look up each one's region.
// This is a 2-triple join: the result of the first pattern feeds into the second.
// Expected rows: N / 10 (one-tenth of entities are in class-0).

fn bench_query_2join(c: &mut Criterion) {
    let ring_quads = generate_quads(RING_QUERY_N);
    let ring_ds = StoreDataset::new(build_ring_store(&ring_quads));
    let mem_quads = generate_quads(MEM_QUERY_N);
    let mem_ds = StoreDataset::new(build_memory_store(&mem_quads));

    let sparql = format!(
        "{PREFIXES}SELECT ?s ?region WHERE {{ \
            ?s wdt:P31 wd:class0 . \
            ?s wdt:P131 ?region \
        }}"
    );

    let mut group = c.benchmark_group("query/2join");
    group.throughput(Throughput::Elements((RING_QUERY_N / 10) as u64));

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ring_ds, &sparql)))
    });
    group.bench_function("memory", |b| {
        b.iter(|| black_box(count_solutions(&mem_ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 5: Three-property star join ────────────────────────────────────
//
// Fetch class, region, and all related neighbors for every entity in one query.
// All three triple patterns share the same subject variable (?s), making this a
// "star" shape. Expected rows: 3×N (each entity has 3 related neighbors).
//
// MemoryStore is capped at MEM_QUERY_N = 1,000 (see note above).

fn bench_query_star(c: &mut Criterion) {
    let ring_quads = generate_quads(RING_QUERY_N);
    let ring_ds = StoreDataset::new(build_ring_store(&ring_quads));
    let mem_quads = generate_quads(MEM_QUERY_N);
    let mem_ds = StoreDataset::new(build_memory_store(&mem_quads));

    let sparql = format!(
        "{PREFIXES}SELECT * WHERE {{ \
            ?s wdt:P31 ?class . \
            ?s wdt:P131 ?region . \
            ?s wdt:related ?friend \
        }}"
    );

    let mut group = c.benchmark_group("query/star");
    group.throughput(Throughput::Elements((RING_QUERY_N * 3) as u64));

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ring_ds, &sparql)))
    });
    group.bench_function("memory", |b| {
        b.iter(|| black_box(count_solutions(&mem_ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 6: Two-hop path (friend-of-a-friend) ───────────────────────────
//
// Traverse the `related` graph two hops: find every (A, B, C) where A→B and B→C.
// Each entity has 3 outgoing links, so each (A, B) pair fans out to 3 values of C:
//   N × 3 pairs × 3 destinations = 9×N result rows.
//
// This is a chain join — the output of the first pattern feeds into the second.
// MemoryStore is capped at MEM_QUERY_N = 1,000 (see note above).

fn bench_query_path(c: &mut Criterion) {
    let ring_quads = generate_quads(RING_QUERY_N);
    let ring_ds = StoreDataset::new(build_ring_store(&ring_quads));
    let mem_quads = generate_quads(MEM_QUERY_N);
    let mem_ds = StoreDataset::new(build_memory_store(&mem_quads));

    let sparql = format!(
        "{PREFIXES}SELECT ?a ?b ?c WHERE {{ \
            ?a wdt:related ?b . \
            ?b wdt:related ?c \
        }}"
    );

    let mut group = c.benchmark_group("query/path");
    group.throughput(Throughput::Elements((RING_QUERY_N * 9) as u64));

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ring_ds, &sparql)))
    });
    group.bench_function("memory", |b| {
        b.iter(|| black_box(count_solutions(&mem_ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 7: Triangle detection (the LFTJ showcase) ──────────────────────
//
// Find every directed triangle in the `related` graph:
//   A → B,  B → C,  A → C  (all three must hold simultaneously)
//
// With each entity having 3 outgoing links (offsets +1, +2, +3), the triangles
// are (i, i+1, i+2), (i, i+1, i+3), and (i, i+2, i+3) — roughly 3×N total.
//
// This is the canonical "hard case" for join algorithms because the query is
// *cyclic* — each variable appears in more than one join condition.
//
// Performance comparison (out-degree d = 3):
//   MemoryStore: checks every (A,B,C) candidate against the full triple list  → O(N²)
//   RingStore + LFTJ: seeks directly to matching positions in sorted arrays   → O(N · log N)
//
// At N = 10,000, the theoretical advantage is ~770×. This benchmark makes that
// gap concrete and measurable.
//
// MemoryStore is capped at MEM_QUERY_N = 1,000 (see note above).

fn bench_query_triangle(c: &mut Criterion) {
    let ring_quads = generate_quads(RING_QUERY_N);
    let ring_ds = StoreDataset::new(build_ring_store(&ring_quads));
    let mem_quads = generate_quads(MEM_QUERY_N);
    let mem_ds = StoreDataset::new(build_memory_store(&mem_quads));

    let sparql = format!(
        "{PREFIXES}SELECT ?a ?b ?c WHERE {{ \
            ?a wdt:related ?b . \
            ?b wdt:related ?c . \
            ?a wdt:related ?c \
        }}"
    );

    let mut group = c.benchmark_group("query/triangle");

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ring_ds, &sparql)))
    });
    group.bench_function("memory", |b| {
        b.iter(|| black_box(count_solutions(&mem_ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 8: Bound-endpoint property paths (bidirectional search) ───────
//
// Validates bidirectional search: for `p+`/`p*`
// property paths where at least one endpoint is a bound constant, the Ring
// evaluator should use endpoint-aware BFS (bidirectional meet-in-the-middle
// when both ends are bound, backward BFS when only the target is bound)
// instead of materializing the whole transitive closure.
//
// The synthetic `related` graph has branching factor 3 (each entity links to
// i+1, i+2, i+3 mod N), so the number of nodes reachable within k hops grows
// roughly as 3^k — meaning any two connected nodes are typically only a few
// hops apart even though N is large. This is exactly the shape where
// meet-in-the-middle search pays off: two BFS frontiers each covering k/2
// hops (≈3^(k/2) nodes each) do dramatically less work than one frontier
// covering the full k hops (≈3^k nodes), or a naive approach that must first
// enumerate all N subjects of `related` before running BFS from each one.
//
// - `bound_both`: `ASK { wd:Q0 wdt:related+ wd:Q40 }` — both endpoints bound;
//   Q40 is reachable from Q0 in a handful of hops (branching factor 3 means
//   ~4 hops covers up to 3^4 = 81 nodes), exercising `bidirectional_reachable`.
// - `bound_target`: `SELECT ?a WHERE { ?a wdt:related+ wd:Q40 }` — only the
//   target is bound; exercises backward BFS (`ring_bfs_transitive_backward`)
//   instead of enumerating all ~N subjects with predicate `related` first.

fn bench_query_path_bound(c: &mut Criterion) {
    let ring_quads = generate_quads(RING_QUERY_N);
    let ring_ds = StoreDataset::new(build_ring_store(&ring_quads));

    let ask_sparql = format!("{PREFIXES}ASK {{ wd:Q0 wdt:related+ wd:Q40 }}");
    let target_sparql = format!("{PREFIXES}SELECT ?a WHERE {{ ?a wdt:related+ wd:Q40 }}");

    let mut group = c.benchmark_group("query/path_bound");
    group.throughput(Throughput::Elements(1));

    group.bench_function("ring_bound_both", |b| {
        b.iter(|| black_box(count_solutions(&ring_ds, &ask_sparql)))
    });
    group.bench_function("ring_bound_target_only", |b| {
        b.iter(|| black_box(count_solutions(&ring_ds, &target_sparql)))
    });
    group.finish();
}

// ── Benchmark 9: Parallel evaluator paths (ORDER BY / GROUP BY / UNION) ──────
//
// These benchmarks exercise `evaluator.rs`'s rayon-parallelized code paths
// directly (`GP::OrderBy`'s par_sort, `eval_group`'s per-group aggregate
// computation, and `GP::Union`'s two-branch fork) at row counts on both
// sides of `PARALLEL_ROW_THRESHOLD` (10_000, see
// `crates/nova-query/src/evaluator.rs`). Used to tune that constant against
// real timing data rather than a guess.
//
// All three queries here use a single triple pattern (no join), so both
// backends would evaluate the *pattern* itself in O(N) with no meaningful
// gap between them -- what's being measured is the cost of the
// post-pattern parallel step (sorting/grouping/unioning), which is
// evaluator-side and backend-agnostic. Only RingStore is benchmarked here
// to keep focus on that step and avoid doubling runtime for a comparison
// that wouldn't be informative.
//
// `PARALLEL_SMALL_N` produces row counts safely below the threshold (the
// sequential path is taken); `PARALLEL_LARGE_N` safely above it (the rayon
// path is taken). Comparing the two shows whether the threshold is set at a
// sensible crossover point -- i.e. that the large case actually benefits
// from going parallel, and the small case isn't paying rayon dispatch
// overhead for too little work.

/// Row count safely below `PARALLEL_ROW_THRESHOLD` (10_000) -- exercises the
/// sequential fallback path in `ORDER BY`/`GROUP BY`.
const PARALLEL_SMALL_N: usize = 3_000;

/// Row count safely above `PARALLEL_ROW_THRESHOLD` (10_000) -- exercises the
/// rayon-parallel path in `ORDER BY`/`GROUP BY`.
const PARALLEL_LARGE_N: usize = 30_000;

/// `ORDER BY` over N rows -- exercises `GP::OrderBy`'s `par_sort_by` once N
/// crosses `PARALLEL_ROW_THRESHOLD`.
fn bench_query_orderby(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/orderby");

    for &n in &[PARALLEL_SMALL_N, PARALLEL_LARGE_N] {
        let quads = generate_quads(n);
        let ds = StoreDataset::new(build_ring_store(&quads));
        let sparql = format!("{PREFIXES}SELECT * WHERE {{ ?s wdt:P31 ?class }} ORDER BY ?s");

        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("ring", n), &ds, |b, ds| {
            b.iter(|| black_box(count_solutions(ds, &sparql)))
        });
    }
    group.finish();
}

/// `GROUP BY ?class` with a `COUNT` aggregate. Always exactly 10 groups
/// (class0..class9) regardless of N -- this is the "few groups, many rows
/// per group" shape task (b) targets: `groups.len()` would never cross the
/// threshold on its own, but total row count does once N is large enough,
/// so this benchmark specifically validates gating on total rows rather
/// than group count.
fn bench_query_groupby(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/groupby");

    for &n in &[PARALLEL_SMALL_N, PARALLEL_LARGE_N] {
        let quads = generate_quads(n);
        let ds = StoreDataset::new(build_ring_store(&quads));
        let sparql = format!(
            "{PREFIXES}SELECT ?class (COUNT(?s) AS ?c) WHERE {{ ?s wdt:P31 ?class }} GROUP BY ?class"
        );

        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("ring", n), &ds, |b, ds| {
            b.iter(|| black_box(count_solutions(ds, &sparql)))
        });
    }
    group.finish();
}

/// Two-branch `UNION`, each branch contributing N rows (2N total) --
/// exercises `GP::Union`'s unconditional `rayon::join` fork of the two
/// branches at both a small and large N, since that call site forks
/// regardless of size (there's no row count to threshold on until after
/// both branches have already been evaluated).
fn bench_query_union(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/union");

    for &n in &[PARALLEL_SMALL_N, PARALLEL_LARGE_N] {
        let quads = generate_quads(n);
        let ds = StoreDataset::new(build_ring_store(&quads));
        let sparql = format!(
            "{PREFIXES}SELECT * WHERE {{ {{ ?s wdt:P31 ?class }} UNION {{ ?s wdt:P131 ?region }} }}"
        );

        group.throughput(Throughput::Elements((n * 2) as u64));
        group.bench_with_input(BenchmarkId::new("ring", n), &ds, |b, ds| {
            b.iter(|| black_box(count_solutions(ds, &sparql)))
        });
    }
    group.finish();
}

// ── Criterion registration ────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_ingest,
    bench_compact,
    bench_query_scan,
    bench_query_2join,
    bench_query_star,
    bench_query_path,
    bench_query_path_bound,
    bench_query_triangle,
    bench_query_orderby,
    bench_query_groupby,
    bench_query_union,
);
criterion_main!(benches);
